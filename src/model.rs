use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PART_SIZE: usize = 4 * 1024 * 1024;
pub const SLICE_SIZE: usize = 256 * 1024;
pub const WEB_UPLOAD_LIMIT: u64 = 5 * 1024 * 1024 * 1024;

/// Only stored in a private configuration file; never derive Debug or print it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub cookie: String,
    pub bdstoken: String,
    pub account_id: String,
}
impl Session {
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(self.account_id.as_bytes()))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub upload_limit: Option<u64>,
    pub download_limit: Option<u64>,
    pub unlimited: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteFile {
    pub fsid: String,
    pub path: String,
    pub size: u64,
    pub md5: Option<String>,
    pub ctime: i64,
    pub mtime: i64,
}
impl RemoteFile {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub tid: String,
    pub title: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadTicket {
    pub return_type: u8,
    pub path: String,
    pub upload_id: Option<String>,
    pub missing: Vec<usize>,
    pub file: Option<RemoteFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub modified_ns: u128,
    pub identity: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartDigest {
    pub md5: String,
    pub sha256: String,
    pub len: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedFile {
    pub id: String,
    pub path: PathBuf,
    pub relative_path: String,
    pub stamp: FileStamp,
    pub md5: String,
    pub sha256: String,
    pub slice_md5: String,
    pub parts: Vec<PartDigest>,
    pub blocked: Option<String>,
    pub album_title: String,
}
impl PlannedFile {
    pub fn name(&self) -> &str {
        self.relative_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.relative_path)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadPlan {
    pub schema: String,
    pub id: String,
    pub account: String,
    pub root: PathBuf,
    pub created_at: String,
    pub upload_limit: u64,
    pub files: Vec<PlannedFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Pending,
    Uploading,
    CommitIntent,
    NeedsReconciliation,
    Uploaded,
    Verified,
    Blocked,
    Failed,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadProgress {
    pub job_id: String,
    pub file_id: String,
    pub account: String,
    pub source_sha256: String,
    pub state: ItemState,
    pub ticket: Option<UploadTicket>,
    pub remote: Option<RemoteFile>,
    pub album: Option<Album>,
    pub album_added: bool,
    pub confirmed_parts: Vec<usize>,
    pub verified_sha256: Option<String>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: String,
    pub plan_id: String,
    pub account: String,
    pub created_at: String,
    pub cancelled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JobSummary {
    pub job_id: String,
    pub total: usize,
    pub verified: usize,
    pub uploaded: usize,
    pub blocked: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_reconciliation: usize,
    pub complete: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadPlan {
    pub schema: String,
    pub id: String,
    pub account: String,
    pub target: PathBuf,
    pub created_at: String,
    pub files: Vec<RemoteFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub job_id: String,
    pub fsid: String,
    pub account: String,
    pub destination: PathBuf,
    pub partial: PathBuf,
    pub offset: u64,
    pub etag: Option<String>,
    pub state: ItemState,
    pub sha256: Option<String>,
    pub error: Option<String>,
}
