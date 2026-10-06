// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel reference` — version-pinned reference corpora.
//!
//! A reference corpus is a local checkout of a dependency (or any other
//! repository) pinned to an exact revision, registered in
//! `.pixel/references.json`. The manifest records the stable corpus ID, the
//! source URL, the user-facing revision, the resolved commit SHA, the local
//! path, licence/provenance metadata and the corpus role (target or
//! reference). Multiple versions of the same dependency coexist as separate
//! entries; they never exchange citations or snippets because each query
//! hit carries the corpus ID and revision it came from.
//!
//! Setup and querying are separate: only `pixel reference setup` may clone,
//! fetch, checkout or build. A query is a cold, read-only operation — it
//! walks the local source tree and runs a regex over it. It never fetches,
//! clones, builds or modifies source, and it never silently falls back to a
//! different revision than the one pinned: a corpus whose HEAD does not
//! match its pinned revision is disclosed as a failure in the coverage
//! report, not searched.
//!
//! Reference text is untrusted evidence, never executable instructions: a
//! hit is a citation to read and verify, not a command to run.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use pixel_index::index::{SHARD_DIR, policy_walk};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const REFERENCES_FILE: &str = "references.json";
const MANIFEST_FORMAT: u32 = 1;

/// The role a corpus plays in a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CorpusRole {
    /// The target project being worked on.
    Target,
    /// A reference dependency implementation.
    Reference,
}

impl CorpusRole {
    pub fn as_str(self) -> &'static str {
        match self {
            CorpusRole::Target => "target",
            CorpusRole::Reference => "reference",
        }
    }
}

/// One pinned reference corpus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusEntry {
    /// Stable corpus ID (e.g. "serde-1.0.188").
    pub id: String,
    /// Target or reference.
    pub role: CorpusRole,
    /// Source URL (e.g. "https://github.com/serde-rs/serde").
    pub source_url: String,
    /// User-facing pinned revision (tag, branch, or SHA).
    pub revision: String,
    /// Resolved commit SHA at setup time; the query verifies HEAD against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_sha: Option<String>,
    /// Local source location.
    pub path: PathBuf,
    /// Licence identifier (e.g. "MIT OR Apache-2.0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Package name (e.g. "serde").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Package version (e.g. "1.0.188").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_version: Option<String>,
    /// Provenance note (how the corpus was obtained).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default = "default_format")]
    format: u32,
    #[serde(default)]
    corpora: Vec<CorpusEntry>,
}

fn default_format() -> u32 {
    MANIFEST_FORMAT
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join(SHARD_DIR).join(REFERENCES_FILE)
}

fn load(root: &Path) -> Result<Manifest, String> {
    let path = manifest_path(root);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("references {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(e) => Err(format!("references {}: {e}", path.display())),
    }
}

fn save(root: &Path, manifest: &Manifest) -> Result<(), String> {
    let path = manifest_path(root);
    if let Some(dir) = path.parent() {
        pixel_git::sidecar::private_dir(dir).map_err(|e| format!("references dir: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    // A fresh temporary file renamed over the name: a link committed at
    // either name is replaced, never written through.
    pixel_git::nofollow::write_replace(&path, &bytes, pixel_git::nofollow::PRIVATE_MODE)
        .map_err(|e| format!("references write: {e}"))
}

/// One hit from a reference corpus query.
#[derive(Debug, Clone, Serialize)]
pub struct ReferenceHit {
    pub corpus_id: String,
    pub role: &'static str,
    pub repository: String,
    pub revision: String,
    pub path: String,
    pub line: u64,
    pub snippet: String,
}

/// Per-corpus coverage: what happened to each corpus in the query.
#[derive(Debug, Clone, Serialize)]
pub struct CorpusCoverage {
    pub corpus_id: String,
    pub ok: bool,
    pub error: Option<String>,
    pub hits: usize,
    pub capped: bool,
}

/// The full query result.
#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub query: String,
    pub hits: Vec<ReferenceHit>,
    pub coverage: Vec<CorpusCoverage>,
    pub partial: bool,
}

/// Query options.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub role: Option<CorpusRole>,
    pub revision: Option<String>,
    pub package: Option<String>,
    pub id: Option<String>,
    pub limit: usize,
    pub per_corpus_limit: usize,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            role: None,
            revision: None,
            package: None,
            id: None,
            limit: 50,
            per_corpus_limit: 20,
        }
    }
}

