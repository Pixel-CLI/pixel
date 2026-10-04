//! `pixel space` — audit how much disk the pixel index (`.pixel/`) takes
//! across every project under a tree, and delete shards to reclaim space.
//!
//! An index is a per-project sidecar that is fully rebuildable from source
//! (`pixel build-index .`), so deleting a shard only costs the re-index time,
//! never source. Across many projects the shards can grow to gigabytes; this
//! command surfaces the accumulated total and offers a one-shot cleanup.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use pixel_daemon::api::GRAPH_DB_FILE;
use pixel_index::delta::DELTA_FILE;
use pixel_index::index::{SHARD_DIR, SHARD_FILE};
use serde_json::json;

/// Directory names we never descend into while hunting shards. None of them
/// holds a project's index (pixel never writes into them), and skipping them
/// keeps the scan cheap where they are enormous — a `node_modules` or every
/// crate's `target/` inside one workspace can dwarf the whole corpus.
const PRUNE: &[&str] = &[".git", "target", "node_modules", ".cache", ".pnpm-store"];

/// On-disk artifacts that mark a `.pixel` directory as a real index rather
/// than pixel's own housekeeping (only `actions.jsonl`, `history.db`,
/// `config.json`, `build.lock`). The overlay layer is in-memory, so it has
/// no file here. A journal-only `.pixel` is not "indexing" and is skipped.
const INDEX_MARKERS: &[&str] = &[SHARD_FILE, DELTA_FILE, GRAPH_DB_FILE];

/// Does this `.pixel` directory hold at least one real index artifact?
fn has_index_marker(shard: &Path) -> bool {
    INDEX_MARKERS
        .iter()
        .any(|marker| shard.join(marker).is_file())
}

/// Human-readable size of a byte count (1024-based, du style).
///
/// Written as literal thresholds over a `for` over the unit table so the
/// mutation gate has one equality edge to probe and no hand-advanced index.
fn human(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_073_741_824, "GiB"), (1_048_576, "MiB"), (1_024, "KiB")];
    for &(threshold, unit) in &UNITS {
        if bytes >= threshold {
            return format!("{:.1} {unit}", bytes as f64 / threshold as f64);
        }
    }
    format!("{bytes} B")
}

/// Every `.pixel` index shard under `base`, a shard being a directory named
/// exactly [`SHARD_DIR`]. A shard is recorded but not descended into; the
/// pruned directories are skipped entirely. Symlinked directories are not
/// followed, so a cycle cannot make the scan loop.
fn find_shards(base: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(ty) = entry.file_type() else { continue };
            if !ty.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let path = entry.path();
            if name == SHARD_DIR {
                if has_index_marker(&path) {
                    out.push(path);
                }
                continue;
            }
            if !PRUNE.contains(&name.to_string_lossy().as_ref()) {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Total bytes a directory tree occupies, summing regular-file sizes. Not
/// allocation size (blocks); the coarse number is what a user reclaims and
/// it is cheap to compute.
fn dir_size(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(ty) = entry.file_type() else { continue };
            let path = entry.path();
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() {
                total += entry.metadata().map_or(0, |m| m.len());
            }
        }
    }
    total
}

/// Is a typed confirmation a "yes"? Pure so the interactive prompt's
/// parsing is testable without driving stdin.
fn confirm_yes(answer: &str) -> bool {
    let a = answer.trim().to_ascii_lowercase();
    a == "y" || a == "yes"
}

/// Audit the index disk usage under `path`. With `--delete`, remove every
/// found shard after a single confirmation (skipped by `--yes`).
pub fn run(path: PathBuf, json: bool, delete: bool, yes: bool) -> Result<(), String> {
    let base = path
        .canonicalize()
        .map_err(|e| format!("space: {}: {e}", path.display()))?;
    let shards = find_shards(&base);
    let mut rows: Vec<(PathBuf, u64)> = shards
        .into_iter()
        .map(|shard| {
            let bytes = dir_size(&shard);
            let project = shard.parent().map(Path::to_path_buf).unwrap_or_default();
            (project, bytes)
        })
        .collect();
    rows.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes)); // biggest first
    let total: u64 = rows.iter().map(|(_, bytes)| bytes).sum();

    if json {
        let entries: Vec<_> = rows
            .iter()
            .map(|(project, bytes)| {
                json!({
                    "project": project.display().to_string(),
                    "shard": project.join(SHARD_DIR).display().to_string(),
                    "bytes": bytes,
                    "human": human(*bytes),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "root": base.display().to_string(),
                "total_bytes": total,
                "entries": entries,
            }))
            .map_err(|e| e.to_string())?
        );
    } else if rows.is_empty() {
        println!("space: no `.pixel` index shards under {}", base.display());
        return Ok(());
    } else {
        for (project, bytes) in &rows {
            println!("{:>10}  {}", human(*bytes), project.display());
        }
        println!(
            "{:>10}  total across {} projects ({total} bytes)",
            human(total),
            rows.len(),
        );
    }

    if delete {
        let confirmed = yes || interactive_confirm(rows.len(), total)?;
        if !confirmed {
            return Err("space: aborted — nothing deleted".to_string());
        }
        for (project, _) in &rows {
            let shard = project.join(SHARD_DIR);
            match std::fs::remove_dir_all(&shard) {
                Ok(()) => println!("removed {}", shard.display()),
                Err(e) => eprintln!("space: failed to remove {}: {e}", shard.display()),
            }
        }
    }
    Ok(())
}

