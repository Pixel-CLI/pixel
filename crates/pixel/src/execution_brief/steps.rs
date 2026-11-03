// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Brief steps: a brief probe written in Rust.
//!
//! A probe (`probes/<name>/`) is a script the plugin host starts for every
//! prompt: an interpreter boots, `git ls-files` and `rg` run, and what it
//! prints is merged into the brief. A step is the same rule as a function of
//! the same request, run inside the brief's process: no interpreter, one
//! `git ls-files` shared by every step, and the same tests as any other code.
//!
//! A step is a [`Step`]: when it applies (`when_kinds`, `when_prompt`,
//! `skip_kinds`, `when_gate_closed`, `budget_ms` and `mode` mean what they do
//! in a probe's `[brief-probe]` table) and a `run` that reads a [`Ctx`] and
//! returns what the script would have printed: `### header` blocks, then
//! lines, and the `drop:` / `first:` directives. The text goes through the
//! same [`merge`](super::probes::merge) as a script's, so the output rules
//! (cap, order, a header already held is skipped) are one specification.
//!
//! To add a step: write `steps/<name>.rs` with a `pub(super) const STEP:
//! Step`, and list the module in the `register!` call below. A step with the
//! name of an installed user-tier probe replaces it: the host does not start
//! the script (`probes::apply`), so the brief never runs both.
//!
//! A step must not panic (the release build aborts on a panic, which would
//! end the hook) and must stop at [`Ctx::timed_out`]: it shares the process,
//! so the brief cannot kill it the way it kills a script at its budget.

use std::cell::OnceCell;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

use pixel_git::{ENUMERATION_MAX_OUTPUT_BYTES, GitOptions, GitRunner};
use pixel_plugin::{ProbeMode, ProbeSpec};

/// Declares the step modules and lists them in [`ALL`]: one name per step.
macro_rules! register {
    ($($module:ident),* $(,)?) => {
        $(mod $module;)*

        /// Every step.
        pub(crate) const ALL: &[&Step] = &[$(&$module::STEP),*];
    };
}

/// Declares the modules of the steps that run after the others (`stage =
/// "post"` in the manifest of their probe) and lists them in [`POST`].
macro_rules! register_post {
    ($($module:ident),* $(,)?) => {
        $(mod $module;)*

        /// Every step of the post stage: each runs on the brief the others
        /// made, one after the other (`probes::apply`).
        pub(crate) const POST: &[&Step] = &[$(&$module::STEP),*];
    };
}

register!(
    cand_j_chain,
    test_bodies,
    workspace_overview,
    overview_answer_evidence,
    member_manifest,
    doc_sections,
    name_files,
    flow_rank,
    cand_j_stop_cause,
    history_fallback,
    cand_k_commit_patch,
    rank_blocks,
    config_registry,
    cand_j_callers,
    cand_k_code_sites,
    error_origin,
);

mod graph;
/// The string rules the steps share, spelled the way Python spells them.
mod text;
// Not steps: what two of them share.
mod commit_lookup;

register_post!(prune_noise);
mod walk;

/// A brief probe in Rust.
pub(crate) struct Step {
    /// The name of the probe it replaces (`probes/<name>/`).
    pub(crate) name: &'static str,
    /// Applies to these question kinds; with `when_prompt`, either is enough.
    pub(crate) when_kinds: &'static [&'static str],
    /// Never applies to these kinds.
    pub(crate) skip_kinds: &'static [&'static str],
    /// Applies when the prompt matches this regex (searched, not anchored).
    pub(crate) when_prompt: Option<&'static str>,
    /// Applies when the relevance gate is closed (needs `when_prompt`).
    pub(crate) when_gate_closed: bool,
    /// Wall time the step gets, inside what is left of the brief's window.
    pub(crate) budget_ms: u64,
    /// Where its blocks go relative to the brief's own.
    pub(crate) mode: ProbeMode,
    /// The step: the blocks and directives it adds to the brief, or `""`.
    pub(crate) run: fn(&Ctx) -> String,
}

impl Step {
    /// The `[brief-probe]` table of the same rule.
    pub(crate) fn spec(&self) -> ProbeSpec {
        let owned = |items: &[&str]| items.iter().map(ToString::to_string).collect();
        ProbeSpec {
            owner: "pixel".to_owned(),
            when_kinds: owned(self.when_kinds),
            skip_kinds: owned(self.skip_kinds),
            when_prompt: self.when_prompt.map(ToString::to_string),
            when_gate_closed: self.when_gate_closed,
            budget_ms: self.budget_ms,
            mode: self.mode,
        }
    }