/// Run a bounded fan-out query over the registered reference corpora.
///
/// The query is cold and read-only: it walks each corpus's local source tree
/// and runs `pattern` as a regex over every line. It never fetches, clones,
/// builds or modifies source. A corpus that is missing, not a git repository,
/// or not at its pinned revision is disclosed in the coverage report and
/// skipped — the remaining corpora still answer.
pub fn query(root: &Path, pattern: &str, opts: &QueryOptions) -> Result<QueryResult, String> {
    let manifest = load(root)?;
    if manifest.corpora.is_empty() {
        return Err(
            "reference query: no corpora registered — `pixel reference add` first".to_string(),
        );
    }
    let regex =
        regex::Regex::new(pattern).map_err(|e| format!("reference query: bad pattern: {e}"))?;
    let mut hits: Vec<ReferenceHit> = Vec::new();
    let mut coverage: Vec<CorpusCoverage> = Vec::new();
    let mut partial = false;
    for corpus in &manifest.corpora {
        if opts.role.is_some_and(|role| corpus.role != role) {
            continue;
        }
        if opts
            .revision
            .as_deref()
            .is_some_and(|rev| corpus.revision != rev)
        {
            continue;
        }
        if opts
            .package
            .as_deref()
            .is_some_and(|pkg| corpus.package.as_deref() != Some(pkg))
        {
            continue;
        }
        if opts.id.as_deref().is_some_and(|id| corpus.id != id) {
            continue;
        }
        match search_corpus(corpus, &regex, opts.per_corpus_limit) {
            Ok((mut corpus_hits, capped)) => {
                let count = corpus_hits.len();
                hits.append(&mut corpus_hits);
                coverage.push(CorpusCoverage {
                    corpus_id: corpus.id.clone(),
                    ok: true,
                    error: None,
                    hits: count,
                    capped,
                });
            }
            Err(e) => {
                partial = true;
                coverage.push(CorpusCoverage {
                    corpus_id: corpus.id.clone(),
                    ok: false,
                    error: Some(e),
                    hits: 0,
                    capped: false,
                });
            }
        }
    }
    if coverage.is_empty() {
        return Err(format!(
            "reference query: no corpora match the filters (role={}, revision={}, package={}, id={})",
            opts.role
                .map_or_else(|| "any".to_string(), |r| r.as_str().to_string()),
            opts.revision.as_deref().unwrap_or("any"),
            opts.package.as_deref().unwrap_or("any"),
            opts.id.as_deref().unwrap_or("any"),
        ));
    }
    // Fusion policy: target-role corpora first (the project being worked on
    // outranks reference material), then reference; within a role, by corpus
    // ID, then path, then line. Per-corpus quotas were applied in
    // `search_corpus`. Raw regex match scores are equal across corpora (each
    // hit is a match), so no cross-corpus score comparison is needed.
    hits.sort_by(|a, b| {
        role_rank(a.role)
            .cmp(&role_rank(b.role))
            .then_with(|| a.corpus_id.cmp(&b.corpus_id))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    let hits: Vec<ReferenceHit> = hits.into_iter().take(opts.limit).collect();
    Ok(QueryResult {
        query: pattern.to_string(),
        hits,
        coverage,
        partial,
    })
}

fn role_rank(role: &str) -> u8 {
    match role {
        "target" => 0,
        _ => 1,
    }
}

/// Search one corpus: verify it is present and at its pinned revision, then
/// walk its source tree and collect regex matches.
fn search_corpus(
    corpus: &CorpusEntry,
    regex: &regex::Regex,
    limit: usize,
) -> Result<(Vec<ReferenceHit>, bool), String> {
    let path = &corpus.path;
    if !path.exists() {
        return Err(format!(
            "{}: corpus path missing: {} — run `pixel reference setup {0}`",
            corpus.id,
            path.display()
        ));
    }
    if !path.join(".git").exists() {
        return Err(format!(
            "{}: not a git repository: {}",
            corpus.id,
            path.display()
        ));
    }
    let head = pixel_git::GitRunner::new(path)
        .rev_parse_head()
        .ok_or_else(|| format!("{}: cannot resolve HEAD", corpus.id))?;
    if let Some(sha) = &corpus.revision_sha {
        if &head != sha {
            return Err(format!(
                "{}: wrong revision — pinned {} ({}), HEAD is {head}",
                corpus.id, corpus.revision, sha
            ));
        }
    } else if head != corpus.revision {
        return Err(format!(
            "{}: wrong revision — pinned {}, HEAD is {head}",
            corpus.id, corpus.revision
        ));
    }
    let mut hits: Vec<ReferenceHit> = Vec::new();
    let mut capped = false;
    for entry in policy_walk(path) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = match entry.path().strip_prefix(path) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let relative_str = relative.to_string_lossy().to_string();
        let file = match std::fs::File::open(entry.path()) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let reader = std::io::BufReader::new(file);
        for (index, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(line) => line,
                Err(_) => continue,
            };
            if regex.is_match(&line) {
                hits.push(ReferenceHit {
                    corpus_id: corpus.id.clone(),
                    role: corpus.role.as_str(),
                    repository: corpus.source_url.clone(),
                    revision: corpus.revision.clone(),
                    path: relative_str.clone(),
                    line: (index + 1) as u64,
                    snippet: line.trim().to_string(),
                });
                if hits.len() >= limit {
                    capped = true;
                    return Ok((hits, capped));
                }
            }
        }
    }
    Ok((hits, capped))
}

