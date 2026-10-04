//! Delta-layer state sidecar (`.pixel/state.json`).
//!
//! Records which commit the base shard is pinned to, which commit the delta
//! shard (if any) covers, and the paths tombstoned out of the base (modified
//! or deleted between base and HEAD).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::index::read_regular_bounded;

pub const STATE_FILE: &str = "state.json";
pub const DELTA_FILE: &str = "delta.shard";
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOMBSTONES: usize = 100_000;
const MAX_STATE_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeltaState {
    /// OID the base shard was built at.
    pub base_oid: String,
    /// OID the delta shard covers (base_oid..delta_oid). None = no delta.
    pub delta_oid: Option<String>,
    /// Paths superseded in base/delta by newer history (matched by path).
    pub tombstones: Vec<String>,
}

pub fn state_path(gitpixel_dir: &Path) -> PathBuf {
    gitpixel_dir.join(STATE_FILE)
}

pub fn delta_shard_path(gitpixel_dir: &Path) -> PathBuf {
    gitpixel_dir.join(DELTA_FILE)
}

impl DeltaState {
    pub fn load(gitpixel_dir: &Path) -> Option<Self> {
        let bytes = read_regular_bounded(&state_path(gitpixel_dir), MAX_STATE_BYTES).ok()?;
        let state: Self = serde_json::from_slice(&bytes).ok()?;
        if state.base_oid.len() > 64
            || state.delta_oid.as_ref().is_some_and(|oid| oid.len() > 64)
            || state.tombstones.len() > MAX_TOMBSTONES
            || state
                .tombstones
                .iter()
                .any(|path| path.len() > MAX_STATE_PATH_BYTES)
        {
            return None;
        }
        Some(state)
    }

    pub fn save(&self, gitpixel_dir: &Path) -> std::io::Result<()> {
        pixel_git::sidecar::private_dir(gitpixel_dir)?;
        let tmp = state_path(gitpixel_dir).with_extension("json.tmp");
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self).expect("state serializes"))?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, state_path(gitpixel_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn save_replaces_temp_symlink_without_touching_target() {
        let root = std::env::temp_dir().join(format!("gpx-state-link-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let external = root.join("external.txt");
        std::fs::write(&external, b"unchanged").unwrap();
        symlink(&external, root.join("state.json.tmp")).unwrap();

        let state = DeltaState {
            base_oid: "abc".into(),
            delta_oid: None,
            tombstones: vec!["src/a.rs".into()],
        };
        state.save(&root).unwrap();

        assert_eq!(std::fs::read(&external).unwrap(), b"unchanged");
        assert_eq!(DeltaState::load(&root).unwrap().base_oid, "abc");
        std::fs::remove_dir_all(&root).ok();
    }
}