    /// Whether the step runs for a brief of `kind` asked `prompt`.
    pub(crate) fn applies(&self, kind: Option<&str>, prompt: &str, gate_closed: bool) -> bool {
        (!gate_closed || self.when_gate_closed) && self.spec().applies(kind, prompt)
    }
}

/// What a step is given: the request a probe gets on stdin, and the files of
/// the repository, read through the brief's deadline.
pub(crate) struct Ctx<'a> {
    /// What the user typed.
    pub(crate) prompt: &'a str,
    /// The brief the step adds to (it opens on `[PIXEL:BRIEF]`).
    pub(crate) brief: &'a str,
    /// The question kind the brief settled on; `None` when it did not.
    pub(crate) kind: Option<&'a str>,
    root: &'a Path,
    deadline: Instant,
    tracked: OnceCell<Option<Vec<String>>>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(root: &'a Path, prompt: &'a str, brief: &'a str, deadline: Instant) -> Self {
        Self {
            prompt,
            brief,
            kind: None,
            root,
            deadline,
            tracked: OnceCell::new(),
        }
    }

    /// The same request, for a brief of question kind `kind`.
    pub(crate) fn with_kind(self, kind: Option<&'a str>) -> Self {
        Self { kind, ..self }
    }

    /// The repository's working directory.
    /// The repository the step reads.
    pub(crate) fn root(&self) -> &Path {
        self.root
    }

    /// Whether the step's time is up: it returns what it has, or nothing.
    pub(crate) fn timed_out(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// The tracked files, listed once for every step of the brief; `None`
    /// when git cannot list them in time.
    pub(crate) fn files(&self) -> Option<&[String]> {
        self.tracked
            .get_or_init(|| {
                let left = self.deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                let git = GitRunner::with_options(
                    self.root,
                    GitOptions {
                        timeout: Some(left),
                        max_output_bytes: Some(ENUMERATION_MAX_OUTPUT_BYTES),
                    },
                );
                // `-z` lists a name as it is, not quoted; a name that is not
                // UTF-8 cannot be a path of a brief block and is left out.
                let out = git.run_isolated(&["ls-files", "-z"]).ok()?;
                Some(
                    out.split(|byte| *byte == 0)
                        .filter_map(|name| std::str::from_utf8(name).ok())
                        .filter(|name| !name.is_empty())
                        .map(ToString::to_string)
                        .collect(),
                )
            })
            .as_deref()
    }

    /// The text of the tracked file `path`, lossily decoded. `None` when it is
    /// not a regular file (a link is never followed, a pipe never opened for
    /// good), cannot be read, or holds more than `limit` bytes.
    pub(crate) fn read(&self, path: &str, limit: u64) -> Option<String> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(open_flags())
            .open(self.root.join(path))
            .ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut raw = Vec::new();
        (&mut file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut raw)
            .ok()?;
        (u64::try_from(raw.len()).ok()? <= limit)
            .then(|| String::from_utf8_lossy(&raw).into_owned())
    }
}

/// How a step opens a file: never through a link, never blocking on a pipe.
// The two flags are disjoint bits, so `|` and `^` yield the same value: no test can tell them apart.
#[cfg_attr(test, mutants::skip)]
const fn open_flags() -> i32 {
    libc::O_NOFOLLOW | libc::O_NONBLOCK
}

#[cfg(test)]
pub(crate) mod testing {
    //! What the tests of a step share: a real git repository in a temp dir
    //! (the steps wrap `git ls-files`, so nothing is mocked), and a step run
    //! on it the way the brief runs it.

    use super::*;
    use std::process::Command;
    use std::time::Duration;

