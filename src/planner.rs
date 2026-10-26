//! Immutable, account-bound source inventory with durable per-file hash reuse.
use crate::{
    api::Api,
    hash,
    model::{Capabilities, FileStamp, PART_SIZE, PlannedFile, UploadPlan, WEB_UPLOAD_LIMIT},
    state::Store,
};
use anyhow::{Context, Result, ensure};
use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

const CACHE_SCHEMA: &str = "yike-source-hashes/v1";

#[derive(Serialize, Deserialize)]
struct CachedHash {
    schema: String,
    account: String,
    source: PlannedFile,
}

struct Candidate {
    path: PathBuf,
    relative_path: String,
    stamp: FileStamp,
    album_title: String,
}

pub async fn build(
    store: &Store,
    api: &Api,
    root: &Path,
    album_template: &str,
    file_limit: Option<usize>,
    cancel: CancellationToken,
) -> Result<UploadPlan> {
    ensure!(!cancel.is_cancelled(), "planning cancelled");
    let upload_limit = effective_upload_limit(&api.capabilities().await?)?;
    build_at_limit(
        store,
        &api.account(),
        root,
        album_template,
        file_limit,
        cancel,
        upload_limit,
    )
    .await
}

fn effective_upload_limit(capabilities: &Capabilities) -> Result<u64> {
    let actual = capabilities
        .upload_limit
        .context("actual account upload limit is unknown; cannot create a safe plan")?;
    ensure!(actual > 0, "account reports no supported upload size");
    Ok(actual.min(WEB_UPLOAD_LIMIT))
}

async fn build_at_limit(
    store: &Store,
    account: &str,
    root: &Path,
    album_template: &str,
    file_limit: Option<usize>,
    cancel: CancellationToken,
    upload_limit: u64,
) -> Result<UploadPlan> {
    ensure!(!cancel.is_cancelled(), "planning cancelled");
    ensure!(
        account.len() == 64 && account.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid account fingerprint"
    );
    ensure!(
        upload_limit > 0 && upload_limit <= WEB_UPLOAD_LIMIT,
        "unsupported upload limit"
    );
    ensure!(
        file_limit != Some(0),
        "rehearsal file limit must be greater than zero"
    );
    ensure!(
        !album_template.trim().is_empty(),
        "album title must not be empty"
    );
    let placeholders_removed = album_template.replace("{date}", "");
    ensure!(
        !placeholders_removed.contains(['{', '}']),
        "unknown album template placeholder; only {{date}} is supported"
    );
    let root_metadata = fs::symlink_metadata(root).context("cannot inspect source root")?;
    ensure!(
        !root_metadata.file_type().is_symlink(),
        "source root must not be a symlink"
    );
    ensure!(
        root_metadata.is_dir() || root_metadata.is_file(),
        "source root must be a directory or MP4 file"
    );
    let root = root.canonicalize().context("cannot resolve source root")?;
    let mut candidates = collect(&root, root_metadata.is_file(), album_template, &cancel)?;
    ensure!(!candidates.is_empty(), "source contains no MP4 files");
    if let Some(limit) = file_limit {
        candidates.retain(|source| source.stamp.len > 0 && source.stamp.len <= upload_limit);
        candidates.sort_by(|a, b| {
            a.stamp
                .len
                .cmp(&b.stamp.len)
                .then_with(|| a.relative_path.cmp(&b.relative_path))
        });
        candidates.truncate(limit);
        ensure!(
            !candidates.is_empty(),
            "no eligible MP4 files for this rehearsal"
        );
    }
    let mut files = Vec::with_capacity(candidates.len());
    for source in candidates {
        ensure!(!cancel.is_cancelled(), "planning cancelled");
        ensure!(
            hash::stamp(&source.path)? == source.stamp,
            "source changed after inventory; restart planning"
        );
        let key = cache_key(account, &source.path, &source.stamp)?;
        let blocked = if source.stamp.len > upload_limit {
            Some(format!(
                "size {} exceeds validated account web upload limit {upload_limit}; original preserved",
                source.stamp.len
            ))
        } else if source.stamp.len == 0 {
            Some("empty MP4 file cannot be uploaded; original preserved".to_string())
        } else {
            None
        };
        if blocked.is_some() {
            files.push(PlannedFile {
                id: key,
                path: source.path,
                relative_path: source.relative_path,
                stamp: source.stamp,
                md5: String::new(),
                sha256: String::new(),
                slice_md5: String::new(),
                parts: Vec::new(),
                blocked,
                album_title: source.album_title,
            });
            continue;
        }
        let cached = store.get::<CachedHash>("hash_cache", &key)?;
        let planned =
            if let Some(entry) = cached.filter(|entry| cache_matches(entry, account, &source)) {
                let mut planned = entry.source;
                planned.relative_path = source.relative_path;
                planned.album_title = source.album_title;
                hash::assert_stamp(&planned)?;
                planned
            } else {
                let path = source.path.clone();
                let relative = source.relative_path;
                let album = source.album_title;
                let token = cancel.clone();
                let planned = tokio::task::spawn_blocking(move || {
                    hash::digest_file_cancellable(&path, relative, album, &token)
                })
                .await
                .context("source hashing task failed")??;
                ensure!(
                    planned.stamp == source.stamp,
                    "source changed after inventory; restart planning"
                );
                ensure!(!cancel.is_cancelled(), "planning cancelled");
                store.put(
                    "hash_cache",
                    &key,
                    &CachedHash {
                        schema: CACHE_SCHEMA.to_string(),
                        account: account.to_string(),
                        source: planned.clone(),
                    },
                )?;
                planned
            };
        files.push(planned);
    }
    ensure!(!cancel.is_cancelled(), "planning cancelled");
    for file in &files {
        hash::assert_stamp(file)?;
    }
    let plan = UploadPlan {
        schema: "yike-upload-plan/v1".to_string(),
        id: uuid::Uuid::new_v4().to_string(),
        account: account.to_string(),
        root,
        created_at: Utc::now().to_rfc3339(),
        upload_limit,
        files,
    };
    store.insert("upload_plans", &plan.id, &plan)?;
    Ok(plan)
}

