//! Original downloads with owned partials and atomic, no-clobber publication.
use crate::{
    api::Api,
    hash,
    model::*,
    state::Store,
    upload::{self, TransferApi},
};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize)]
struct PartialReceipt {
    stamp: FileStamp,
    prefix_sha256: String,
}
fn item_key(job: &str, fsid: &str) -> String {
    format!("{job}:{fsid}")
}
fn byte_prefix(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}
fn safe_name(remote: &RemoteFile) -> String {
    let base: String = remote
        .name()
        .chars()
        .map(|c| {
            if c.is_control() || "/\\<>:\"|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let base = base.trim_matches(['.', ' ']);
    let base = if base.is_empty() { "media" } else { base };
    let (stem, extension) = base
        .rsplit_once('.')
        .filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
        .unwrap_or((base, ""));
    let extension = byte_prefix(extension, 16);
    let suffix = format!(
        "--{}{}{}",
        hex::encode(Sha256::digest(remote.fsid.as_bytes())),
        if extension.is_empty() { "" } else { "." },
        extension
    );
    // Reserve enough UTF-8 bytes for .<name>.<36-byte job UUID>.part on every platform.
    let stem = byte_prefix(stem, 160 - "media-".len() - suffix.len());
    format!("media-{stem}{suffix}")
}
fn reject_link(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "download path must not be a link"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "download reparse points are not supported"
            );
        }
    }
    Ok(())
}
fn target_directory(target: &Path) -> Result<PathBuf> {
    ensure!(target.is_absolute(), "download target must be absolute");
    // Resolve trusted system ancestors (e.g. macOS /var); the user-specified leaf may not be a link.
    reject_link(target)?;
    fs::create_dir_all(target).context("cannot create download directory")?;
    reject_link(target)?;
    target
        .canonicalize()
        .context("cannot resolve download directory")
}
fn part_file(path: &Path, create: bool) -> Result<File> {
    reject_link(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options
        .open(path)
        .context("cannot open owned download partial")?;
    ensure!(
        file.metadata()?.is_file(),
        "download partial is not regular"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            file.metadata()?.nlink() == 1,
            "download partial has unexpected hard links"
        );
    }
    reject_link(path)?;
    Ok(file)
}
fn parse_content_range(value: &str, offset: u64, total: u64) -> Result<u64> {
    let range = value
        .strip_prefix("bytes ")
        .context("invalid Content-Range unit")?;
    let (range, actual_total) = range.split_once('/').context("invalid Content-Range")?;
    let (start, end) = range
        .split_once('-')
        .context("invalid Content-Range interval")?;
    let start: u64 = start.parse().context("invalid range start")?;
    let end: u64 = end.parse().context("invalid range end")?;
    let actual_total: u64 = actual_total.parse().context("unknown range size")?;
    ensure!(
        start == offset && start <= end && end < total && actual_total == total,
        "Content-Range differs from requested object"
    );
    end.checked_sub(start)
        .and_then(|n| n.checked_add(1))
        .context("range length overflow")
}
fn header_text(response: &reqwest::Response, name: header::HeaderName) -> Result<Option<&str>> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().context("invalid download response header"))
        .transpose()
}
fn etag(response: &reqwest::Response) -> Result<Option<String>> {
    Ok(header_text(response, header::ETAG)?
        .filter(|value| {
            value.starts_with('"')
                && value.ends_with('"')
                && value.len() >= 2
                && !value.starts_with("W/")
        })
        .map(str::to_owned))
}
fn digest_prefix(path: &Path, length: u64) -> Result<Sha256> {
    let before = hash::stamp(path)?;
    ensure!(before.len >= length, "partial prefix was truncated");
    let mut input = part_file(path, false)?;
    let mut sha = Sha256::new();
    let mut remaining = length;
    let mut buffer = vec![0; PART_SIZE];
    while remaining > 0 {
        let len = remaining.min(buffer.len() as u64) as usize;
        input
            .read_exact(&mut buffer[..len])
            .context("partial prefix was truncated")?;
        sha.update(&buffer[..len]);
        remaining -= len as u64;
    }
    ensure!(
        hash::stamp(path)? == before,
        "partial changed during resume validation"
    );
    Ok(sha)
}
fn save(store: &Store, p: &DownloadProgress, file: &File, sha: &Sha256) -> Result<()> {
    file.sync_all()
        .context("cannot durably save partial download")?;
    let stamp = hash::stamp(&p.partial)?;
    ensure!(
        stamp.len == p.offset && file.metadata()?.len() == p.offset,
        "partial download length changed"
    );
    let key = item_key(&p.job_id, &p.fsid);
    let receipt = PartialReceipt {
        stamp,
        prefix_sha256: hex::encode(sha.clone().finalize()),
    };
    store.put_pair(
        ("download_receipts", &key, &receipt),
        ("download_items", &key, p),
    )
}
fn check_partial(store: &Store, p: &DownloadProgress) -> Result<()> {
    let receipt = store
        .get::<PartialReceipt>("download_receipts", &item_key(&p.job_id, &p.fsid))?
        .context("partial download has no ownership receipt")?;
    ensure!(
        hash::stamp(&p.partial)? == receipt.stamp && receipt.stamp.len == p.offset,
        "partial download changed outside this job"
    );
    Ok(())
}
fn recover_partial(store: &Store, p: &DownloadProgress) -> Result<Sha256> {
    let current = hash::stamp(&p.partial)?;
    let receipt =
        store.get::<PartialReceipt>("download_receipts", &item_key(&p.job_id, &p.fsid))?;
    if let Some(receipt) = receipt {
        ensure!(
            current.identity == receipt.stamp.identity
                && current.len >= p.offset
                && receipt.stamp.len == p.offset,
            "owned partial identity changed or durable prefix disappeared"
        );
        let sha = digest_prefix(&p.partial, p.offset)?;
        ensure!(
            hex::encode(sha.clone().finalize()) == receipt.prefix_sha256,
            "durable partial prefix changed outside this job"
        );
        if current.len != p.offset || current != receipt.stamp {
            // A crash may leave an uncheckpointed tail. Keep only the proven, durable prefix.
            let file = part_file(&p.partial, false)?;
            file.set_len(p.offset)?;
            save(store, p, &file, &sha)?;
        }
        Ok(sha)
    } else {
        // A crash between create_new and its first zero-byte receipt cannot contain media bytes.
        ensure!(
            p.offset == 0 && current.len == 0,
            "unowned partial has no recoverable receipt"
        );
        let file = part_file(&p.partial, false)?;
        let sha = Sha256::new();
        save(store, p, &file, &sha)?;
        Ok(sha)
    }
}
fn hashes(path: &Path) -> Result<(String, String)> {
    let before = hash::stamp(path)?;
    let mut file = part_file(path, false)?;
    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let mut bytes = vec![0; PART_SIZE];
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        sha.update(&bytes[..count]);
        md5.update(&bytes[..count]);
        total += count as u64;
    }
    ensure!(
        hash::stamp(path)? == before && total == before.len,
        "download changed during validation"
    );
    Ok((hex::encode(sha.finalize()), hex::encode(md5.finalize())))
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn publish(store: &Store, p: &mut DownloadProgress, remote: &RemoteFile) -> Result<()> {
    check_partial(store, p)?;
    ensure!(p.offset == remote.size, "download is incomplete");
    let expected_md5 = remote
        .md5
        .as_deref()
        .context("remote original has no full MD5; integrity cannot be verified")?;
    let (sha, md5) = hashes(&p.partial)?;
    ensure!(
        md5.eq_ignore_ascii_case(expected_md5),
        "download MD5 differs from remote original"
    );
    // Save the validation result before publication, so a crash after linking can recover.
    p.sha256 = Some(sha);
    p.state = ItemState::Uploaded;
    check_partial(store, p)?;
    store.put("download_items", &item_key(&p.job_id, &p.fsid), p)?;
    // hard_link atomically fails if destination exists; rename could overwrite on Unix.
    fs::hard_link(&p.partial, &p.destination)
        .context("cannot publish without overwriting; existing destination preserved")?;
    sync_directory(
        p.destination
            .parent()
            .context("download destination has no directory")?,
    )?;
    p.state = ItemState::Verified;
    p.error = None;
    store.put("download_items", &item_key(&p.job_id, &p.fsid), p)?;
    fs::remove_file(&p.partial)
        .context("published download verified, but temporary link could not be removed")?;
    sync_directory(p.destination.parent().unwrap())
}
fn recover_publication(
    store: &Store,
    p: &mut DownloadProgress,
    remote: &RemoteFile,
) -> Result<bool> {
    if !p.destination.exists() {
        return Ok(false);
    }
    ensure!(
        p.offset == remote.size && p.sha256.is_some(),
        "destination already exists; refusing overwrite"
    );
    reject_link(&p.destination)?;
    // A durable expected SHA is mandatory. A coincidental matching filename never proves ownership.
    let before = hash::stamp(&p.destination)?;
    ensure!(
        before.len == remote.size,
        "existing destination differs from completed download"
    );
    let mut file = File::open(&p.destination)?;
    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let mut buffer = vec![0; PART_SIZE];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        sha.update(&buffer[..n]);
        md5.update(&buffer[..n]);
    }
    ensure!(
        hash::stamp(&p.destination)? == before
            && Some(hex::encode(sha.finalize())) == p.sha256
            && remote
                .md5
                .as_deref()
                .is_some_and(|expected| hex::encode(md5.finalize()).eq_ignore_ascii_case(expected)),
        "existing destination no longer matches verified download"
    );
    p.state = ItemState::Verified;
    p.error = None;
    store.put("download_items", &item_key(&p.job_id, &p.fsid), p)?;
    // Remove only a still-owned partial. If replaced after publication, preserve it for inspection.
    if p.partial.exists() && check_partial(store, p).is_ok() {
        fs::remove_file(&p.partial)?;
    }
    Ok(true)
}