/// Explicit setup: clone (or fetch), checkout the pinned revision, resolve
/// the commit SHA into the manifest, and build the index. This is the only
/// operation that may clone, fetch, checkout or build.
pub fn setup(root: &Path, ids: &[String]) -> Result<Value, String> {
    let mut manifest = load(root)?;
    if manifest.corpora.is_empty() {
        return Err(
            "reference setup: no corpora registered — `pixel reference add` first".to_string(),
        );
    }
    let mut results: Vec<Value> = Vec::new();
    for id in ids {
        let corpus = manifest
            .corpora
            .iter_mut()
            .find(|corpus| &corpus.id == id)
            .ok_or_else(|| format!("reference setup: unknown corpus '{id}'"))?;
        let result = setup_corpus(corpus)?;
        results.push(result);
    }
    save(root, &manifest)?;
    Ok(json!({ "setup": results }))
}

fn setup_corpus(corpus: &mut CorpusEntry) -> Result<Value, String> {
    let path = &corpus.path;
    if !path.exists() {
        let parent = path
            .parent()
            .ok_or_else(|| format!("{}: no parent directory", corpus.id))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("{}: create {}: {e}", corpus.id, parent.display()))?;
        // `git -C <parent> clone <url> <path>`: the runner's `-C` is the
        // parent because the target directory does not exist yet.
        let runner = pixel_git::GitRunner::new(parent);
        runner
            .run(&["clone", &corpus.source_url, &path.to_string_lossy()])
            .map_err(|e| format!("{}: clone {}: {e}", corpus.id, corpus.source_url))?;
    } else {
        let runner = pixel_git::GitRunner::new(path);
        runner
            .run(&["fetch", "--all"])
            .map_err(|e| format!("{}: fetch: {e}", corpus.id))?;
    }
    let runner = pixel_git::GitRunner::new(path);
    runner
        .run(&["checkout", &corpus.revision])
        .map_err(|e| format!("{}: checkout {}: {e}", corpus.id, corpus.revision))?;
    let sha = runner
        .rev_parse_head()
        .ok_or_else(|| format!("{}: cannot resolve HEAD after checkout", corpus.id))?;
    corpus.revision_sha = Some(sha.clone());
    corpus.provenance = Some(format!("set up by `pixel reference setup` at {sha}"));
    // Build the index so the corpus is usable with every pixel retrieval
    // mode, not just `pixel reference query`.
    let index_note = build_index(path)?;
    Ok(json!({
        "corpus_id": corpus.id,
        "revision": corpus.revision,
        "revision_sha": sha,
        "index": index_note,
    }))
}

/// Build the text index for a corpus checkout (the same in-process build
/// `pixel build-index` falls back to).
fn build_index(path: &Path) -> Result<String, String> {
    let _lock = pixel_index::BuildLock::acquire(path).map_err(|e| format!("build lock: {e}"))?;
    let extractor = pixel_index::gram::SparseGramExtractor::new(pixel_index::weights::Crc32Weigher);
    let stats = pixel_index::index::build(path, &extractor).map_err(|e| e.to_string())?;
    Ok(format!("indexed {} files", stats.files))
}

/// Optional metadata for a corpus entry.
#[derive(Debug, Default, Clone)]
pub struct CorpusMetadata {
    pub license: Option<String>,
    pub package: Option<String>,
    pub package_version: Option<String>,
}

/// Add (or update) a corpus entry in the manifest.
pub fn add(
    root: &Path,
    id: String,
    role: CorpusRole,
    source_url: String,
    revision: String,
    path: PathBuf,
    metadata: CorpusMetadata,
) -> Result<CorpusEntry, String> {
    let mut manifest = load(root)?;
    let stored_path = if path.exists() {
        path.canonicalize()
            .map_err(|e| format!("reference add: {}: {e}", path.display()))?
    } else {
        // The path may not exist yet (setup clones it later); store the
        // absolute form so setup can create it.
        std::path::absolute(&path).map_err(|e| format!("reference add: {}: {e}", path.display()))?
    };
    let entry = CorpusEntry {
        id: id.clone(),
        role,
        source_url,
        revision,
        revision_sha: None,
        path: stored_path,
        license: metadata.license,
        package: metadata.package,
        package_version: metadata.package_version,
        provenance: None,
    };
    if let Some(existing) = manifest.corpora.iter_mut().find(|c| c.id == id) {
        *existing = entry.clone();
    } else {
        manifest.corpora.push(entry.clone());
    }
    save(root, &manifest)?;
    Ok(entry)
}

/// Remove a corpus entry from the manifest.
pub fn remove(root: &Path, id: &str) -> Result<(), String> {
    let mut manifest = load(root)?;
    let before = manifest.corpora.len();
    manifest.corpora.retain(|corpus| corpus.id != id);
    if manifest.corpora.len() == before {
        return Err(format!("reference remove: '{id}' is not registered"));
    }
    save(root, &manifest)?;
    Ok(())
}

/// List the registered corpora.
pub fn list(root: &Path) -> Result<Vec<CorpusEntry>, String> {
    Ok(load(root)?.corpora)
}

