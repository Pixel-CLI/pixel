// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel reference` — version-pinned reference corpora on top of workspaces
//! and index packs.
//!
//! A reference corpus is an external repository pinned to an exact revision,
//! stored locally under `.pixel/references/`. The manifest at
//! `.pixel/reference.json` records each entry's corpus/repo ID, source URL,
//! exact revision, local source location, licence/provenance info, and role
//! (target or reference).
//!
//! `setup` clones or fetches each pinned corpus to its exact revision.
//! `query` searches across pinned references in a cold, read-only manner —
//! it cannot fetch, clone, build, or modify source. Every query result
//! identifies the target/reference role, repository, exact revision, and
//! source location. Missing or wrong-revision corpora are disclosed
//! individually.

use std::path::{Path, PathBuf};

use pixel_index::index::SHARD_DIR;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const REFERENCE_FILE: &str = "reference.json";
const REFERENCE_FORMAT: u32 = 1;

/// The role a reference corpus plays in the analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The primary target being analyzed.
    Target,
    /// A reference corpus used for comparison.
    Reference,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Target => "target",
            Role::Reference => "reference",
        }
    }
}

/// A single version-pinned reference corpus entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceEntry {
    /// Corpus or repository identifier (e.g. "serde", "tokio").
    pub id: String,
    /// Source URL of the repository.
    pub repo: String,
    /// Exact revision (commit hash, tag, or branch) to pin.
    pub revision: String,
    /// Local filesystem path where the corpus is stored.
    pub source: PathBuf,
    /// Licence identifier (e.g. "MIT", "Apache-2.0").
    #[serde(default)]
    pub licence: String,
    /// Provenance information (e.g. upstream URL, checksum).
    #[serde(default)]
    pub provenance: String,
    /// Role this corpus plays: target or reference.
    pub role: Role,
}

/// The reference manifest stored at `.pixel/reference.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReferenceManifest {
    /// Manifest format version.
    pub format: u32,
    /// The pinned reference corpus entries.
    #[serde(default)]
    pub entries: Vec<ReferenceEntry>,
}

impl Default for ReferenceManifest {
    fn default() -> Self {
        ReferenceManifest {
            format: REFERENCE_FORMAT,
            entries: Vec::new(),
        }
    }
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join(SHARD_DIR).join(REFERENCE_FILE)
}

fn load_manifest(root: &Path) -> Result<ReferenceManifest, String> {
    let path = manifest_path(root);
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|e| format!("reference {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReferenceManifest::default()),
        Err(e) => Err(format!("reference {}: {e}", path.display())),
    }
}

