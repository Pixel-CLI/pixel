// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Byte-level goldens of every file `pixel setup` writes, one per agent.
//!
//! Each test picks one agent of the wizard (`--selected-agents=<n>`, the
//! default features `1,2,3`) on the same fixture: a home where every harness
//! is already configured, each with one file of the user's own, and a git
//! repository as the working directory. It then runs setup a second time and
//! `pixel setup --remove`, and compares what changed with
//! `tests/setup/goldens/<agent>.txt`: every new or changed path with its mode
//! and bytes, every backup written beside a changed file, and what `--remove`
//! leaves. A harness that was not picked appears in no golden, so a write to
//! it fails the comparison.
//!
//! On top of the golden, the contract a golden cannot state alone:
//!
//! - the second run changes nothing, not even a new backup;
//! - every backup `--remove` leaves is one its report names, and no other file
//!   still calls the pixel binary.
//!
//! What varies between runs is replaced, never ignored: the fixture paths
//! (`{HOME}`, `{REPO}`), the binary the hooks call (`{PIXEL_BIN}`), the
//! nanosecond stamp of a backup name (`{STAMP1}`, in time order) and the crate
//! version (`{VERSION}`). A mode is the permission bits under `0o755`: the
//! group and other write bits follow the umask, not setup. A file whose bytes
//! are a file of this repository (a bundled probe, the skill golden) is
//! written as `= <path>` and compared byte for byte with it, and so is a file
//! every agent writes alike (`shared/`) or one already shown above it.
//!
//! Regenerate after an intended change, then read the diff:
//! `PIXEL_SETUP_UPDATE_GOLDENS=1 cargo nextest run -p pixel-cli -E 'test(/setup_goldens_cli::/)'`.
//!
//! TEMPORARY (dev-flags): this drives the wizard with `--selected-agents` and
//! `--selected-features`. When they are removed, port these tests to the
//! wizard's typed answers instead of deleting them: the goldens outlive the
//! flags.

use crate::support::{Scratch, git, pixel_command};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The user's own files in the fixture home: every harness is configured, so
/// each agent's conditional steps run and its rewrites leave a backup.
const SEED: &[(&str, &str)] = &[
    (".claude/settings.json", "{\"theme\":\"dark\"}\n"),
    (".codex/config.toml", "model = \"o3\"\n"),
    (".config/devin/config.json", "{\"model\":\"swe-1\"}\n"),
    (
        ".config/opencode/opencode.json",
        "{\"theme\":\"opencode\"}\n",
    ),
    (".cursor/cli-config.json", "{\"editor\":\"vim\"}\n"),
    (".gemini/config/config.json", "{\"user\":\"kept\"}\n"),
    (".gemini/settings.json", "{\"theme\":\"Default\"}\n"),
    (".pi/agent/settings.json", "{\"defaultModel\":\"sonnet\"}\n"),
];

/// The default feature selection: prompt, metrics, daemon.
const FEATURES: &str = "--selected-features=1,2,3";

/// Where the goldens live, relative to the repository root.
const GOLDENS: &str = "tests/setup/goldens";

/// Files every agent's run writes alike, kept once under `<GOLDENS>/shared/`.
const SHARED: &[(&str, &str)] = &[("home/.pixel/config.yaml", "config.yaml")];

/// One file or empty directory under the fixture root.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File { mode: u32, bytes: Vec<u8> },
    EmptyDir,
}

type Snapshot = BTreeMap<String, Entry>;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/pixel has two parents")
        .to_path_buf()
}

