// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Pinned-history precision harness for `pixel scope-task`.
//!
//! Every case is a real commit of this repository: the task text an agent
//! would see (the commit message, except for the session case which keeps
//! the wording that exposed the failure), and the `crates/**/*.rs` files the
//! commit actually changed — the labels. Each probe runs in a git worktree
//! at the commit's PARENT, so the engine searches the tree as it was before
//! the change: the label's new bytes are not in the index (leakage control)
//! while the label path exists and is findable.
//!
//! Macro-averaged metrics over the corpus:
//!   - `recall@P0`     labels present in tier P0 (per case: |L∩P0| / |L|)
//!   - `recall@P1`     labels present in P0 or P1
//!   - `precision@P0`  P0 entries that are labels
//!   - `precision@1`   the first target in report order is a label (the
//!     report is tier-ordered, so this is the head a caller sees, not the
//!     highest raw score)
//!
//! This is `#[ignore]`d: it needs the full git history (a shallow clone has
//! neither the pinned commits nor their parents) and a few minutes of wall
//! time, and it is deliberately not part of the CI gates. Run:
//!
//! ```console
//! cargo test -p pixel-cli --test cli scope_task_precision -- --ignored --nocapture
//! ```
//!
//! Baseline measured 2026-09-16 before the path/symbol/tier fixes (41 cases):
//! recall@P0 0.7317, recall@P1 0.9146, precision@P0 0.2244, precision@1
//! 0.5122. After the fixes: recall@P0 0.8049, recall@P1 0.9634, precision@P0
//! 0.2390, precision@1 0.6098 — no probe regresses, three go from 0.00 to
//! 1.00 and one from 0.33 to 1.00. The floors below encode the post-fix
//! numbers with margin, so any ranking regression fails this harness.
//!
//! Rebuild with `cargo test -p pixel-cli` (or `cargo build -p pixel-cli`) so
//! `CARGO_BIN_EXE_pixel` matches the working tree; the harness never touches
//! the network and never talks to a daemon (`PIXEL_DAEMON_AUTO_START=0`). For
//! a baseline column, set `PIXEL_SCOPE_CORPUS_BIN` to another `pixel` binary
//! (the assertions are expected to fail there; the printed table is the
//! measurement).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

use crate::support::pixel_command;

/// One pinned probe: commit OID, task text, and the changed Rust files.
struct Case {
    oid: &'static str,
    task: &'static str,
    labels: &'static [&'static str],
}

/// Post-fix floors. `precision@P0` only guards against a drop: the tier is
/// capped at five and refills from P1, so its value is dominated by how many
/// of the five are labels, while `precision@1`/`recall@P0` measure the
/// ranking itself.
const MIN_MEAN_RECALL_P0: f64 = 0.795;
const MIN_MEAN_RECALL_P1: f64 = 0.95;
const MIN_MEAN_PRECISION_P0: f64 = 0.235;
const MIN_MEAN_PRECISION_1: f64 = 0.60;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The binary under test: the one cargo built for this target, or the
/// baseline named by `PIXEL_SCOPE_CORPUS_BIN` (see the module docs).
fn corpus_command() -> Command {
    std::env::var_os("PIXEL_SCOPE_CORPUS_BIN").map_or_else(pixel_command, |binary| {
        let mut command = Command::new(binary);
        command.env("PIXEL_DAEMON_AUTO_START", "0");
        command
    })
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap()
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {} — the corpus needs the full pixel history; a shallow clone does not have it",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn commit_parent(repo: &Path, oid: &str) -> String {
    git_stdout(repo, &["rev-parse", &format!("{oid}^")])
        .trim()
        .to_string()
}

/// The `crates/**/*.rs` files a commit changed. Must equal the pinned labels
/// exactly; a mismatch means the pinned OID no longer describes the case.
fn rust_labels(repo: &Path, oid: &str) -> Vec<String> {
    let mut labels: Vec<String> =
        git_stdout(repo, &["show", "--name-only", "--format=", "-r", oid])
            .lines()
            .filter(|line| line.starts_with("crates/") && line.ends_with(".rs"))
            .map(ToString::to_string)
            .collect();
    labels.sort();
    labels.dedup();
    labels
}