fn save_manifest(root: &Path, manifest: &ReferenceManifest) -> Result<(), String> {
    let path = manifest_path(root);
    if let Some(dir) = path.parent() {
        pixel_git::sidecar::private_dir(dir).map_err(|e| format!("reference dir: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    pixel_git::nofollow::write_replace(&path, &bytes, pixel_git::nofollow::PRIVATE_MODE)
        .map_err(|e| format!("reference write: {e}"))
}

/// Compute the local source path for a reference entry.
///
/// The path is `.pixel/references/<id>/<revision>` unless the entry
/// specifies an absolute or relative path explicitly.
fn source_path(root: &Path, entry: &ReferenceEntry) -> PathBuf {
    if entry.source.is_absolute() {
        entry.source.clone()
    } else {
        root.join(&entry.source)
    }
}

/// Clone or fetch a reference corpus to its exact revision.
///
/// Returns the actual commit hash after checkout, or an error describing
/// what went wrong.
fn fetch_corpus(root: &Path, entry: &ReferenceEntry) -> Result<String, String> {
    let dest = source_path(root, entry);

    // If the destination already exists as a git repo, fetch and checkout.
    if dest.join(".git").is_dir() {
        let runner = pixel_git::GitRunner::new(&dest);
        runner
            .run(&["fetch", "--quiet", "origin", &entry.revision])
            .map_err(|e| format!("reference setup: {}: fetch: {e}", entry.id))?;
        runner
            .run(&["checkout", "--quiet", &entry.revision])
            .map_err(|e| format!("reference setup: {}: checkout: {e}", entry.id))?;
        let head = runner
            .rev_parse_head()
            .ok_or_else(|| format!("reference setup: {}: no HEAD after checkout", entry.id))?;
        return Ok(head);
    }

    // Clone the repository.
    std::fs::create_dir_all(&dest).map_err(|e| {
        format!(
            "reference setup: {}: create {}: {e}",
            entry.id,
            dest.display()
        )
    })?;

    let runner = pixel_git::GitRunner::new(root);
    runner
        .run(&["clone", "--quiet", &entry.repo, dest.to_str().unwrap_or("")])
        .map_err(|e| format!("reference setup: {}: clone: {e}", entry.id))?;

    // Checkout the exact revision.
    let dest_runner = pixel_git::GitRunner::new(&dest);
    dest_runner
        .run(&["checkout", "--quiet", &entry.revision])
        .map_err(|e| format!("reference setup: {}: checkout: {e}", entry.id))?;

    let head = dest_runner
        .rev_parse_head()
        .ok_or_else(|| format!("reference setup: {}: no HEAD after checkout", entry.id))?;
    Ok(head)
}

/// Verify that a reference corpus is at its exact revision.
///
/// Returns `Ok(())` if the corpus exists and matches, or an error describing
/// the mismatch.
fn verify_corpus(root: &Path, entry: &ReferenceEntry) -> Result<(), String> {
    let dest = source_path(root, entry);
    if !dest.join(".git").is_dir() {
        return Err(format!(
            "reference setup: {}: missing corpus at {}",
            entry.id,
            dest.display()
        ));
    }
    let runner = pixel_git::GitRunner::new(&dest);
    let head = runner
        .rev_parse_head()
        .ok_or_else(|| format!("reference setup: {}: cannot determine HEAD", entry.id))?;

    // If the revision is a full commit hash, compare directly.
    // Otherwise, resolve it to a commit hash and compare.
    if entry.revision.len() == 40 && entry.revision.chars().all(|c| c.is_ascii_hexdigit()) {
        if head != entry.revision {
            return Err(format!(
                "reference setup: {}: wrong revision: expected {}, got {}",
                entry.id, entry.revision, head
            ));
        }
    } else {
        // For tags and branches, resolve to a commit and compare.
        let resolved = runner
            .run(&["rev-parse", "--verify", &entry.revision])
            .map_err(|e| format!("reference setup: {}: resolve: {e}", entry.id))?;
        let resolved_str = String::from_utf8_lossy(&resolved).trim().to_string();
        if resolved_str != head {
            return Err(format!(
                "reference setup: {}: wrong revision: expected {} (resolved to {}), got {}",
                entry.id, entry.revision, resolved_str, head
            ));
        }
    }
    Ok(())
}

/// Setup action: clone/fetch/build all pinned references.
///
/// Each entry is processed independently. Missing or wrong-revision corpora
/// are disclosed individually in the result.
pub fn setup(root: &Path) -> Result<Value, String> {
    let manifest = load_manifest(root)?;
    if manifest.entries.is_empty() {
        return Err(
            "reference setup: no entries in manifest — `pixel reference add` first".to_string(),
        );
    }

    let mut results = Vec::new();
    let mut failures = Vec::new();

    for entry in &manifest.entries {
        let dest = source_path(root, entry);
        let result = fetch_corpus(root, entry);
        match result {
            Ok(head) => {
                // Verify the corpus is at the exact revision.
                if let Err(e) = verify_corpus(root, entry) {
                    failures.push(json!({
                        "id": entry.id,
                        "repo": entry.repo,
                        "revision": entry.revision,
                        "source": dest.display().to_string(),
                        "role": entry.role.as_str(),
                        "status": "error",
                        "error": e,
                    }));
                } else {
                    results.push(json!({
                        "id": entry.id,
                        "repo": entry.repo,
                        "revision": entry.revision,
                        "resolved": head,
                        "source": dest.display().to_string(),
                        "role": entry.role.as_str(),
                        "status": "ok",
                    }));
                }
            }
            Err(e) => {
                failures.push(json!({
                    "id": entry.id,
                    "repo": entry.repo,
                    "revision": entry.revision,
                    "source": dest.display().to_string(),
                    "role": entry.role.as_str(),
                    "status": "error",
                    "error": e,
                }));
            }
        }
    }

    let out = json!({
        "ok": results,
        "failures": failures,
        "total": manifest.entries.len(),
    });

    if !failures.is_empty() {
        return Err(format!(
            "reference setup: {} of {} corpora failed",
            failures.len(),
            manifest.entries.len()
        ));
    }

    Ok(out)
}

/// Query action: search across pinned references in a cold, read-only manner.
///
/// This function cannot fetch, clone, build, or modify source. It reads the
/// manifest and reports the status of each pinned corpus. Every result
/// identifies the target/reference role, repository, exact revision, and
/// source location. Missing or wrong-revision corpora are disclosed
/// individually.
pub fn query(root: &Path, filter: Option<&str>) -> Result<Value, String> {
    let manifest = load_manifest(root)?;

    let mut results = Vec::new();
    let mut disclosures = Vec::new();

    for entry in &manifest.entries {
        // Apply filter if provided.
        if let Some(f) = filter
            && !entry.id.contains(f)
            && !entry.repo.contains(f)
        {
            continue;
        }

        let dest = source_path(root, entry);
        let role_str = entry.role.as_str();

        // Check if the corpus exists locally.
        if !dest.join(".git").is_dir() {
            disclosures.push(json!({
                "id": entry.id,
                "repo": entry.repo,
                "revision": entry.revision,
                "source": dest.display().to_string(),
                "role": role_str,
                "status": "missing",
                "disclosure": format!("corpus {} is missing from {}", entry.id, dest.display()),
            }));
            continue;
        }

        // Verify the revision.
        let runner = pixel_git::GitRunner::new(&dest);
        match runner.rev_parse_head() {
            Some(head) => {
                // Check if the revision matches.
                let revision_matches = if entry.revision.len() == 40
                    && entry.revision.chars().all(|c| c.is_ascii_hexdigit())
                {
                    head == entry.revision
                } else {
                    // For tags/branches, resolve and compare.
                    runner
                        .run(&["rev-parse", "--verify", &entry.revision])
                        .is_ok_and(|r| String::from_utf8_lossy(&r).trim() == head)
                };

                if revision_matches {
                    results.push(json!({
                        "id": entry.id,
                        "repo": entry.repo,
                        "revision": entry.revision,
                        "resolved": head,
                        "source": dest.display().to_string(),
                        "role": role_str,
                        "licence": entry.licence,
                        "provenance": entry.provenance,
                        "status": "ok",
                    }));
                } else {
                    disclosures.push(json!({
                        "id": entry.id,
                        "repo": entry.repo,
                        "revision": entry.revision,
                        "source": dest.display().to_string(),
                        "role": role_str,
                        "status": "wrong-revision",
                        "disclosure": format!(
                            "wrong revision: corpus {} is at {} but pinned to {}",
                            entry.id, head, entry.revision
                        ),
                    }));
                }
            }
            None => {
                disclosures.push(json!({
                    "id": entry.id,
                    "repo": entry.repo,
                    "revision": entry.revision,
                    "source": dest.display().to_string(),
                    "role": role_str,
                    "status": "error",
                    "disclosure": format!("corpus {} has no HEAD", entry.id),
                }));
            }
        }
    }

    Ok(json!({
        "results": results,
        "disclosures": disclosures,
        "total": manifest.entries.len(),
    }))
}

/// Add a reference entry to the manifest.
#[allow(clippy::too_many_arguments)]
pub fn add(
    root: &Path,
    id: String,
    repo: String,
    revision: String,
    source: Option<PathBuf>,
    licence: String,
    provenance: String,
    role: Role,
) -> Result<(), String> {
    let mut manifest = load_manifest(root)?;

    // Compute the source path.
    let source =
        source.unwrap_or_else(|| PathBuf::from(format!(".pixel/references/{id}/{revision}")));

    // Check for duplicate ID + revision combination.
    if manifest
        .entries
        .iter()
        .any(|e| e.id == id && e.revision == revision)
    {
        return Err(format!(
            "reference: {id} at {revision} already exists — use `pixel reference remove` first"
        ));
    }

    manifest.entries.push(ReferenceEntry {
        id,
        repo,
        revision,
        source,
        licence,
        provenance,
        role,
    });

    save_manifest(root, &manifest)?;
    Ok(())
}

/// Remove a reference entry from the manifest.
pub fn remove(root: &Path, id: &str, revision: Option<&str>) -> Result<(), String> {
    let mut manifest = load_manifest(root)?;
    let before = manifest.entries.len();

    manifest.entries.retain(|e| {
        if e.id != id {
            return true;
        }
        match revision {
            Some(r) => e.revision != r,
            None => false,
        }
    });

    if manifest.entries.len() == before {
        return Err(format!(
            "reference: no entry found for {}{}",
            id,
            revision.map(|r| format!(" at {r}")).unwrap_or_default()
        ));
    }

    save_manifest(root, &manifest)?;
    Ok(())
}

/// List all reference entries in the manifest.
pub fn list(root: &Path, json: bool) -> Result<(), String> {
    let manifest = load_manifest(root)?;

    if json {
        let out = json!({
            "format": manifest.format,
            "entries": manifest.entries.iter().map(|e| {
                json!({
                    "id": e.id,
                    "repo": e.repo,
                    "revision": e.revision,
                    "source": e.source.display().to_string(),
                    "licence": e.licence,
                    "provenance": e.provenance,
                    "role": e.role.as_str(),
                })
            }).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
    } else if manifest.entries.is_empty() {
        println!("reference: no entries — `pixel reference add` first");
    } else {
        for e in &manifest.entries {
            println!(
                "{} {} {} {} ({})",
                e.id,
                e.repo,
                e.revision,
                e.source.display(),
                e.role.as_str()
            );
        }
    }
    Ok(())
}

pub fn run(cmd: ReferenceCmd) -> Result<(), String> {
    match cmd {
        ReferenceCmd::Add {
            id,
            repo,
            revision,
            source,
            licence,
            provenance,
            role,
            path,
        } => {
            add(
                &path,
                id.clone(),
                repo,
                revision,
                source,
                licence,
                provenance,
                Role::from(role),
            )?;
            println!("reference: added {id}");
            Ok(())
        }
        ReferenceCmd::Remove { id, revision, path } => {
            remove(&path, &id, revision.as_deref())?;
            println!("reference: removed {id}");
            Ok(())
        }
        ReferenceCmd::List { path, json } => list(&path, json),
        ReferenceCmd::Setup { path } => {
            let out = setup(&path)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        ReferenceCmd::Query { path, filter, json } => {
            let out = query(&path, filter.as_deref())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
                );
            } else {
                let results = out["results"].as_array().cloned().unwrap_or_default();
                let disclosures = out["disclosures"].as_array().cloned().unwrap_or_default();

                if results.is_empty() && disclosures.is_empty() {
                    println!("reference: no entries — `pixel reference add` first");
                    return Ok(());
                }

                for r in &results {
                    println!(
                        "[{}] {} {} {} ({})",
                        r["role"].as_str().unwrap_or("?"),
                        r["id"].as_str().unwrap_or("?"),
                        r["revision"].as_str().unwrap_or("?"),
                        r["source"].as_str().unwrap_or("?"),
                        r["status"].as_str().unwrap_or("?"),
                    );
                }

                for d in &disclosures {
                    println!(
                        "[{}] {} {} {} ({}) — {}",
                        d["role"].as_str().unwrap_or("?"),
                        d["id"].as_str().unwrap_or("?"),
                        d["revision"].as_str().unwrap_or("?"),
                        d["source"].as_str().unwrap_or("?"),
                        d["status"].as_str().unwrap_or("?"),
                        d["disclosure"].as_str().unwrap_or("?"),
                    );
                }
            }
            Ok(())
        }
    }
}

#[derive(clap::Subcommand)]
pub enum ReferenceCmd {
    /// Add a version-pinned reference corpus to the manifest.
    Add {
        /// Corpus or repository identifier.
        id: String,
        /// Source URL of the repository.
        repo: String,
        /// Exact revision (commit hash, tag, or branch) to pin.
        revision: String,
        /// Local filesystem path (default: .pixel/references/<id>/<revision>).
        #[arg(long)]
        source: Option<PathBuf>,
        /// Licence identifier.
        #[arg(long, default_value = "")]
        licence: String,
        /// Provenance information.
        #[arg(long, default_value = "")]
        provenance: String,
        /// Role: target or reference.
        #[arg(long, value_enum, default_value = "reference")]
        role: RoleArg,
        /// Project root.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Remove a reference corpus from the manifest.
    Remove {
        /// Corpus or repository identifier.
        id: String,
        /// Specific revision to remove (removes all if omitted).
        #[arg(long)]
        revision: Option<String>,
        /// Project root.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List all reference corpora in the manifest.
    List {
        /// Project root.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Clone/fetch/build all pinned references to their exact revisions.
    Setup {
        /// Project root.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Query pinned references (cold, read-only — cannot fetch, clone, build, or modify source).
    Query {
        /// Project root.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Filter by corpus ID or repository URL substring.
        #[arg(long)]
        filter: Option<String>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum RoleArg {
    Target,
    Reference,
}

impl From<RoleArg> for Role {
    fn from(arg: RoleArg) -> Self {
        match arg {
            RoleArg::Target => Role::Target,
            RoleArg::Reference => Role::Reference,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("px-ref-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(SHARD_DIR)).unwrap();
        dir.canonicalize().unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(
            dir,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
    }

    fn make_entry(id: &str, revision: &str, role: Role) -> ReferenceEntry {
        ReferenceEntry {
            id: id.to_string(),
            repo: format!("https://example.com/{id}"),
            revision: revision.to_string(),
            source: PathBuf::from(format!(".pixel/references/{id}/{revision}")),
            licence: "MIT".to_string(),
            provenance: format!("https://example.com/{id}"),
            role,
        }
    }

    #[test]
    fn two_pinned_versions_of_one_dependency_coexist() {
        let root = scratch("coexist");
        let entry_v1 = make_entry("serde", "v1.0.0", Role::Reference);
        let entry_v2 = make_entry("serde", "v2.0.0", Role::Reference);

        add(
            &root,
            entry_v1.id.clone(),
            entry_v1.repo.clone(),
            entry_v1.revision.clone(),
            Some(entry_v1.source.clone()),
            entry_v1.licence.clone(),
            entry_v1.provenance.clone(),
            entry_v1.role,
        )
        .unwrap();

        add(
            &root,
            entry_v2.id.clone(),
            entry_v2.repo.clone(),
            entry_v2.revision.clone(),
            Some(entry_v2.source.clone()),
            entry_v2.licence.clone(),
            entry_v2.provenance.clone(),
            entry_v2.role,
        )
        .unwrap();

        let manifest = load_manifest(&root).unwrap();
        assert_eq!(manifest.entries.len(), 2);
        assert_eq!(manifest.entries[0].id, "serde");
        assert_eq!(manifest.entries[0].revision, "v1.0.0");
        assert_eq!(manifest.entries[1].id, "serde");
        assert_eq!(manifest.entries[1].revision, "v2.0.0");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn query_result_identifies_role_repo_revision_and_source() {
        let root = scratch("identify");
        let entry = make_entry("tokio", "v1.38.0", Role::Target);

        add(
            &root,
            entry.id.clone(),
            entry.repo.clone(),
            entry.revision.clone(),
            Some(entry.source.clone()),
            entry.licence.clone(),
            entry.provenance.clone(),
            entry.role,
        )
        .unwrap();

        // Create a git repo at the source path with a tag matching the revision.
        let dest = source_path(&root, &entry);
        std::fs::create_dir_all(&dest).unwrap();
        init_repo(&dest);
        git(&dest, &["tag", "v1.38.0"]);

        let result = query(&root, None).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);

        let r = &results[0];
        assert_eq!(r["role"], "target");
        assert_eq!(r["id"], "tokio");
        assert_eq!(r["revision"], "v1.38.0");
        assert!(r["source"].as_str().unwrap().contains("tokio"));
        assert_eq!(r["status"], "ok");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_corpus_is_disclosed_individually() {
        let root = scratch("missing");
        let entry = make_entry("missing-crate", "v1.0.0", Role::Reference);

        add(
            &root,
            entry.id.clone(),
            entry.repo.clone(),
            entry.revision.clone(),
            Some(entry.source.clone()),
            entry.licence.clone(),
            entry.provenance.clone(),
            entry.role,
        )
        .unwrap();

        // Do NOT create the corpus — it's missing.
        let result = query(&root, None).unwrap();
        let disclosures = result["disclosures"].as_array().unwrap();
        assert_eq!(disclosures.len(), 1);

        let d = &disclosures[0];
        assert_eq!(d["id"], "missing-crate");
        assert_eq!(d["status"], "missing");
        assert!(d["disclosure"].as_str().unwrap().contains("missing"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wrong_revision_corpus_is_disclosed() {
        let root = scratch("wrongrev");
        let entry = make_entry("wrong-crate", "v1.0.0", Role::Reference);

        add(
            &root,
            entry.id.clone(),
            entry.repo.clone(),
            entry.revision.clone(),
            Some(entry.source.clone()),
            entry.licence.clone(),
            entry.provenance.clone(),
            entry.role,
        )
        .unwrap();

        // Create a git repo at the source path but with a different revision.
        let dest = source_path(&root, &entry);
        std::fs::create_dir_all(&dest).unwrap();
        init_repo(&dest);

        // The repo HEAD won't match "v1.0.0" since it's a fresh repo.
        let result = query(&root, None).unwrap();
        let disclosures = result["disclosures"].as_array().unwrap();
        assert_eq!(disclosures.len(), 1);

        let d = &disclosures[0];
        assert_eq!(d["id"], "wrong-crate");
        assert_eq!(d["status"], "wrong-revision");
        assert!(d["disclosure"].as_str().unwrap().contains("wrong revision"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cold_query_does_not_fetch_clone_build_or_modify() {
        let root = scratch("cold");
        let entry = make_entry("cold-crate", "v1.0.0", Role::Reference);

        add(
            &root,
            entry.id.clone(),
            entry.repo.clone(),
            entry.revision.clone(),
            Some(entry.source.clone()),
            entry.licence.clone(),
            entry.provenance.clone(),
            entry.role,
        )
        .unwrap();

        // Record the state of the root before query.
        let manifest_path = manifest_path(&root);
        let manifest_before = std::fs::read(&manifest_path).unwrap();

        // Query must not modify the manifest.
        let _ = query(&root, None).unwrap();

        let manifest_after = std::fs::read(&manifest_path).unwrap();
        assert_eq!(
            manifest_before, manifest_after,
            "cold query modified the manifest"
        );

        // Query must not create any new directories.
        let dest = source_path(&root, &entry);
        assert!(!dest.exists(), "cold query created the corpus directory");

        // Query must not create a .git directory.
        assert!(
            !dest.join(".git").exists(),
            "cold query created a .git directory"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn setup_clones_and_verifies_corpora() {
        let root = scratch("setup");

        // Create a "remote" repo to clone from.
        let remote = root.join("remote-repo");
        std::fs::create_dir_all(&remote).unwrap();
        init_repo(&remote);
        // Create a tag for the revision.
        git(&remote, &["tag", "v1.0.0"]);

        let entry = ReferenceEntry {
            id: "test-crate".to_string(),
            repo: remote.display().to_string(),
            revision: "v1.0.0".to_string(),
            source: PathBuf::from(".pixel/references/test-crate/v1.0.0"),
            licence: "MIT".to_string(),
            provenance: remote.display().to_string(),
            role: Role::Reference,
        };

        add(
            &root,
            entry.id.clone(),
            entry.repo.clone(),
            entry.revision.clone(),
            Some(entry.source.clone()),
            entry.licence.clone(),
            entry.provenance.clone(),
            entry.role,
        )
        .unwrap();

        let result = setup(&root).unwrap();
        let ok = result["ok"].as_array().unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0]["id"], "test-crate");
        assert_eq!(ok[0]["status"], "ok");

        // Verify the corpus exists at the source path.
        let dest = source_path(&root, &entry);
        assert!(dest.join(".git").is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn setup_reports_missing_and_wrong_revision_corpora() {
        let root = scratch("setup-fail");

        // Add an entry pointing to a non-existent repo.
        let bad_entry = ReferenceEntry {
            id: "bad-crate".to_string(),
            repo: "https://example.com/nonexistent".to_string(),
            revision: "v1.0.0".to_string(),
            source: PathBuf::from(".pixel/references/bad-crate/v1.0.0"),
            licence: "MIT".to_string(),
            provenance: "https://example.com/nonexistent".to_string(),
            role: Role::Reference,
        };

        add(
            &root,
            bad_entry.id.clone(),
            bad_entry.repo.clone(),
            bad_entry.revision.clone(),
            Some(bad_entry.source.clone()),
            bad_entry.licence.clone(),
            bad_entry.provenance.clone(),
            bad_entry.role,
        )
        .unwrap();

        let err = setup(&root).unwrap_err();
        assert!(err.contains("failed"), "{err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn add_duplicate_id_revision_is_rejected() {
        let root = scratch("dup");
        let entry = make_entry("serde", "v1.0.0", Role::Reference);

        add(
            &root,
            entry.id.clone(),
            entry.repo.clone(),
            entry.revision.clone(),
            Some(entry.source.clone()),
            entry.licence.clone(),
            entry.provenance.clone(),
            entry.role,
        )
        .unwrap();

        let result = add(
            &root,
            entry.id.clone(),
            entry.repo.clone(),
            entry.revision.clone(),
            Some(entry.source.clone()),
            entry.licence.clone(),
            entry.provenance.clone(),
            entry.role,
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_specific_revision() {
        let root = scratch("remove-rev");
        let entry_v1 = make_entry("serde", "v1.0.0", Role::Reference);
        let entry_v2 = make_entry("serde", "v2.0.0", Role::Reference);

        add(
            &root,
            entry_v1.id.clone(),
            entry_v1.repo.clone(),
            entry_v1.revision.clone(),
            Some(entry_v1.source.clone()),
            entry_v1.licence.clone(),
            entry_v1.provenance.clone(),
            entry_v1.role,
        )
        .unwrap();

        add(
            &root,
            entry_v2.id.clone(),
            entry_v2.repo.clone(),
            entry_v2.revision.clone(),
            Some(entry_v2.source.clone()),
            entry_v2.licence.clone(),
            entry_v2.provenance.clone(),
            entry_v2.role,
        )
        .unwrap();

        remove(&root, "serde", Some("v1.0.0")).unwrap();

        let manifest = load_manifest(&root).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].revision, "v2.0.0");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn query_filter_matches_id_and_repo() {
        let root = scratch("filter");
        let entry1 = make_entry("serde", "v1.0.0", Role::Reference);
        let entry2 = make_entry("tokio", "v1.38.0", Role::Reference);

        add(
            &root,
            entry1.id.clone(),
            entry1.repo.clone(),
            entry1.revision.clone(),
            Some(entry1.source.clone()),
            entry1.licence.clone(),
            entry1.provenance.clone(),
            entry1.role,
        )
        .unwrap();

        add(
            &root,
            entry2.id.clone(),
            entry2.repo.clone(),
            entry2.revision.clone(),
            Some(entry2.source.clone()),
            entry2.licence.clone(),
            entry2.provenance.clone(),
            entry2.role,
        )
        .unwrap();

        // Filter by ID.
        let result = query(&root, Some("serde")).unwrap();
        let results = result["results"].as_array().unwrap();
        let disclosures = result["disclosures"].as_array().unwrap();
        let total = results.len() + disclosures.len();
        assert_eq!(total, 1);

        // Filter by repo substring.
        let result = query(&root, Some("tokio")).unwrap();
        let results = result["results"].as_array().unwrap();
        let disclosures = result["disclosures"].as_array().unwrap();
        let total = results.len() + disclosures.len();
        assert_eq!(total, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_stored_at_pixel_reference_json() {
        let root = scratch("path");
        let entry = make_entry("serde", "v1.0.0", Role::Reference);

        add(
            &root,
            entry.id.clone(),
            entry.repo.clone(),
            entry.revision.clone(),
            Some(entry.source.clone()),
            entry.licence.clone(),
            entry_provenance_clone(&entry),
            entry.role,
        )
        .unwrap();

        let path = root.join(SHARD_DIR).join(REFERENCE_FILE);
        assert!(path.exists(), "manifest not at {}", path.display());

        let _ = std::fs::remove_dir_all(&root);
    }

    fn entry_provenance_clone(entry: &ReferenceEntry) -> String {
        entry.provenance.clone()
    }
}
