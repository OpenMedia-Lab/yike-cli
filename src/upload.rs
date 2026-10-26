//! Account-bound transfers with durable, account-wide commit receipts.
use crate::{
    api::{self, Api},
    hash,
    model::*,
    state::Store,
};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::HashSet, future::Future, time::Duration};
use tokio_util::sync::CancellationToken;

#[allow(async_fn_in_trait)]
pub(crate) trait TransferApi {
    fn account(&self) -> String;
    async fn capabilities(&self) -> Result<Capabilities>;
    async fn files(&self) -> Result<Vec<RemoteFile>>;
    async fn albums(&self) -> Result<Vec<Album>>;
    async fn album_create(&self, title: &str) -> Result<Album>;
    async fn add_to_album(&self, album: &Album, file: &RemoteFile) -> Result<()>;
    async fn precreate(&self, file: &PlannedFile) -> Result<UploadTicket>;
    async fn upload_part(
        &self,
        file: &PlannedFile,
        ticket: &UploadTicket,
        index: usize,
        bytes: Vec<u8>,
    ) -> Result<()>;
    async fn create_file(&self, file: &PlannedFile, ticket: &UploadTicket) -> Result<RemoteFile>;
    async fn download_response(
        &self,
        file: &RemoteFile,
        offset: u64,
        etag: Option<&str>,
    ) -> Result<reqwest::Response>;
}
impl TransferApi for Api {
    fn account(&self) -> String {
        Api::account(self)
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        Api::capabilities(self).await
    }
    async fn files(&self) -> Result<Vec<RemoteFile>> {
        Api::files(self, None).await
    }
    async fn albums(&self) -> Result<Vec<Album>> {
        Api::albums(self).await
    }
    async fn album_create(&self, title: &str) -> Result<Album> {
        Api::album_create(self, title).await
    }
    async fn add_to_album(&self, album: &Album, file: &RemoteFile) -> Result<()> {
        Api::add_to_album(self, album, file).await
    }
    async fn precreate(&self, file: &PlannedFile) -> Result<UploadTicket> {
        Api::precreate(self, file).await
    }
    async fn upload_part(
        &self,
        file: &PlannedFile,
        ticket: &UploadTicket,
        index: usize,
        bytes: Vec<u8>,
    ) -> Result<()> {
        Api::upload_part(self, file, ticket, index, bytes).await
    }
    async fn create_file(&self, file: &PlannedFile, ticket: &UploadTicket) -> Result<RemoteFile> {
        Api::create_file(self, file, ticket).await
    }
    async fn download_response(
        &self,
        file: &RemoteFile,
        offset: u64,
        etag: Option<&str>,
    ) -> Result<reqwest::Response> {
        Api::download_response(self, file, offset, etag).await
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Claim {
    stamp: FileStamp,
    size: u64,
    progress: UploadProgress,
}
#[derive(Serialize, Deserialize)]
struct AlbumIntent {
    account: String,
    title: String,
    state: ItemState,
    album: Option<Album>,
}

#[derive(Debug)]
pub(crate) struct Cancelled;
#[derive(Debug)]
struct ReconciliationRequired;
impl std::fmt::Display for ReconciliationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stable album ID binding required")
    }
}
impl std::error::Error for ReconciliationRequired {}
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transfer cancelled")
    }
}
impl std::error::Error for Cancelled {}

pub(crate) fn active(store: &Store, job_id: &str, token: &CancellationToken) -> Result<()> {
    if token.is_cancelled()
        || store
            .get::<JobRecord>("jobs", job_id)?
            .is_some_and(|job| job.cancelled)
    {
        return Err(Cancelled.into());
    }
    Ok(())
}
pub(crate) async fn checked<T>(
    store: &Store,
    job_id: &str,
    token: &CancellationToken,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::pin!(future);
    loop {
        active(store, job_id, token)?;
        tokio::select! {
            biased;
            _ = token.cancelled() => return Err(Cancelled.into()),
            result = &mut future => return result,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}
#[derive(Debug)]
struct SourceChanged;
impl std::fmt::Display for SourceChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("source changed; create a new immutable plan")
    }
}
impl std::error::Error for SourceChanged {}
fn source_unchanged(file: &PlannedFile) -> Result<()> {
    hash::assert_stamp(file).map_err(|_| SourceChanged.into())
}
pub(crate) async fn retry<T, F, Fut>(
    store: &Store,
    job_id: &str,
    token: &CancellationToken,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    for attempt in 0..3 {
        active(store, job_id, token)?;
        let result = checked(store, job_id, token, operation()).await;
        match result {
            Ok(value) => {
                active(store, job_id, token)?;
                return Ok(value);
            }
            Err(error) => {
                let Some(delay) = transient_delay(&error) else {
                    return Err(error);
                };
                if attempt == 2 {
                    return Err(error);
                }
                let delay = delay.max(Duration::from_secs(1 << attempt));
                // Poll persistent cancellation while honoring Retry-After.
                let deadline = tokio::time::Instant::now() + delay;
                while tokio::time::Instant::now() < deadline {
                    active(store, job_id, token)?;
                    tokio::select! { _ = token.cancelled() => return Err(Cancelled.into()), _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_millis(250))) => {} }
                }
            }
        }
    }
    unreachable!()
}
pub(crate) fn transient_delay(error: &anyhow::Error) -> Option<Duration> {
    api::retry_delay(error).or_else(|| {
        error
            .downcast_ref::<reqwest::Error>()
            .filter(|e| e.is_timeout() || e.is_connect() || e.is_body())
            .map(|_| Duration::from_secs(1))
    })
}
pub(crate) fn safe_error(error: &anyhow::Error) -> String {
    if error.downcast_ref::<Cancelled>().is_some() {
        "cancelled; resume to continue".into()
    } else if error.downcast_ref::<SourceChanged>().is_some() {
        "source changed since planning; create a new immutable plan".into()
    } else if error.downcast_ref::<ReconciliationRequired>().is_some() {
        "album commit outcome unknown or title ambiguous; explicitly bind a stable album ID".into()
    } else if let Some(diagnostic) = error.downcast_ref::<api::ApiError>() {
        // This type's Display contains only fixed operation labels, enum kind,
        // HTTP status and numeric errno. Never render arbitrary anyhow context
        // or a reqwest error chain, which can contain a signed media URL.
        if api::is_auth_error(error) {
            format!("authentication required; log in and resume; {diagnostic}")
        } else {
            diagnostic.to_string()
        }
    } else {
        "transfer failed validation or transport; original and durable state preserved".into()
    }
}
fn key(file: &PlannedFile, account: &str) -> String {
    format!("{account}:{}:{}", file.sha256, file.stamp.len)
}
fn item_key(job: &str, file: &str) -> String {
    format!("{job}:{file}")
}
fn save(store: &Store, file: &PlannedFile, p: &UploadProgress) -> Result<()> {
    // The account-wide record must win a crash between these two writes.
    store.put(
        "transfer_claims",
        &key(file, &p.account),
        &Claim {
            stamp: file.stamp.clone(),
            size: file.stamp.len,
            progress: p.clone(),
        },
    )?;
    store.put("upload_items", &item_key(&p.job_id, &p.file_id), p)
}
pub(crate) fn pin_plan<T: Serialize + for<'de> Deserialize<'de>>(
    store: &Store,
    kind: &str,
    id: &str,
    plan: &T,
) -> Result<()> {
    if let Some(existing) = store.get::<T>(kind, id)? {
        ensure!(
            serde_json::to_value(existing)? == serde_json::to_value(plan)?,
            "immutable plan differs from stored plan"
        );
    } else {
        store.insert(kind, id, plan)?;
    }
    Ok(())
}
pub(crate) fn job(
    store: &Store,
    kind: &str,
    plan_id: &str,
    account: &str,
    resume: Option<&str>,
) -> Result<JobRecord> {
    if let Some(id) = resume {
        let record = store
            .get::<JobRecord>("jobs", id)?
            .context("job not found")?;
        ensure!(
            record.kind == kind && record.plan_id == plan_id && record.account == account,
            "job belongs to another account or plan"
        );
        ensure!(
            record.id == id && uuid::Uuid::parse_str(id).is_ok(),
            "invalid stored job identity"
        );
        Ok(record)
    } else {
        let record = JobRecord {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.into(),
            plan_id: plan_id.into(),
            account: account.into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            cancelled: false,
        };
        store.insert("jobs", &record.id, &record)?;
        Ok(record)
    }
}

