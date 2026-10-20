// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Opening a plugin's own files to read them: never through a link, never
//! blocking. A repository commits its `.pixel/plugins/` and a clone brings
//! whatever a source holds, so a name there can be a symbolic link to
//! `/dev/zero` or `/dev/tty`, or (copied from a local directory) a FIFO:
//! an open that followed or waited on it would hang or exhaust the process
//! that reads it, the prompt-submit brief included.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::ops::BitOr as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// `path` opened read-only with `O_NOFOLLOW | O_NONBLOCK`, kept only when it
/// is a regular file and, with `seen`, the very file `seen` describes (same
/// device and inode): a name swapped after the directory was read is
/// refused, not read.
///
/// # Errors
///
/// `NotFound` when nothing is at `path`, `InvalidInput` for a link, a FIFO,
/// a device, a directory or a swapped file, or any other error of the open.
pub(crate) fn open_regular(path: &Path, seen: Option<&Metadata>) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW.bitor(libc::O_NONBLOCK))
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                refused(path, "is a symbolic link")
            } else {
                e
            }
        })?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(refused(path, "is not a regular file"));
    }
    if seen.is_some_and(|seen| (seen.dev(), seen.ino()) != (meta.dev(), meta.ino())) {
        return Err(refused(path, "was replaced while it was read"));
    }
    Ok(file)
}

fn refused(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{} {why}; a plugin's files are read without following links",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_regular_file_opens_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "body").unwrap();
        let seen = std::fs::symlink_metadata(&path).unwrap();
        let mut text = String::new();
        open_regular(&path, Some(&seen))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "body");
        let err = open_regular(&dir.path().join("absent"), None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    // A committed link to a device would hand the reader endless or
    // blocking input; one to a file elsewhere would put that file's
    // content where the plugin's is expected.
    #[test]
    fn a_link_is_refused_whatever_it_points_at() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "secret").unwrap();
        for target in [outside.as_path(), Path::new("/dev/zero")] {
            let link = dir.path().join("link");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(target, &link).unwrap();
            let err = open_regular(&link, None).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{target:?}");
            assert!(err.to_string().contains("is a symbolic link"), "{err}");
        }
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(open_regular(&fifo, None).map(|_| ()));
        });
        let err = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the open waited for a writer")
            .unwrap_err();
        assert!(err.to_string().contains("is not a regular file"), "{err}");
    }

    #[test]
    fn a_file_swapped_after_it_was_listed_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "first").unwrap();
        let seen = std::fs::symlink_metadata(&path).unwrap();
        std::fs::write(dir.path().join("g"), "second").unwrap();
        std::fs::rename(dir.path().join("g"), &path).unwrap();
        let err = open_regular(&path, Some(&seen)).unwrap_err();
        assert!(err.to_string().contains("was replaced"), "{err}");
        assert!(open_regular(&path, None).is_ok());
    }
}