fn collect(
    root: &Path,
    exact_file: bool,
    template: &str,
    cancel: &CancellationToken,
) -> Result<Vec<Candidate>> {
    if exact_file {
        ensure!(is_mp4(root), "exact source root must be an MP4 file");
    }
    let mut candidates = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .follow_root_links(false)
        .sort_by_file_name()
    {
        ensure!(!cancel.is_cancelled(), "planning cancelled");
        // Never filter_map errors: unreadable directories would silently narrow
        // the promised backup boundary and could create a false complete plan.
        let entry = entry.context("source inventory failed; no plan was created")?;
        ensure!(
            !entry.file_type().is_symlink(),
            "source tree contains a symlink; resolve the source boundary explicitly"
        );
        if entry.file_type().is_dir() || !is_mp4(entry.path()) {
            continue;
        }
        let path = entry.path().to_path_buf();
        let relative = if exact_file {
            Path::new(path.file_name().context("source has no filename")?)
        } else {
            path.strip_prefix(root).context("source escaped its root")?
        };
        let relative_path = relative
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .map(str::to_string)
                    .context("source path is not valid Unicode")
            })
            .collect::<Result<Vec<_>>>()?
            .join("/");
        let album_title = album_title(&path, if exact_file { None } else { Some(root) }, template)?;
        candidates.push(Candidate {
            stamp: hash::stamp(&path)?,
            path,
            relative_path,
            album_title,
        });
    }
    Ok(candidates)
}

fn is_mp4(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
}