pub async fn run(
    store: &Store,
    api: &Api,
    plan: &DownloadPlan,
    resume: Option<&str>,
    token: CancellationToken,
) -> Result<JobSummary> {
    run_inner(store, api, plan, resume, token).await
}
async fn run_inner<A: TransferApi>(
    store: &Store,
    api: &A,
    plan: &DownloadPlan,
    resume: Option<&str>,
    token: CancellationToken,
) -> Result<JobSummary> {
    ensure!(
        api.account() == plan.account,
        "download plan belongs to another account"
    );
    let _lock = store.lock_account(&plan.account)?;
    upload::pin_plan(store, "download_plans", &plan.id, plan)?;
    let target = target_directory(&plan.target)?;
    let mut paths = HashSet::new();
    let mut ids = HashSet::new();
    for remote in &plan.files {
        ensure!(
            !remote.fsid.is_empty()
                && ids.insert(remote.fsid.clone())
                && paths.insert(safe_name(remote).to_lowercase()),
            "duplicate download object or target path"
        );
    }
    let record = upload::job(store, "download", &plan.id, &plan.account, resume)?;
    let capabilities = upload::retry(store, &record.id, &token, || api.capabilities()).await?;
    for (index, remote) in plan.files.iter().enumerate() {
        eprintln!(
            "download {}/{}: {} ({} bytes)",
            index + 1,
            plan.files.len(),
            remote.name(),
            remote.size
        );
        let key = item_key(&record.id, &remote.fsid);
        let destination = target.join(safe_name(remote));
        let partial = target.join(format!(".{}.{}.part", safe_name(remote), record.id));
        let existing = store.get::<DownloadProgress>("download_items", &key)?;
        let is_new = existing.is_none();
        let mut p = existing.unwrap_or_else(|| DownloadProgress {
            job_id: record.id.clone(),
            fsid: remote.fsid.clone(),
            account: plan.account.clone(),
            destination: destination.clone(),
            partial: partial.clone(),
            offset: 0,
            etag: None,
            state: ItemState::Pending,
            sha256: None,
            error: None,
        });
        ensure!(
            p.account == plan.account
                && p.destination == destination
                && p.partial == partial
                && p.fsid == remote.fsid,
            "download receipt belongs to another account or destination"
        );
        if capabilities
            .download_limit
            .is_none_or(|limit| remote.size > limit)
            || remote.md5.is_none()
        {
            p.state = ItemState::Blocked;
            p.error =
                Some("remote size limit or full original MD5 is unavailable or exceeded".into());
            store.put("download_items", &key, &p)?;
            continue;
        }
        if is_new {
            store.insert("download_items", &key, &p)?;
        }
        if let Err(error) = download_one(store, api, remote, &mut p, &token).await {
            record_failure(store, &mut p, &error)?;
            if crate::api::is_auth_error(&error) {
                break;
            }
        }
    }
    summary(store, &record.id)
}
fn record_failure(store: &Store, p: &mut DownloadProgress, error: &anyhow::Error) -> Result<()> {
    let key = item_key(&p.job_id, &p.fsid);
    let durable = store
        .get::<DownloadProgress>("download_items", &key)?
        .context("durable download checkpoint disappeared")?;
    ensure!(
        durable.job_id == p.job_id
            && durable.fsid == p.fsid
            && durable.account == p.account
            && durable.destination == p.destination
            && durable.partial == p.partial,
        "download checkpoint ownership changed"
    );
    // In-memory progress may include successful writes below the checkpoint size,
    // or a write_all may have written only part of its final chunk. Only the
    // atomic receipt/progress checkpoint may claim bytes, ETag or verified hash.
    *p = durable;
    p.state = if error.downcast_ref::<upload::Cancelled>().is_some() {
        ItemState::Cancelled
    } else {
        ItemState::Failed
    };
    p.error = Some(upload::safe_error(error));
    store.put("download_items", &key, p)
}
fn reset_partial(store: &Store, p: &mut DownloadProgress, file: &File) -> Result<()> {
    check_partial(store, p)?;
    let key = item_key(&p.job_id, &p.fsid);
    let mut receipt = store
        .get::<PartialReceipt>("download_receipts", &key)?
        .context("reset requires an owned partial receipt")?;
    let mut zero = p.clone();
    zero.offset = 0;
    zero.sha256 = None;
    zero.etag = None;
    zero.error = None;
    zero.state = ItemState::Uploading;
    receipt.stamp.len = 0;
    receipt.prefix_sha256 = hex::encode(Sha256::digest([]));
    // The logical zero-prefix intent retains native identity. If execution stops
    // before or after truncation, recover_partial can prove this empty prefix,
    // discard any old tail, and establish a normal checkpoint without guessing.
    store.put_pair(
        ("download_receipts", &key, &receipt),
        ("download_items", &key, &zero),
    )?;
    *p = zero;
    file.set_len(0)?;
    save(store, p, file, &Sha256::new())
}
async fn download_one<A: TransferApi>(
    store: &Store,
    api: &A,
    remote: &RemoteFile,
    p: &mut DownloadProgress,
    token: &CancellationToken,
) -> Result<()> {
    upload::active(store, &p.job_id, token)?;
    if recover_publication(store, p, remote)? {
        return Ok(());
    }
    let exists = fs::symlink_metadata(&p.partial).is_ok();
    let mut prefix_sha = if exists {
        recover_partial(store, p)?
    } else {
        ensure!(p.offset == 0, "owned download partial disappeared");
        Sha256::new()
    };
    let mut file = part_file(&p.partial, !exists)?;
    if !exists {
        save(store, p, &file, &prefix_sha)?;
    }
    ensure!(p.offset <= remote.size, "partial is larger than original");
    if p.offset == remote.size {
        drop(file);
        return publish(store, p, remote);
    }
    // A strong ETag is required to trust an existing prefix across fresh signed URLs.
    if p.offset > 0 && p.etag.is_none() {
        reset_partial(store, p, &file)?;
        prefix_sha = Sha256::new();
    }
    let mut resets = 0;
    let mut reported = p.offset;
    while p.offset < remote.size {
        upload::active(store, &p.job_id, token)?;
        check_partial(store, p)?;
        let response = upload::retry(store, &p.job_id, token, || {
            api.download_response(remote, p.offset, p.etag.as_deref())
        })
        .await?;
        let status = response.status();
        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            let total = header_text(&response, header::CONTENT_RANGE)?
                .and_then(|value| value.strip_prefix("bytes */"))
                .and_then(|n| n.parse::<u64>().ok());
            ensure!(
                total == Some(remote.size) && p.offset == remote.size,
                "server rejected an incomplete range"
            );
            break;
        }
        ensure!(
            status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT,
            "original download request was rejected"
        );
        let response_etag = etag(&response)?;
        let expected_bytes;
        if status == StatusCode::PARTIAL_CONTENT {
            expected_bytes = parse_content_range(
                header_text(&response, header::CONTENT_RANGE)?
                    .context("206 response lacks Content-Range")?,
                p.offset,
                remote.size,
            )?;
            if p.offset > 0 {
                ensure!(
                    p.etag.is_some() && response_etag == p.etag,
                    "original identity changed while resuming"
                );
            }
        } else {
            ensure!(
                header_text(&response, header::CONTENT_RANGE)?.is_none(),
                "200 response unexpectedly contains Content-Range"
            );
            if p.offset > 0 {
                ensure!(resets == 0, "server repeatedly ignored resume ranges");
                resets += 1;
                reset_partial(store, p, &file)?;
                prefix_sha = Sha256::new();
            }
            expected_bytes = remote.size;
        }
        if let Some(length) = response.content_length() {
            ensure!(
                length == expected_bytes,
                "response length differs from original range"
            );
        }
        p.etag = response_etag;
        p.state = ItemState::Uploading;
        p.error = None;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(p.offset))?;
        let mut received = 0_u64;
        let mut dirty = 0_usize;
        let mut stream = response.bytes_stream();
        loop {
            if let Err(error) = upload::active(store, &p.job_id, token) {
                save(store, p, &file, &prefix_sha)?;
                return Err(error);
            }
            let next =
                match upload::checked(store, &p.job_id, token, async { Ok(stream.next().await) })
                    .await
                {
                    Ok(next) => next,
                    Err(error) => {
                        save(store, p, &file, &prefix_sha)?;
                        return Err(error);
                    }
                };
            let Some(bytes) = next else {
                break;
            };
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(error) => {
                    save(store, p, &file, &prefix_sha)?;
                    return Err(error)
                        .context("original stream interrupted; owned partial preserved");
                }
            };
            received = received
                .checked_add(bytes.len() as u64)
                .context("range byte count overflow")?;
            ensure!(
                received <= expected_bytes && p.offset + bytes.len() as u64 <= remote.size,
                "original stream exceeded declared range"
            );
            file.write_all(&bytes)?;
            prefix_sha.update(&bytes);
            p.offset += bytes.len() as u64;
            dirty += bytes.len();
            if dirty >= PART_SIZE {
                save(store, p, &file, &prefix_sha)?;
                dirty = 0;
            }
            if p.offset.saturating_sub(reported) >= 64 * 1024 * 1024 {
                eprintln!("downloaded: {}/{} bytes", p.offset, remote.size);
                reported = p.offset;
            }
        }
        save(store, p, &file, &prefix_sha)?;
        ensure!(received == expected_bytes, "original stream was truncated");
        ensure!(received > 0, "original stream made no progress");
        if p.offset < remote.size && p.etag.is_none() {
            bail!("partial response lacks a strong ETag; cannot safely append");
        }
    }
    drop(file);
    upload::active(store, &p.job_id, token)?;
    publish(store, p, remote)
}
pub fn summary(store: &Store, id: &str) -> Result<JobSummary> {
    let record = store
        .get::<JobRecord>("jobs", id)?
        .context("job not found")?;
    ensure!(record.kind == "download", "job is not a download");
    let plan = store
        .get::<DownloadPlan>("download_plans", &record.plan_id)?
        .context("download plan not found")?;
    let mut result = JobSummary {
        job_id: id.into(),
        total: plan.files.len(),
        ..Default::default()
    };
    let mut count = 0;
    for (_, p) in store.list::<DownloadProgress>("download_items")? {
        if p.job_id != id {
            continue;
        }
        ensure!(
            p.account == record.account,
            "download receipt account mismatch"
        );
        count += 1;
        match p.state {
            ItemState::Verified if p.sha256.is_some() => result.verified += 1,
            ItemState::Blocked => result.blocked += 1,
            ItemState::Failed => result.failed += 1,
            _ => result.pending += 1,
        }
    }
    ensure!(count <= result.total, "unexpected download receipts");
    result.pending += result.total - count;
    result.complete = result.total > 0 && result.verified == result.total;
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn range_must_start_at_the_requested_offset() {
        assert!(parse_content_range("bytes 0-9/20", 10, 20).is_err());
        assert_eq!(parse_content_range("bytes 10-19/20", 10, 20).unwrap(), 10);
    }
    #[test]
    fn malformed_and_overflowing_ranges_are_rejected() {
        for value in [
            "bytes 10-9/20",
            "bytes 0-20/20",
            "bytes 0-9/*",
            "bytes */20",
        ] {
            assert!(parse_content_range(value, 0, 20).is_err());
        }
    }
    #[test]
    fn remote_names_cannot_escape_the_target() {
        let f = RemoteFile {
            fsid: "../../id".into(),
            path: "../../evil\\name.mp4".into(),
            size: 1,
            md5: None,
            ctime: 0,
            mtime: 0,
        };
        let name = safe_name(&f);
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        assert!(!name.starts_with('.'));
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::upload::fixtures::{self, ACCOUNT, Fake, Reply};
    fn setup() -> (tempfile::TempDir, Store, Fake, DownloadPlan) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let source = fixtures::source(dir.path(), b"abcdef");
        let api = Fake::new(&source, b"abcdef");
        let plan = DownloadPlan {
            schema: "yike.download.v1".into(),
            id: uuid::Uuid::new_v4().to_string(),
            account: ACCOUNT.into(),
            target: dir.path().join("downloads"),
            created_at: "fixture".into(),
            files: vec![api.remote.clone()],
        };
        (dir, store, api, plan)
    }
    fn seed(store: &Store, plan: &DownloadPlan, prefix: &[u8]) -> DownloadProgress {
        upload::pin_plan(store, "download_plans", &plan.id, plan).unwrap();
        let job = upload::job(store, "download", &plan.id, &plan.account, None).unwrap();
        let target = target_directory(&plan.target).unwrap();
        let name = safe_name(&plan.files[0]);
        let p = DownloadProgress {
            job_id: job.id.clone(),
            fsid: plan.files[0].fsid.clone(),
            account: plan.account.clone(),
            destination: target.join(&name),
            partial: target.join(format!(".{name}.{}.part", job.id)),
            offset: prefix.len() as u64,
            etag: Some("\"fixture-v1\"".into()),
            state: ItemState::Uploading,
            sha256: None,
            error: None,
        };
        let mut file = part_file(&p.partial, true).unwrap();
        file.write_all(prefix).unwrap();
        save(store, &p, &file, &Sha256::new_with_prefix(prefix)).unwrap();
        p
    }
    #[tokio::test]
    async fn standalone_download_verifies_md5_and_records_computed_sha() {
        let (_dir, store, api, plan) = setup();
        let result = run_inner(&store, &api, &plan, None, CancellationToken::new())
            .await
            .unwrap();
        assert!(result.complete);
        assert_eq!(result.verified, 1);
        let p = store
            .get::<DownloadProgress>(
                "download_items",
                &item_key(&result.job_id, &api.remote.fsid),
            )
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(&p.destination).unwrap(), b"abcdef");
        assert!(!p.partial.exists());
        assert_eq!(p.sha256, Some(hex::encode(Sha256::digest(b"abcdef"))));
    }
    #[tokio::test]
    async fn valid_206_resumes_owned_prefix_with_if_range() {
        let (_dir, store, api, plan) = setup();
        let p = seed(&store, &plan, b"abc");
        api.replies
            .borrow_mut()
            .push_back(Reply::range(b"def", 3, 6));
        let result = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.complete);
        assert_eq!(fs::read(&p.destination).unwrap(), b"abcdef");
        assert_eq!(
            *api.reads.borrow(),
            vec![(3, Some("\"fixture-v1\"".into()))]
        );
    }
    #[tokio::test]
    async fn ignored_range_200_restarts_instead_of_appending() {
        let (_dir, store, api, plan) = setup();
        let p = seed(&store, &plan, b"abc");
        api.replies.borrow_mut().push_back(Reply::full(b"abcdef"));
        let result = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.complete);
        assert_eq!(fs::read(&p.destination).unwrap(), b"abcdef");
    }
    #[tokio::test]
    async fn truncated_range_preserves_partial_without_publication() {
        let (_dir, store, api, plan) = setup();
        let p = seed(&store, &plan, b"abc");
        let mut reply = Reply::range(b"def", 3, 6);
        reply.headers.push(("Content-Length".into(), "3".into()));
        reply.bytes.pop();
        api.replies.borrow_mut().push_back(reply);
        let result = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!result.complete);
        assert_eq!(result.verified, 0);
        assert!(!p.destination.exists());
        assert!(p.partial.exists());
    }
    #[tokio::test]
    async fn wrong_original_md5_does_not_publish_a_finished_file() {
        let (_dir, store, api, plan) = setup();
        *api.bytes.borrow_mut() = b"xxxxxx".to_vec();
        let result = run_inner(&store, &api, &plan, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.failed, 1);
        assert!(!result.complete);
        assert!(!plan.target.join(safe_name(&api.remote)).exists());
    }
    #[tokio::test]
    async fn mismatched_range_or_etag_cannot_append() {
        for changed_etag in [false, true] {
            let (_dir, store, api, plan) = setup();
            let p = seed(&store, &plan, b"abc");
            let mut reply = Reply::range(b"def", if changed_etag { 3 } else { 2 }, 6);
            if changed_etag {
                reply.headers[0].1 = "\"different\"".into();
            }
            api.replies.borrow_mut().push_back(reply);
            let result = run_inner(
                &store,
                &api,
                &plan,
                Some(&p.job_id),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(!result.complete);
            assert_eq!(fs::read(&p.partial).unwrap(), b"abc");
            assert!(!p.destination.exists());
        }
    }
    #[tokio::test]
    async fn existing_destination_is_never_overwritten() {
        let (_dir, store, api, plan) = setup();
        fs::create_dir(&plan.target).unwrap();
        let path = plan.target.join(safe_name(&api.remote));
        fs::write(&path, b"existing-user-file").unwrap();
        let result = run_inner(&store, &api, &plan, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.failed, 1);
        assert_eq!(fs::read(&path).unwrap(), b"existing-user-file");
        assert!(api.reads.borrow().is_empty());
    }
    #[tokio::test]
    async fn partial_mutation_and_incomplete_416_never_publish() {
        let (_dir, store, api, plan) = setup();
        let p = seed(&store, &plan, b"abc");
        fs::write(&p.partial, b"modified-prefix").unwrap();
        let result = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed, 1);
        assert!(api.reads.borrow().is_empty());
        assert!(!p.destination.exists());
        let (_dir, store, api, plan) = setup();
        let p = seed(&store, &plan, b"abc");
        api.replies.borrow_mut().push_back(Reply {
            status: 416,
            headers: vec![("Content-Range".into(), "bytes */6".into())],
            bytes: vec![],
        });
        let result = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!result.complete);
        assert!(!p.destination.exists());
    }
    #[tokio::test]
    async fn duplicate_ids_are_rejected_before_transfers() {
        let (_dir, store, api, mut plan) = setup();
        plan.files.push(plan.files[0].clone());
        assert!(
            run_inner(&store, &api, &plan, None, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(api.reads.borrow().is_empty());
    }
    #[tokio::test]
    async fn completed_owned_partial_can_publish_after_interruption() {
        let (_dir, store, api, plan) = setup();
        let p = seed(&store, &plan, b"abcdef");
        let result = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.complete);
        assert_eq!(fs::read(&p.destination).unwrap(), b"abcdef");
        assert!(api.reads.borrow().is_empty());
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use crate::upload::fixtures::{self, ACCOUNT, Fake, Reply};
    #[test]
    fn utf8_names_leave_byte_budget_for_the_complete_partial_component() {
        let remote = RemoteFile {
            fsid: "id".into(),
            path: format!("{}.mp4", "旅行".repeat(100)),
            size: 1,
            md5: None,
            ctime: 0,
            mtime: 0,
        };
        let name = safe_name(&remote);
        let partial = format!(".{name}.{}.part", uuid::Uuid::new_v4());
        assert!(name.len() <= 160);
        assert!(partial.len() <= 255);
        assert!(name.ends_with(".mp4"));
    }
    #[tokio::test]
    async fn crash_tail_is_truncated_only_after_proving_the_durable_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let source = fixtures::source(dir.path(), b"abcdef");
        let api = Fake::new(&source, b"abcdef");
        let plan = DownloadPlan {
            schema: "v1".into(),
            id: "tail-plan".into(),
            account: ACCOUNT.into(),
            target: dir.path().join("downloads"),
            created_at: "fixture".into(),
            files: vec![api.remote.clone()],
        };
        upload::pin_plan(&store, "download_plans", &plan.id, &plan).unwrap();
        let job = upload::job(&store, "download", &plan.id, ACCOUNT, None).unwrap();
        let target = target_directory(&plan.target).unwrap();
        let name = safe_name(&api.remote);
        let p = DownloadProgress {
            job_id: job.id.clone(),
            fsid: api.remote.fsid.clone(),
            account: ACCOUNT.into(),
            destination: target.join(&name),
            partial: target.join(format!(".{name}.{}.part", job.id)),
            offset: 3,
            etag: Some("\"fixture-v1\"".into()),
            state: ItemState::Uploading,
            sha256: None,
            error: None,
        };
        let mut file = part_file(&p.partial, true).unwrap();
        file.write_all(b"abc").unwrap();
        save(&store, &p, &file, &Sha256::new_with_prefix(b"abc")).unwrap();
        file.write_all(b"uncheckpointed").unwrap();
        file.sync_all().unwrap();
        drop(file);
        api.replies
            .borrow_mut()
            .push_back(Reply::range(b"def", 3, 6));
        let result = run_inner(&store, &api, &plan, Some(&job.id), CancellationToken::new())
            .await
            .unwrap();
        assert!(result.complete);
        assert_eq!(fs::read(&p.destination).unwrap(), b"abcdef");
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use crate::upload::fixtures::{self, ACCOUNT, Fake};
    #[tokio::test]
    async fn publication_acknowledgment_loss_recovers_without_a_second_download() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let source = fixtures::source(dir.path(), b"abcdef");
        let api = Fake::new(&source, b"abcdef");
        let plan = DownloadPlan {
            schema: "v1".into(),
            id: "publish-plan".into(),
            account: ACCOUNT.into(),
            target: dir.path().join("downloads"),
            created_at: "fixture".into(),
            files: vec![api.remote.clone()],
        };
        upload::pin_plan(&store, "download_plans", &plan.id, &plan).unwrap();
        let job = upload::job(&store, "download", &plan.id, ACCOUNT, None).unwrap();
        let target = target_directory(&plan.target).unwrap();
        let name = safe_name(&api.remote);
        let p = DownloadProgress {
            job_id: job.id.clone(),
            fsid: api.remote.fsid.clone(),
            account: ACCOUNT.into(),
            destination: target.join(&name),
            partial: target.join(format!(".{name}.{}.part", job.id)),
            offset: 6,
            etag: Some("\"fixture-v1\"".into()),
            state: ItemState::Uploaded,
            sha256: Some(hex::encode(Sha256::digest(b"abcdef"))),
            error: None,
        };
        let mut file = part_file(&p.partial, true).unwrap();
        file.write_all(b"abcdef").unwrap();
        save(&store, &p, &file, &Sha256::new_with_prefix(b"abcdef")).unwrap();
        drop(file);
        fs::hard_link(&p.partial, &p.destination).unwrap();
        let result = run_inner(&store, &api, &plan, Some(&job.id), CancellationToken::new())
            .await
            .unwrap();
        assert!(result.complete);
        assert!(!p.partial.exists());
        assert!(api.reads.borrow().is_empty());
    }
    #[test]
    fn restored_mtime_cannot_hide_mutated_durable_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let path = dir.path().join("owned.part");
        let p = DownloadProgress {
            job_id: "fixture".into(),
            fsid: "id".into(),
            account: ACCOUNT.into(),
            destination: dir.path().join("final"),
            partial: path.clone(),
            offset: 3,
            etag: None,
            state: ItemState::Uploading,
            sha256: None,
            error: None,
        };
        let mut file = part_file(&path, true).unwrap();
        file.write_all(b"abc").unwrap();
        save(&store, &p, &file, &Sha256::new_with_prefix(b"abc")).unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        drop(file);
        fs::write(&path, b"xyz").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(recover_partial(&store, &p).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"xyz");
    }
}