pub async fn verify_job(
    store: &Store,
    api: &Api,
    id: &str,
    token: CancellationToken,
) -> Result<JobSummary> {
    verify_inner(store, api, id, token).await
}
async fn verify_inner<A: TransferApi>(
    store: &Store,
    api: &A,
    id: &str,
    token: CancellationToken,
) -> Result<JobSummary> {
    let record = store
        .get::<JobRecord>("jobs", id)?
        .context("job not found")?;
    ensure!(
        record.kind == "upload" && record.account == api.account(),
        "verification job belongs to another account or kind"
    );
    let _lock = store.lock_account(&record.account)?;
    let plan = store
        .get::<UploadPlan>("upload_plans", &record.plan_id)?
        .context("job plan not found")?;
    ensure!(
        plan.account == record.account,
        "verification plan account mismatch"
    );
    for file in &plan.files {
        active(store, id, &token)?;
        let Some(mut p) = store.get::<UploadProgress>("upload_items", &item_key(id, &file.id))?
        else {
            continue;
        };
        ensure!(
            p.account == record.account && p.source_sha256 == file.sha256,
            "verification receipt ownership mismatch"
        );
        let Some(remote) = p.remote.clone() else {
            continue;
        };
        // An explicit verify always streams again. Keep the previous signed receipt as history.
        store.insert(
            "verification_history",
            &format!("{id}:{}:{}", file.id, uuid::Uuid::new_v4()),
            &p,
        )?;
        p.verified_sha256 = None;
        p.state = ItemState::Uploaded;
        p.error = None;
        save(store, file, &p)?;
        eprintln!(
            "verify original: {} ({} bytes)",
            file.relative_path, file.stamp.len
        );
        let result = async {
            source_unchanged(file)?;
            ensure!(
                metadata_matches(&remote, file.stamp.len, &file.md5),
                "verification metadata mismatch"
            );
            retry(store, id, &token, || {
                readback(store, api, id, &remote, &file.sha256, &file.md5, &token)
            })
            .await?;
            source_unchanged(file)
        }
        .await;
        match result {
            Ok(()) => {
                p.verified_sha256 = Some(file.sha256.clone());
                p.state = if p.album_added {
                    ItemState::Verified
                } else {
                    ItemState::Uploaded
                };
                eprintln!("verified original: {}", file.relative_path);
            }
            Err(error) => {
                if error.downcast_ref::<SourceChanged>().is_some() {
                    p.state = ItemState::Failed;
                }
                p.error = Some(safe_error(&error));
                save(store, file, &p)?;
                if api::is_auth_error(&error) || error.downcast_ref::<Cancelled>().is_some() {
                    break;
                }
            }
        }
        save(store, file, &p)?;
    }
    summary(store, id)
}
pub async fn ensure_album(store: &Store, api: &Api, title: &str) -> Result<Album> {
    let account = api.account();
    let _lock = store.lock_account(&account)?;
    album_for(
        store,
        api,
        &account,
        title,
        "standalone-album",
        &CancellationToken::new(),
    )
    .await
}
pub async fn run(
    store: &Store,
    api: &Api,
    plan: &UploadPlan,
    resume: Option<&str>,
    token: CancellationToken,
    verify: bool,
) -> Result<JobSummary> {
    run_inner(store, api, plan, resume, token, verify).await
}
async fn run_inner<A: TransferApi>(
    store: &Store,
    api: &A,
    plan: &UploadPlan,
    resume: Option<&str>,
    token: CancellationToken,
    verify: bool,
) -> Result<JobSummary> {
    ensure!(
        api.account() == plan.account,
        "plan belongs to another account"
    );
    let _lock = store.lock_account(&plan.account)?;
    pin_plan(store, "upload_plans", &plan.id, plan)?;
    let mut seen = HashSet::new();
    ensure!(
        plan.files.iter().all(|file| seen.insert(&file.id)),
        "duplicate file IDs in upload plan"
    );
    let record = job(store, "upload", &plan.id, &plan.account, resume)?;
    // Unknown account limits are a hard block, never an assumed entitlement.
    let capabilities = retry(store, &record.id, &token, || api.capabilities()).await?;
    let limit = capabilities.upload_limit.map(|v| v.min(WEB_UPLOAD_LIMIT));
    for (file_index, file) in plan.files.iter().enumerate() {
        let started = std::time::Instant::now();
        eprintln!(
            "upload {}/{}: {} ({} bytes)",
            file_index + 1,
            plan.files.len(),
            file.relative_path,
            file.stamp.len
        );
        let mut progress = store
            .get::<UploadProgress>("upload_items", &item_key(&record.id, &file.id))?
            .unwrap_or_else(|| UploadProgress {
                job_id: record.id.clone(),
                file_id: file.id.clone(),
                account: plan.account.clone(),
                source_sha256: file.sha256.clone(),
                state: ItemState::Pending,
                ticket: None,
                remote: None,
                album: None,
                album_added: false,
                confirmed_parts: vec![],
                verified_sha256: None,
                error: None,
            });
        ensure!(
            progress.account == plan.account && progress.source_sha256 == file.sha256,
            "stored upload ownership or source hash mismatch"
        );
        if let Some(claim) = store.get::<Claim>("transfer_claims", &key(file, &plan.account))? {
            ensure!(
                claim.progress.account == plan.account
                    && claim.progress.source_sha256 == file.sha256
                    && claim.size == file.stamp.len,
                "stored transfer claim ownership mismatch"
            );
            // A commit intent is never downgraded by stale per-job progress.
            let reusable = claim.progress.remote.is_some()
                || matches!(
                    claim.progress.state,
                    ItemState::CommitIntent | ItemState::NeedsReconciliation | ItemState::Uploading
                );
            if reusable {
                progress = claim.progress;
                progress.job_id = record.id.clone();
                progress.file_id = file.id.clone();
                if claim.stamp != file.stamp {
                    progress.verified_sha256 = None;
                    if progress.state == ItemState::Verified {
                        progress.state = ItemState::Uploaded;
                    }
                }
                if progress
                    .album
                    .as_ref()
                    .is_some_and(|a| a.title != file.album_title)
                {
                    progress.album = None;
                    progress.album_added = false;
                }
            }
        }
        if let Err(error) = active(store, &record.id, &token) {
            if !matches!(
                progress.state,
                ItemState::CommitIntent | ItemState::NeedsReconciliation | ItemState::Verified
            ) {
                progress.state = ItemState::Cancelled;
            }
            progress.error = Some(safe_error(&error));
            save(store, file, &progress)?;
            continue;
        }
        if let Err(error) = source_unchanged(file) {
            progress.state = ItemState::Failed;
            progress.error = Some(safe_error(&error));
            // Preserve uncertain account-wide intent even if this source became invalid.
            store.put("upload_items", &item_key(&record.id, &file.id), &progress)?;
            continue;
        }
        if file.blocked.is_some() || limit.is_none_or(|max| file.stamp.len > max) {
            progress.state = ItemState::Blocked;
            progress.error = Some(
                "source exceeds the current validated upload limit, or capability is unknown"
                    .into(),
            );
            store.put("upload_items", &item_key(&record.id, &file.id), &progress)?;
            continue;
        }
        save(store, file, &progress)?;
        let reused_verified_receipt = progress.verified_sha256.as_deref() == Some(&file.sha256);
        if let Err(error) = transfer_one(store, api, file, &mut progress, &token, verify).await {
            if progress.state == ItemState::CommitIntent {
                progress.state = ItemState::NeedsReconciliation;
            }
            if error.downcast_ref::<ReconciliationRequired>().is_some() {
                progress.state = ItemState::NeedsReconciliation;
            }
            if !matches!(
                progress.state,
                ItemState::NeedsReconciliation | ItemState::CommitIntent
            ) {
                progress.state = if error.downcast_ref::<SourceChanged>().is_some() {
                    ItemState::Failed
                } else if error.downcast_ref::<Cancelled>().is_some() {
                    ItemState::Cancelled
                } else if progress.remote.is_some() {
                    ItemState::Uploaded
                } else {
                    ItemState::Failed
                };
            }
            progress.error = Some(safe_error(&error));
            save(store, file, &progress)?;
            if api::is_auth_error(&error) {
                break;
            }
        }
        if progress.state == ItemState::Verified && reused_verified_receipt {
            eprintln!(
                "reused verified receipt {}/{}: {}",
                file_index + 1,
                plan.files.len(),
                file.relative_path
            );
        } else if progress.state == ItemState::Verified {
            eprintln!(
                "verified {}/{}: {} ({:.1} MiB/s elapsed)",
                file_index + 1,
                plan.files.len(),
                file.relative_path,
                file.stamp.len as f64 / 1_048_576.0 / started.elapsed().as_secs_f64().max(0.001)
            );
        } else {
            eprintln!(
                "saved {}/{}: {:?}",
                file_index + 1,
                plan.files.len(),
                progress.state
            );
        }
    }
    // Initialize all not-yet-started rows so an interrupted job cannot look complete.
    for file in &plan.files {
        let k = item_key(&record.id, &file.id);
        if store.get::<UploadProgress>("upload_items", &k)?.is_none() {
            store.put(
                "upload_items",
                &k,
                &UploadProgress {
                    job_id: record.id.clone(),
                    file_id: file.id.clone(),
                    account: plan.account.clone(),
                    source_sha256: file.sha256.clone(),
                    state: ItemState::Pending,
                    ticket: None,
                    remote: None,
                    album: None,
                    album_added: false,
                    confirmed_parts: vec![],
                    verified_sha256: None,
                    error: None,
                },
            )?;
        }
    }
    summary(store, &record.id)
}