fn album_title(path: &Path, boundary: Option<&Path>, template: &str) -> Result<String> {
    if !template.contains("{date}") {
        return Ok(template.to_string());
    }
    let mut dates = BTreeSet::new();
    for parent in path.parent().into_iter().flat_map(Path::ancestors) {
        if let Some(name) = parent.file_name().and_then(|name| name.to_str()) {
            for token in name
                .split(|character: char| !character.is_ascii_digit())
                .filter(|token| token.len() == 8)
            {
                NaiveDate::parse_from_str(token, "%Y%m%d").context(
                    "invalid YYYYMMDD directory date; cannot guess from modification time",
                )?;
                dates.insert(token.to_string());
            }
        }
        if boundary == Some(parent) {
            break;
        }
    }
    ensure!(
        dates.len() == 1,
        "album date is missing or ambiguous; use one known YYYYMMDD directory date or a literal album title"
    );
    let date = dates.first().context("album date is missing")?;
    Ok(template.replace("{date}", date))
}

fn cache_key(account: &str, path: &Path, stamp: &FileStamp) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(CACHE_SCHEMA.as_bytes());
    digest.update(PART_SIZE.to_le_bytes());
    digest.update(account.as_bytes());
    digest.update([0]);
    digest.update(path.as_os_str().as_encoded_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(stamp)?);
    Ok(hex::encode(digest.finalize()))
}

