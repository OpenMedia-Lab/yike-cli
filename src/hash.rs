//! Bounded source hashing and immutable-plan reads.
use crate::model::{FileStamp, PART_SIZE, PartDigest, PlannedFile, SLICE_SIZE};
use anyhow::{Context, Result, ensure};
use md5::{Digest, Md5};
use sha2::Sha256;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;
use tokio_util::sync::CancellationToken;

/// Capture the current regular file, including its native filesystem identity.
pub fn stamp(path: &Path) -> Result<FileStamp> {
    require_regular(&fs::symlink_metadata(path).context("cannot inspect source")?)?;
    let file = open_read(path)?;
    let actual = handle_stamp(&file)?;
    check_path(path, &file, &actual)?;
    Ok(actual)
}

pub fn assert_stamp(file: &PlannedFile) -> Result<()> {
    ensure!(
        stamp(&file.path)? == file.stamp,
        "source changed since planning; create a new plan"
    );
    Ok(())
}
pub fn digest_file(path: &Path, relative_path: String, album_title: String) -> Result<PlannedFile> {
    digest_file_cancellable(path, relative_path, album_title, &CancellationToken::new())
}
/// Hash one source with at most one 4 MiB source buffer. A completed file is the
/// cache boundary: cancellation never publishes partial whole-file digests.
pub fn digest_file_cancellable(
    path: &Path,
    relative_path: String,
    album_title: String,
    cancel: &CancellationToken,
) -> Result<PlannedFile> {
    ensure!(!cancel.is_cancelled(), "planning cancelled");
    let expected = stamp(path)?;
    let mut file = open_read(path)?;
    ensure!(
        handle_stamp(&file)? == expected,
        "source changed while opening"
    );
    let mut whole_md5 = Md5::new();
    let mut whole_sha256 = Sha256::new();
    let mut slice_md5 = Md5::new();
    let mut buffer = vec![0; PART_SIZE];
    let mut offset = 0_u64;
    let mut parts = Vec::new();
    while offset < expected.len {
        ensure!(!cancel.is_cancelled(), "planning cancelled");
        let len = (expected.len - offset).min(PART_SIZE as u64) as usize;
        file.read_exact(&mut buffer[..len])
            .context("source became truncated or unreadable while hashing")?;
        let bytes = &buffer[..len];
        whole_md5.update(bytes);
        whole_sha256.update(bytes);
        if offset == 0 {
            slice_md5.update(&bytes[..len.min(SLICE_SIZE)]);
        }
        parts.push(PartDigest {
            md5: hex::encode(Md5::digest(bytes)),
            sha256: hex::encode(Sha256::digest(bytes)),
            len,
        });
        offset += len as u64;
    }
    ensure!(!cancel.is_cancelled(), "planning cancelled");
    // Verify both the held descriptor and the name: a rename/replacement must
    // not turn a digest of a detached inode into a plan for a different file.
    check_path(path, &file, &expected)?;
    ensure!(stamp(path)? == expected, "source changed while hashing");
    let md5 = hex::encode(whole_md5.finalize());
    let sha256 = hex::encode(whole_sha256.finalize());
    let mut identity = Sha256::new();
    identity.update(path.as_os_str().as_encoded_bytes());
    identity.update([0]);
    identity.update(expected.identity.as_bytes());
    identity.update(expected.len.to_le_bytes());
    identity.update(expected.modified_ns.to_le_bytes());
    identity.update(sha256.as_bytes());
    Ok(PlannedFile {
        id: hex::encode(identity.finalize()),
        path: path.to_path_buf(),
        relative_path,
        stamp: expected,
        md5,
        sha256,
        slice_md5: hex::encode(slice_md5.finalize()),
        parts,
        blocked: None,
        album_title,
    })
}

/// Read one immutable planned part; verify its bytes before any network call.
pub fn read_part(planned: &PlannedFile, index: usize) -> Result<Vec<u8>> {
    ensure!(planned.blocked.is_none(), "source is blocked by its plan");
    let part = planned
        .parts
        .get(index)
        .context("part index is outside the plan")?;
    let offset = (index as u64)
        .checked_mul(PART_SIZE as u64)
        .context("part offset overflow")?;
    ensure!(offset < planned.stamp.len, "part lies outside the source");
    let expected_len = (planned.stamp.len - offset).min(PART_SIZE as u64) as usize;
    ensure!(part.len == expected_len, "invalid planned part length");
    assert_stamp(planned)?;
    let mut file = open_read(&planned.path)?;
    ensure!(
        handle_stamp(&file)? == planned.stamp,
        "source changed while opening a part"
    );
    file.seek(SeekFrom::Start(offset))
        .context("cannot seek source part")?;
    let mut bytes = vec![0; part.len];
    file.read_exact(&mut bytes)
        .context("source part became truncated or unreadable")?;
    check_path(&planned.path, &file, &planned.stamp)?;
    assert_stamp(planned)?;
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == part.sha256
            && hex::encode(Md5::digest(&bytes)) == part.md5,
        "source part digest changed; create a new plan"
    );
    Ok(bytes)
}

fn require_regular(metadata: &Metadata) -> Result<()> {
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "source must be a regular file, never a symlink"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "source reparse points are not supported"
        );
    }
    Ok(())
}

fn open_read(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A swapped FIFO must not hang the scan, nor may a swapped symlink be
        // followed between the path metadata check and opening the source.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: inspect/reject a link rather than follow.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path).context("cannot open source read-only")?;
    require_regular(&file.metadata().context("cannot inspect open source")?)?;
    Ok(file)
}