async fn transfer_one<A: TransferApi>(
    store: &Store,
    api: &A,
    file: &PlannedFile,
    p: &mut UploadProgress,
    token: &CancellationToken,
    verify: bool,
) -> Result<()> {
    if matches!(
        p.state,
        ItemState::CommitIntent | ItemState::NeedsReconciliation
    ) && p.remote.is_none()
    {
        p.state = ItemState::NeedsReconciliation;
        save(store, file, p)?;
        let candidates = retry(store, &p.job_id, token, || api.files()).await?;
        let mut matching = Vec::new();
        for remote in candidates
            .into_iter()
            .filter(|remote| metadata_matches(remote, file.stamp.len, &file.md5))
        {
            // Hash, size and account narrow candidates; an original stream proves identity.
            if readback(
                store,
                api,
                &p.job_id,
                &remote,
                &file.sha256,
                &file.md5,
                token,
            )
            .await
            .is_ok()
            {
                matching.push(remote);
            }
            active(store, &p.job_id, token)?;
        }
        if matching.len() != 1 {
            bail!("commit is unresolved; do not submit again");
        }
        p.remote = matching.pop();
        p.verified_sha256 = Some(file.sha256.clone());
        p.state = ItemState::Uploaded;
        save(store, file, p)?;
    }
    if p.remote.is_none() {
        source_unchanged(file)?;
        // Precreate with autoinit may commit an instant object. Record intent
        // before its single attempt, including resumed missing-part queries.
        // A timeout or cancellation cannot justify another precreate request.
        active(store, &p.job_id, token)?;
        p.state = ItemState::CommitIntent;
        p.error = None;
        save(store, file, p)?;
        let ticket = match checked(store, &p.job_id, token, api.precreate(file)).await {
            Ok(ticket) => ticket,
            Err(error) => {
                p.state = ItemState::NeedsReconciliation;
                p.error = Some(safe_error(&error));
                save(store, file, p)?;
                return Err(error);
            }
        };
        ensure!(
            matches!(ticket.return_type, 1..=3),
            "unknown precreate result type"
        );
        ensure!(
            ticket.return_type != 3 || ticket.file.is_some(),
            "instant precreate result has no stable object"
        );
        let mut indices = HashSet::new();
        ensure!(
            ticket
                .missing
                .iter()
                .all(|index| *index < file.parts.len() && indices.insert(*index)),
            "server returned invalid missing parts"
        );
        p.ticket = Some(ticket.clone());
        p.error = None;
        p.confirmed_parts = (0..file.parts.len())
            .filter(|index| !indices.contains(index))
            .collect();
        if let Some(remote) = ticket.file.clone() {
            // An instant object's existence remains uncertain until listing
            // confirms its stable ID and metadata. Keep its commit intent.
            save(store, file, p)?;
            ensure!(
                metadata_matches(&remote, file.stamp.len, &file.md5),
                "instant upload metadata differs from plan"
            );
            let listed = retry(store, &p.job_id, token, || api.files()).await?;
            ensure!(
                listed.iter().any(
                    |f| f.fsid == remote.fsid && metadata_matches(f, file.stamp.len, &file.md5)
                ),
                "instant-upload object could not be confirmed"
            );
            p.remote = Some(remote);
            p.state = ItemState::Uploaded;
            save(store, file, p)?;
        } else {
            ensure!(
                matches!(ticket.return_type, 1 | 2),
                "unknown upload ticket type"
            );
            // A positively received non-instant ticket clears the implicit
            // commit uncertainty. Its missing-part list is the resume authority.
            p.state = ItemState::Uploading;
            save(store, file, p)?;
            for index in &ticket.missing {
                retry(store, &p.job_id, token, || async {
                    let bytes = hash::read_part(file, *index)
                        .map_err(|_| anyhow::Error::from(SourceChanged))?;
                    api.upload_part(file, &ticket, *index, bytes).await
                })
                .await?;
                p.confirmed_parts.push(*index);
                p.confirmed_parts.sort_unstable();
                p.confirmed_parts.dedup();
                save(store, file, p)?;
                if p.confirmed_parts.len().is_multiple_of(16)
                    || p.confirmed_parts.len() == file.parts.len()
                {
                    let bytes: usize = p
                        .confirmed_parts
                        .iter()
                        .map(|index| file.parts[*index].len)
                        .sum();
                    eprintln!("uploaded parts: {bytes}/{} bytes", file.stamp.len);
                }
            }
            source_unchanged(file)?;
            active(store, &p.job_id, token)?;
            p.state = ItemState::CommitIntent;
            save(store, file, p)?;
            // NEVER use the retry helper here: timeout/cancel/rejected response can follow a committed mutation.
            let committed = checked(store, &p.job_id, token, api.create_file(file, &ticket)).await;
            match committed {
                Ok(remote) if metadata_matches(&remote, file.stamp.len, &file.md5) => {
                    p.remote = Some(remote);
                    p.state = ItemState::Uploaded;
                    p.error = None;
                    save(store, file, p)?;
                }
                _ => {
                    p.state = ItemState::NeedsReconciliation;
                    p.error = Some(
                        "file commit outcome unknown; reconcile by account, full digest and size"
                            .into(),
                    );
                    save(store, file, p)?;
                    bail!("file commit requires reconciliation");
                }
            }
        }
    }
    let remote = p
        .remote
        .clone()
        .context("uploaded object has no stable identity")?;
    ensure!(
        metadata_matches(&remote, file.stamp.len, &file.md5),
        "uploaded metadata differs from plan"
    );
    if verify && p.verified_sha256.as_deref() != Some(&file.sha256) {
        retry(store, &p.job_id, token, || {
            readback(
                store,
                api,
                &p.job_id,
                &remote,
                &file.sha256,
                &file.md5,
                token,
            )
        })
        .await?;
        source_unchanged(file)?;
        p.verified_sha256 = Some(file.sha256.clone());
        save(store, file, p)?;
    }
    source_unchanged(file)?;
    if !p.album_added {
        let album = album_for(store, api, &p.account, &file.album_title, &p.job_id, token).await?;
        retry(store, &p.job_id, token, || {
            api.add_to_album(&album, &remote)
        })
        .await?;
        p.album = Some(album);
        p.album_added = true;
        save(store, file, p)?;
    }
    source_unchanged(file)?;
    p.state = if p.album_added && p.verified_sha256.as_deref() == Some(&file.sha256) {
        ItemState::Verified
    } else {
        ItemState::Uploaded
    };
    p.error = None;
    save(store, file, p)
}
fn album_key(account: &str, title: &str) -> String {
    format!(
        "{account}:{}",
        hex::encode(Sha256::digest(title.as_bytes()))
    )
}
/// Record a human-selected, stable album ID after the caller has validated it with the API.
pub fn bind_album(store: &Store, account: &str, title: &str, album: &Album) -> Result<()> {
    let _lock = store.lock_account(account)?;
    ensure!(
        !album.id.is_empty() && !album.tid.is_empty(),
        "album needs stable IDs"
    );
    store.put(
        "album_intents",
        &album_key(account, title),
        &AlbumIntent {
            account: account.into(),
            title: title.into(),
            state: ItemState::Uploaded,
            album: Some(album.clone()),
        },
    )
}
async fn album_for<A: TransferApi>(
    store: &Store,
    api: &A,
    account: &str,
    title: &str,
    job: &str,
    token: &CancellationToken,
) -> Result<Album> {
    ensure!(!title.trim().is_empty(), "album title is empty");
    let key = album_key(account, title);
    if let Some(intent) = store.get::<AlbumIntent>("album_intents", &key)? {
        ensure!(
            intent.account == account && intent.title == title,
            "album receipt ownership mismatch"
        );
        if let Some(album) = intent.album {
            return Ok(album);
        }
        return Err(ReconciliationRequired.into());
    }
    let albums = retry(store, job, token, || api.albums()).await?;
    let matching: Vec<_> = albums
        .into_iter()
        .filter(|album| album.title == title)
        .collect();
    if matching.len() > 1 {
        return Err(ReconciliationRequired.into());
    }
    let mut intent = AlbumIntent {
        account: account.into(),
        title: title.into(),
        state: ItemState::CommitIntent,
        album: None,
    };
    if let Some(album) = matching.into_iter().next() {
        intent.album = Some(album.clone());
        intent.state = ItemState::Uploaded;
        store.put("album_intents", &key, &intent)?;
        return Ok(album);
    }
    active(store, job, token)?;
    store.insert("album_intents", &key, &intent)?;
    let result = checked(store, job, token, api.album_create(title)).await;
    match result {
        Ok(album) if !album.id.is_empty() && !album.tid.is_empty() && album.title == title => {
            intent.state = ItemState::Uploaded;
            intent.album = Some(album.clone());
            store.put("album_intents", &key, &intent)?;
            Ok(album)
        }
        _ => {
            intent.state = ItemState::NeedsReconciliation;
            store.put("album_intents", &key, &intent)?;
            Err(ReconciliationRequired.into())
        }
    }
}
fn metadata_matches(remote: &RemoteFile, size: u64, md5: &str) -> bool {
    !remote.fsid.is_empty()
        && remote.size == size
        && remote
            .md5
            .as_ref()
            .is_some_and(|value| value.eq_ignore_ascii_case(md5))
}
pub(crate) async fn readback<A: TransferApi>(
    store: &Store,
    api: &A,
    job: &str,
    remote: &RemoteFile,
    expected_sha: &str,
    expected_md5: &str,
    token: &CancellationToken,
) -> Result<()> {
    active(store, job, token)?;
    let response = checked(store, job, token, api.download_response(remote, 0, None)).await?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "original readback must return a complete 200 response"
    );
    if let Some(length) = response.content_length() {
        ensure!(length == remote.size, "original readback length mismatch");
    }
    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let mut length = 0_u64;
    let mut reported = 0_u64;
    let mut stream = response.bytes_stream();
    loop {
        active(store, job, token)?;
        let next = checked(store, job, token, async { Ok(stream.next().await) }).await?;
        let Some(bytes) = next else {
            break;
        };
        let bytes = bytes.context("original stream interrupted")?;
        length = length
            .checked_add(bytes.len() as u64)
            .context("original stream length overflow")?;
        ensure!(
            length <= remote.size,
            "original stream longer than planned source"
        );
        sha.update(&bytes);
        md5.update(&bytes);
        if length - reported >= 64 * 1024 * 1024 {
            eprintln!("original readback: {length}/{} bytes", remote.size);
            reported = length;
        }
    }
    ensure!(length == remote.size, "original stream was truncated");
    ensure!(
        hex::encode(sha.finalize()) == expected_sha
            && hex::encode(md5.finalize()).eq_ignore_ascii_case(expected_md5),
        "original stream hash differs from immutable plan"
    );
    active(store, job, token)
}
fn summarize(job: &str, rows: &[UploadProgress]) -> JobSummary {
    let mut result = JobSummary {
        job_id: job.into(),
        total: rows.len(),
        ..Default::default()
    };
    for row in rows {
        match row.state {
            ItemState::Verified
                if row.album_added
                    && row.verified_sha256.as_deref() == Some(&row.source_sha256) =>
            {
                result.verified += 1
            }
            ItemState::Blocked => result.blocked += 1,
            ItemState::Failed => result.failed += 1,
            ItemState::NeedsReconciliation | ItemState::CommitIntent => {
                result.needs_reconciliation += 1
            }
            _ if row.remote.is_some() => result.uploaded += 1,
            _ => result.pending += 1,
        }
    }
    result.complete = result.total > 0 && result.verified == result.total;
    result
}
pub fn summary(store: &Store, id: &str) -> Result<JobSummary> {
    let job = store
        .get::<JobRecord>("jobs", id)?
        .context("job not found")?;
    ensure!(job.kind == "upload", "job is not an upload");
    let plan = store
        .get::<UploadPlan>("upload_plans", &job.plan_id)?
        .context("job plan not found")?;
    let rows: Vec<_> = store
        .list::<UploadProgress>("upload_items")?
        .into_iter()
        .map(|(_, value)| value)
        .filter(|row| row.job_id == id)
        .collect();
    ensure!(
        rows.iter().all(|row| row.account == job.account),
        "job receipt account mismatch"
    );
    let mut result = summarize(id, &rows);
    ensure!(result.total <= plan.files.len(), "job has unexpected items");
    result.pending += plan.files.len() - result.total;
    result.total = plan.files.len();
    result.complete = result.total > 0 && result.verified == result.total;
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_result_is_never_complete() {
        let p = |state: ItemState| UploadProgress {
            job_id: "job".into(),
            file_id: "file".into(),
            account: "account".into(),
            source_sha256: "sha".into(),
            state: state.clone(),
            ticket: None,
            remote: None,
            album: None,
            album_added: state == ItemState::Verified,
            confirmed_parts: vec![],
            verified_sha256: (state == ItemState::Verified).then(|| "sha".into()),
            error: None,
        };
        let s = summarize("job", &[p(ItemState::Verified), p(ItemState::Blocked)]);
        assert!(!s.complete);
        assert_eq!((s.verified, s.blocked), (1, 1));
    }
    #[test]
    fn matching_name_is_not_reconciliation_evidence() {
        let f = RemoteFile {
            fsid: "1".into(),
            path: "same.mp4".into(),
            size: 12,
            md5: Some("wrong".into()),
            ctime: 0,
            mtime: 0,
        };
        assert!(!metadata_matches(&f, 12, "right"));
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        io::{Read, Write},
        net::TcpListener,
    };
    pub const ACCOUNT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub bytes: Vec<u8>,
    }
    impl Reply {
        pub fn full(bytes: &[u8]) -> Self {
            Self {
                status: 200,
                headers: vec![("ETag".into(), "\"fixture-v1\"".into())],
                bytes: bytes.to_vec(),
            }
        }
        pub fn range(bytes: &[u8], start: u64, total: u64) -> Self {
            Self {
                status: 206,
                headers: vec![
                    ("ETag".into(), "\"fixture-v1\"".into()),
                    (
                        "Content-Range".into(),
                        format!("bytes {start}-{}/{total}", start + bytes.len() as u64 - 1),
                    ),
                ],
                bytes: bytes.to_vec(),
            }
        }
    }
    pub async fn response(reply: Reply) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            let has_length = reply
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Content-Length"));
            let mut headers = format!("HTTP/1.1 {} Fixture\r\nConnection: close\r\n", reply.status);
            if !has_length {
                headers.push_str(&format!("Content-Length: {}\r\n", reply.bytes.len()));
            }
            for (key, value) in reply.headers {
                headers.push_str(&format!("{key}: {value}\r\n"));
            }
            headers.push_str("\r\n");
            let _ = socket.write_all(headers.as_bytes());
            let _ = socket.write_all(&reply.bytes);
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
    pub struct Fake {
        pub account: String,
        pub bytes: RefCell<Vec<u8>>,
        pub remote: RemoteFile,
        pub visible: Cell<bool>,
        pub ambiguous: Cell<bool>,
        pub creates: Cell<usize>,
        pub precreate_calls: Cell<usize>,
        pub precreate_ambiguous: Cell<bool>,
        pub precreate_instant: Cell<bool>,
        pub precreate_instant_visible: Cell<bool>,
        pub precreate_cancel: RefCell<Option<CancellationToken>>,
        pub precreate_created: Cell<bool>,
        pub precreate_intent_path: Option<std::path::PathBuf>,
        pub precreate_intent_seen: Cell<bool>,
        pub album_ambiguous: Cell<bool>,
        pub album_creates: Cell<usize>,
        pub missing: RefCell<Vec<usize>>,
        pub part_calls: RefCell<Vec<usize>>,
        pub cancel_after_part: RefCell<Option<CancellationToken>>,
        pub mutate_on_read: RefCell<Option<std::path::PathBuf>>,
        pub caps: Capabilities,
        pub replies: RefCell<std::collections::VecDeque<Reply>>,
        pub reads: RefCell<Vec<(u64, Option<String>)>>,
    }
    impl Fake {
        pub fn new(file: &PlannedFile, bytes: &[u8]) -> Self {
            Self {
                account: ACCOUNT.into(),
                bytes: RefCell::new(bytes.to_vec()),
                remote: RemoteFile {
                    fsid: "anonymous-id".into(),
                    path: "/anonymous.mp4".into(),
                    size: file.stamp.len,
                    md5: Some(file.md5.clone()),
                    ctime: 0,
                    mtime: 0,
                },
                visible: Cell::new(false),
                ambiguous: Cell::new(false),
                creates: Cell::new(0),
                precreate_calls: Cell::new(0),
                precreate_ambiguous: Cell::new(false),
                precreate_instant: Cell::new(false),
                precreate_instant_visible: Cell::new(true),
                precreate_cancel: RefCell::new(None),
                precreate_created: Cell::new(false),
                precreate_intent_path: None,
                precreate_intent_seen: Cell::new(false),
                album_ambiguous: Cell::new(false),
                album_creates: Cell::new(0),
                missing: RefCell::new((0..file.parts.len()).collect()),
                part_calls: RefCell::new(vec![]),
                cancel_after_part: RefCell::new(None),
                mutate_on_read: RefCell::new(None),
                caps: Capabilities {
                    upload_limit: Some(WEB_UPLOAD_LIMIT),
                    download_limit: Some(WEB_UPLOAD_LIMIT),
                    unlimited: Some(true),
                },
                replies: RefCell::new(Default::default()),
                reads: RefCell::new(vec![]),
            }
        }
    }
    impl TransferApi for Fake {
        fn account(&self) -> String {
            self.account.clone()
        }
        async fn capabilities(&self) -> Result<Capabilities> {
            Ok(self.caps.clone())
        }
        async fn files(&self) -> Result<Vec<RemoteFile>> {
            Ok(if self.visible.get() {
                vec![self.remote.clone()]
            } else {
                vec![]
            })
        }
        async fn albums(&self) -> Result<Vec<Album>> {
            Ok(if self.album_ambiguous.get() {
                vec![]
            } else {
                vec![Album {
                    id: "album-id".into(),
                    tid: "album-tid".into(),
                    title: "Fixture".into(),
                }]
            })
        }
        async fn album_create(&self, title: &str) -> Result<Album> {
            self.album_creates.set(self.album_creates.get() + 1);
            if self.album_ambiguous.get() {
                bail!("anonymous lost album acknowledgment");
            }
            Ok(Album {
                id: "album-id".into(),
                tid: "album-tid".into(),
                title: title.into(),
            })
        }
        async fn add_to_album(&self, _: &Album, _: &RemoteFile) -> Result<()> {
            Ok(())
        }
        async fn precreate(&self, file: &PlannedFile) -> Result<UploadTicket> {
            self.precreate_calls.set(self.precreate_calls.get() + 1);
            if let Some(path) = &self.precreate_intent_path {
                let observed = Store::open(path)?;
                let claim = observed.get::<Claim>("transfer_claims", &key(file, &self.account))?;
                self.precreate_intent_seen.set(
                    claim.is_some_and(|claim| claim.progress.state == ItemState::CommitIntent),
                );
            }
            let cancel = self.precreate_cancel.borrow_mut().take();
            if let Some(cancel) = cancel {
                self.precreate_created.set(true);
                cancel.cancel();
                return std::future::pending().await;
            }
            if self.precreate_ambiguous.get() {
                self.precreate_created.set(true);
                return Err(api::ApiError {
                    kind: api::ApiErrorKind::Timeout,
                    status: None,
                    errno: None,
                    retry_after: None,
                    operation: "precreate",
                }
                .into());
            }
            if self.precreate_instant.get() {
                self.precreate_created.set(true);
                self.visible.set(self.precreate_instant_visible.get());
                return Ok(UploadTicket {
                    return_type: 3,
                    path: self.remote.path.clone(),
                    upload_id: None,
                    missing: vec![],
                    file: Some(self.remote.clone()),
                });
            }
            Ok(UploadTicket {
                return_type: 1,
                path: "/anonymous.mp4".into(),
                upload_id: Some("anonymous-upload".into()),
                missing: self.missing.borrow().clone(),
                file: None,
            })
        }
        async fn upload_part(
            &self,
            file: &PlannedFile,
            _: &UploadTicket,
            index: usize,
            bytes: Vec<u8>,
        ) -> Result<()> {
            ensure!(
                bytes.len() <= PART_SIZE
                    && hex::encode(Sha256::digest(&bytes)) == file.parts[index].sha256,
                "incorrect source bytes"
            );
            self.part_calls.borrow_mut().push(index);
            self.missing.borrow_mut().retain(|i| *i != index);
            if let Some(token) = self.cancel_after_part.borrow_mut().take() {
                token.cancel();
            }
            Ok(())
        }
        async fn create_file(&self, _: &PlannedFile, _: &UploadTicket) -> Result<RemoteFile> {
            self.creates.set(self.creates.get() + 1);
            if self.ambiguous.get() {
                bail!("anonymous lost acknowledgment");
            }
            self.visible.set(true);
            Ok(self.remote.clone())
        }
        async fn download_response(
            &self,
            _: &RemoteFile,
            offset: u64,
            tag: Option<&str>,
        ) -> Result<reqwest::Response> {
            self.reads
                .borrow_mut()
                .push((offset, tag.map(str::to_owned)));
            let reply = self
                .replies
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Reply::full(&self.bytes.borrow()));
            let response = response(reply).await;
            if let Some(path) = self.mutate_on_read.borrow_mut().take() {
                std::fs::write(path, b"source-mutated-during-readback").unwrap();
            }
            Ok(response)
        }
    }
    pub fn source(dir: &Path, bytes: &[u8]) -> PlannedFile {
        let path = dir.join("anonymous.mp4");
        std::fs::write(&path, bytes).unwrap();
        hash::digest_file(&path, "anonymous.mp4".into(), "Fixture".into()).unwrap()
    }
    use std::path::Path;
    pub fn plan(file: PlannedFile) -> UploadPlan {
        UploadPlan {
            schema: "yike.upload.v1".into(),
            id: uuid::Uuid::new_v4().to_string(),
            account: ACCOUNT.into(),
            root: file.path.parent().unwrap().into(),
            created_at: "fixture".into(),
            upload_limit: WEB_UPLOAD_LIMIT,
            files: vec![file],
        }
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::fixtures::*;
    use super::*;
    #[tokio::test]
    async fn ambiguous_commit_is_never_repeated_across_new_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"anonymous-original"));
        let api = Fake::new(&plan.files[0], b"anonymous-original");
        api.ambiguous.set(true);
        let first = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(first.needs_reconciliation, 1);
        assert!(!first.complete);
        assert_eq!(api.creates.get(), 1);
        let second = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(second.needs_reconciliation, 1);
        assert_eq!(api.creates.get(), 1);
        api.visible.set(true);
        let reconciled = run_inner(
            &store,
            &api,
            &plan,
            Some(&second.job_id),
            CancellationToken::new(),
            true,
        )
        .await
        .unwrap();
        assert!(reconciled.complete);
        assert_eq!(api.creates.get(), 1);
        let reused = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert!(reused.complete);
        assert_eq!(api.creates.get(), 1);
        assert_eq!(api.reads.borrow().len(), 1);
    }
    #[tokio::test]
    async fn wrong_original_bytes_remain_uploaded_and_never_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"right"));
        let api = Fake::new(&plan.files[0], b"wrong");
        let result = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!((result.uploaded, result.verified), (1, 0));
        assert!(!result.complete);
        *api.bytes.borrow_mut() = b"right".to_vec();
        let resumed = run_inner(
            &store,
            &api,
            &plan,
            Some(&result.job_id),
            CancellationToken::new(),
            true,
        )
        .await
        .unwrap();
        assert!(resumed.complete);
        assert_eq!(api.creates.get(), 1);
    }
    #[tokio::test]
    async fn interrupted_parts_resume_from_server_missing_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let bytes = vec![17; PART_SIZE + 11];
        let plan = plan(source(dir.path(), &bytes));
        let api = Fake::new(&plan.files[0], &bytes);
        let token = CancellationToken::new();
        *api.cancel_after_part.borrow_mut() = Some(token.clone());
        let first = run_inner(&store, &api, &plan, None, token, true)
            .await
            .unwrap();
        assert!(!first.complete);
        assert_eq!(api.creates.get(), 0);
        let resumed = run_inner(
            &store,
            &api,
            &plan,
            Some(&first.job_id),
            CancellationToken::new(),
            true,
        )
        .await
        .unwrap();
        assert!(resumed.complete);
        assert_eq!(*api.part_calls.borrow(), vec![0, 1]);
    }
    #[tokio::test]
    async fn account_mismatch_cannot_create_a_job_or_submit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"fixture"));
        let mut api = Fake::new(&plan.files[0], b"fixture");
        api.account = "b".repeat(64);
        assert!(
            run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
                .await
                .is_err()
        );
        assert!(store.list::<JobRecord>("jobs").unwrap().is_empty());
        assert_eq!(api.creates.get(), 0);
    }
    #[tokio::test]
    async fn ambiguous_album_creation_requires_explicit_stable_binding() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"fixture"));
        let api = Fake::new(&plan.files[0], b"fixture");
        api.album_ambiguous.set(true);
        let first = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(first.needs_reconciliation, 1);
        assert_eq!(api.album_creates.get(), 1);
        let next = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(next.needs_reconciliation, 1);
        assert_eq!(api.album_creates.get(), 1);
        bind_album(
            &store,
            ACCOUNT,
            "Fixture",
            &Album {
                id: "chosen-stable-id".into(),
                tid: "chosen-tid".into(),
                title: "Fixture".into(),
            },
        )
        .unwrap();
        let recovered = run_inner(
            &store,
            &api,
            &plan,
            Some(&next.job_id),
            CancellationToken::new(),
            true,
        )
        .await
        .unwrap();
        assert!(recovered.complete);
        assert_eq!(api.creates.get(), 1);
        assert_eq!(api.album_creates.get(), 1);
    }
    #[tokio::test]
    async fn changed_source_and_unknown_capability_never_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"fixture"));
        let mut api = Fake::new(&plan.files[0], b"fixture");
        std::fs::write(&plan.files[0].path, b"mutated-longer").unwrap();
        let changed = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(changed.failed, 1);
        assert_eq!(api.creates.get(), 0);
        api.caps.upload_limit = None;
        let fresh = fixtures::plan(source(dir.path(), b"fixture"));
        let blocked = run_inner(&store, &api, &fresh, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(blocked.blocked, 1);
        assert_eq!(api.creates.get(), 0);
    }
}