/// One whole-tree confirmation read from stdin. Returns `Ok(true)` to
/// proceed, `Ok(false)` to abort. Non-terminal input that is not a yes
/// counts as a no.
fn interactive_confirm(count: usize, total: u64) -> Result<bool, String> {
    eprint!(
        "space: delete {count} index shards ({})? [y/N] ",
        human(total)
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("space: reading confirmation: {e}"))?;
    // End the prompt line so an abort message or the next output starts on
    // its own line rather than appended to `[y/N] `.
    eprintln!();
    Ok(confirm_yes(&line))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "px-space-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(base: &Path, rel: &str, bytes: &[u8]) {
        let path = base.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn shard(path: &Path) -> PathBuf {
        path.join(SHARD_DIR)
    }

    #[test]
    fn human_sizes_cross_unit_boundaries() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1_024), "1.0 KiB");
        assert_eq!(human(1_048_576), "1.0 MiB");
        assert_eq!(human(1_073_741_824), "1.0 GiB");
        // One below each threshold still names the smaller unit; this is the
        // edge that kills the `N-1` literal mutant on the unit-table entry.
        assert_eq!(human(1_048_575), "1024.0 KiB");
        assert_eq!(human(1_073_741_823), "1024.0 MiB");
        assert_eq!(human(2_097_152), "2.0 MiB");
        assert_eq!(human(1_536), "1.5 KiB");
    }

    #[test]
    fn find_shards_records_indexes_and_prunes_noise() {
        let base = scratch("find");
        // Real indexed projects: three shards at different depths. Each
        // carries a real index artifact (base.shard / graph.v2.db).
        write(&base, "a/.pixel/base.shard", b"x");
        write(&base, "deep/one/two/.pixel/graph.v2.db", b"y");
        write(&base, "b/.pixel/delta.shard", b"z");
        // Journal-only housekeeping (actions.jsonl, no index artifact):
        // pixel ran here but never built an index, so it is not "indexing".
        write(&base, "journaled/.pixel/actions.jsonl", b"notes");
        // Noise that must never be counted: shards inside pruned dirs.
        write(&base, "huge/node_modules/.pixel/base.shard", b"noise1");
        write(&base, "huge/target/.pixel/base.shard", b"noise2");
        write(&base, "huge/.cache/.pixel/base.shard", b"noise3");
        write(&base, "repo/.git/.pixel/base.shard", b"noise4");
        // A non-index dot-directory is not a shard.
        write(&base, "c/.hidden/keep.txt", b"x");

        let shards = find_shards(&base);
        let got: Vec<String> = shards
            .iter()
            .map(|p| p.strip_prefix(&base).unwrap().display().to_string())
            .collect();
        assert_eq!(got, ["a/.pixel", "b/.pixel", "deep/one/two/.pixel"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn has_index_marker_requires_a_real_index_artifact() {
        let shard = scratch("marker");
        assert!(!has_index_marker(&shard));
        write(&shard, "actions.jsonl", b"housekeeping");
        assert!(!has_index_marker(&shard), "journal-only is not an index");
        write(&shard, SHARD_FILE, b"base");
        assert!(has_index_marker(&shard), "base.shard marks a real index");
        let _ = std::fs::remove_dir_all(&shard);

        let delta = scratch("marker-delta");
        write(&delta, DELTA_FILE, b"d");
        assert!(has_index_marker(&delta), "delta.shard also marks an index");
        let _ = std::fs::remove_dir_all(&delta);

        let graph = scratch("marker-graph");
        write(&graph, GRAPH_DB_FILE, b"g");
        assert!(has_index_marker(&graph), "graph.v2.db also marks an index");
        let _ = std::fs::remove_dir_all(&graph);
    }

    #[test]
    fn find_shards_does_not_follow_a_symlinked_dir() {
        let base = scratch("symlink");
        let target = scratch("symlink-target");
        write(&target, ".pixel/graph.db", b"x");
        let _ = std::os::unix::fs::symlink(&target, base.join("link"));
        assert!(find_shards(&base).is_empty(), "must not follow the symlink");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn dir_size_sums_files_recursively() {
        let base = scratch("size");
        write(&base, ".pixel/index.bin", &[0u8; 100]);
        write(&base, ".pixel/sub/graph.db", &[0u8; 200]);
        write(&base, "src/main.rs", &[0u8; 64]); // outside `.pixel`: not counted
        assert_eq!(dir_size(&shard(&base)), 300);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn confirm_yes_accepts_y_and_yes_case_insensitively() {
        for yes in ["y", "Y", "yes", " yes\n", "YES"] {
            assert!(confirm_yes(yes), "{yes:?}");
        }
        for no in ["n", "no", "", "maybe", "\n"] {
            assert!(!confirm_yes(no), "{no:?}");
        }
    }
}