#[cfg(test)]
mod error_checkpoint_tests {
    use super::*;
    use crate::upload::fixtures::{self, ACCOUNT, Fake};
    use std::{cell::RefCell, io, net::TcpListener, time::Duration};

    fn setup(
        bytes: &[u8],
    ) -> (
        tempfile::TempDir,
        Store,
        Fake,
        DownloadPlan,
        DownloadProgress,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let source = fixtures::source(dir.path(), bytes);
        let api = Fake::new(&source, bytes);
        let plan = DownloadPlan {
            schema: "v1".into(),
            id: uuid::Uuid::new_v4().to_string(),
            account: ACCOUNT.into(),
            target: dir.path().join("downloads"),
            created_at: "fixture".into(),
            files: vec![api.remote.clone()],
        };
        upload::pin_plan(&store, "download_plans", &plan.id, &plan).unwrap();
        let record = upload::job(&store, "download", &plan.id, ACCOUNT, None).unwrap();
        let target = target_directory(&plan.target).unwrap();
        let name = safe_name(&api.remote);
        let p = DownloadProgress {
            job_id: record.id.clone(),
            fsid: api.remote.fsid.clone(),
            account: ACCOUNT.into(),
            destination: target.join(&name),
            partial: target.join(format!(".{name}.{}.part", record.id)),
            offset: 0,
            etag: None,
            state: ItemState::Pending,
            sha256: None,
            error: None,
        };
        let file = part_file(&p.partial, true).unwrap();
        save(&store, &p, &file, &Sha256::new()).unwrap();
        (dir, store, api, plan, p)
    }
    struct OversizedApi {
        inner: Fake,
        first: RefCell<Option<reqwest::Response>>,
    }
    impl TransferApi for OversizedApi {
        fn account(&self) -> String {
            self.inner.account()
        }
        async fn capabilities(&self) -> Result<Capabilities> {
            self.inner.capabilities().await
        }
        async fn files(&self) -> Result<Vec<RemoteFile>> {
            self.inner.files().await
        }
        async fn albums(&self) -> Result<Vec<Album>> {
            self.inner.albums().await
        }
        async fn album_create(&self, title: &str) -> Result<Album> {
            self.inner.album_create(title).await
        }
        async fn add_to_album(&self, album: &Album, file: &RemoteFile) -> Result<()> {
            self.inner.add_to_album(album, file).await
        }
        async fn precreate(&self, file: &PlannedFile) -> Result<UploadTicket> {
            self.inner.precreate(file).await
        }
        async fn upload_part(
            &self,
            file: &PlannedFile,
            ticket: &UploadTicket,
            index: usize,
            bytes: Vec<u8>,
        ) -> Result<()> {
            self.inner.upload_part(file, ticket, index, bytes).await
        }
        async fn create_file(
            &self,
            file: &PlannedFile,
            ticket: &UploadTicket,
        ) -> Result<RemoteFile> {
            self.inner.create_file(file, ticket).await
        }
        async fn download_response(
            &self,
            file: &RemoteFile,
            offset: u64,
            etag: Option<&str>,
        ) -> Result<reqwest::Response> {
            let first = self.first.borrow_mut().take();
            if let Some(response) = first {
                Ok(response)
            } else {
                self.inner.download_response(file, offset, etag).await
            }
        }
    }
    async fn oversized_response() -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nETag: \"oversized\"\r\nConnection: close\r\n\r\n");
            let chunk = vec![7; 2 * 1024 * 1024];
            for _ in 0..2 {
                if socket
                    .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                    .is_err()
                    || socket.write_all(&chunk).is_err()
                    || socket.write_all(b"\r\n").is_err()
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(60));
            }
            let _ = socket.write_all(b"0\r\n\r\n");
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/anonymous"))
            .send()
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn oversized_body_keeps_the_durable_offset_and_can_resume() {
        let bytes = vec![7; 3 * 1024 * 1024];
        let (_dir, store, inner, plan, p) = setup(&bytes);
        let api = OversizedApi {
            inner,
            first: RefCell::new(Some(oversized_response().await)),
        };
        let failed = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(failed.failed, 1);
        assert!(!p.destination.exists());
        let durable = store
            .get::<DownloadProgress>("download_items", &item_key(&p.job_id, &p.fsid))
            .unwrap()
            .unwrap();
        assert_eq!(
            durable.offset, 0,
            "failure reporting must not claim uncheckpointed bytes"
        );
        assert!(durable.etag.is_none());
        assert!(fs::metadata(&p.partial).unwrap().len() > 0);
        let resumed = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(resumed.complete);
        assert_eq!(fs::read(&p.destination).unwrap(), bytes);
    }
    struct PartialDiskFailure<'a> {
        file: &'a mut File,
        remaining: usize,
    }
    impl Write for PartialDiskFailure<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("anonymous disk write failure"));
            }
            let n = bytes.len().min(self.remaining);
            let written = self.file.write(&bytes[..n])?;
            self.remaining -= written;
            Ok(written)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }
    #[tokio::test]
    async fn partial_disk_write_error_does_not_advance_the_durable_checkpoint() {
        let bytes = vec![7; 3 * 1024 * 1024];
        let (_dir, store, api, plan, mut p) = setup(&bytes);
        let mut file = part_file(&p.partial, false).unwrap();
        file.write_all(&bytes[..2 * 1024 * 1024]).unwrap();
        p.offset = 2 * 1024 * 1024;
        p.etag = Some("\"uncheckpointed\"".into());
        let error = PartialDiskFailure {
            file: &mut file,
            remaining: 1024,
        }
        .write_all(&bytes[..2048])
        .unwrap_err();
        drop(file);
        record_failure(&store, &mut p, &error.into()).unwrap();
        let durable = store
            .get::<DownloadProgress>("download_items", &item_key(&p.job_id, &p.fsid))
            .unwrap()
            .unwrap();
        assert_eq!(durable.offset, 0);
        assert!(durable.etag.is_none());
        let resumed = run_inner(
            &store,
            &api,
            &plan,
            Some(&p.job_id),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(resumed.complete);
        assert_eq!(fs::read(&p.destination).unwrap(), bytes);
    }
    #[tokio::test]
    async fn reset_intent_recovers_before_and_after_truncation() {
        for crash_after_truncate in [false, true] {
            let (_dir, store, api, plan, mut p) = setup(b"abcdef");
            let mut file = part_file(&p.partial, false).unwrap();
            file.write_all(b"abc").unwrap();
            p.offset = 3;
            p.etag = Some("\"old-etag\"".into());
            save(&store, &p, &file, &Sha256::new_with_prefix(b"abc")).unwrap();
            drop(file);
            // A read-only handle forces a stop exactly at truncate, without any test-only production hook.
            let readonly = File::open(&p.partial).unwrap();
            assert!(reset_partial(&store, &mut p, &readonly).is_err());
            drop(readonly);
            let durable = store
                .get::<DownloadProgress>("download_items", &item_key(&p.job_id, &p.fsid))
                .unwrap()
                .unwrap();
            assert_eq!(
                durable.offset, 0,
                "zero-prefix reset intent must be durable before truncate"
            );
            assert!(durable.etag.is_none());
            if crash_after_truncate {
                let file = part_file(&p.partial, false).unwrap();
                file.set_len(0).unwrap();
                file.sync_all().unwrap();
            }
            let database = store.root_dir().join("db");
            drop(store);
            let store = Store::open(&database).unwrap();
            let resumed = run_inner(
                &store,
                &api,
                &plan,
                Some(&p.job_id),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(resumed.complete);
            assert_eq!(fs::read(&p.destination).unwrap(), b"abcdef");
        }
    }
}