#[cfg(test)]
mod explicit_verification_tests {
    use super::fixtures::*;
    use super::*;
    #[tokio::test]
    async fn explicit_verify_reads_again_and_preserves_previous_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"right"));
        let api = Fake::new(&plan.files[0], b"right");
        let initial = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert!(initial.complete);
        assert_eq!(api.reads.borrow().len(), 1);
        *api.bytes.borrow_mut() = b"wrong".to_vec();
        let checked = verify_inner(&store, &api, &initial.job_id, CancellationToken::new())
            .await
            .unwrap();
        assert!(!checked.complete);
        assert_eq!(checked.verified, 0);
        assert_eq!(api.reads.borrow().len(), 2);
        assert_eq!(api.creates.get(), 1);
        let history = store
            .list::<UploadProgress>("verification_history")
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1.state, ItemState::Verified);
    }
}

#[cfg(test)]
mod source_race_tests {
    use super::fixtures::*;
    use super::*;
    #[tokio::test]
    async fn source_changed_during_reconciliation_cannot_be_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"right"));
        let api = Fake::new(&plan.files[0], b"right");
        api.ambiguous.set(true);
        let initial = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(initial.needs_reconciliation, 1);
        api.visible.set(true);
        *api.mutate_on_read.borrow_mut() = Some(plan.files[0].path.clone());
        let result = run_inner(
            &store,
            &api,
            &plan,
            Some(&initial.job_id),
            CancellationToken::new(),
            true,
        )
        .await
        .unwrap();
        assert!(!result.complete);
        assert_eq!(result.verified, 0);
        assert_eq!(result.failed, 1);
        assert_eq!(api.creates.get(), 1);
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::fixtures::*;
    use super::*;
    #[tokio::test]
    async fn persisted_cancel_stops_an_idle_network_wait() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let mut record = job(&store, "upload", "plan", ACCOUNT, None).unwrap();
        let token = CancellationToken::new();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let wait = checked(
                &store,
                &record.id,
                &token,
                std::future::pending::<Result<()>>(),
            );
            let cancel = async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                let mut cancelled_record = record.clone();
                cancelled_record.cancelled = true;
                store.put("jobs", &record.id, &cancelled_record).unwrap();
            };
            let (result, _) = tokio::join!(wait, cancel);
            result
        })
        .await
        .unwrap();
        assert!(result.unwrap_err().downcast_ref::<Cancelled>().is_some());
        assert!(!token.is_cancelled());
        record.cancelled = false;
        store.put("jobs", &record.id, &record).unwrap();
    }
    #[tokio::test]
    async fn explicit_verify_never_submits_pending_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"right"));
        let api = Fake::new(&plan.files[0], b"right");
        let token = CancellationToken::new();
        *api.cancel_after_part.borrow_mut() = Some(token.clone());
        let initial = run_inner(&store, &api, &plan, None, token, true)
            .await
            .unwrap();
        assert_eq!(api.creates.get(), 0);
        let before = api.part_calls.borrow().clone();
        let checked = verify_inner(&store, &api, &initial.job_id, CancellationToken::new())
            .await
            .unwrap();
        assert!(!checked.complete);
        assert_eq!(api.creates.get(), 0);
        assert_eq!(*api.part_calls.borrow(), before);
        assert!(api.reads.borrow().is_empty());
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn typed_api_diagnostics_survive_without_raw_context_or_urls() {
        let typed: anyhow::Error = api::ApiError {
            kind: api::ApiErrorKind::Rejected,
            status: Some(503),
            errno: Some(31066),
            retry_after: None,
            operation: "upload_part",
        }
        .into();
        let wrapped =
            typed.context("anonymous outer context https://example.invalid/?secret=DO_NOT_RENDER");
        let rendered = safe_error(&wrapped);
        assert!(rendered.contains("Rejected"));
        assert!(rendered.contains("503"));
        assert!(rendered.contains("31066"));
        assert!(rendered.contains("upload_part"));
        assert!(!rendered.contains("DO_NOT_RENDER"));
        assert!(!rendered.contains("https://"));
        assert!(!rendered.contains("outer context"));
        let raw = anyhow::anyhow!("https://example.invalid/?secret=DO_NOT_RENDER");
        assert!(!safe_error(&raw).contains("DO_NOT_RENDER"));
    }
}