fn handle_stamp(file: &File) -> Result<FileStamp> {
    let metadata = file.metadata().context("cannot inspect open source")?;
    require_regular(&metadata)?;
    Ok(FileStamp {
        len: metadata.len(),
        modified_ns: metadata
            .modified()
            .context("source modification time is unavailable")?
            .duration_since(UNIX_EPOCH)
            .context("source modification time predates the Unix epoch")?
            .as_nanos(),
        identity: native_identity(file, &metadata)?,
    })
}

fn check_path(path: &Path, file: &File, expected: &FileStamp) -> Result<()> {
    ensure!(
        handle_stamp(file)? == *expected,
        "source changed during reading"
    );
    let metadata = fs::symlink_metadata(path).context("source path disappeared during reading")?;
    require_regular(&metadata)?;
    ensure!(
        metadata.len() == expected.len
            && metadata.modified()?.duration_since(UNIX_EPOCH)?.as_nanos() == expected.modified_ns,
        "source path changed during reading"
    );
    #[cfg(unix)]
    ensure!(
        native_identity(file, &metadata)? == expected.identity,
        "source identity changed during reading"
    );
    #[cfg(windows)]
    ensure!(
        handle_stamp(&open_read(path)?)? == *expected,
        "source identity changed during reading"
    );
    Ok(())
}

#[cfg(unix)]
fn native_identity(_file: &File, metadata: &Metadata) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn native_identity(file: &File, _metadata: &Metadata) -> Result<String> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        attributes: u32,
        creation: FileTime,
        access: FileTime,
        write: FileTime,
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            information: *mut FileInformation,
        ) -> i32;
    }
    let mut information = FileInformation::default();
    // SAFETY: file supplies a live owned Windows handle and information has
    // the documented BY_HANDLE_FILE_INFORMATION layout and writable storage.
    let success = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
    if success == 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot determine native source identity");
    }
    Ok(format!(
        "windows:{}:{}:{}",
        information.volume, information.index_high, information.index_low
    ))
}

#[cfg(not(any(unix, windows)))]
fn native_identity(_file: &File, _metadata: &Metadata) -> Result<String> {
    anyhow::bail!("this platform does not expose a supported stable source identity")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PART_SIZE, SLICE_SIZE};
    use md5::{Digest, Md5};
    use sha2::Sha256;
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn records_known_whole_and_slice_digests() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("anonymous.mp4");
        fs::write(&path, b"abc").unwrap();
        let plan = digest_file(&path, "anonymous.mp4".into(), "Fixture".into()).unwrap();
        assert_eq!(plan.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            plan.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(plan.slice_md5, plan.md5);
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(plan.parts[0].len, 3);
        assert_eq!(read_part(&plan, 0).unwrap(), b"abc");
        assert!(read_part(&plan, 1).is_err());
    }

    #[test]
    fn hashes_and_reads_fixed_size_parts_with_a_short_tail() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("parts.MP4");
        let mut bytes = vec![91; PART_SIZE + 31];
        bytes[PART_SIZE..].fill(17);
        fs::write(&path, &bytes).unwrap();
        let plan = digest_file(&path, "parts.MP4".into(), "Fixture".into()).unwrap();
        assert_eq!(plan.md5, hex::encode(Md5::digest(&bytes)));
        assert_eq!(plan.sha256, hex::encode(Sha256::digest(&bytes)));
        assert_eq!(
            plan.slice_md5,
            hex::encode(Md5::digest(&bytes[..SLICE_SIZE]))
        );
        assert_eq!(
            plan.parts.iter().map(|p| p.len).collect::<Vec<_>>(),
            vec![PART_SIZE, 31]
        );
        assert_eq!(read_part(&plan, 1).unwrap(), vec![17; 31]);
    }

    #[test]
    fn rejects_changed_source_and_replacement_identity() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("source.mp4");
        fs::write(&path, b"original").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let plan = digest_file(&path, "source.mp4".into(), "Fixture".into()).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        assert!(assert_stamp(&plan).is_err());
        assert!(read_part(&plan, 0).is_err());
        let replacement = dir.path().join("replacement.mp4");
        fs::write(&replacement, b"original").unwrap();
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        fs::remove_file(&path).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(assert_stamp(&plan).is_err());
    }

    #[test]
    fn verifies_part_bytes_even_if_size_and_mtime_are_restored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("source.mp4");
        fs::write(&path, b"original").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let plan = digest_file(&path, "source.mp4".into(), "Fixture".into()).unwrap();
        fs::write(&path, b"tampered").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(
            read_part(&plan, 0)
                .unwrap_err()
                .to_string()
                .contains("digest")
        );
    }

    #[test]
    fn handles_empty_file_without_inventing_a_part() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.mp4");
        fs::write(&path, []).unwrap();
        let plan = digest_file(&path, "empty.mp4".into(), "Fixture".into()).unwrap();
        assert_eq!(plan.md5, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(plan.sha256, hex::encode(Sha256::digest([])));
        assert_eq!(plan.slice_md5, plan.md5);
        assert!(plan.parts.is_empty());
        assert!(read_part(&plan, 0).is_err());
    }

    #[test]
    fn cancellation_does_not_return_a_partial_digest() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("source.mp4");
        fs::write(&path, b"fixture").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            digest_file_cancellable(&path, "source.mp4".into(), "Fixture".into(), &cancel).is_err()
        );
    }

    #[test]
    fn rejects_a_directory_as_a_regular_source() {
        let dir = tempdir().unwrap();
        assert!(stamp(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_fifo_sources() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let path = dir.path().join("source.mp4");
        let link = dir.path().join("link.mp4");
        fs::write(&path, b"fixture").unwrap();
        symlink(&path, &link).unwrap();
        assert!(stamp(&link).is_err());
        let fifo = dir.path().join("fifo.mp4");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c_path is a valid NUL-terminated path and mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(stamp(&fifo).is_err());
    }
}
