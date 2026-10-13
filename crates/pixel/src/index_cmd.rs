// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel index pack` / `pixel index unpack` — portable index bundles.
//!
//! The team's sharing story: CI (or one developer) runs `pixel build-index`
//! once, `pack` freezes `.pixel`'s index files into a single `.pxpack` tar
//! with a per-file xxh3 manifest, and everyone else `unpack`s it — from a
//! path or an `https://` URL — instead of rebuilding. `history.db` stays
//! opt-in behind `--include-history`: it is the lazy index this PR made
//! demand-driven, so it is not packed by default.
//!
//! `unpack` refuses while a daemon is running for the repo (it holds open
//! handles on the files being replaced) unless `--force`, verifies every
//! file's hash before it lands, and warns — does not refuse — when the
//! pack's `repo_head` differs from the checkout's: a stale index is a
//! freshness question the incremental path already answers.

use std::io::Read;
use std::path::{Path, PathBuf};

use pixel_daemon::api::GRAPH_DB_FILE;
use pixel_facts::store::HISTORY_DB_FILE;
use pixel_index::index::SHARD_DIR;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MANIFEST: &str = "manifest.json";
const PACK_FORMAT: u32 = 1;
/// URL fetch ceiling: a pack is index bytes, not a page — generous but
/// bounded so a wrong URL can't stream forever.
const FETCH_CAP: u64 = 2 * 1024 * 1024 * 1024;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Index-bearing files packed by default, besides the graph db itself
/// (`GRAPH_DB_FILE`, whose name moves with its schema). Journals, locks,
/// targets, and plan/workspace state are local and never travel.
const INDEX_FILES: &[&str] = &["base.shard", "calls.json", "state.json"];

#[derive(clap::Subcommand)]
pub enum IndexCmd {
    /// Freeze this repo's index into a single `.pxpack` bundle.
    Pack {
        /// Output file (e.g. index.pxpack).
        #[arg(long)]
        out: PathBuf,
        /// Also pack history.db (the on-demand facts index).
        #[arg(long)]
        include_history: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Install a packed index into this repo's `.pixel/`.
    Unpack {
        /// Pack file path or https:// URL.
        source: String,
        /// Replace the index while a daemon is running.
        #[arg(long)]
        force: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    pixel_version: String,
    created_at_ms: i64,
    repo_head: Option<String>,
    files: Vec<ManifestFile>,
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    path: String,
    size: u64,
    xxh3: String,
}

fn xxh3_of(bytes: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(bytes))
}

/// The files `pack` captures: the current graph db, INDEX_FILES, and the
/// `-wal`/`-shm` SQLite sidecars when present, so a WAL-mode graph travels
/// complete. A graph left under an older schema's name stays behind.
fn pack_list(shard_dir: &Path, include_history: bool) -> Vec<PathBuf> {
    let mut names: Vec<String> = vec![GRAPH_DB_FILE.to_string()];
    names.extend(INDEX_FILES.iter().map(ToString::to_string));
    if include_history {
        names.push(HISTORY_DB_FILE.to_string());
    }
    // SQLite sidecars must travel with their db or the pack loses pages.
    let mut dbs = vec![GRAPH_DB_FILE];
    if include_history {
        dbs.push(HISTORY_DB_FILE);
    }
    for db in dbs {
        names.push(format!("{db}-wal"));
        names.push(format!("{db}-shm"));
    }
    names
        .into_iter()
        .map(|n| shard_dir.join(&n))
        .filter(|p| p.is_file())
        .collect()
}

fn pack(root: &Path, out: &Path, include_history: bool) -> Result<Value, String> {
    let shard_dir = root.join(SHARD_DIR);
    let files = pack_list(&shard_dir, include_history);
    if files.is_empty() {
        return Err(format!(
            "index pack: nothing to pack under {} — run `pixel build-index` first",
            shard_dir.display()
        ));
    }
    let mut entries = Vec::new();
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("index pack: bad name {}", path.display()))?;
        let bytes =
            std::fs::read(path).map_err(|e| format!("index pack: {}: {e}", path.display()))?;
        entries.push(ManifestFile {
            path: name.to_string(),
            size: bytes.len() as u64,
            xxh3: xxh3_of(&bytes),
        });
        blobs.push((name.to_string(), bytes));
    }
    let manifest = Manifest {
        format: PACK_FORMAT,
        pixel_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64),
        repo_head: pixel_git::GitRunner::new(root).rev_parse_head(),
        files: entries,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;

    let file = std::fs::File::create(out).map_err(|e| format!("index pack: {e}"))?;
    let mut builder = tar::Builder::new(file);
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, MANIFEST, manifest_bytes.as_slice())
        .map_err(|e| format!("index pack: {e}"))?;
    for (name, bytes) in &blobs {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("files/{name}"), bytes.as_slice())
            .map_err(|e| format!("index pack: {e}"))?;
    }
    builder.finish().map_err(|e| format!("index pack: {e}"))?;
    Ok(json!({
        "out": out.display().to_string(),
        "files": manifest.files.len(),
        "bytes": std::fs::metadata(out).map_or(0, |m| m.len()),
        "repo_head": manifest.repo_head,
    }))
}