#[cfg(test)]
mod precreate_intent_tests {
    use super::fixtures::*;
    use super::*;
    #[tokio::test]
    async fn lost_precreate_ack_never_repeats_across_jobs_and_reconciles_original() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("state/db");
        let store = Store::open(&database).unwrap();
        let plan = plan(source(dir.path(), b"anonymous-original"));
        let mut api = Fake::new(&plan.files[0], b"anonymous-original");
        api.precreate_ambiguous.set(true);
        api.precreate_intent_path = Some(database.clone());
        let first = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(first.needs_reconciliation, 1);
        assert_eq!(api.precreate_calls.get(), 1);
        assert!(api.precreate_created.get());
        assert!(api.precreate_intent_seen.get());
        assert_eq!(api.creates.get(), 0);
        drop(store);
        let store = Store::open(&database).unwrap();
        let unresolved = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(unresolved.needs_reconciliation, 1);
        assert_eq!(api.precreate_calls.get(), 1);
        assert!(!unresolved.complete);
        api.visible.set(true);
        let reconciled = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert!(reconciled.complete);
        assert_eq!(api.precreate_calls.get(), 1);
        assert_eq!(api.creates.get(), 0);
        assert!(api.part_calls.borrow().is_empty());
        assert_eq!(api.reads.borrow().len(), 1);
    }
    #[tokio::test]
    async fn received_instant_precreate_result_verifies_without_final_create() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"anonymous-original"));
        let api = Fake::new(&plan.files[0], b"anonymous-original");
        api.precreate_instant.set(true);
        let result = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert!(result.complete);
        assert_eq!(api.precreate_calls.get(), 1);
        assert_eq!(api.creates.get(), 0);
        assert!(api.part_calls.borrow().is_empty());
        assert_eq!(api.reads.borrow().len(), 1);
    }
}

