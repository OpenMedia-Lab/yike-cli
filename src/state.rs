use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

/// Durable local records. Plans are create-only; per-item receipts are independent rows.
pub struct Store {
    connection: Connection,
    directory: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        require_native_path(path)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .context("state path needs a directory")?;
        private_directory(parent)?;
        if let Ok(meta) = fs::symlink_metadata(path) {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "state path must be a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(meta.nlink() == 1, "state file must not have hard links");
            }
        }
        // Precreate privately before SQLite opens it; WAL/SHM inherit private file permissions.
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options
            .open(path)
            .context("cannot create private state database")?;
        protect_path(path, false)?;
        // macOS /var is a system symlink; resolve trusted ancestors while retaining
        // the explicit no-follow check for the database leaf.
        let directory = parent.canonicalize()?;
        let database_path = directory.join(path.file_name().context("invalid state filename")?);
        let connection = Connection::open_with_flags(
            database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS records (kind TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL, updated INTEGER NOT NULL, PRIMARY KEY(kind,id)); PRAGMA user_version=1;")?;
        Ok(Self {
            connection,
            directory,
        })
    }

    pub fn root_dir(&self) -> &Path {
        &self.directory
    }

    pub fn get<T: DeserializeOwned>(&self, kind: &str, id: &str) -> Result<Option<T>> {
        let body: Option<String> = self
            .connection
            .query_row(
                "SELECT body FROM records WHERE kind=?1 AND id=?2",
                params![kind, id],
                |r| r.get(0),
            )
            .optional()?;
        body.map(|v| {
            serde_json::from_str(&v).context("invalid state record; preserve database for recovery")
        })
        .transpose()
    }

    pub fn put<T: Serialize>(&self, kind: &str, id: &str, value: &T) -> Result<()> {
        ensure!(
            !matches!(kind, "upload_plans" | "download_plans"),
            "plans are immutable; create a new plan"
        );
        self.connection.execute("INSERT INTO records(kind,id,body,updated) VALUES(?1,?2,?3,unixepoch()) ON CONFLICT(kind,id) DO UPDATE SET body=excluded.body,updated=excluded.updated",params![kind,id,serde_json::to_string(value)?])?;
        Ok(())
    }

    pub fn insert<T: Serialize>(&self, kind: &str, id: &str, value: &T) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO records(kind,id,body,updated) VALUES(?1,?2,?3,unixepoch())",
                params![kind, id, serde_json::to_string(value)?],
            )
            .context("record already exists or could not be persisted")?;
        Ok(())
    }

    /// Atomically checkpoint coupled progress and ownership receipts.
    pub fn put_pair<T: Serialize, U: Serialize>(
        &self,
        first: (&str, &str, &T),
        second: (&str, &str, &U),
    ) -> Result<()> {
        for kind in [first.0, second.0] {
            ensure!(
                !matches!(kind, "upload_plans" | "download_plans"),
                "plans are immutable; create a new plan"
            );
        }
        let first_body = serde_json::to_string(first.2)?;
        let second_body = serde_json::to_string(second.2)?;
        let transaction = self.connection.unchecked_transaction()?;
        let sql = "INSERT INTO records(kind,id,body,updated) VALUES(?1,?2,?3,unixepoch()) ON CONFLICT(kind,id) DO UPDATE SET body=excluded.body,updated=excluded.updated";
        transaction.execute(sql, params![first.0, first.1, first_body])?;
        transaction.execute(sql, params![second.0, second.1, second_body])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn list<T: DeserializeOwned>(&self, kind: &str) -> Result<Vec<(String, T)>> {
        let mut statement = self
            .connection
            .prepare("SELECT id,body FROM records WHERE kind=?1 ORDER BY updated,id")?;
        let rows = statement.query_map([kind], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, body) = row?;
            Ok((
                id,
                serde_json::from_str(&body)
                    .context("invalid state record; preserve database for recovery")?,
            ))
        })
        .collect()
    }

    pub fn lock_account(&self, fingerprint: &str) -> Result<LockGuard> {
        ensure!(
            fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid account fingerprint"
        );
        let locks = self.directory.join("locks");
        private_directory(&locks)?;
        let path = locks.join(format!("{fingerprint}.lock"));
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("unsafe lock path");
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        file.try_lock_exclusive()
            .context("another transfer for this account is running")?;
        Ok(LockGuard { file })
    }
}

pub struct LockGuard {
    file: File,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub fn private_directory(path: &Path) -> Result<()> {
    require_native_path(path)?;
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "private directory must not be a link"
        );
    }
    fs::create_dir_all(path).context("cannot create private directory")?;
    protect_path(path, true)
}

/// Resolve existing ancestors before creating any private data. This catches
/// relative paths and home-directory links that actually lead to a media drive.
pub fn resolved_future_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.try_exists()? {
        suffix.push(
            ancestor
                .file_name()
                .context("cannot resolve private path")?
                .to_os_string(),
        );
        ancestor = ancestor
            .parent()
            .context("cannot resolve private path ancestor")?;
    }
    let mut resolved = ancestor.canonicalize()?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