/// Print a query result as text: one line per hit with full attribution,
/// then the coverage report.
pub fn print_query_result(result: &QueryResult) {
    if result.hits.is_empty() {
        println!("no matches for \"{}\"", result.query);
    } else {
        println!("reference matches for \"{}\":", result.query);
        for hit in &result.hits {
            println!(
                "  [{} | {} | {} | {}] {}:{}: {}",
                hit.role,
                hit.corpus_id,
                hit.revision,
                hit.repository,
                hit.path,
                hit.line,
                hit.snippet
            );
        }
    }
    println!("coverage:");
    for coverage in &result.coverage {
        if coverage.ok {
            println!(
                "  {}: {} hit(s){}",
                coverage.corpus_id,
                coverage.hits,
                if coverage.capped { " (capped)" } else { "" }
            );
        } else {
            println!(
                "  {}: unavailable — {}",
                coverage.corpus_id,
                coverage.error.as_deref().unwrap_or("unknown")
            );
        }
    }
    if result.partial {
        println!("note: partial coverage — some corpora were unavailable");
    }
}

pub fn run(cmd: ReferenceCmd) -> Result<(), String> {
    match cmd {
        ReferenceCmd::Add {
            id,
            role,
            url,
            revision,
            path,
            license,
            package,
            package_version,
            root,
        } => {
            let entry = add(
                &root,
                id,
                role.into(),
                url,
                revision,
                path,
                CorpusMetadata {
                    license,
                    package,
                    package_version,
                },
            )?;
            println!(
                "reference: added {} ({}) at {}",
                entry.id,
                entry.role.as_str(),
                entry.path.display()
            );
            Ok(())
        }
        ReferenceCmd::List { root, json } => {
            let corpora = list(&root)?;
            if json {
                let rows: Vec<Value> = corpora
                    .iter()
                    .map(|corpus| {
                        json!({
                            "id": corpus.id,
                            "role": corpus.role.as_str(),
                            "source_url": corpus.source_url,
                            "revision": corpus.revision,
                            "revision_sha": corpus.revision_sha,
                            "path": corpus.path.display().to_string(),
                            "license": corpus.license,
                            "package": corpus.package,
                            "package_version": corpus.package_version,
                            "provenance": corpus.provenance,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "corpora": rows }))
                        .map_err(|e| e.to_string())?
                );
            } else if corpora.is_empty() {
                println!("reference: no corpora registered — `pixel reference add` first");
            } else {
                for entry in &corpora {
                    println!(
                        "{} ({}) {} @ {} -> {}",
                        entry.id,
                        entry.role.as_str(),
                        entry.source_url,
                        entry.revision,
                        entry.path.display()
                    );
                }
            }
            Ok(())
        }
        ReferenceCmd::Remove { id, root } => {
            remove(&root, &id)?;
            println!("reference: removed {id}");
            Ok(())
        }
        ReferenceCmd::Setup { ids, root } => {
            let report = setup(&root, &ids)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        ReferenceCmd::Query {
            pattern,
            role,
            revision,
            package,
            id,
            limit,
            per_corpus_limit,
            json,
            root,
        } => {
            let opts = QueryOptions {
                role: role.map(CorpusRole::from),
                revision,
                package,
                id,
                limit,
                per_corpus_limit,
            };
            let result = query(&root, &pattern, &opts)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
                );
            } else {
                print_query_result(&result);
            }
            Ok(())
        }
    }
}

#[derive(clap::Subcommand)]
pub enum ReferenceCmd {
    /// Register a version-pinned reference corpus.
    Add {
        /// Stable corpus ID (e.g. "serde-1.0.188").
        id: String,
        /// Corpus role.
        #[arg(long, value_enum, default_value = "reference")]
        role: CorpusRoleArg,
        /// Source URL (e.g. https://github.com/serde-rs/serde).
        #[arg(long)]
        url: String,
        /// Pinned revision (tag, branch, or commit SHA).
        #[arg(long)]
        revision: String,
        /// Local source location.
        path: PathBuf,
        /// Licence identifier (e.g. "MIT OR Apache-2.0").
        #[arg(long)]
        license: Option<String>,
        /// Package name (e.g. "serde").
        #[arg(long)]
        package: Option<String>,
        /// Package version (e.g. "1.0.188").
        #[arg(long)]
        package_version: Option<String>,
        /// Repository holding the references manifest.
        #[arg(default_value = ".")]
        root: PathBuf,
    },
    /// List registered reference corpora.
    List {
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Remove a registered corpus.
    Remove {
        id: String,
        #[arg(default_value = ".")]
        root: PathBuf,
    },
    /// Explicit setup: clone/fetch, checkout the pinned revision, build the
    /// index. The only operation that may mutate.
    Setup {
        /// Corpus IDs to set up.
        ids: Vec<String>,
        #[arg(default_value = ".")]
        root: PathBuf,
    },
    /// Cold, read-only query over the registered reference corpora.
    Query {
        /// Regex pattern to search for.
        pattern: String,
        /// Filter by corpus role.
        #[arg(long, value_enum)]
        role: Option<CorpusRoleArg>,
        /// Filter by pinned revision.
        #[arg(long)]
        revision: Option<String>,
        /// Filter by package name.
        #[arg(long)]
        package: Option<String>,
        /// Filter by corpus ID.
        #[arg(long)]
        id: Option<String>,
        /// Maximum total hits to return.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Maximum hits per corpus (per-corpus quota).
        #[arg(long, default_value_t = 20)]
        per_corpus_limit: usize,
        #[arg(long)]
        json: bool,
        #[arg(default_value = ".")]
        root: PathBuf,
    },
}

/// Clap-friendly role argument.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum CorpusRoleArg {
    Target,
    Reference,
}

