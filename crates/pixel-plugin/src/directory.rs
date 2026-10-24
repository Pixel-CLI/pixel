// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Plugin source traversal relative to pinned directory descriptors.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags, Stat};

pub(crate) struct Directory(File);

pub(crate) struct Entry {
    pub name: OsString,
    pub stat: Stat,
}

impl Entry {
    pub fn kind(&self) -> FileType {
        FileType::from_raw_mode(self.stat.st_mode)
    }

    pub fn len(&self) -> u64 {
        self.stat.st_size as u64
    }
}

impl Directory {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self(File::from(fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)))
    }

    pub fn entries(&self) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for entry in Dir::read_from(&self.0)? {
            let entry = entry?;
            if matches!(entry.file_name().to_bytes(), b"." | b"..") {
                continue;
            }
            // A mount point's readdir inode can differ from its mounted root.
            // Snapshot with nofollow statat; open_entry compares it to fstat.
            let stat = fs::statat(&self.0, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)?;
            entries.push(Entry {
                name: OsString::from_vec(entry.file_name().to_bytes().to_vec()),
                stat,
            });
        }
        Ok(entries)
    }

    pub fn descend(&self, entry: &Entry) -> io::Result<Self> {
        Ok(Self(self.open_entry(entry, FileType::Directory)?))
    }

    pub fn open_file(&self, entry: &Entry) -> io::Result<File> {
        self.open_entry(entry, FileType::RegularFile)
    }

    fn open_entry(&self, entry: &Entry, kind: FileType) -> io::Result<File> {
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if kind == FileType::Directory {
            flags |= OFlags::DIRECTORY;
        }
        let fd = fs::openat(&self.0, entry.name.as_os_str(), flags, Mode::empty())?;
        let actual = fs::fstat(&fd)?;
        if (actual.st_dev, actual.st_ino) != (entry.stat.st_dev, entry.stat.st_ino)
            || FileType::from_raw_mode(actual.st_mode) != kind
        {
            return Err(replaced());
        }
        Ok(File::from(fd))
    }

    pub fn read_link(&self, entry: &Entry) -> io::Result<PathBuf> {
        let target = fs::readlinkat(&self.0, entry.name.as_os_str(), Vec::new())?;
        Ok(PathBuf::from(OsString::from_vec(target.into_bytes())))
    }
}

fn replaced() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "plugin entry was replaced while it was read",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;

    #[test]
    fn directory_should_keep_file_and_link_reads_pinned_after_its_path_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file"), b"original").unwrap();
        symlink("original-target", root.join("link")).unwrap();
        let directory = Directory::open(&root).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("file"), b"outside secret").unwrap();
        symlink("foreign-target", outside.path().join("link")).unwrap();
        std::fs::rename(&root, temp.path().join("held")).unwrap();
        symlink(outside.path(), &root).unwrap();
        assert!(Directory::open(&root).is_err());
        for _ in 0..2 {
            let mut entries = directory.entries().unwrap();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            assert_eq!(
                entries
                    .iter()
                    .map(|e| e.name.as_os_str())
                    .collect::<Vec<_>>(),
                ["file", "link"]
            );
            let mut body = String::new();
            directory
                .open_file(&entries[0])
                .unwrap()
                .read_to_string(&mut body)
                .unwrap();
            assert_eq!(body, "original");
            assert_eq!(
                directory.read_link(&entries[1]).unwrap(),
                Path::new("original-target")
            );
        }
    }

    #[test]
    fn file_should_refuse_replacement_by_a_file_link_or_fifo_without_blocking() {
        for kind in ["file", "link", "fifo"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("file");
            std::fs::write(&path, "original").unwrap();
            let directory = Directory::open(temp.path()).unwrap();
            let entry = directory.entries().unwrap().pop().unwrap();
            std::fs::rename(&path, temp.path().join("held")).unwrap();
            match kind {
                "file" => std::fs::write(&path, "replacement").unwrap(),
                "link" => symlink(temp.path().join("held"), &path).unwrap(),
                _ => assert!(
                    std::process::Command::new("mkfifo")
                        .arg(&path)
                        .status()
                        .unwrap()
                        .success()
                ),
            }
            let (send, receive) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                send.send(directory.open_file(&entry).is_err()).unwrap();
            });
            assert!(
                receive
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("opening a replaced FIFO must not wait for a writer"),
                "{kind}"
            );
            worker.join().unwrap();
        }
    }
}