/// A throwaway git worktree of the repository under test, removed on drop so
/// a failing assertion still cleans up. One worktree is reused for the whole
/// corpus: each checkout only moves the tree a few commits, which keeps the
/// graph update incremental.
struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

impl Worktree {
    fn add(repo: &Path, commit: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("pixel-scope-task-corpus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = git(repo, &["worktree", "prune"]);
        let out = git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap(),
                commit,
            ],
        );
        assert!(
            out.status.success(),
            "git worktree add: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self {
            repo: repo.to_path_buf(),
            path,
        }
    }

    fn checkout(&self, commit: &str) {
        let out = git(&self.path, &["checkout", "-q", "--detach", commit]);
        assert!(
            out.status.success(),
            "git checkout {commit}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn scope_task(&self, task: &str) -> Value {
        let indexed = corpus_command()
            .args(["build-index", "."])
            .current_dir(&self.path)
            .env("PIXEL_INDEX_BUDGET_MS", "0")
            .output()
            .unwrap();
        assert!(
            indexed.status.success(),
            "build-index for {task:?}: {}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        let out = corpus_command()
            .args(["scope-task", task, ".", "--json", "--no-manifest"])
            .current_dir(&self.path)
            .env("PIXEL_INDEX_BUDGET_MS", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "scope-task {task:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("scope-task JSON for {task:?}: {e}"))
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = git(
            &self.repo,
            &["worktree", "remove", "--force", self.path.to_str().unwrap()],
        );
    }
}

struct Row {
    p0: Vec<String>,
    recall_p0: f64,
    recall_p1: f64,
    precision_p0: f64,
    precision1: f64,
}

fn evaluate(labels: &[&str], report: &Value) -> Row {
    let targets = report["targets"].as_array().expect("targets array");
    let paths_in_tier = |tier: &str| -> Vec<String> {
        targets
            .iter()
            .filter(|t| t["tier"] == tier)
            .filter_map(|t| t["path"].as_str().map(ToString::to_string))
            .collect()
    };
    let p0 = paths_in_tier("P0");
    let p1 = paths_in_tier("P1");
    let labels: BTreeSet<&str> = labels.iter().copied().collect();
    let hits_p0 = p0.iter().filter(|p| labels.contains(p.as_str())).count();
    let hits_p0_p1 = p0
        .iter()
        .chain(&p1)
        .filter(|p| labels.contains(p.as_str()))
        .count();
    // The report is already rank-ordered; take its head so a score tie (or a
    // higher-scored P1 above a P0) cannot measure a different target than the
    // one shown first.
    let best_is_label = targets
        .first()
        .and_then(|t| t["path"].as_str())
        .is_some_and(|path| labels.contains(path));
    Row {
        recall_p0: hits_p0 as f64 / labels.len() as f64,
        recall_p1: hits_p0_p1 as f64 / labels.len() as f64,
        precision_p0: if p0.is_empty() {
            0.0
        } else {
            hits_p0 as f64 / p0.len() as f64
        },
        precision1: if best_is_label { 1.0 } else { 0.0 },
        p0,
    }
}

#[test]
#[ignore = "pinned history corpus: needs full git history and ~2-3 minutes; run with -- --ignored --nocapture"]
fn scope_task_precision_over_pinned_history_corpus() {
    let repo = repo_root();
    let mut parents: Vec<String> = Vec::with_capacity(CORPUS.len());
    for case in CORPUS {
        parents.push(commit_parent(&repo, case.oid));
        let actual = rust_labels(&repo, case.oid);
        let mut expected: Vec<String> = case.labels.iter().map(ToString::to_string).collect();
        expected.sort();
        assert_eq!(
            actual, expected,
            "pinned labels drifted for {}; pin exactly the crates/**/*.rs files the commit changed",
            case.oid
        );
    }

    let tree = Worktree::add(&repo, &parents[0]);
    let mut rows: Vec<Row> = Vec::with_capacity(CORPUS.len());
    for (index, case) in CORPUS.iter().enumerate() {
        tree.checkout(&parents[index]);
        let report = tree.scope_task(case.task);
        let row = evaluate(case.labels, &report);
        println!(
            "[{:2}/{:2}] recall@P0={:.2} recall@P1={:.2} precision@P0={:.2} precision@1={:.0} {} {}",
            index + 1,
            CORPUS.len(),
            row.recall_p0,
            row.recall_p1,
            row.precision_p0,
            row.precision1,
            &case.oid[..12],
            case.task.lines().next().unwrap_or("")
        );
        rows.push(row);
    }

    let count = rows.len() as f64;
    let mean = |f: fn(&Row) -> f64| rows.iter().map(f).sum::<f64>() / count;
    let mean_recall_p0 = mean(|r| r.recall_p0);
    let mean_recall_p1 = mean(|r| r.recall_p1);
    let mean_precision_p0 = mean(|r| r.precision_p0);
    let mean_precision_1 = mean(|r| r.precision1);
    println!("\n=== scope-task corpus: {} cases ===", rows.len());
    println!("mean recall@P0    {mean_recall_p0:.4}");
    println!("mean recall@P1    {mean_recall_p1:.4}");
    println!("mean precision@P0 {mean_precision_p0:.4}");
    println!("mean precision@1  {mean_precision_1:.4}");

    // The session example (`rows.last()` is the pinned session case): the
    // task names `upgrade_cli.rs`, the only file the commit changed. It must
    // be the FIRST P0 target, not merely present.
    let session = rows.last().expect("the corpus ends with the session case");
    assert_eq!(
        session.p0.first().map(String::as_str),
        Some("crates/pixel/tests/cli/upgrade_cli.rs"),
        "session case P0: {:?}",
        session.p0
    );

    let floor = MIN_MEAN_RECALL_P0;
    assert!(
        mean_recall_p0 >= floor,
        "mean recall@P0 {mean_recall_p0:.4} < {floor}"
    );
    let floor = MIN_MEAN_RECALL_P1;
    assert!(
        mean_recall_p1 >= floor,
        "mean recall@P1 {mean_recall_p1:.4} < {floor}"
    );
    let floor = MIN_MEAN_PRECISION_P0;
    assert!(
        mean_precision_p0 >= floor,
        "mean precision@P0 {mean_precision_p0:.4} < {floor}"
    );
    let floor = MIN_MEAN_PRECISION_1;
    assert!(
        mean_precision_1 >= floor,
        "mean precision@1 {mean_precision_1:.4} < {floor}"
    );
}

const CORPUS: &[Case] = &[
    Case {
        oid: "fa56bf6e64dcfa5244da365f2e684d227a4c061c",
        task: "chore: apply /claude-api prompt-audit findings to hook text and rule files\n\n- post-compaction: drop the hard 'do NOT read or edit outside this list'\n  prohibition; the guard is advisory by design (hard scoping was measured\n  to collapse recall), so the injected text now matches that contract\n- prompt-submit: restate the /compact suggestion positively instead of a\n  'do NOT attempt a mental reset' prohibition\n- guard: remove the '<50ms' latency claim and 'Work P0 first' coaching\n  from the retrieval advisory, keeping only the contract sentence\n- CLAUDE.md / AGENTS.md: drop the 'you MUST' booster and bring the two\n  files back in sync (skip conditions, code-signature explanation)",
        labels: &[
            "crates/pixel/src/guard.rs",
            "crates/pixel/src/post_compaction.rs",
            "crates/pixel/src/prompt_submit.rs",
        ],
    },
    Case {
        oid: "9747e51b57e03a77e7a2c622ef59c214ae77ff55",
        task: "fix(upgrade): install over the running/PATH pixel, not a fixed ~/.local/bin; add --dry-run (#26)\n\nresolve_upgrade_target() picks the install location in priority order:\n--install-path, running binary (unless in target/), first pixel on PATH\n(shim dirs skipped), then ~/.local/bin/pixel. Adds --dry-run flag and\nshadow warning when another pixel precedes the target on PATH.\nAlso syncs AGENTS.md with the CLAUDE.md upgrade step change.\n\nGenerated with [Devin](https://devin.ai)\n\nCo-Authored-By: Devin <158243242+devin-ai-integration[bot]@users.noreply.github.com>",
        labels: &[
            "crates/pixel/src/main.rs",
            "crates/pixel/tests/upgrade_cli.rs",
        ],
    },
    Case {
        oid: "7b1fd0814aa255ea4265b1860cd248515c32c975",
        task: "fix(publish): commit deletions already staged with git rm instead of failing on git add\n\n`git add -- <path>` rejects a path absent from both the worktree and the\nindex (\"pathspec did not match any files\"), so `pixel publish --files`\ncould not commit a deletion staged with `git rm`. Paths in that state now\nskip the staging step; the pathspec-scoped `git commit` commits them, and\na path git has never known is still rejected without creating a commit.",
        labels: &[
            "crates/pixel-ops/src/publish.rs",
            "crates/pixel-ops/tests/publish_property.rs",
        ],
    },
    Case {
        oid: "e531744ca74364fc10adea85c2f6d3a5bcca75d0",
        task: "fix(install): pass the codex prompt through developer_instructions, not model_instructions_file",
        labels: &[
            "crates/pixel-install/src/install.rs",
            "crates/pixel-install/tests/install_tests.rs",
        ],
    },
    Case {
        oid: "0a3691858ffa1ea29c99319ee6101f40ca95ea92",
        task: "fix(upgrade): keep target/release for build commands that do not invoke cargo",
        labels: &["crates/pixel/src/main.rs"],
    },
    Case {
        oid: "a1241568f25c7165659691e59984a679222c81c6",
        task: "fix(guard): keep a search native only under its own tool's configuration; make guard tests hermetic\n\nsearch_compat::rewrite refused every rewrite when either RIPGREP_CONFIG_PATH\nor GREP_OPTIONS was set, so a shell with an rgrc never got a grep rewrite,\nand guard_deny::bash_literal_file_search_uses_compatibility_rewrite and\ncomposed_codex_merges_context_and_rewrites_after_exact_stdin_replay\ninherited that shell and failed locally while passing in CI's bare env.\n\n- native_configuration(tool): RIPGREP_CONFIG_PATH gates rg, GREP_OPTIONS\n  gates grep, at rewrite time and at execution time.\n- guard_deny clears both variables before every hook invocation.\n- search_compat_cli covers own-tool config (still native) and other-tool\n  config (rewritten, and executed on the pixel backend).",
        labels: &[
            "crates/pixel/src/search_compat.rs",
            "crates/pixel/tests/cli/guard_deny.rs",
            "crates/pixel/tests/cli/search_compat_cli.rs",
        ],
    },
    Case {
        oid: "0bfc0120ddb757189532e21193dba65f9f3ade9f",
        task: "refactor(bench): clear pedantic lint items_after_statements",
        labels: &["crates/pixel-bench/benches/ndcg_relevance.rs"],
    },
    Case {
        oid: "21af1c9c6dd9a4f1d8a1d1896a50e17c7fe0fe63",
        task: "refactor(git): clear pedantic lints items_after_statements, redundant_closure_for_method_calls",
        labels: &[
            "crates/pixel-git/src/plumbing.rs",
            "crates/pixel-git/src/runner.rs",
        ],
    },
    Case {
        oid: "c0f986eb55ed2978be3ae8ec222fb268e9c15a2b",
        task: "refactor(rank): clear pedantic lints, pin the RRF fusion formula",
        labels: &[
            "crates/pixel-rank/src/lib.rs",
            "crates/pixel-rank/src/signals.rs",
        ],
    },
    Case {
        oid: "d19d6dded603f9183075e4037e7401b90c7827fc",
        task: "refactor(index): clear pedantic lints items_after_statements, map_unwrap_or, uninlined_format_args",
        labels: &[
            "crates/pixel-index/src/gitsync.rs",
            "crates/pixel-index/src/index.rs",
            "crates/pixel-index/src/indexset.rs",
        ],
    },
    Case {
        oid: "2952ab2db8ae62c338fda8a50ecd4ebb6bb1316b",
        task: "refactor(daemon): clear pedantic lints, hoist per-op constants, test note/map/facts/signals",
        labels: &[
            "crates/pixel-daemon/src/api.rs",
            "crates/pixel-daemon/src/daemon.rs",
            "crates/pixel-daemon/src/recall_service.rs",
        ],
    },
    Case {
        oid: "8639d8d465531069e74af8c0fed58a51ca6379f2",
        task: "test(sniper): cap_message finds the char boundary without a mutable loop",
        labels: &["crates/pixel-session/src/parsers/ruby.rs"],
    },
    Case {
        oid: "464182167bf93c614ec708f9ee88a6b390455be3",
        task: "test(ops): force-add the tracked sidecar fixture so a global .pixel/ ignore cannot void the test",
        labels: &["crates/pixel-ops/src/reconcile.rs"],
    },
    Case {
        oid: "776314739d3364e8830d72441fa2fd8b7010e85a",
        task: "test(daemon): cover machine_sources, op, apply_change and watch_paths on the recall service",
        labels: &["crates/pixel-daemon/src/recall_service.rs"],
    },
    Case {
        oid: "45577565ccf2e5aa2e9883a383efaf8c1bc6be7b",
        task: "docs(skill): fold Apollo's rust-best-practices into rust-guidelines, add refresh script and id drift test\n\nSKILL.md gains the points Microsoft's guidelines do not cover, restated\nfrom Apollo GraphQL's rust-best-practices (MIT, ideas not text): a\n\"Borrowing and ownership\" block, the type-state pattern under API shape,\nTODO(#NN) under Docs, test naming and insta snapshot rules under Tests.\nThe precedence paragraph resolves the one known conflict: M-PANIC-ON-BUG\nwins over Apollo's \"never unwrap outside tests\". \"Out of scope\" records\nwhy async rules are not applied and when to revisit (the daemon going\nasync).\n\nscripts/refresh-guidelines.sh re-downloads the upstream text and prints\nthe diff of `## … (M-…)` headings. Running it refreshed guidelines.txt:\nupstream dropped M-INTEGRATION-TEST-UTILS (folded into M-TEST-UTIL), so\nthe skill no longer names it. The docs_drift test now fails the build\nwhen SKILL.md names an id that is not a heading of guidelines.txt.\nNOTICE credits Apollo.",
        labels: &["crates/pixel/tests/cli/docs_drift.rs"],
    },
    Case {
        oid: "61f845fe1468b9a06092715e668884afdaabd064",
        task: "fix(rename): handle multi-line args arrays in test files\n\nAdd patterns for \"old-name\" on its own line in multi-line .args([...])\narrays, not just [\"old-name\" and , \"old-name\".\n\nGenerated with [Devin](https://devin.ai)\n\nCo-Authored-By: Devin <158243242+devin-ai-integration[bot]@users.noreply.github.com>",
        labels: &["crates/pixel/src/rename.rs"],
    },
    Case {
        oid: "27b10c9cfeebe4c16928490829f82ec3a225c533",
        task: "fix(rename): actually add guard.rs to file list (was reverted)",
        labels: &["crates/pixel/src/rename.rs"],
    },
    Case {
        oid: "ff40c90d6deb4dc6cec65de8af479e973dcd536a",
        task: "fix(rename): remove '-' suffix from user_facing + fix search_compat output\n\nThe '-' suffix caused 'pixel search-' to match inside 'pixel search-content'\ncausing double-rename to 'search-content-content'. Also fix search_compat.rs\nto emit 'search-like-rg' (new name) instead of 'search-compat' (old name).\n\nGenerated with [Devin](https://devin.ai)\n\nCo-Authored-By: Devin <158243242+devin-ai-integration[bot]@users.noreply.github.com>",
        labels: &[
            "crates/pixel/src/rename.rs",
            "crates/pixel/src/search_compat.rs",
        ],
    },
    Case {
        oid: "4fa0f0f89c97b110ea9b9f8c7022c4f38961a84e",
        task: "test(ops): kill the two mutants the CI gate reported\n\n- reconcile_into: a --into fixture where both sides append to the same\n  file proves the auto-resolved rebase is continued and reported as\n  rebased with the resolved path (kills deleting the ! on\n  rebase_resolved.is_empty()).\n- git_supports_merge_tree_write_tree: mutants::skip, it is the one-line\n  adapter over the real git; -> true is what a modern git answers and the\n  parsing lives in the tested pure helper.",
        labels: &["crates/pixel-ops/src/reconcile.rs"],
    },
    Case {
        oid: "935150b07fa9f4b39800b5fcbf5b28b3e0f34a07",
        task: "test(cli): pin hash_object to git's blob oid (kills the two Mutants survivors)",
        labels: &["crates/pixel/src/task_sandbox.rs"],
    },
    Case {
        oid: "8a501cbab7b56e7b66647a106779d5ee846fd00e",
        task: "fix(rank): drop French stopwords and homograph expansions only from French tasks\n\n5a93490 made the French stopword list apply to every task: `comment`,\n`car`, `plus`, `sans`, `des`, `est` vanished from English tasks too\n(\"fix comment parsing\" searched `parsing`), and French relation keys that\nare English words expanded in English tasks (`client` → customer, user;\n`message` → notification). `très` could never match: tasks are\naccent-folded before the stopword check.\n\ntokenize_task now detects the task language (two distinct French function\nwords or unambiguous French relation keys) and records it on TaskQuery.\nEnglish stopwords always apply, French ones only to a French task.\nFRENCH_RELATIONS is split from the general thesaurus; its homograph keys\nexpand only in a French task, the others in any. expand_keywords,\nsemantic_expand and lexical_rank take the language.",
        labels: &[
            "crates/pixel-daemon/src/api.rs",
            "crates/pixel-rank/src/lib.rs",
        ],
    },
    Case {
        oid: "153594fc078b0cfa976f596ffde9d2a57e2b1a87",
        task: "fix(ops): attribute lines when blame.ignoreRevsFile names a missing file\n\nA blame.ignoreRevsFile that the repository does not have (a common global\ndefault of .git-blame-ignore-revs) makes git blame exit 128 with \"could\nnot open object name list\", so provenance (pixel who-wrote) failed on\nevery file of such a repository; six pixel-ops tests failed on any machine\nwith that setting. -c blame.ignoreRevsFile= and --ignore-revs-file= do not\nclear a configured list; --no-ignore-revs-file does. provenance retries with\nit on that exact error and adds a warning; an existing file is honoured.\n\nThe truncation test looked for its warning at index 0; it now looks for it\namong the warnings.",
        labels: &[
            "crates/pixel-ops/src/provenance.rs",
            "crates/pixel-ops/tests/all/provenance.rs",
        ],
    },
    Case {
        oid: "c8ea7da96eb6bf1faa2290ddad6712e6c79b4ea5",
        task: "fix(plan): read a directory after --query as the path\n\npixel plan takes an optional prompt and an optional path as positionals, so\n`pixel plan --query hotspots ../repo` parsed ../repo as the prompt and\nplanned the current directory. Only by-concept reads a prompt next to\n--query; for any other query, a prompt naming a directory while the path is\nat its default becomes the path.",
        labels: &["crates/pixel/src/plan_cmd.rs"],
    },
    Case {
        oid: "937b8c395bbda79c73c725fba1970979e257caf3",
        task: "test(ops): pin the blame retry to its one error and the truncation edge\n\nKills the three Mutants survivors: the retry guard replaced with true and\nnames_a_missing_ignore_revs_file -> true (a failed retry now says it ran\nwithout ignored revisions, and a range past the end of the file must fail\nas git's own error), and total_regions > limit -> >= (a count equal to the\nlimit is not truncated).",
        labels: &[
            "crates/pixel-ops/src/provenance.rs",
            "crates/pixel-ops/tests/all/provenance.rs",
        ],
    },
    Case {
        oid: "f2a92e5aae05e41f84e0acd69155316bbfc21de2",
        task: "test(ops): keep the reconcile fixtures on git's built-in merge\n\nA developer's global core.attributesFile mapping every path to a merge\ndriver (mergiraf) resolved the additive conflict of\nreconcile_into_auto_resolves_an_additive_conflict_and_continues_the_rebase\nby itself: the probe came back clean and the reconcile answered\nintegrated instead of rebased. The fixture now writes\n.git/info/attributes with * merge=text, which outranks both\ncore.attributesFile and in-tree .gitattributes, and a test installs an\nalways-clean driver in the same attribute slot to prove it.",
        labels: &["crates/pixel-ops/src/reconcile.rs"],
    },
    Case {
        oid: "e2cb53a988eabcdbe40af4343144e8083417df98",
        task: "test(index): give every IndexSet test its own shard cache\n\nTen indexset tests opened an index without CACHE_TEST_LOCK or a\ntemporary XDG_CACHE_HOME, so each git fixture commit linked from and\npublished into the developer's real ~/.cache/pixel/shards (three new\nentries per cargo test -p pixel-index run). An IsolatedCache guard takes\nthe lock, points XDG_CACHE_HOME at a scratch directory and restores the\nprevious value on drop, a failed assertion included; a test checks the\nshard lands in that directory, and another fails when a test that opens\nan index does not hold the guard.",
        labels: &["crates/pixel-index/src/indexset.rs"],
    },
    Case {
        oid: "4c5281dfe66d32461ef3956c03f580a84bd25def",
        task: "test(install): kill NotFound guard mutants in pi prompt read paths\n\nAdd tests that an unreadable (write-only) pi prompt file is an error for both write_pi_prompt and remove_agent_prompt, not treated as absent.",
        labels: &[
            "crates/pixel-install/src/install.rs",
            "crates/pixel-install/src/uninstall.rs",
        ],
    },
    Case {
        oid: "50197d558989dd18776f5d65ed779b8fc674b884",
        task: "fmt(install): fix rustfmt on antigravity path assertions",
        labels: &["crates/pixel-install/src/antigravity.rs"],
    },
    Case {
        oid: "00173dfcd2bedacf62f7c45ea8fa2625886e7fcb",
        task: "fmt(install): cargo fmt antigravity.rs",
        labels: &["crates/pixel-install/src/antigravity.rs"],
    },
    Case {
        oid: "81692580ddcab39c56e2a957a5bbcdd0c9d8a37e",
        task: "fix(cli): make search --json self-describing and cap --context output",
        labels: &[
            "crates/pixel/src/main.rs",
            "crates/pixel/tests/cli/json_contract.rs",
            "crates/pixel/tests/cli/search_compat_cli.rs",
        ],
    },
    Case {
        oid: "feddde2911f722ac4b3235d163a02392d06704b7",
        task: "fix(graph): leave ambiguous Go and Java imports unresolved",
        labels: &[
            "crates/pixel-graph/src/imports.rs",
            "crates/pixel-graph/tests/all/import_resolution.rs",
        ],
    },
    Case {
        oid: "3c0778f7c59661cdb9a416f80af1ed1caeadd692",
        task: "test(ops): kill mutants in journal and recovery read paths\n\nAdd targeted tests for the NotFound guard and the idempotency || guards in OperationJournal::read_existing and PublishRecoveryStore::read.",
        labels: &[
            "crates/pixel-ops/src/journal.rs",
            "crates/pixel-ops/src/recovery.rs",
        ],
    },
    Case {
        oid: "d86e8f274f29302611b5860e6b78f6ba01153249",
        task: "fix(install): format the guard tests and pin the unreadable prompt contract (#157)",
        labels: &[
            "crates/pixel-install/src/install.rs",
            "crates/pixel-install/src/uninstall.rs",
        ],
    },
    Case {
        oid: "1d76e2029f9d3a3302f4f2eae9aa9f46a6887f7d",
        task: "test(daemon): exercise the rerank adapter so an empty result cannot pass",
        labels: &["crates/pixel-daemon/src/api.rs"],
    },
    Case {
        oid: "69ed04b79e1ead46d0ca1debb7b8c12c9d51b2e9",
        task: "fix(cli): raise daemon probe timeout; fix install test PATH (#161)\n\n* fix(cli): raise try_daemon probe timeout so a busy daemon is not mistaken for dead\n\nThe daemon drains its debounced watcher batch before serving a connection (since #144). On a cold or loaded CI host that drain can outlast the 1.5s probe timeout in try_daemon_inner, so build-index falls back to in-process and never prints 'indexed via daemon'. Raise the probe timeout to 5s.\n\n* fix(scripts): put gzip on the restricted PATH in the install contract\n\n`install.sh` extracts the archive with `tar xzf`; GNU tar execs `gzip` as\na child, so the restricted PATH (SYSTEM_TOOLS + chosen fakes) needs it.\nThe two restricted install tests failed in CI with `gzip: Cannot exec`.\n\nGenerated with [Devin](https://devin.ai)\n\nCo-Authored-By: Devin <158243242+devin-ai-integration[bot]@users.noreply.github.com>\n\n---------\n\nCo-authored-by: Devin <158243242+devin-ai-integration[bot]@users.noreply.github.com>",
        labels: &["crates/pixel/src/main.rs"],
    },
    Case {
        oid: "f99b354380d74009f243c780483b5a2036336b6c",
        task: "test(rename): close last mutant — drop unreachable alias-kind fallback clause",
        labels: &["crates/pixel-graph/src/rename.rs"],
    },
    Case {
        oid: "03fe1526cf4a6bcc960f97c1b088c29d6f88d647",
        task: "feat(self-update): guide upgrades through the owning package manager\n\n`pixel self-update` refuses to overwrite a mise or Homebrew install, but\nthe refusal only offered the developer escape hatches (`--install-path`,\n`--dev`). A user who typed the old `pixel upgrade` alias got no path\nforward: the command that updates that install is the package manager's\nown. The refusal now names it (`mise upgrade pixel`,\n`brew update && brew upgrade LivioGama/tap/pixel`).\n\nEvery upgrade channel replaces the binary only. The agent prompt, shell\nwrapper and per-agent config keys stay on the old release until\n`pixel install` runs, which is the step a team upgrading through brew\nforgets. The generated Homebrew formula now carries that follow-up as\ncaveats, printed by Homebrew after both install and upgrade, and the\nREADME gains a per-channel Updating section.",
        labels: &[
            "crates/pixel/src/main.rs",
            "crates/pixel/tests/cli/upgrade_cli.rs",
        ],
    },
    Case {
        oid: "356691ff51050e9527fbd77dba2bcf81ba0ceb76",
        task: "test(cli): de-flake unresponsive-daemon upgrade timeout check\n\nThe test timed from before Command::spawn() to after wait_with_output(), so\nthe child's startup sat inside the `elapsed < 4 s` assertion. On a loaded\nmachine that startup reaches several seconds (3-4 s observed here for the\n148 MB debug binary), which failed the assertion, and the same startup could\nexceed the fixture's 5 s kill and turn a correct client into a `timed_out`\nfailure.\n\nThe fake daemon now withholds the reply and reports whether the client\ndropped its end (`Ok(0)`, its own budget expired) or stayed connected until\nthe 8 s read timeout. That is measured from the request, not from spawn, so\nstartup is out of the measurement while a client without its own timeout\nstill fails. The fixture deadline becomes a 15 s hang guard (`timed_out`\nstays asserted), and the now-unused elapsed Duration leaves the fixture's\nreturn tuple.",
        labels: &["crates/pixel/tests/cli/upgrade_cli.rs"],
    },
    Case {
        oid: "e8a6918131307deb66d94409caa8d8fb2259e7b0",
        task: "test(cli): bound the unresponsive-daemon give-up check\n\nThe fake daemon accepted any client close within its 8 s post-request\nread as the client giving up on its own, so the test would still pass\nfor a client whose budget regressed from 1500 ms to several seconds.\nThe give-up read now carries its own 2.5 s deadline — the production\nbudget plus 1 s of slack for a loaded runner — while the 8 s timeout\nstays only on the read that waits for startup to deliver the request.\n\nDoc comments on the four functions this diff touches without one also\nbring CodeRabbit's docstring-coverage pre-merge check over its 80%\nthreshold.\n\nVerified: with upgrade_daemon_request's read timeout raised to 10 s the\ntest fails at \"upgrade did not drop the daemon connection within its own\nbudget\"; with it restored, cargo nextest run --workspace --profile ci\nreports 1458 passed, and fmt/clippy are clean.",
        labels: &["crates/pixel/tests/cli/upgrade_cli.rs"],
    },
    Case {
        oid: "ea7918bc675a190b461404e41cf7c9bd467e23dc",
        task: "test(graph): pin the matched variant name in the ident-tier regression",
        labels: &["crates/pixel-graph/tests/all/concept_tests.rs"],
    },
    Case {
        oid: "356691ff51050e9527fbd77dba2bcf81ba0ceb76",
        task: "de-flake upgrade_reports_unresponsive_daemon_without_claiming_completion test in upgrade_cli.rs",
        labels: &["crates/pixel/tests/cli/upgrade_cli.rs"],
    },
];
