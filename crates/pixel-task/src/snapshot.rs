// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Strict source manifests and private copies used by task verification.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{SourceFile, SourceSnapshot, TaskContract, overlaps, relative_path};
use crate::{Error, Result, digest, now_ms};

/// Refuse unexpectedly large snapshots instead of silently omitting source.
const MAX_SOURCE_FILES: usize = 100_000;
const MAX_FILE_BYTES: u64 = 268_435_456;

#[derive(Clone, Copy)]
struct CaptureLimits {
    files: usize,
    file_bytes: u64,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            files: MAX_SOURCE_FILES,
            file_bytes: MAX_FILE_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stat {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}

impl From<&fs::Metadata> for Stat {
    fn from(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.len(),
            mode: meta.mode(),
            mtime: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_ns: meta.ctime_nsec(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    stat: Stat,
    file: SourceFile,
}

fn excluded(path: &str) -> bool {
    path == ".git"
        || path.starts_with(".git/")
        || path == ".pixel"
        || path.starts_with(".pixel/")
        || path == "target"
        || path.starts_with("target/")
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    pixel_git::GitRunner::new(root)
        .with_max_output_bytes(Some(33_554_432))
        .run_isolated(args)
        .map_err(|error| Error::Unavailable(format!("git source enumeration: {error}")))
}

fn git_refs(root: &Path) -> Result<Vec<u8>> {
    git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
        ],
    )
}

fn git_state(root: &Path) -> Result<(Option<String>, String, String)> {
    let head = parse_head(git(root, &["rev-parse", "--revs-only", "HEAD"])?)?;
    let index = git(root, &["ls-files", "--stage", "-v", "-z"])?;
    Ok((
        head,
        hex::encode(Sha256::digest(index)),
        hex::encode(Sha256::digest(git_refs(root)?)),
    ))
}

fn parse_head(bytes: Vec<u8>) -> Result<Option<String>> {
    let head = String::from_utf8(bytes)
        .map_err(|_| Error::Unavailable("invalid Git HEAD identity".into()))?;
    let head = head.trim();
    if !head.is_empty()
        && (head.len() != 40 && head.len() != 64 || !head.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::Unavailable("invalid Git HEAD identity".into()));
    }
    Ok((!head.is_empty()).then(|| head.to_string()))
}

pub(crate) fn source_identity(
    files: &[SourceFile],
    head: &Option<String>,
    index_id: &str,
    refs_id: &str,
) -> Result<String> {
    digest(&(files, head, index_id, refs_id))
}