impl From<CorpusRoleArg> for CorpusRole {
    fn from(arg: CorpusRoleArg) -> Self {
        match arg {
            CorpusRoleArg::Target => CorpusRole::Target,
            CorpusRoleArg::Reference => CorpusRole::Reference,
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

    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn init_repo(root: &Path) {
        git(root, &["init", "-q"]);
        git(
            root,
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

    fn corpus_entry(id: &str, revision: &str, path: &Path) -> CorpusEntry {
        CorpusEntry {
            id: id.to_string(),
            role: CorpusRole::Reference,
            source_url: "https://example.com/repo".to_string(),
            revision: revision.to_string(),
            revision_sha: None,
            path: path.to_path_buf(),
            license: Some("MIT".to_string()),
            package: Some("serde".to_string()),
            package_version: Some(revision.to_string()),
            provenance: None,
        }
    }

    #[test]
    fn add_then_list_round_trips() {
        let root = scratch("add");
        let path = scratch("add-path");
        let entry = add(
            &root,
            "serde-1.0.188".to_string(),
            CorpusRole::Reference,
            "https://github.com/serde-rs/serde".to_string(),
            "v1.0.188".to_string(),
            path.clone(),
            CorpusMetadata {
                license: Some("MIT OR Apache-2.0".to_string()),
                package: Some("serde".to_string()),
                package_version: Some("1.0.188".to_string()),
            },
        )
        .unwrap();
        assert_eq!(entry.id, "serde-1.0.188");
        assert_eq!(entry.role, CorpusRole::Reference);
        let corpora = list(&root).unwrap();
        assert_eq!(corpora.len(), 1);
        assert_eq!(corpora[0].package.as_deref(), Some("serde"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn add_updates_existing_id() {
        let root = scratch("update");
        let path = scratch("update-path");
        add(
            &root,
            "serde-1.0.188".to_string(),
            CorpusRole::Reference,
            "https://github.com/serde-rs/serde".to_string(),
            "v1.0.188".to_string(),
            path.clone(),
            CorpusMetadata::default(),
        )
        .unwrap();
        add(
            &root,
            "serde-1.0.188".to_string(),
            CorpusRole::Reference,
            "https://github.com/serde-rs/serde".to_string(),
            "v1.0.200".to_string(),
            path,
            CorpusMetadata::default(),
        )
        .unwrap();
        let corpora = list(&root).unwrap();
        assert_eq!(corpora.len(), 1, "same id must update, not duplicate");
        assert_eq!(corpora[0].revision, "v1.0.200");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_reports_unknown_id() {
        let root = scratch("remove");
        let err = remove(&root, "nope").unwrap_err();
        assert!(err.contains("not registered"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn query_on_empty_manifest_errors() {
        let root = scratch("empty");
        let err = query(&root, "process", &QueryOptions::default()).unwrap_err();
        assert!(err.contains("no corpora"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn query_discloses_a_missing_corpus() {
        let root = scratch("missing");
        let missing = root.join("does-not-exist");
        let mut manifest = Manifest::default();
        manifest
            .corpora
            .push(corpus_entry("absent", "v1", &missing));
        save(&root, &manifest).unwrap();
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert!(
            result.partial,
            "a missing corpus must mark the query partial"
        );
        assert!(result.hits.is_empty());
        assert_eq!(result.coverage.len(), 1);
        assert!(!result.coverage[0].ok);
        assert!(
            result.coverage[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("missing")),
            "{:?}",
            result.coverage[0].error
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn query_discloses_a_wrong_revision() {
        let root = scratch("wrongrev");
        let path = scratch("wrongrev-path");
        init_repo(&path);
        let head = pixel_git::GitRunner::new(&path).rev_parse_head().unwrap();
        let mut manifest = Manifest::default();
        let mut entry = corpus_entry("serde", "v1.0.188", &path);
        entry.revision_sha = Some(head);
        manifest.corpora.push(entry);
        save(&root, &manifest).unwrap();
        // Move the checkout to a different commit.
        git(
            &path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "second",
            ],
        );
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert!(result.partial);
        assert!(!result.coverage[0].ok);
        assert!(
            result.coverage[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("wrong revision")),
            "{:?}",
            result.coverage[0].error
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn query_never_uses_latest_when_revision_mismatches() {
        // The corpus is pinned to v1.0.188 but the checkout has moved on.
        // The query must refuse, not silently search the new HEAD.
        let root = scratch("nolatest");
        let path = scratch("nolatest-path");
        init_repo(&path);
        std::fs::write(path.join("lib.rs"), "fn process_v1() {}\n").unwrap();
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "v1 content",
            ],
        );
        let head_v1 = pixel_git::GitRunner::new(&path).rev_parse_head().unwrap();
        std::fs::write(path.join("lib.rs"), "fn process_v2() {}\n").unwrap();
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "v2 content",
            ],
        );
        let mut manifest = Manifest::default();
        let mut entry = corpus_entry("serde", "v1.0.188", &path);
        entry.revision_sha = Some(head_v1);
        manifest.corpora.push(entry);
        save(&root, &manifest).unwrap();
        // HEAD is now the v2 commit; the pinned v1 must not be searched.
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert!(result.partial);
        assert!(
            result.hits.is_empty(),
            "a wrong-revision corpus must not be searched: {:?}",
            result.hits
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn query_returns_attributed_hits() {
        let root = scratch("hits");
        let path = scratch("hits-path");
        init_repo(&path);
        std::fs::write(path.join("lib.rs"), "fn process() {\n    run();\n}\n").unwrap();
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "content",
            ],
        );
        let head = pixel_git::GitRunner::new(&path).rev_parse_head().unwrap();
        let mut manifest = Manifest::default();
        let mut entry = corpus_entry("serde", "v1.0.188", &path);
        entry.revision_sha = Some(head);
        manifest.corpora.push(entry);
        save(&root, &manifest).unwrap();
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert!(!result.partial);
        assert_eq!(result.hits.len(), 1);
        let hit = &result.hits[0];
        assert_eq!(hit.corpus_id, "serde");
        assert_eq!(hit.role, "reference");
        assert_eq!(hit.repository, "https://example.com/repo");
        assert_eq!(hit.revision, "v1.0.188");
        assert_eq!(hit.path, "lib.rs");
        assert_eq!(hit.line, 1);
        assert!(hit.snippet.contains("process"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn two_pinned_versions_coexist_without_cross_contamination() {
        // Two versions of the same dependency, each at its own revision and
        // path. Hits from each carry their own corpus ID and revision.
        let root = scratch("coexist");
        let path_v1 = scratch("coexist-v1");
        let path_v2 = scratch("coexist-v2");
        init_repo(&path_v1);
        init_repo(&path_v2);
        std::fs::write(path_v1.join("lib.rs"), "fn process() { v1(); }\n").unwrap();
        std::fs::write(path_v2.join("lib.rs"), "fn process() { v2(); }\n").unwrap();
        for path in [&path_v1, &path_v2] {
            git(path, &["add", "."]);
            git(
                path,
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "-m",
                    "content",
                ],
            );
        }
        let head_v1 = pixel_git::GitRunner::new(&path_v1)
            .rev_parse_head()
            .unwrap();
        let head_v2 = pixel_git::GitRunner::new(&path_v2)
            .rev_parse_head()
            .unwrap();
        let mut manifest = Manifest::default();
        let mut entry_v1 = corpus_entry("serde-1.0.188", "v1.0.188", &path_v1);
        entry_v1.revision_sha = Some(head_v1);
        entry_v1.package_version = Some("1.0.188".to_string());
        let mut entry_v2 = corpus_entry("serde-1.0.200", "v1.0.200", &path_v2);
        entry_v2.revision_sha = Some(head_v2);
        entry_v2.package_version = Some("1.0.200".to_string());
        manifest.corpora.push(entry_v1);
        manifest.corpora.push(entry_v2);
        save(&root, &manifest).unwrap();
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert!(!result.partial);
        assert_eq!(result.hits.len(), 2);
        let v1_hit = result
            .hits
            .iter()
            .find(|hit| hit.corpus_id == "serde-1.0.188")
            .expect("v1 hit");
        let v2_hit = result
            .hits
            .iter()
            .find(|hit| hit.corpus_id == "serde-1.0.200")
            .expect("v2 hit");
        assert_eq!(v1_hit.revision, "v1.0.188");
        assert_eq!(v2_hit.revision, "v1.0.200");
        assert!(v1_hit.snippet.contains("v1"));
        assert!(v2_hit.snippet.contains("v2"));
        // Filtering by revision selects exactly one corpus.
        let opts = QueryOptions {
            revision: Some("v1.0.188".to_string()),
            ..Default::default()
        };
        let filtered = query(&root, "process", &opts).unwrap();
        assert_eq!(filtered.hits.len(), 1);
        assert_eq!(filtered.hits[0].corpus_id, "serde-1.0.188");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path_v1);
        let _ = std::fs::remove_dir_all(&path_v2);
    }

    #[test]
    fn conflicting_symbol_names_prove_source_attribution() {
        // Both corpora define `fn process()` — the same symbol name. Each
        // hit must carry the corpus it came from.
        let root = scratch("conflict");
        let path_a = scratch("conflict-a");
        let path_b = scratch("conflict-b");
        init_repo(&path_a);
        init_repo(&path_b);
        std::fs::write(path_a.join("lib.rs"), "fn process() { a(); }\n").unwrap();
        std::fs::write(path_b.join("lib.rs"), "fn process() { b(); }\n").unwrap();
        for path in [&path_a, &path_b] {
            git(path, &["add", "."]);
            git(
                path,
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "-m",
                    "content",
                ],
            );
        }
        let head_a = pixel_git::GitRunner::new(&path_a).rev_parse_head().unwrap();
        let head_b = pixel_git::GitRunner::new(&path_b).rev_parse_head().unwrap();
        let mut manifest = Manifest::default();
        let mut entry_a = corpus_entry("lib-a", "v1", &path_a);
        entry_a.revision_sha = Some(head_a);
        let mut entry_b = corpus_entry("lib-b", "v1", &path_b);
        entry_b.revision_sha = Some(head_b);
        manifest.corpora.push(entry_a);
        manifest.corpora.push(entry_b);
        save(&root, &manifest).unwrap();
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert_eq!(result.hits.len(), 2);
        let hit_a = result
            .hits
            .iter()
            .find(|hit| hit.corpus_id == "lib-a")
            .expect("lib-a hit");
        let hit_b = result
            .hits
            .iter()
            .find(|hit| hit.corpus_id == "lib-b")
            .expect("lib-b hit");
        assert!(hit_a.snippet.contains("a()"));
        assert!(hit_b.snippet.contains("b()"));
        // Scoped selection: querying one corpus by id returns only its hits.
        let opts = QueryOptions {
            id: Some("lib-a".to_string()),
            ..Default::default()
        };
        let scoped = query(&root, "process", &opts).unwrap();
        assert_eq!(scoped.hits.len(), 1);
        assert_eq!(scoped.hits[0].corpus_id, "lib-a");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path_a);
        let _ = std::fs::remove_dir_all(&path_b);
    }

    #[test]
    fn multi_corpus_failure_preserves_usable_results() {
        // One corpus is missing; the other still answers, and the coverage
        // report discloses the failure.
        let root = scratch("partial");
        let path = scratch("partial-path");
        init_repo(&path);
        std::fs::write(path.join("lib.rs"), "fn process() {}\n").unwrap();
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "content",
            ],
        );
        let head = pixel_git::GitRunner::new(&path).rev_parse_head().unwrap();
        let mut manifest = Manifest::default();
        let mut entry = corpus_entry("present", "v1", &path);
        entry.revision_sha = Some(head);
        manifest.corpora.push(entry);
        manifest
            .corpora
            .push(corpus_entry("absent", "v1", &root.join("missing")));
        save(&root, &manifest).unwrap();
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert!(result.partial);
        assert_eq!(result.hits.len(), 1, "the healthy corpus still answers");
        assert_eq!(result.hits[0].corpus_id, "present");
        assert_eq!(result.coverage.len(), 2);
        let absent = result
            .coverage
            .iter()
            .find(|c| c.corpus_id == "absent")
            .expect("absent coverage");
        assert!(!absent.ok);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn per_corpus_quota_caps_hits_and_discloses_it() {
        let root = scratch("quota");
        let path = scratch("quota-path");
        init_repo(&path);
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("fn process_{i}() {{}}\n"));
        }
        std::fs::write(path.join("lib.rs"), content).unwrap();
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "content",
            ],
        );
        let head = pixel_git::GitRunner::new(&path).rev_parse_head().unwrap();
        let mut manifest = Manifest::default();
        let mut entry = corpus_entry("serde", "v1", &path);
        entry.revision_sha = Some(head);
        manifest.corpora.push(entry);
        save(&root, &manifest).unwrap();
        let opts = QueryOptions {
            per_corpus_limit: 3,
            ..Default::default()
        };
        let result = query(&root, "process", &opts).unwrap();
        assert_eq!(result.hits.len(), 3, "per-corpus quota must cap hits");
        assert!(result.coverage[0].capped, "the cap must be disclosed");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn target_role_outranks_reference_in_fusion() {
        let root = scratch("fusion");
        let target_path = scratch("fusion-target");
        let ref_path = scratch("fusion-ref");
        init_repo(&target_path);
        init_repo(&ref_path);
        std::fs::write(target_path.join("lib.rs"), "fn process() {}\n").unwrap();
        std::fs::write(ref_path.join("lib.rs"), "fn process() {}\n").unwrap();
        for path in [&target_path, &ref_path] {
            git(path, &["add", "."]);
            git(
                path,
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "-m",
                    "content",
                ],
            );
        }
        let target_head = pixel_git::GitRunner::new(&target_path)
            .rev_parse_head()
            .unwrap();
        let ref_head = pixel_git::GitRunner::new(&ref_path)
            .rev_parse_head()
            .unwrap();
        let mut manifest = Manifest::default();
        let mut target = corpus_entry("my-project", "main", &target_path);
        target.role = CorpusRole::Target;
        target.revision_sha = Some(target_head);
        let mut reference = corpus_entry("dep", "v1", &ref_path);
        reference.revision_sha = Some(ref_head);
        manifest.corpora.push(reference);
        manifest.corpora.push(target);
        save(&root, &manifest).unwrap();
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert_eq!(result.hits.len(), 2);
        assert_eq!(
            result.hits[0].role, "target",
            "target-role corpora outrank reference material"
        );
        assert_eq!(result.hits[0].corpus_id, "my-project");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&target_path);
        let _ = std::fs::remove_dir_all(&ref_path);
    }

    #[test]
    fn setup_clones_and_pins_a_local_remote() {
        // A local "remote" repository acts as the source URL; setup clones
        // it, checks out the pinned revision and records the SHA.
        let root = scratch("setup");
        let remote = scratch("setup-remote");
        init_repo(&remote);
        std::fs::write(remote.join("lib.rs"), "fn process() {}\n").unwrap();
        git(&remote, &["add", "."]);
        git(
            &remote,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "v1",
            ],
        );
        let head = pixel_git::GitRunner::new(&remote).rev_parse_head().unwrap();
        let target = root.join("serde");
        let mut manifest = Manifest::default();
        manifest.corpora.push(CorpusEntry {
            id: "serde".to_string(),
            role: CorpusRole::Reference,
            source_url: remote.display().to_string(),
            revision: head.clone(),
            revision_sha: None,
            path: target.clone(),
            license: None,
            package: Some("serde".to_string()),
            package_version: None,
            provenance: None,
        });
        save(&root, &manifest).unwrap();
        let report = setup(&root, &["serde".to_string()]).unwrap();
        assert_eq!(report["setup"][0]["corpus_id"], "serde");
        assert!(target.join(".git").exists(), "setup must clone");
        assert!(target.join("lib.rs").exists());
        let corpora = list(&root).unwrap();
        assert_eq!(corpora[0].revision_sha.as_deref(), Some(head.as_str()));
        // The query now answers from the set-up corpus.
        let result = query(&root, "process", &QueryOptions::default()).unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].corpus_id, "serde");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote);
    }

    #[test]
    fn setup_reports_unknown_corpus() {
        let root = scratch("setup-unknown");
        let path = scratch("setup-unknown-path");
        init_repo(&path);
        let mut manifest = Manifest::default();
        manifest.corpora.push(corpus_entry("serde", "v1", &path));
        save(&root, &manifest).unwrap();
        let err = setup(&root, &["nope".to_string()]).unwrap_err();
        assert!(err.contains("unknown corpus"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn query_filters_by_package() {
        let root = scratch("pkgfilter");
        let path = scratch("pkgfilter-path");
        init_repo(&path);
        std::fs::write(path.join("lib.rs"), "fn process() {}\n").unwrap();
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "content",
            ],
        );
        let head = pixel_git::GitRunner::new(&path).rev_parse_head().unwrap();
        let mut manifest = Manifest::default();
        let mut entry = corpus_entry("serde-1.0.188", "v1.0.188", &path);
        entry.revision_sha = Some(head);
        entry.package = Some("serde".to_string());
        manifest.corpora.push(entry);
        save(&root, &manifest).unwrap();
        let opts = QueryOptions {
            package: Some("serde".to_string()),
            ..Default::default()
        };
        let result = query(&root, "process", &opts).unwrap();
        assert_eq!(result.hits.len(), 1);
        let opts = QueryOptions {
            package: Some("tokio".to_string()),
            ..Default::default()
        };
        let err = query(&root, "process", &opts).unwrap_err();
        assert!(err.contains("no corpora match"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn query_filters_by_role() {
        let root = scratch("rolefilter");
        let path = scratch("rolefilter-path");
        init_repo(&path);
        std::fs::write(path.join("lib.rs"), "fn process() {}\n").unwrap();
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "content",
            ],
        );
        let head = pixel_git::GitRunner::new(&path).rev_parse_head().unwrap();
        let mut manifest = Manifest::default();
        let mut entry = corpus_entry("my-project", "main", &path);
        entry.role = CorpusRole::Target;
        entry.revision_sha = Some(head);
        manifest.corpora.push(entry);
        save(&root, &manifest).unwrap();
        let opts = QueryOptions {
            role: Some(CorpusRole::Target),
            ..Default::default()
        };
        let result = query(&root, "process", &opts).unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].role, "target");
        let opts = QueryOptions {
            role: Some(CorpusRole::Reference),
            ..Default::default()
        };
        let err = query(&root, "process", &opts).unwrap_err();
        assert!(err.contains("no corpora match"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn manifest_format_is_v1() {
        assert_eq!(MANIFEST_FORMAT, 1);
    }
}