#[cfg(test)]
mod precreate_cancellation_tests {
    use super::fixtures::*;
    use super::*;
    #[tokio::test]
    async fn cancelled_precreate_keeps_ambiguity_and_reconciles_without_resubmit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"anonymous-original"));
        let api = Fake::new(&plan.files[0], b"anonymous-original");
        let token = CancellationToken::new();
        *api.precreate_cancel.borrow_mut() = Some(token.clone());
        let cancelled = run_inner(&store, &api, &plan, None, token, true)
            .await
            .unwrap();
        assert_eq!(cancelled.needs_reconciliation, 1);
        assert_eq!(api.precreate_calls.get(), 1);
        assert_eq!(api.creates.get(), 0);
        api.visible.set(true);
        let reconciled = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert!(reconciled.complete);
        assert_eq!(api.precreate_calls.get(), 1);
        assert_eq!(api.creates.get(), 0);
    }
    #[tokio::test]
    async fn unlisted_instant_precreate_object_stays_uncertain_across_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state/db")).unwrap();
        let plan = plan(source(dir.path(), b"anonymous-original"));
        let api = Fake::new(&plan.files[0], b"anonymous-original");
        api.precreate_instant.set(true);
        api.precreate_instant_visible.set(false);
        let unlisted = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(unlisted.needs_reconciliation, 1);
        assert_eq!(unlisted.uploaded, 0);
        let unresolved = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(unresolved.needs_reconciliation, 1);
        assert_eq!(api.precreate_calls.get(), 1);
        assert_eq!(api.creates.get(), 0);
        assert!(api.reads.borrow().is_empty());
        api.visible.set(true);
        let reconciled = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert!(reconciled.complete);
        assert_eq!(api.precreate_calls.get(), 1);
        assert_eq!(api.creates.get(), 0);
    }
}