fn listed(root: &Path, contract: &TaskContract, limits: CaptureLimits) -> Result<BTreeSet<String>> {
    let bytes = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut paths = BTreeSet::new();
    for raw in bytes.split(|byte| *byte == 0).filter(|raw| !raw.is_empty()) {
        let path = std::str::from_utf8(raw)
            .map_err(|_| Error::Unavailable("non-UTF8 source path".into()))?;
        relative_path(path, false)?;
        if excluded(path) {
            if !git(root, &["ls-files", "-z", "--", path])?.is_empty() {
                return Err(Error::Unavailable(format!(
                    "tracked source is in a reserved runtime directory: {path}"
                )));
            }
            continue;
        }
        if contract.outputs.iter().any(|output| overlaps(path, output)) {
            // A tracked file may never be declared disposable output.
            let tracked = git(root, &["ls-files", "-z", "--", path])?;
            if !tracked.is_empty() {
                return Err(Error::Invalid(format!(
                    "output overlaps tracked source: {path}"
                )));
            }
            continue;
        }
        match fs::symlink_metadata(root.join(path)) {
            Ok(meta) if meta.is_dir() => {
                return Err(Error::Unavailable(format!(
                    "submodule or directory source requires explicit capture support: {path}"
                )));
            }
            Ok(_) => {
                paths.insert(path.to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    for input in &contract.inputs {
        if excluded(input) {
            return Err(Error::Invalid(format!("reserved source input: {input}")));
        }
        add_input(root, root.join(input), &mut paths, limits.files)?;
    }
    if paths.len() > limits.files {
        return Err(Error::Unavailable("source file limit exceeded".into()));
    }
    Ok(paths)
}

fn add_input(root: &Path, path: PathBuf, paths: &mut BTreeSet<String>, limit: usize) -> Result<()> {
    let mut pending = vec![path];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        visited += 1;
        if visited > limit {
            return Err(Error::Unavailable(
                "declared input enumeration limit exceeded".into(),
            ));
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| Error::Invalid("input escaped root".into()))?
                .to_str()
                .ok_or_else(|| Error::Invalid("non-UTF8 input path".into()))?;
            paths.insert(relative.to_string());
        }
    }
    Ok(())
}

fn source_file(
    root: &Path,
    path: &str,
    paths: &BTreeSet<String>,
    byte_limit: u64,
) -> Result<SourceFile> {
    let absolute = root.join(path);
    let before = fs::symlink_metadata(&absolute)?;
    if before.len() > byte_limit {
        return Err(Error::Unavailable(format!(
            "source file exceeds capture limit: {path}"
        )));
    }
    let (bytes, link) = if before.file_type().is_symlink() {
        let target = fs::read_link(&absolute)?;
        if target.is_absolute() {
            return Err(Error::Unavailable(format!(
                "absolute source symlink: {path}"
            )));
        }
        let resolved = absolute.canonicalize()?;
        let relative = resolved
            .strip_prefix(root)
            .map_err(|_| Error::Unavailable(format!("source symlink escapes snapshot: {path}")))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| Error::Unavailable("non-UTF8 symlink target".into()))?;
        if !paths.contains(relative)
            && !paths
                .iter()
                .any(|candidate| Path::new(candidate).starts_with(relative))
        {
            return Err(Error::Unavailable(format!(
                "symlink target is not a captured input: {path}"
            )));
        }
        let text = target
            .to_str()
            .ok_or_else(|| Error::Unavailable("non-UTF8 source symlink".into()))?
            .to_string();
        (text.as_bytes().to_vec(), Some(text))
    } else if before.is_file() {
        (fs::read(&absolute)?, None)
    } else {
        return Err(Error::Unavailable(format!(
            "unsupported source file type: {path}"
        )));
    };
    if Stat::from(&before) != Stat::from(&fs::symlink_metadata(&absolute)?) {
        return Err(Error::Unavailable(format!(
            "source changed during capture: {path}"
        )));
    }
    Ok(SourceFile {
        path: path.to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        mode: before.mode() & 0o777,
        symlink: link,
    })
}

/// Capture current source; strict mode bypasses the metadata digest cache.
pub fn capture(root: &Path, contract: &TaskContract, strict: bool) -> Result<SourceSnapshot> {
    capture_with(root, contract, strict, CaptureLimits::default(), || Ok(()))
}

fn capture_with(
    root: &Path,
    contract: &TaskContract,
    strict: bool,
    limits: CaptureLimits,
    after_read: impl FnOnce() -> Result<()>,
) -> Result<SourceSnapshot> {
    contract.validate()?;
    let root = root.canonicalize()?;
    let (head, index_id, refs_id) = git_state(&root)?;
    let before = listed(&root, contract, limits)?;
    let cache_path = root.join(".pixel/tasks/source-cache.json");
    let cache: BTreeMap<String, Cached> = if strict {
        BTreeMap::new()
    } else {
        fs::read(&cache_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    };
    let mut next = BTreeMap::new();
    let mut files = Vec::with_capacity(before.len());
    for path in &before {
        let stat = Stat::from(&fs::symlink_metadata(root.join(path))?);
        let file = match cache
            .get(path)
            .filter(|entry| entry.stat == stat && entry.file.symlink.is_none())
        {
            Some(entry) => entry.file.clone(),
            None => source_file(&root, path, &before, limits.file_bytes)?,
        };
        next.insert(
            path.clone(),
            Cached {
                stat,
                file: file.clone(),
            },
        );
        files.push(file);
    }
    after_read()?;
    if (head.clone(), index_id.clone(), refs_id.clone()) != git_state(&root)?
        || before != listed(&root, contract, limits)?
        || next.iter().any(|(path, entry)| {
            fs::symlink_metadata(root.join(path))
                .map(|meta| Stat::from(&meta))
                .ok()
                .as_ref()
                != Some(&entry.stat)
        })
    {
        return Err(Error::Unavailable(
            "source changed during snapshot capture".into(),
        ));
    }
    let content_id = source_identity(&files, &head, &index_id, &refs_id)?;
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
        pixel_ops::durable::write_durably(&cache_path, &serde_json::to_vec(&next)?)?;
    }
    Ok(SourceSnapshot {
        content_id,
        root: root.display().to_string(),
        head,
        index_id,
        refs_id,
        captured_ms: now_ms(),
        files,
    })
}

/// A private copy owns every source file; live worktree edits cannot affect it.
pub fn materialize(snapshot: &SourceSnapshot) -> Result<tempfile::TempDir> {
    materialize_with(snapshot, |_| Ok(()), |_| Ok(()))
}

fn materialize_with(
    snapshot: &SourceSnapshot,
    mut before_copy: impl FnMut(&str) -> Result<()>,
    after_copy: impl FnOnce(&Path) -> Result<()>,
) -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("pixel-task-check-")
        .tempdir()?;
    let live_root = Path::new(&snapshot.root);
    let expected_git = (
        snapshot.head.clone(),
        snapshot.index_id.clone(),
        snapshot.refs_id.clone(),
    );
    if git_state(live_root)? != expected_git {
        return Err(Error::Unavailable(
            "Git state changed before capture copy".into(),
        ));
    }
    if !git(live_root, &["ls-files", "--unmerged", "-z"])?.is_empty()
        || !git(live_root, &["rev-parse", "--shared-index-path"])?.is_empty()
    {
        return Err(Error::Unavailable(
            "unmerged or split Git index cannot yet be captured safely".into(),
        ));
    }
    let index_path = String::from_utf8(git(live_root, &["rev-parse", "--git-path", "index"])?)
        .map_err(|_| Error::Unavailable("non-UTF8 Git index path".into()))?;
    let index_path = live_root.join(index_path.trim());
    let index_bytes = optional_index_bytes(fs::read(index_path))?;
    let refs = git_refs(live_root)?;
    // Copy object storage and history, without shared inodes or borrowed object
    // databases. Do not commit the captured worktree: its real diff is evidence.
    git(
        live_root,
        &[
            "clone",
            "--quiet",
            "--mirror",
            "--no-hardlinks",
            "--local",
            "--dissociate",
            "--",
            &snapshot.root,
            directory
                .path()
                .join(".git")
                .to_str()
                .ok_or_else(|| Error::Unavailable("non-UTF8 capture path".into()))?,
        ],
    )?;
    git(directory.path(), &["config", "core.bare", "false"])?;
    git(directory.path(), &["config", "core.hooksPath", "/dev/null"])?;
    git(directory.path(), &["config", "core.fsmonitor", "false"])?;
    // Clone may dereference symbolic refs. Recreate their captured topology.
    for record in refs
        .split(|byte| *byte == b'\n')
        .filter(|record| !record.is_empty())
    {
        let fields: Vec<_> = record.split(|byte| *byte == 0).collect();
        if fields.len() != 3 {
            return Err(Error::Unavailable("invalid captured Git reference".into()));
        }
        if !fields[2].is_empty() {
            let name = std::str::from_utf8(fields[0])
                .map_err(|_| Error::Unavailable("non-UTF8 Git reference".into()))?;
            let target = std::str::from_utf8(fields[2])
                .map_err(|_| Error::Unavailable("non-UTF8 Git symbolic target".into()))?;
            git(directory.path(), &["symbolic-ref", name, target])?;
        }
    }
    if let Some(head) = &snapshot.head {
        git(
            directory.path(),
            &["update-ref", "--no-deref", "HEAD", head],
        )?;
    }
    if let Some(bytes) = index_bytes {
        fs::write(directory.path().join(".git/index"), bytes)?;
    }
    let paths: BTreeSet<_> = snapshot
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    for file in &snapshot.files {
        let live = source_file(
            Path::new(&snapshot.root),
            &file.path,
            &paths,
            MAX_FILE_BYTES,
        )?;
        if &live != file {
            return Err(Error::Unavailable(format!(
                "source changed before copy: {}",
                file.path
            )));
        }
        before_copy(&file.path)?;
        let target = directory.path().join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(link) = &file.symlink {
            symlink(link, &target)?;
        } else {
            let bytes = fs::read(Path::new(&snapshot.root).join(&file.path))?;
            if hex::encode(Sha256::digest(&bytes)) != file.sha256 {
                return Err(Error::Unavailable(format!(
                    "source changed while copying: {}",
                    file.path
                )));
            }
            fs::write(&target, bytes)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(file.mode))?;
        }
    }
    after_copy(directory.path())?;
    if git_state(live_root)? != expected_git || git_state(directory.path())? != expected_git {
        return Err(Error::Unavailable(
            "Git state changed during capture copy".into(),
        ));
    }
    Ok(directory)
}

