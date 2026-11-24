// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A provider's session follows its latest prompt across registered worktrees.
//! Store a checkout hash in private user state. No path read from that state
//! is ever used as a write destination.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pixel_git::{GitRunner, nofollow, sidecar};

use super::decision_log::{project_id, sha256_hex};

const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn binding(root: &Path, provider: &str, session: &str) -> Option<PathBuf> {
    let state = pixel_ops::durable::state_root();
    if !state.is_absolute() {
        return None;
    }
    let repo = pixel_ops::repo_identity(root);
    let key = sha256_hex(&format!("{repo}\0{provider}\0{session}"));
    Some(state.join("brief-turns").join(key))
}

pub(crate) fn remember(root: &Path, provider: &str, session: &str) {
    let Some(path) = binding(root, provider, session) else {
        return;
    };
    let Some(dir) = path.parent() else { return };
    if sidecar::private_dir(dir).is_ok() {
        crate::brief_parts::prune(dir, SystemTime::now(), MAX_AGE);
        let _ = nofollow::write_replace(&path, project_id(root).as_bytes(), nofollow::PRIVATE_MODE);
    }
}

/// Only a change of checkout needs the worktree list. Require Git to list
/// both ends and check the destination sidecar before appending to it.
pub(crate) fn sibling(root: &Path, provider: &str, session: &str) -> Option<PathBuf> {
    let mut owner = String::new();
    nofollow::open_read(&binding(root, provider, session)?)
        .ok()?
        .take(17)
        .read_to_string(&mut owner)
        .ok()?;
    let here = project_id(root);
    if owner == here || owner.len() != here.len() {
        return None;
    }
    let list = GitRunner::new(root)
        .run_isolated(&["worktree", "list", "--porcelain", "-z"])
        .ok()?;
    let mut current_registered = false;
    let mut destination = None;
    for field in list.split(|byte| *byte == 0) {
        if let Some(bytes) = field.strip_prefix(b"worktree ") {
            let path = Path::new(OsStr::from_bytes(bytes));
            let id = project_id(path);
            current_registered |= id == here;
            if id == owner {
                destination = Some(path.to_owned());
            }
        }
    }
    let destination = destination.filter(|_| current_registered)?;
    sidecar::check(&destination).ok()?;
    Some(destination)
}