/// Whether `name` is a file `pack` can write ([`pack_list`]): the index
/// files, and the graph and history databases with their SQLite sidecars;
/// nothing else.
fn packable(name: &str) -> bool {
    INDEX_FILES.contains(&name)
        || [GRAPH_DB_FILE, HISTORY_DB_FILE].iter().any(|db| {
            name.strip_prefix(db)
                .is_some_and(|rest| matches!(rest, "" | "-wal" | "-shm"))
        })
}

/// A pack source is a URL only when it opens with http(s):// — anything
/// else is a filesystem path.
fn is_url(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}

/// Read the pack bytes from a path or an https URL.
fn fetch_source(source: &str) -> Result<Vec<u8>, String> {
    if is_url(source) {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .user_agent("pixel-cli index-unpack")
            .build();
        let agent = ureq::Agent::new_with_config(agent);
        let mut response = agent
            .get(source)
            .call()
            .map_err(|e| format!("index unpack: fetch {source}: {e}"))?;
        return response
            .body_mut()
            .with_config()
            .limit(FETCH_CAP)
            .read_to_vec()
            .map_err(|e| format!("index unpack: read {source}: {e}"));
    }
    std::fs::read(source).map_err(|e| format!("index unpack: {source}: {e}"))
}

/// Verified extraction: every file lands under `files/` in the tar, is
/// hash-checked against the manifest, staged in `.unpack-tmp`, then
/// renamed into place.
fn unpack(root: &Path, source: &str, force: bool) -> Result<Value, String> {
    if crate::daemon_ping(root) && !force {
        return Err(
            "index unpack: a daemon is running for this repo — stop it or pass --force".to_string(),
        );
    }
    let bytes = fetch_source(source)?;
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut manifest: Option<Manifest> = None;
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in archive
        .entries()
        .map_err(|e| format!("index unpack: tar: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("index unpack: tar: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| format!("index unpack: tar: {e}"))?
            .to_string_lossy()
            .to_string();
        let mut body = Vec::new();
        entry
            .read_to_end(&mut body)
            .map_err(|e| format!("index unpack: {name}: {e}"))?;
        if name == MANIFEST {
            manifest = Some(
                serde_json::from_slice(&body)
                    .map_err(|e| format!("index unpack: manifest: {e}"))?,
            );
        } else if let Some(file) = name.strip_prefix("files/") {
            // Only the files `pack` writes may land in `.pixel/`: a pack is
            // fetched from a path or a URL, and a member named `targets.json`
            // or `..` would be a write the index never asked for.
            if !packable(file) {
                return Err(format!("index unpack: unsafe member {name}"));
            }
            blobs.push((file.to_string(), body));
        }
    }
    let manifest = manifest.ok_or_else(|| "index unpack: no manifest.json".to_string())?;
    if manifest.format != PACK_FORMAT {
        return Err(format!(
            "index unpack: pack format {} — this pixel understands {PACK_FORMAT}",
            manifest.format
        ));
    }
    // Every manifest entry must be present and hash-true before anything
    // touches .pixel.
    let mut staged = Vec::new();
    for want in &manifest.files {
        let Some((_, body)) = blobs.iter().find(|(n, _)| n == &want.path) else {
            return Err(format!("index unpack: missing member {}", want.path));
        };
        if body.len() as u64 != want.size || xxh3_of(body) != want.xxh3 {
            return Err(format!("index unpack: corrupt member {}", want.path));
        }
        staged.push((want.path.clone(), body.clone()));
    }

    let shard_dir = root.join(SHARD_DIR);
    // The same trust check as every store open: a `.pixel/` the repository
    // commits is not one to install an index into.
    pixel_git::sidecar::check(root).map_err(|e| format!("index unpack: {e}"))?;
    let tmp_dir = shard_dir.join(format!(".unpack-{}", std::process::id()));
    pixel_git::sidecar::private_dir(&tmp_dir).map_err(|e| format!("index unpack: {e}"))?;
    for (name, body) in &staged {
        let stage = tmp_dir.join(name);
        let _ = std::fs::remove_file(&stage);
        pixel_git::nofollow::create_new(&stage, pixel_git::nofollow::PRIVATE_MODE)
            .and_then(|mut file| std::io::Write::write_all(&mut file, body))
            .map_err(|e| format!("index unpack: stage {name}: {e}"))?;
    }
    let mut landed = Vec::new();
    for (name, _) in &staged {
        let from = tmp_dir.join(name);
        let to = shard_dir.join(name);
        std::fs::rename(&from, &to).map_err(|e| format!("index unpack: install {name}: {e}"))?;
        landed.push(name.clone());
    }
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let head_now = pixel_git::GitRunner::new(root).rev_parse_head();
    let head_matches = match (&manifest.repo_head, &head_now) {
        (Some(packed), Some(now)) => Some(packed == now),
        _ => None,
    };
    Ok(json!({
        "installed": landed,
        "repo_head_packed": manifest.repo_head,
        "repo_head_now": head_now,
        "head_matches": head_matches,
        "packed_by_pixel": manifest.pixel_version,
    }))
}