    /// A repository whose index holds `files` (`path`, `text`).
    pub(crate) fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let file = dir.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["add", "."]);
        dir
    }

    pub(crate) fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// What `step` prints for `prompt` in `root`, the brief being `brief`.
    pub(crate) fn run(step: &Step, root: &Path, prompt: &str, brief: &str) -> String {
        run_kind(step, root, prompt, brief, None)
    }

    /// [`run`] for a brief of question kind `kind`.
    pub(crate) fn run_kind(
        step: &Step,
        root: &Path,
        prompt: &str,
        brief: &str,
        kind: Option<&str>,
    ) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        (step.run)(&Ctx::new(root, prompt, brief, deadline).with_kind(kind))
    }

    /// Commits one more line of `source.py` (`feature = True`) with `subject`
    /// and `body` as the message, and returns the new commit.
    pub(crate) fn commit_line(root: &Path, subject: &str, body: &str) -> String {
        let source = root.join("source.py");
        let before = std::fs::read_to_string(&source).unwrap_or_default();
        std::fs::write(&source, format!("{before}feature = True\n")).unwrap();
        git(root, &["add", "source.py"]);
        commit_staged(root, subject, body)
    }

    /// Commits what is staged, as a fixed author, and returns the new commit.
    pub(crate) fn commit_staged(root: &Path, subject: &str, body: &str) -> String {
        git(
            root,
            &[
                "-c",
                "user.name=Probe Test",
                "-c",
                "user.email=probe@example.invalid",
                "commit",
                "-q",
                "-m",
                subject,
                "-m",
                body,
            ],
        );
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8(head.stdout).unwrap().trim().to_owned()
    }

    /// The abbreviated id of the commit `HEAD` names in `root`.
    pub(crate) fn short_head(root: &Path) -> String {
        let head = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8(head.stdout).unwrap().trim().to_owned()
    }

    /// What the brief's probe stage makes of `brief` for a `kind` question in
    /// `root`, with a script named like the step installed in a throwaway home
    /// (it prints `### script.rs:1-1 — the script ran`): the merged brief, and
    /// the `(name, outcome)` of each run. Proves a step is registered and that
    /// it takes the script's place.
    pub(crate) fn applied(
        name: &str,
        root: &Path,
        kind: &str,
        prompt: &str,
        brief: &str,
    ) -> (Option<String>, Vec<(String, &'static str)>) {
        use super::super::probes::{self, Stage};
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".pixel/plugins").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pixel-plugin.toml"),
            format!(
                "name = \"{name}\"\nversion = \"0.1.0\"\napi = 1\ndescription = \"d\"\nrun = \"probe\"\ncapabilities = [\"brief-probe\"]\n\n[brief-probe]\nowner = \"o\"\nbudget_ms = 400\n"
            ),
        )
        .unwrap();
        let script = dir.join("probe");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null\nprintf '### script.rs:1-1 — the script ran\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let host = pixel_plugin::Host {
            home: tmp.path().to_path_buf(),
            repo_root: None,
            path: std::ffi::OsString::new(),
            github_base: None,
        };
        let stage = Stage {
            host: &host,
            root,
            prompt,
            kind: Some(kind),
            gate_closed: false,
            deadline: Instant::now() + Duration::from_secs(10),
            cap: 3 * 9_800,
            max_bytes: None,
        };
        let (merged, runs) = probes::apply(&stage, brief);
        (
            merged,
            runs.into_iter()
                .map(|run| (run.name, run.outcome))
                .collect(),
        )
    }

    /// The outcomes of the runs of `name` in the runs [`applied`] gives. Steps
    /// that apply to every brief (`rank-blocks`, `prune-noise`) run beside it.
    pub(crate) fn outcomes_of(runs: &[(String, &'static str)], name: &str) -> Vec<&'static str> {
        runs.iter()
            .filter(|(run, _)| run == name)
            .map(|(_, outcome)| *outcome)
            .collect()
    }

    /// The `### ` lines of an output.
    pub(crate) fn headers(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|line| line.starts_with("### "))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn every_step_applies_exactly_where_the_manifest_of_its_probe_does() {
        let probes = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../probes");
        let staged = ALL
            .iter()
            .map(|step| (*step, false))
            .chain(POST.iter().map(|step| (*step, true)));
        for (step, post) in staged {
            let manifest = probes.join(step.name).join("pixel-plugin.toml");
            // A probe deleted after its port has nothing left to match.
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let doc: toml_edit::DocumentMut = text.parse().unwrap();
            let table = &doc["brief-probe"];
            let strings = |key: &str| -> Vec<String> {
                table
                    .get(key)
                    .and_then(|item| item.as_array())
                    .map_or_else(Vec::new, |array| {
                        array
                            .iter()
                            .filter_map(|item| item.as_str().map(ToString::to_string))
                            .collect()
                    })
            };
            let spec = step.spec();
            assert_eq!(spec.when_kinds, strings("when_kinds"), "{}", step.name);
            assert_eq!(spec.skip_kinds, strings("skip_kinds"), "{}", step.name);
            assert_eq!(
                spec.when_prompt.as_deref(),
                table.get("when_prompt").and_then(|item| item.as_str()),
                "{}",
                step.name
            );
            assert_eq!(
                spec.when_gate_closed,
                table
                    .get("when_gate_closed")
                    .and_then(toml_edit::Item::as_bool)
                    .unwrap_or(false),
                "{}",
                step.name
            );
            assert_eq!(
                i64::try_from(spec.budget_ms).ok(),
                table.get("budget_ms").and_then(toml_edit::Item::as_integer),
                "{}",
                step.name
            );
            let mode = table.get("mode").and_then(|item| item.as_str());
            assert_eq!(
                spec.mode == ProbeMode::Lead,
                mode == Some("lead"),
                "{}",
                step.name
            );
            assert_eq!(table["owner"].as_str(), Some("pixel"), "{}", step.name);
            assert_eq!(
                table.get("stage").and_then(|item| item.as_str()),
                post.then_some("post"),
                "{}",
                step.name
            );
        }
    }

    #[test]
    fn a_step_never_runs_for_a_closed_gate_unless_it_says_so() {
        let open = Step {
            name: "x",
            when_kinds: &["tests"],
            skip_kinds: &["history"],
            when_prompt: None,
            when_gate_closed: false,
            budget_ms: 100,
            mode: ProbeMode::Append,
            run: |_| String::new(),
        };
        assert!(open.applies(Some("tests"), "p", false));
        assert!(!open.applies(Some("tests"), "p", true));
        assert!(!open.applies(Some("history"), "p", false));
        assert!(!open.applies(Some("flow"), "p", false));
        let closed = Step {
            when_prompt: Some("tests"),
            when_gate_closed: true,
            ..open
        };
        assert!(closed.applies(None, "which tests", true));
        assert!(!closed.applies(None, "which spans", true));
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "abc").unwrap();
        std::os::unix::fs::symlink(dir.path().join("a.txt"), dir.path().join("link.txt")).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        dir
    }

    fn ctx(root: &Path, ms: u64) -> Ctx<'_> {
        Ctx::new(root, "p", "b", Instant::now() + Duration::from_millis(ms))
    }

    #[test]
    fn read_returns_a_regular_file_within_the_limit_and_nothing_else() {
        let dir = fixture();
        let ctx = ctx(dir.path(), 5_000);
        assert_eq!(ctx.read("a.txt", 3).as_deref(), Some("abc"));
        assert_eq!(ctx.read("a.txt", 2), None, "one byte over the limit");
        assert_eq!(ctx.read("link.txt", 100), None, "a link is not followed");
        assert_eq!(ctx.read("sub", 100), None, "a directory is not a file");
        assert_eq!(ctx.read("absent.txt", 100), None);
    }

    #[test]
    fn files_lists_the_tracked_names_once_and_not_the_untracked_ones() {
        let dir = testing::repo(&[("tracked.rs", "x"), ("é.rs", "x")]);
        let root = dir.path();
        std::fs::write(root.join("loose.rs"), "x").unwrap();
        let ctx = ctx(root, 5_000);
        let mut files = ctx.files().unwrap().to_vec();
        files.sort();
        assert_eq!(files, ["tracked.rs", "é.rs"]);
        std::fs::remove_file(root.join("tracked.rs")).unwrap();
        assert_eq!(ctx.files().unwrap().len(), 2, "listed once per brief");
    }

    #[test]
    fn a_request_carries_the_kind_of_the_question_when_it_is_given_one() {
        let dir = fixture();
        assert_eq!(ctx(dir.path(), 5_000).kind, None);
        let asked = ctx(dir.path(), 5_000).with_kind(Some("flow"));
        assert_eq!(
            (asked.kind, asked.prompt, asked.brief),
            (Some("flow"), "p", "b")
        );
    }

    #[test]
    fn a_step_out_of_time_lists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path(), "p", "b", Instant::now());
        assert!(ctx.timed_out());
        assert!(ctx.files().is_none());
        let ahead = self::ctx(dir.path(), 5_000);
        assert!(!ahead.timed_out());
    }

    /// How `scripts/bench/step-parity.py` runs a step on the request a probe
    /// gets on stdin: the step named by `PIXEL_STEP`, its output between two
    /// marker lines.
    #[test]
    #[ignore = "driven by scripts/bench/step-parity.py"]
    fn print_the_output_of_the_step_named_by_the_environment() {
        let name = std::env::var("PIXEL_STEP").unwrap();
        let step = ALL
            .iter()
            .chain(POST)
            .find(|step| step.name == name)
            .unwrap();
        let request: serde_json::Value = serde_json::from_reader(std::io::stdin()).unwrap();
        let text = |key: &str| request[key].as_str().unwrap_or_default().to_owned();
        let (root, prompt, brief) = (text("repo_root"), text("prompt"), text("brief"));
        let kind = request["kind"].as_str().map(ToOwned::to_owned);
        let ctx = Ctx::new(
            Path::new(&root),
            &prompt,
            &brief,
            Instant::now() + Duration::from_millis(step.budget_ms),
        )
        .with_kind(kind.as_deref());
        print!("<<<STEP\n{}STEP>>>\n", (step.run)(&ctx));
    }
}