/// Every file and empty directory under `root`, by `/`-joined relative path.
/// The repository's `.git/` and the CLI's own action log are not setup's.
fn snapshot(root: &Path) -> Snapshot {
    let mut entries = Snapshot::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let mut children = fs::read_dir(&dir).unwrap().peekable();
        if children.peek().is_none() && dir != root {
            entries.insert(relative(root, &dir), Entry::EmptyDir);
        }
        for child in children {
            let path = child.unwrap().path();
            let rel = relative(root, &path);
            if rel == "repo/.git" || rel == "repo/.pixel/actions.jsonl" {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            assert!(!meta.is_symlink(), "setup wrote a symlink at {rel}");
            if meta.is_dir() {
                pending.push(path);
            } else {
                let mode = meta.permissions().mode() & 0o755;
                let bytes = fs::read(&path).unwrap();
                entries.insert(rel, Entry::File { mode, bytes });
            }
        }
    }
    entries
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap();
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Run the built binary with nothing from the developer's environment: no
/// `XDG_CONFIG_HOME`, `CODEX_HOME` or `PI_CODING_AGENT_DIR` moving a harness
/// directory, no agent marker, and a `PATH` with no `pi` or `agy` to call.
fn run(root: &Path, args: &[&str]) -> String {
    let empty_path = root.join("empty-path");
    fs::create_dir_all(&empty_path).unwrap();
    let out = pixel_command()
        .env_clear()
        .env("HOME", root.join("home"))
        .env("PATH", &empty_path)
        .env("TMPDIR", std::env::temp_dir())
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .env("NO_COLOR", "1")
        .current_dir(root.join("repo"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "pixel {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::remove_dir(&empty_path).unwrap();
    String::from_utf8(out.stdout).unwrap()
}

/// The fixture root, with the seeded home and a one-commit repository.
fn fixture(tag: &str) -> Scratch {
    let root = Scratch::for_test("setup-goldens", tag);
    for (path, content) in SEED {
        let file = root.join("home").join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
    }
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("README.md"), "# fixture\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    root
}

/// The text replacements of one run, in the order they apply.
struct Normaliser(Vec<(String, String)>);

impl Normaliser {
    fn new(root: &Path, phases: &[&Snapshot]) -> Self {
        let bin = Path::new(env!("CARGO_BIN_EXE_pixel"));
        let mut pairs = vec![
            (
                bin.canonicalize().unwrap().display().to_string(),
                "{PIXEL_BIN}".into(),
            ),
            (bin.display().to_string(), "{PIXEL_BIN}".into()),
            (root.join("home").display().to_string(), "{HOME}".into()),
            (root.join("repo").display().to_string(), "{REPO}".into()),
        ];
        // Backup stamps (`<nanos>-<seq>`) in time order: the run's first
        // backup is `{STAMP1}` in every phase that still holds it.
        let stamps: BTreeSet<(u128, u64, String)> = phases
            .iter()
            .flat_map(|phase| phase.keys())
            .filter_map(|path| path.split(".pixel-bak.").nth(1))
            .map(|rest| rest.split('/').next().unwrap())
            .map(|stamp| {
                let (nanos, seq) = stamp.split_once('-').expect("<nanos>-<seq>");
                (
                    nanos.parse().unwrap(),
                    seq.parse().unwrap(),
                    stamp.to_string(),
                )
            })
            .collect();
        for (index, (_, _, stamp)) in stamps.into_iter().enumerate() {
            pairs.push((
                format!(".pixel-bak.{stamp}"),
                format!(".pixel-bak.{{STAMP{}}}", index + 1),
            ));
        }
        // Receipts hash deployed bytes that contain fixture paths. Pin each hash to
        // the actual file it authenticates, then normalize the path like other fields.
        let mut hashes = BTreeSet::new();
        for phase in phases {
            for (path, entry) in *phase {
                if let Entry::File { bytes, .. } = entry {
                    let digest = Sha256::digest(bytes)
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    if hashes.insert(digest.clone()) {
                        pairs.insert(0, (digest, format!("{{SHA256:{path}}}")));
                    }
                }
            }
        }
        pairs.push((env!("CARGO_PKG_VERSION").into(), "{VERSION}".into()));
        Self(pairs)
    }

    fn apply(&self, text: &str) -> String {
        self.0
            .iter()
            .fold(text.to_string(), |text, (from, to)| text.replace(from, to))
    }
}

/// Bytes of the repository files a deployed file may copy verbatim.
fn repository_sources() -> BTreeMap<Vec<u8>, String> {
    let root = repo_root();
    let mut sources = BTreeMap::new();
    let shared = format!("{GOLDENS}/shared");
    for dir in ["probes", "tests/setup/.agents", shared.as_str()] {
        for (path, entry) in snapshot(&root.join(dir)) {
            if let Entry::File { bytes, .. } = entry {
                sources.entry(bytes).or_insert(format!("{dir}/{path}"));
            }
        }
    }
    sources
}

/// The paths of `after` that are new or differ from `before`, and the paths
/// of `before` that are gone, rendered for the golden. A file whose bytes are
/// in `sources` (a repository file, or one already rendered) is a reference.
fn render(
    section: &str,
    before: &Snapshot,
    after: &Snapshot,
    names: &Normaliser,
    sources: &mut BTreeMap<Vec<u8>, String>,
) -> String {
    let mut out = format!("\n## {section}\n");
    for (path, entry) in after {
        if before.get(path) == Some(entry) {
            continue;
        }
        let path = names.apply(path);
        match entry {
            Entry::EmptyDir => out.push_str(&format!("--- {path}/ (empty directory)\n")),
            Entry::File { mode, bytes } => {
                out.push_str(&format!("--- {path} {mode:04o}"));
                if let Some(source) = sources.get(bytes) {
                    out.push_str(&format!(" = {source}\n"));
                    continue;
                }
                sources.insert(bytes.clone(), format!("{path} ({section})"));
                let text = std::str::from_utf8(bytes).expect("setup writes text files");
                out.push('\n');
                out.push_str(&names.apply(text));
                if !text.ends_with('\n') {
                    out.push_str("\n\\ no newline at end of file\n");
                }
            }
        }
    }
    for path in before.keys().filter(|path| !after.contains_key(*path)) {
        out.push_str(&format!("--- {} (removed)\n", names.apply(path)));
    }
    out
}

/// The backups `--remove` names in its report, as fixture-relative paths.
fn reported_backups(report: &str, root: &Path) -> BTreeSet<String> {
    let report: serde_json::Value = serde_json::from_str(report).unwrap();
    let step = report["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["id"] == "backups")
        .expect("the remove report has a backups step");
    let detail = step["detail"].as_str().unwrap_or_default();
    detail
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(|path| relative(root, Path::new(path)))
        .collect()
}

/// Every backup in `snapshot`: the path up to the component that names it.
fn backups_in(snapshot: &Snapshot) -> BTreeSet<String> {
    snapshot
        .keys()
        .filter_map(|path| {
            let stamp = path.find(".pixel-bak.")?;
            let end = path[stamp..].find('/').map_or(path.len(), |i| stamp + i);
            Some(path[..end].to_string())
        })
        .collect()
}

fn check(index: usize, slug: &str, name: &str) {
    let root = fixture(slug);
    let seeded = snapshot(&root);
    let agents = format!("--selected-agents={index}");

    run(&root, &["setup", &agents, FEATURES]);
    let installed = snapshot(&root);
    run(&root, &["setup", &agents, FEATURES]);
    assert_eq!(
        snapshot(&root),
        installed,
        "{name}: a second setup must change nothing, not even write a backup"
    );

    let home = root.join("home");
    let absent_binary = home.join(".local/bin/pixel");
    let report = run(
        &root,
        &[
            "setup",
            "--remove",
            "--binary-path",
            absent_binary.to_str().unwrap(),
            "--json",
        ],
    );
    let removed = snapshot(&root);
    assert_eq!(
        backups_in(&removed),
        reported_backups(&report, &root),
        "{name}: every backup left behind is one the remove report tells the user about"
    );
    let without_backups: Snapshot = removed
        .iter()
        .filter(|(path, _)| !path.contains(".pixel-bak."))
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect();
    assert_eq!(
        without_backups, seeded,
        "{name}: removal returns the HOME byte for byte, except reported backups"
    );
    let bin = Path::new(env!("CARGO_BIN_EXE_pixel"))
        .canonicalize()
        .unwrap();
    let calls_pixel = |entry: &Entry| match entry {
        Entry::File { bytes, .. } => {
            String::from_utf8_lossy(bytes).contains(&*bin.to_string_lossy())
        }
        Entry::EmptyDir => false,
    };
    let still_wired: Vec<&String> = removed
        .iter()
        .filter(|(path, entry)| !path.contains(".pixel-bak.") && calls_pixel(entry))
        .map(|(path, _)| path)
        .collect();
    assert!(
        still_wired.is_empty(),
        "{name}: after --remove nothing but a backup may still call pixel: {still_wired:?}"
    );

    let update = std::env::var_os("PIXEL_SETUP_UPDATE_GOLDENS").is_some();
    let goldens = repo_root().join(GOLDENS);
    if update {
        fs::create_dir_all(goldens.join("shared")).unwrap();
        for (path, file) in SHARED {
            if let Some(Entry::File { bytes, .. }) = installed.get(*path) {
                // Every test writes it: a rename keeps a reader from a torn copy.
                let tmp = goldens.join(format!(".{file}.{slug}"));
                fs::write(&tmp, bytes).unwrap();
                fs::rename(tmp, goldens.join("shared").join(file)).unwrap();
            }
        }
    }
    let names = Normaliser::new(&root, &[&installed, &removed]);
    let sources = &mut repository_sources();
    let setup = render("after setup, twice", &seeded, &installed, &names, sources);
    let remove = render("after setup --remove", &seeded, &removed, &names, sources);
    let actual = format!(
        "# `pixel setup {agents} {FEATURES}` ({name}), then `pixel setup --remove`,\n\
         # on the fixture of crates/pixel/tests/cli/setup_goldens_cli.rs.\n\
         # Paths are relative to the fixture root; see the module doc to regenerate.\n\
         {setup}{remove}"
    );
    let golden = goldens.join(format!("{slug}.txt"));
    if update {
        fs::write(&golden, &actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&golden).unwrap_or_else(|err| {
        panic!(
            "missing golden {}: {err}; regenerate with PIXEL_SETUP_UPDATE_GOLDENS=1",
            golden.display()
        )
    });
    assert!(
        expected == actual,
        "{name}: what setup writes moved from {}; if intended, regenerate with \
         PIXEL_SETUP_UPDATE_GOLDENS=1 and review the diff.\n--- actual ---\n{actual}",
        golden.display()
    );
}

/// One test per agent, by its 1-based `--selected-agents` index.
macro_rules! goldens {
    ($($test:ident: $index:literal, $slug:literal, $name:literal;)*) => {$(
        #[test]
        fn $test() {
            check($index, $slug, $name);
        }
    )*};
}

goldens! {
    claude_code_setup_and_remove_match_the_golden: 1, "claude-code", "Claude Code";
    codex_setup_and_remove_match_the_golden: 2, "codex", "Codex";
    devin_setup_and_remove_match_the_golden: 3, "devin", "Devin";
    cursor_setup_and_remove_match_the_golden: 4, "cursor", "Cursor";
    gemini_setup_and_remove_match_the_golden: 5, "gemini", "Gemini CLI";
    antigravity_setup_and_remove_match_the_golden: 6, "antigravity", "Antigravity";
    opencode_setup_and_remove_match_the_golden: 7, "opencode", "OpenCode";
    pi_setup_and_remove_match_the_golden: 8, "pi", "pi";
    agents_md_setup_and_remove_match_the_golden: 9, "agents-md", "AGENTS.md";
}

#[test]
fn remove_should_keep_and_report_files_edited_after_setup_without_touching_a_preview() {
    let root = fixture("edited");
    run(&root, &["setup", "--selected-agents=1", FEATURES]);
    let paths = [".pixel/config.yaml", ".claude/skills/pixel/SKILL.md"];
    for path in paths {
        fs::write(root.join("home").join(path), "my edited file\n").unwrap();
    }
    let before = snapshot(&root);
    run(&root, &["setup", "--remove", "--dry-run", "--json"]);
    assert_eq!(
        snapshot(&root),
        before,
        "preview changes no bytes or directories"
    );
    let report = run(
        &root,
        &[
            "setup",
            "--remove",
            "--binary-path",
            root.join("absent-pixel").to_str().unwrap(),
            "--json",
        ],
    );
    let report: serde_json::Value = serde_json::from_str(&report).unwrap();
    for path in paths {
        let file = root.join("home").join(path);
        assert_eq!(fs::read_to_string(&file).unwrap(), "my edited file\n");
        assert!(
            report["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["status"] == "yellow"
                    && step["detail"] == file.to_str().unwrap()
                    && step["summary"] == "kept a file changed after setup"),
            "{report}"
        );
    }
}

#[test]
fn remove_should_restore_a_preexisting_skill_and_name_its_recovery_copies() {
    let root = fixture("preexisting-skill");
    let skill = root.join("home/.claude/skills/pixel/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(&skill, "my own skill\n").unwrap();
    let original = snapshot(&root);
    run(&root, &["setup", "--selected-agents=1", FEATURES]);
    let report = run(
        &root,
        &[
            "setup",
            "--remove",
            "--binary-path",
            root.join("absent-pixel").to_str().unwrap(),
            "--json",
        ],
    );
    let removed = snapshot(&root);
    assert_eq!(backups_in(&removed), reported_backups(&report, &root));
    let without_backups: Snapshot = removed
        .into_iter()
        .filter(|(path, _)| !path.contains(".pixel-bak."))
        .collect();
    assert_eq!(without_backups, original);
}

#[test]
fn remove_should_keep_preexisting_empty_directories_and_user_yaml_bytes() {
    let root = fixture("preexisting-empty");
    let home = root.join("home");
    fs::create_dir_all(home.join(".claude/skills/pixel")).unwrap();
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::write(
        home.join(".pixel/config.yaml"),
        "# my choices\nmetrics: 'off'\n",
    )
    .unwrap();
    let original = snapshot(&root);
    run(&root, &["setup", "--selected-agents=1", FEATURES]);
    let report = run(
        &root,
        &[
            "setup",
            "--remove",
            "--binary-path",
            root.join("absent-pixel").to_str().unwrap(),
            "--json",
        ],
    );
    let removed = snapshot(&root);
    assert_eq!(backups_in(&removed), reported_backups(&report, &root));
    let without_backups: Snapshot = removed
        .into_iter()
        .filter(|(path, _)| !path.contains(".pixel-bak."))
        .collect();
    assert_eq!(without_backups, original);
}