fn cache_matches(entry: &CachedHash, account: &str, source: &Candidate) -> bool {
    let file = &entry.source;
    let valid_hex =
        |text: &str, len| text.len() == len && text.bytes().all(|b| b.is_ascii_hexdigit());
    entry.schema == CACHE_SCHEMA
        && entry.account == account
        && file.path == source.path
        && file.stamp == source.stamp
        && file.blocked.is_none()
        && valid_hex(&file.md5, 32)
        && valid_hex(&file.sha256, 64)
        && valid_hex(&file.slice_md5, 32)
        && file.parts.len() as u64 == source.stamp.len.div_ceil(PART_SIZE as u64)
        && file.parts.iter().enumerate().all(|(index, part)| {
            part.len
                == (source.stamp.len - index as u64 * PART_SIZE as u64).min(PART_SIZE as u64)
                    as usize
                && valid_hex(&part.md5, 32)
                && valid_hex(&part.sha256, 64)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PlannedFile, WEB_UPLOAD_LIMIT};
    use std::fs;
    use tempfile::tempdir;

    fn account() -> String {
        "a".repeat(64)
    }

    #[test]
    fn rejects_unknown_or_zero_capabilities_and_caps_the_actual_limit() {
        let cap = |upload_limit| Capabilities {
            upload_limit,
            download_limit: None,
            unlimited: Some(true),
        };
        assert!(effective_upload_limit(&cap(None)).is_err());
        assert!(effective_upload_limit(&cap(Some(0))).is_err());
        assert_eq!(effective_upload_limit(&cap(Some(23))).unwrap(), 23);
        assert_eq!(
            effective_upload_limit(&cap(Some(WEB_UPLOAD_LIMIT * 2))).unwrap(),
            WEB_UPLOAD_LIMIT
        );
    }

    #[tokio::test]
    async fn full_plan_keeps_oversize_and_empty_metadata_and_persists_immutable_plan() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("video");
        let date = root.join("20000102");
        fs::create_dir_all(&date).unwrap();
        fs::write(date.join("small.Mp4"), b"abc").unwrap();
        fs::write(date.join("oversize.mp4"), b"0123456789").unwrap();
        fs::write(date.join("empty.mp4"), []).unwrap();
        fs::write(date.join("ignore.mov"), b"fixture").unwrap();
        let store = Store::open(&dir.path().join("private/state.sqlite3")).unwrap();
        let plan = build_at_limit(
            &store,
            &account(),
            &root,
            "Trip {date}",
            None,
            CancellationToken::new(),
            5,
        )
        .await
        .unwrap();
        assert_eq!(plan.files.len(), 3);
        assert_eq!(plan.files.iter().filter(|f| f.blocked.is_some()).count(), 2);
        let normal = plan.files.iter().find(|f| f.name() == "small.Mp4").unwrap();
        assert_eq!(normal.album_title, "Trip 20000102");
        assert_eq!(normal.relative_path, "20000102/small.Mp4");
        for file in plan.files.iter().filter(|f| f.blocked.is_some()) {
            assert!(file.parts.is_empty());
            assert!(file.sha256.is_empty());
        }
        let saved = store
            .get::<UploadPlan>("upload_plans", &plan.id)
            .unwrap()
            .unwrap();
        assert_eq!(saved.account, account());
        assert!(store.insert("upload_plans", &plan.id, &plan).is_err());
        assert_eq!(
            store.list::<serde_json::Value>("hash_cache").unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn rehearsal_selects_smallest_eligible_files_deterministically() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("20000102");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("z.mp4"), b"12").unwrap();
        fs::write(root.join("a.mp4"), b"12").unwrap();
        fs::write(root.join("b.mp4"), b"123").unwrap();
        fs::write(root.join("oversize.mp4"), b"123456").unwrap();
        let store = Store::open(&dir.path().join("private/state.sqlite3")).unwrap();
        let plan = build_at_limit(
            &store,
            &account(),
            &root,
            "Trip {date}",
            Some(1),
            CancellationToken::new(),
            5,
        )
        .await
        .unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].name(), "a.mp4");
        assert_eq!(
            store.list::<serde_json::Value>("hash_cache").unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn accepts_one_exact_file_root_for_the_rehearsal() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("20000102");
        fs::create_dir(&root).unwrap();
        let path = root.join("camera anonymous.MP4");
        fs::write(&path, b"fixture").unwrap();
        let store = Store::open(&dir.path().join("private/state.sqlite3")).unwrap();
        let plan = build_at_limit(
            &store,
            &account(),
            &path,
            "Trip {date}",
            Some(1),
            CancellationToken::new(),
            100,
        )
        .await
        .unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].relative_path, "camera anonymous.MP4");
        assert_eq!(plan.files[0].album_title, "Trip 20000102");
    }

    #[tokio::test]
    async fn missing_invalid_and_conflicting_dates_fail_without_guessing_mtime() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("private/state.sqlite3")).unwrap();
        for suffix in ["video", "20000230", "20000102/20000103"] {
            let root = dir.path().join("sources").join(suffix);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("a.mp4"), b"fixture").unwrap();
            let boundary = dir.path().join("sources");
            assert!(
                build_at_limit(
                    &store,
                    &account(),
                    &boundary,
                    "Trip {date}",
                    None,
                    CancellationToken::new(),
                    100
                )
                .await
                .is_err()
            );
            fs::remove_dir_all(&boundary).unwrap();
        }
        assert!(store.list::<UploadPlan>("upload_plans").unwrap().is_empty());
    }

    #[tokio::test]
    async fn hash_cache_survives_reopening_and_does_not_cross_accounts_or_changed_stamps() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("video");
        fs::create_dir(&root).unwrap();
        let path = root.join("匿名 source.mp4");
        fs::write(&path, b"fixture").unwrap();
        let state_path = dir.path().join("private/state.sqlite3");
        let store = Store::open(&state_path).unwrap();
        let first = build_at_limit(
            &store,
            &account(),
            &root,
            "Original",
            None,
            CancellationToken::new(),
            100,
        )
        .await
        .unwrap();
        drop(store);
        let store = Store::open(&state_path).unwrap();
        let second = build_at_limit(
            &store,
            &account(),
            &root,
            "Changed album",
            None,
            CancellationToken::new(),
            100,
        )
        .await
        .unwrap();
        assert_eq!(first.files[0].sha256, second.files[0].sha256);
        assert_eq!(first.files[0].id, second.files[0].id);
        assert_eq!(second.files[0].album_title, "Changed album");
        assert_eq!(
            store.list::<serde_json::Value>("hash_cache").unwrap().len(),
            1
        );
        build_at_limit(
            &store,
            &"b".repeat(64),
            &root,
            "Other account",
            None,
            CancellationToken::new(),
            100,
        )
        .await
        .unwrap();
        assert_eq!(
            store.list::<serde_json::Value>("hash_cache").unwrap().len(),
            2
        );
        fs::write(&path, b"changed length").unwrap();
        let changed = build_at_limit(
            &store,
            &account(),
            &root,
            "Original",
            None,
            CancellationToken::new(),
            100,
        )
        .await
        .unwrap();
        assert_ne!(first.files[0].sha256, changed.files[0].sha256);
        assert_eq!(
            store.list::<serde_json::Value>("hash_cache").unwrap().len(),
            3
        );
    }

    #[tokio::test]
    async fn cancellation_and_missing_root_never_insert_a_plan() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("private/state.sqlite3")).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(
            build_at_limit(&store, &account(), dir.path(), "Fixture", None, token, 100)
                .await
                .is_err()
        );
        assert!(
            build_at_limit(
                &store,
                &account(),
                &dir.path().join("missing"),
                "Fixture",
                None,
                CancellationToken::new(),
                100
            )
            .await
            .is_err()
        );
        assert!(
            store
                .list::<PlannedFile>("upload_plans")
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn source_symlinks_are_errors_instead_of_silent_omissions() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let root = dir.path().join("video");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.mp4"), b"fixture").unwrap();
        symlink(root.join("a.mp4"), root.join("linked.mp4")).unwrap();
        let store = Store::open(&dir.path().join("private/state.sqlite3")).unwrap();
        assert!(
            build_at_limit(
                &store,
                &account(),
                &root,
                "Fixture",
                None,
                CancellationToken::new(),
                100
            )
            .await
            .is_err()
        );
        assert!(store.list::<UploadPlan>("upload_plans").unwrap().is_empty());
    }

    #[tokio::test]
    async fn interruption_keeps_only_completed_file_hashes_without_publishing_a_plan() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("video");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a-small.mp4"), b"fixture").unwrap();
        let large = root.join("z-large.mp4");
        fs::File::create(&large)
            .unwrap()
            .set_len(32 * 1024 * 1024)
            .unwrap();
        let store = Store::open(&dir.path().join("private/state.sqlite3")).unwrap();
        let token = CancellationToken::new();
        let fingerprint = account();
        let planning = build_at_limit(
            &store,
            &fingerprint,
            &root,
            "Fixture",
            None,
            token.clone(),
            64 * 1024 * 1024,
        );
        let cancel_after_first = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if !store.list::<CachedHash>("hash_cache").unwrap().is_empty() {
                        token.cancel();
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        };
        let (result, ()) = tokio::join!(planning, cancel_after_first);
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        let cache = store.list::<CachedHash>("hash_cache").unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[0].1.source.name(), "a-small.mp4");
        assert!(store.list::<UploadPlan>("upload_plans").unwrap().is_empty());
        fs::remove_file(&large).unwrap();
        let recovered = build_at_limit(
            &store,
            &account(),
            &root,
            "Fixture",
            None,
            CancellationToken::new(),
            64 * 1024 * 1024,
        )
        .await
        .unwrap();
        assert_eq!(recovered.files[0].id, cache[0].1.source.id);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_nested_directory_does_not_silently_reduce_the_inventory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let root = dir.path().join("video");
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("a.mp4"), b"fixture").unwrap();
        let store = Store::open(&dir.path().join("private/state.sqlite3")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o0)).unwrap();
        let result = build_at_limit(
            &store,
            &account(),
            &root,
            "Fixture",
            None,
            CancellationToken::new(),
            100,
        )
        .await;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        // Root can bypass Unix DAC; do not mislabel that environment as a
        // successful unreadability test. CI/macOS run as an ordinary user.
        // SAFETY: geteuid takes no parameters and retains no pointers.
        if unsafe { libc::geteuid() } != 0 {
            assert!(result.unwrap_err().to_string().contains("inventory failed"));
            assert!(store.list::<UploadPlan>("upload_plans").unwrap().is_empty());
        }
    }
}