pub fn require_native_path(path: &Path) -> Result<()> {
    let resolved = resolved_future_path(path)?;
    #[cfg(target_os = "linux")]
    if std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|s| s.to_lowercase().contains("microsoft"))
    {
        ensure!(
            !resolved.starts_with("/mnt"),
            "private state must be on the native WSL filesystem, outside mounted media drives"
        );
        use std::os::unix::ffi::OsStrExt;
        let mut ancestor = resolved.as_path();
        while !ancestor.try_exists()? {
            ancestor = ancestor
                .parent()
                .context("cannot inspect private filesystem")?;
        }
        let name = std::ffi::CString::new(ancestor.as_os_str().as_bytes())?;
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: CString is live and statfs receives valid writable storage.
        ensure!(
            unsafe { libc::statfs(name.as_ptr(), info.as_mut_ptr()) } == 0,
            "cannot inspect private filesystem"
        );
        let filesystem = unsafe { info.assume_init() }.f_type;
        ensure!(
            matches!(filesystem, 0xEF53 | 0x58465342 | 0x9123683E | 0x01021994),
            "private state requires a native Linux filesystem"
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = resolved;
    Ok(())
}

pub fn protect_path(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
    }
    #[cfg(windows)]
    {
        use std::process::{Command, Stdio};
        // Replace the DACL rather than merely granting current-user access:
        // pre-existing explicit access rules must not survive.
        let script = r#"$ErrorActionPreference='Stop';try{$p=$env:YIKE_PRIVATE_PATH;$u=[Security.Principal.WindowsIdentity]::GetCurrent().User;$a=Get-Acl -LiteralPath $p;$a.SetAccessRuleProtection($true,$false);foreach($r in @($a.Access)){$a.RemoveAccessRuleSpecific($r)};$a.SetOwner($u);if(Test-Path -LiteralPath $p -PathType Container){$r=[Security.AccessControl.FileSystemAccessRule]::new($u,'FullControl','ContainerInherit,ObjectInherit','None','Allow')}else{$r=[Security.AccessControl.FileSystemAccessRule]::new($u,'FullControl','Allow')};$a.AddAccessRule($r);Set-Acl -LiteralPath $p -AclObject $a;exit 0}catch{exit 1}"#;
        let status = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .env("YIKE_PRIVATE_PATH", path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("cannot restrict Windows private path ACL")?;
        ensure!(status.success(), "cannot restrict Windows private path ACL");
        let _ = directory;
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, directory);
        bail!("unsupported private storage platform");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn future_private_path_resolves_existing_links_before_creation() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("actual");
        fs::create_dir(&target).unwrap();
        let link = directory.path().join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(
            resolved_future_path(&link.join("new/state.db")).unwrap(),
            target.canonicalize().unwrap().join("new/state.db")
        );
        assert!(!target.join("new").exists());
    }

    #[test]
    fn paired_checkpoint_rolls_back_both_records_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(&dir.path().join("db")).unwrap();
        db.connection.execute_batch("CREATE TRIGGER reject_fixture BEFORE INSERT ON records WHEN NEW.id='reject' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(
            db.put_pair(("receipts", "first", &1), ("progress", "reject", &2))
                .is_err()
        );
        assert!(db.get::<u64>("receipts", "first").unwrap().is_none());
        db.put_pair(("receipts", "first", &1), ("progress", "second", &2))
            .unwrap();
        assert_eq!(db.get::<u64>("progress", "second").unwrap(), Some(2));
    }

    #[test]
    fn immutable_plans_survive_reopen_without_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let db = Store::open(&path).unwrap();
        db.insert("upload_plans", "p", &json!({"sha":"original"}))
            .unwrap();
        assert!(
            db.insert("upload_plans", "p", &json!({"sha":"changed"}))
                .is_err()
        );
        assert!(
            db.put("upload_plans", "p", &json!({"sha":"changed"}))
                .is_err()
        );
        drop(db);
        let db = Store::open(&path).unwrap();
        assert_eq!(
            db.get::<serde_json::Value>("upload_plans", "p")
                .unwrap()
                .unwrap()["sha"],
            "original"
        );
    }

    #[test]
    fn account_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        let fingerprint = "a".repeat(64);
        let guard = db.lock_account(&fingerprint).unwrap();
        assert!(db.lock_account(&fingerprint).is_err());
        drop(guard);
        assert!(db.lock_account(&fingerprint).is_ok());
        assert!(db.lock_account("../bad").is_err());
    }

    #[test]
    fn mutable_progress_is_durable_and_lists_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        db.put("upload_items", "job:file", &1_u64).unwrap();
        db.put("upload_items", "job:file", &2_u64).unwrap();
        assert_eq!(
            db.list::<u64>("upload_items").unwrap(),
            vec![("job:file".to_string(), 2)]
        );
        assert_eq!(db.get::<u64>("upload_items", "missing").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn database_and_directory_are_private_and_symlinks_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        let path = private.join("state.sqlite3");
        Store::open(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        symlink(&path, private.join("linked.sqlite3")).unwrap();
        assert!(Store::open(&private.join("linked.sqlite3")).is_err());
    }
}