#[cfg(test)]
mod precreate_checkpoint_tests {
    use super::fixtures::*;
    use super::*;
    #[tokio::test]
    async fn first_noninstant_ticket_checkpoint_is_uploading() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("state/db");
        let store = Store::open(&database).unwrap();
        let observer = rusqlite::Connection::open(&database).unwrap();
        observer.execute_batch("CREATE TABLE anonymous_checkpoint_states(state TEXT NOT NULL); CREATE TRIGGER observe_ticket_checkpoint AFTER UPDATE ON records WHEN NEW.kind='transfer_claims' AND json_type(NEW.body,'$.progress.ticket')='object' BEGIN INSERT INTO anonymous_checkpoint_states(state) VALUES(json_extract(NEW.body,'$.progress.state')); END;").unwrap();
        drop(observer);
        let plan = plan(source(dir.path(), b"anonymous-original"));
        let api = Fake::new(&plan.files[0], b"anonymous-original");
        let result = run_inner(&store, &api, &plan, None, CancellationToken::new(), true)
            .await
            .unwrap();
        assert!(result.complete);
        let observer = rusqlite::Connection::open(&database).unwrap();
        let first: String = observer
            .query_row(
                "SELECT state FROM anonymous_checkpoint_states ORDER BY rowid LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            first, "uploading",
            "a confirmed session ticket must not leave a durable commit-intent intermediate state"
        );
    }
}