fn optional_index_bytes(read: std::io::Result<Vec<u8>>) -> Result<Option<Vec<u8>>> {
    match read {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Check captured source paths, and reject unexpected undeclared output files.
pub fn unchanged(root: &Path, snapshot: &SourceSnapshot, contract: &TaskContract) -> Result<bool> {
    let current = capture(root, contract, true)?;
    Ok(current.content_id == snapshot.content_id)
}

/// Metadata of captured files detects write-and-restore during a private check.
pub(crate) fn mutation_marker(root: &Path, snapshot: &SourceSnapshot) -> Result<String> {
    let stats: Vec<_> = snapshot
        .files
        .iter()
        .map(|file| {
            fs::symlink_metadata(root.join(&file.path))
                .map(|meta| (file.path.clone(), Stat::from(&meta)))
        })
        .collect::<std::io::Result<_>>()?;
    digest(&stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{contract, git, repo};

    #[test]
    fn optional_index_read_preserves_bytes_and_only_accepts_not_found() {
        assert_eq!(
            optional_index_bytes(Ok(vec![1, 2, 3])).unwrap(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            optional_index_bytes(Err(std::io::ErrorKind::NotFound.into())).unwrap(),
            None
        );
        assert!(matches!(
            optional_index_bytes(Err(std::io::ErrorKind::PermissionDenied.into())),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn reserved_runtime_paths_are_exact_and_not_similarly_named_source() {
        for path in [
            ".git",
            ".git/index",
            ".pixel",
            ".pixel/tasks/a",
            "target",
            "target/debug/a",
        ] {
            assert!(excluded(path), "{path}");
        }
        for path in [
            ".gitignore",
            ".pixel-source",
            "targets",
            "src/target",
            "src/.git",
        ] {
            assert!(!excluded(path), "{path}");
        }
    }

    #[test]
    fn head_parser_distinguishes_unborn_valid_hashes_and_corrupt_output() {
        assert_eq!(parse_head(Vec::new()).unwrap(), None);
        for size in [40, 64] {
            let value = "a".repeat(size);
            assert_eq!(
                parse_head(format!("{value}\n").into_bytes()).unwrap(),
                Some(value)
            );
        }
        for value in [
            "a".repeat(39),
            "a".repeat(41),
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(40),
            "z".repeat(64),
        ] {
            assert!(parse_head(value.into_bytes()).is_err());
        }
        assert!(parse_head(vec![255]).is_err());
    }

    #[test]
    fn enumeration_and_file_size_limits_allow_the_boundary_and_reject_one_more() {
        let root = repo();
        let limits = CaptureLimits {
            files: 2,
            file_bytes: MAX_FILE_BYTES,
        };
        let exact = capture_with(root.path(), &contract(), true, limits, || Ok(())).unwrap();
        assert_eq!(exact.files.len(), 2);
        fs::write(root.path().join("third"), "x").unwrap();
        assert!(matches!(
            capture_with(root.path(), &contract(), true, limits, || Ok(())),
            Err(Error::Unavailable(_))
        ));
        let paths = BTreeSet::from(["source.txt".into()]);
        let exact = source_file(root.path(), "source.txt", &paths, 5).unwrap();
        assert_eq!(exact.sha256, hex::encode(Sha256::digest(b"value")));
        assert!(matches!(
            source_file(root.path(), "source.txt", &paths, 4),
            Err(Error::Unavailable(_))
        ));
        assert_eq!(CaptureLimits::default().files, 100_000);
        assert_eq!(CaptureLimits::default().file_bytes, 268_435_456);
    }

    #[test]
    fn declared_directory_traversal_is_bounded_and_includes_ignored_inputs() {
        let root = repo();
        fs::create_dir(root.path().join("ignored")).unwrap();
        fs::write(root.path().join("ignored/one"), "one").unwrap();
        let mut paths = BTreeSet::new();
        add_input(root.path(), root.path().join("ignored"), &mut paths, 2).unwrap();
        assert_eq!(paths, BTreeSet::from(["ignored/one".into()]));
        fs::write(root.path().join("ignored/two"), "two").unwrap();
        assert!(matches!(
            add_input(
                root.path(),
                root.path().join("ignored"),
                &mut BTreeSet::new(),
                2
            ),
            Err(Error::Unavailable(_))
        ));
        let mut configured = contract();
        configured.inputs.push("ignored".into());
        let captured = capture(root.path(), &configured, true).unwrap();
        assert!(captured.files.iter().any(|file| file.path == "ignored/one"));
        assert!(captured.files.iter().any(|file| file.path == "ignored/two"));
    }

    #[test]
    fn source_listing_rejects_submodule_directories_before_capturing_files() {
        let root = repo();
        let head = String::from_utf8(git(root.path(), &["rev-parse", "HEAD"])).unwrap();
        git(
            root.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},module", head.trim()),
            ],
        );
        fs::create_dir(root.path().join("module")).unwrap();
        assert!(matches!(
            listed(root.path(), &contract(), CaptureLimits::default()),
            Err(Error::Unavailable(message))
                if message == "submodule or directory source requires explicit capture support: module"
        ));
    }

    #[test]
    fn tracked_reserved_paths_submodules_and_nonmissing_io_errors_fail_closed() {
        for reserved in [".pixel", "target"] {
            let root = repo();
            fs::create_dir(root.path().join(reserved)).unwrap();
            fs::write(root.path().join(reserved).join("source"), "x").unwrap();
            git(root.path(), &["add", "-f", reserved]);
            assert!(matches!(
                capture(root.path(), &contract(), true),
                Err(Error::Unavailable(_))
            ));
        }
        let root = repo();
        let head = String::from_utf8(git(root.path(), &["rev-parse", "HEAD"])).unwrap();
        git(
            root.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},module", head.trim()),
            ],
        );
        fs::create_dir(root.path().join("module")).unwrap();
        assert!(matches!(
            capture(root.path(), &contract(), true),
            Err(Error::Unavailable(_))
        ));
        let root = repo();
        fs::create_dir(root.path().join("folder")).unwrap();
        fs::write(root.path().join("folder/file"), "x").unwrap();
        git(root.path(), &["add", "folder/file"]);
        fs::remove_dir_all(root.path().join("folder")).unwrap();
        fs::write(root.path().join("folder"), "not a directory").unwrap();
        assert!(matches!(
            capture(root.path(), &contract(), true),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn symlinks_capture_only_internal_file_and_directory_targets() {
        let root = repo();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("dir/value"), "inside").unwrap();
        symlink("source.txt", root.path().join("file-link")).unwrap();
        symlink("dir", root.path().join("dir-link")).unwrap();
        fs::set_permissions(
            root.path().join("source.txt"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let captured = capture(root.path(), &contract(), true).unwrap();
        assert_eq!(
            captured
                .files
                .iter()
                .find(|file| file.path == "source.txt")
                .unwrap()
                .mode,
            0o755
        );
        assert_eq!(
            captured
                .files
                .iter()
                .find(|file| file.path == "file-link")
                .unwrap()
                .symlink
                .as_deref(),
            Some("source.txt")
        );
        let copy = materialize(&captured).unwrap();
        assert_eq!(
            fs::read_link(copy.path().join("dir-link")).unwrap(),
            Path::new("dir")
        );
        fs::create_dir(root.path().join("ignored")).unwrap();
        fs::write(root.path().join("ignored/hidden"), "hidden").unwrap();
        symlink("ignored/hidden", root.path().join("hidden-link")).unwrap();
        assert!(matches!(
            capture(root.path(), &contract(), true),
            Err(Error::Unavailable(_))
        ));
        let mut configured = contract();
        configured.inputs.push("ignored/hidden".into());
        assert!(capture(root.path(), &configured, true).is_ok());
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "outside").unwrap();
        let target = format!(
            "../{}/secret",
            outside.path().file_name().unwrap().to_str().unwrap()
        );
        symlink(target, root.path().join("escape")).unwrap();
        assert!(matches!(
            capture(root.path(), &configured, true),
            Err(Error::Unavailable(_))
        ));
    }

    #[test]
    fn capture_rejects_independent_content_path_and_ref_changes_during_collection() {
        for change in ["content", "path", "ref"] {
            let root = repo();
            let result = capture_with(
                root.path(),
                &contract(),
                true,
                CaptureLimits::default(),
                || {
                    match change {
                        "content" => fs::write(root.path().join("source.txt"), "other")?,
                        "path" => fs::write(root.path().join("new"), "new")?,
                        _ => {
                            git(root.path(), &["update-ref", "refs/heads/moved", "HEAD"]);
                        }
                    }
                    Ok(())
                },
            );
            assert!(matches!(result, Err(Error::Unavailable(_))), "{change}");
        }
    }

    #[test]
    fn materialization_rejects_stale_source_copy_races_and_each_git_race() {
        let root = repo();
        let captured = capture(root.path(), &contract(), true).unwrap();
        fs::write(root.path().join("source.txt"), "other").unwrap();
        assert!(matches!(materialize(&captured), Err(Error::Unavailable(_))));
        fs::write(root.path().join("source.txt"), "value").unwrap();
        let result = materialize_with(
            &captured,
            |path| {
                if path == "source.txt" {
                    fs::write(root.path().join(path), "racing")?;
                }
                Ok(())
            },
            |_| Ok(()),
        );
        assert!(matches!(result, Err(Error::Unavailable(_))));
        for change_private in [false, true] {
            let root = repo();
            let captured = capture(root.path(), &contract(), true).unwrap();
            let result = materialize_with(
                &captured,
                |_| Ok(()),
                |private| {
                    git(
                        if change_private { private } else { root.path() },
                        &["update-ref", "refs/heads/race", "HEAD"],
                    );
                    Ok(())
                },
            );
            assert!(matches!(result, Err(Error::Unavailable(_))));
        }
    }

    #[test]
    fn unborn_repositories_and_missing_index_are_captured_without_fabricated_head() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join("file"), "untracked").unwrap();
        let captured = capture(root.path(), &contract(), true).unwrap();
        assert_eq!(captured.head, None);
        assert_eq!(captured.files.len(), 1);
        let copied = materialize(&captured).unwrap();
        assert_eq!(
            fs::read_to_string(copied.path().join("file")).unwrap(),
            "untracked"
        );
    }

    #[test]
    fn unmerged_and_split_indexes_are_rejected_independently_before_copy() {
        let root = repo();
        git(root.path(), &["update-index", "--split-index"]);
        let captured = capture(root.path(), &contract(), true).unwrap();
        assert!(
            matches!(materialize(&captured), Err(Error::Unavailable(message)) if message.contains("unmerged or split"))
        );

        let root = repo();
        git(root.path(), &["checkout", "-qb", "other"]);
        fs::write(root.path().join("source.txt"), "other").unwrap();
        git(root.path(), &["commit", "-qam", "other"]);
        git(root.path(), &["checkout", "-q", "--detach", "HEAD~1"]);
        fs::write(root.path().join("source.txt"), "ours").unwrap();
        git(root.path(), &["commit", "-qam", "ours"]);
        assert!(
            pixel_git::GitRunner::new(root.path())
                .run_isolated(&[
                    "-c",
                    "user.name=Task tests",
                    "-c",
                    "user.email=task@example.com",
                    "merge",
                    "other",
                ])
                .is_err()
        );
        assert!(!git(root.path(), &["ls-files", "--unmerged"]).is_empty());
        let captured = capture(root.path(), &contract(), true).unwrap();
        assert!(
            matches!(materialize(&captured), Err(Error::Unavailable(message)) if message.contains("unmerged or split"))
        );
    }
}