pub fn run(cmd: IndexCmd) -> Result<(), String> {
    let out = match cmd {
        IndexCmd::Pack {
            out,
            include_history,
            path,
        } => pack(&path, &out, include_history)?,
        IndexCmd::Unpack {
            source,
            force,
            path,
        } => unpack(&path, &source, force)?,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("px-idx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(SHARD_DIR)).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn pack_then_unpack_round_trips_with_hashes_verified() {
        let src = scratch("src");
        let dst = scratch("dst");
        std::fs::write(src.join(SHARD_DIR).join(GRAPH_DB_FILE), b"graph-bytes").unwrap();
        std::fs::write(src.join(SHARD_DIR).join("state.json"), b"{}").unwrap();
        let pack_file = src.join("index.pxpack");
        pack(&src, &pack_file, false).unwrap();

        let report = unpack(&dst, pack_file.to_str().unwrap(), true).unwrap();
        assert_eq!(report["installed"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            std::fs::read(dst.join(SHARD_DIR).join(GRAPH_DB_FILE)).unwrap(),
            b"graph-bytes"
        );
        // A byte-flipped pack must be refused by the manifest hash.
        let mut bytes = std::fs::read(&pack_file).unwrap();
        // graph.db lands under "files/graph.db" — find its content block
        // and corrupt it.
        let pos = bytes
            .windows(b"graph-bytes".len())
            .position(|w| w == b"graph-bytes")
            .expect("packed content");
        bytes[pos] ^= 0xff;
        let bad = src.join("bad.pxpack");
        std::fs::write(&bad, bytes).unwrap();
        // tar checksum may fail first, or the manifest hash — either way it
        // must not install.
        let dst2 = scratch("dst2");
        let refused = unpack(&dst2, bad.to_str().unwrap(), true);
        assert!(refused.is_err());
        assert!(!dst2.join(SHARD_DIR).join(GRAPH_DB_FILE).exists());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        let _ = std::fs::remove_dir_all(&dst2);
    }

    #[test]
    fn pack_errors_on_an_empty_index() {
        let root = scratch("empty");
        let err = pack(&root, &root.join("x.pxpack"), false).unwrap_err();
        assert!(err.contains("nothing to pack"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The fetch ceiling is a literal contract, not arithmetic — pin the
    /// byte value so `*` mutants can't shrink or inflate it silently.
    #[test]
    fn fetch_cap_is_two_gib() {
        assert_eq!(FETCH_CAP, 2_147_483_648);
    }

    /// history.db's SQLite sidecars travel only with the db they belong to:
    /// `--include-history` absent means no `history.*` member at all.
    #[test]
    fn pack_list_gates_history_sidecars_on_the_flag() {
        let root = scratch("list");
        let shard = root.join(SHARD_DIR);
        let graph_wal = format!("{GRAPH_DB_FILE}-wal");
        for name in [
            GRAPH_DB_FILE,
            &graph_wal,
            "history.db",
            "history.db-wal",
            "graph.db",
        ] {
            std::fs::write(shard.join(name), b"x").unwrap();
        }
        let plain: Vec<String> = pack_list(&shard, false)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(plain.contains(&GRAPH_DB_FILE.to_string()), "{plain:?}");
        assert!(plain.contains(&graph_wal), "{plain:?}");
        assert!(
            !plain.contains(&"graph.db".to_string()),
            "a graph under the old schema's name is not the index: {plain:?}"
        );
        assert!(!plain.iter().any(|n| n.starts_with("history")), "{plain:?}");
        let full: Vec<String> = pack_list(&shard, true)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(full.contains(&"history.db-wal".to_string()), "{full:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn urls_take_the_fetch_path_not_the_filesystem() {
        assert!(is_url("https://x.example/p.pxpack"));
        assert!(is_url("http://x.example/p.pxpack"));
        assert!(!is_url("pack.pxpack"));
        assert!(!is_url("/tmp/pack.pxpack"));
    }

    /// A tar member like `files/sub/evil` must be refused — a `||`→`&&`
    /// mutant on the unsafe-member check would let nested paths land.
    #[test]
    fn unpack_refuses_a_nested_member_name() {
        let dst = scratch("evil");
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let manifest = serde_json::to_vec(&Manifest {
                format: PACK_FORMAT,
                pixel_version: "test".to_string(),
                created_at_ms: 0,
                repo_head: None,
                files: vec![],
            })
            .unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, MANIFEST, manifest.as_slice())
                .unwrap();
            let body = b"evil";
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "files/sub/evil", body.as_slice())
                .unwrap();
            builder.finish().unwrap();
        }
        let pack_file = dst.join("evil.pxpack");
        std::fs::write(&pack_file, &tar_bytes).unwrap();
        let err = unpack(&dst, pack_file.to_str().unwrap(), true).unwrap_err();
        assert!(err.contains("unsafe member"), "{err}");
        let _ = std::fs::remove_dir_all(&dst);
    }

    /// Only the files `pack` writes may land: a fetched pack naming
    /// `targets.json` or `config.yaml` is refused before anything is staged.
    #[test]
    fn unpack_should_refuse_a_member_pack_never_writes() {
        for member in [
            "targets.json",
            "config.yaml",
            "state.json.bak",
            "history.db-journal",
        ] {
            let dst = scratch("foreign");
            let mut tar_bytes = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut tar_bytes);
                let body = b"planted";
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("files/{member}"), body.as_slice())
                    .unwrap();
                builder.finish().unwrap();
            }
            let pack_file = dst.join("foreign.pxpack");
            std::fs::write(&pack_file, &tar_bytes).unwrap();
            let err = unpack(&dst, pack_file.to_str().unwrap(), true).unwrap_err();
            assert!(err.contains("unsafe member"), "{member}: {err}");
            let staged: Vec<_> = std::fs::read_dir(dst.join(SHARD_DIR)).unwrap().collect();
            assert!(staged.is_empty(), "{member}: {staged:?}");
            let _ = std::fs::remove_dir_all(&dst);
        }
    }

    #[test]
    fn packable_should_accept_every_name_pack_list_can_produce() {
        let root = scratch("packable");
        let shard = root.join(SHARD_DIR);
        for db in [GRAPH_DB_FILE, HISTORY_DB_FILE] {
            for suffix in ["", "-wal", "-shm"] {
                std::fs::write(shard.join(format!("{db}{suffix}")), b"x").unwrap();
            }
        }
        for name in INDEX_FILES {
            std::fs::write(shard.join(name), b"x").unwrap();
        }
        let listed = pack_list(&shard, true);
        assert_eq!(listed.len(), 9);
        for path in listed {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            assert!(packable(&name), "{name}");
        }
        assert!(
            !packable("graph.db"),
            "an older schema's graph never travels"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `head_matches` compares the packed head with the checkout's — both
    /// Some and equal must yield `Some(true)`, not `Some(false)`.
    #[test]
    fn unpack_reports_a_matching_repo_head() {
        let root = scratch("git");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", &root)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        std::fs::write(root.join(SHARD_DIR).join(GRAPH_DB_FILE), b"g").unwrap();
        let pack_file = root.join("p.pxpack");
        pack(&root, &pack_file, false).unwrap();
        let report = unpack(&root, pack_file.to_str().unwrap(), true).unwrap();
        assert_eq!(report["head_matches"], serde_json::json!(true), "{report}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Without a daemon the guard must let an unpack proceed — a `&&`→`||`
    /// flip would refuse on `!force` alone.
    #[test]
    fn unpack_without_force_proceeds_when_no_daemon_runs() {
        let dst = scratch("nodae");
        let src = scratch("nodae-src");
        std::fs::write(src.join(SHARD_DIR).join(GRAPH_DB_FILE), b"g").unwrap();
        let pack_file = src.join("p.pxpack");
        pack(&src, &pack_file, false).unwrap();
        unpack(&dst, pack_file.to_str().unwrap(), false).unwrap();
        let _ = std::fs::remove_dir_all(&dst);
        let _ = std::fs::remove_dir_all(&src);
    }

    /// A live daemon + no --force must refuse — the `!` on `!force` is the
    /// whole protection.
    #[test]
    fn unpack_refuses_while_a_daemon_answers() {
        use std::io::{BufRead, BufReader, Write};
        let root = scratch("live");
        let listener =
            std::os::unix::net::UnixListener::bind(pixel_daemon::daemon::socket_path(&root))
                .unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_ok() {
                    let reply = pixel_daemon::api::Response::success(
                        "ping",
                        serde_json::json!({
                            "pong": true,
                            "protocol_version": pixel_daemon::api::PROTOCOL_VERSION,
                        }),
                    );
                    let _ = writeln!(stream, "{}", serde_json::to_string(&reply).unwrap());
                }
            }
        });
        let src = scratch("live-src");
        std::fs::write(src.join(SHARD_DIR).join(GRAPH_DB_FILE), b"g").unwrap();
        let pack_file = src.join("p.pxpack");
        pack(&src, &pack_file, false).unwrap();
        let err = unpack(&root, pack_file.to_str().unwrap(), false).unwrap_err();
        assert!(err.contains("daemon is running"), "{err}");
        // --force overrides the same live daemon.
        unpack(&root, pack_file.to_str().unwrap(), true).unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&src);
    }

    /// `run` on an empty index must surface pack's error, not Ok(()).
    #[test]
    fn run_propagates_pack_errors() {
        let root = scratch("run");
        let err = run(IndexCmd::Pack {
            out: root.join("x.pxpack"),
            include_history: false,
            path: root.clone(),
        })
        .unwrap_err();
        assert!(err.contains("nothing to pack"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
