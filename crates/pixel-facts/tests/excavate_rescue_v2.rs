//! Rescue-v2 (`excavate`) integration tests against a real git fixture that
//! reproduces PLAN.md's canonical Scenario 1 ("dropped-svelte"): a feature
//! file is added, modified, then deleted in favor of an unrelated
//! replacement — and the deleting commit's SUBJECT deliberately never
//! mentions the feature, so a naive subject-substring heuristic would miss
//! it. Also covers the `commits.reach` bitmask (branch-only / stash-only
//! content) and confirms diff-content-overlap suspect detection over the
//! weaker subject-substring approach it replaces.

use std::fs;
use std::path::Path;
use std::process::Command;

use pixel_facts::ingest::{IngestOptions, ingest_until_fresh};
use pixel_facts::store::FactsStore;
use tempfile::TempDir;

const PHRASE: &str = "legacy widget renderer";

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git command");
    if !out.status.success() {
        panic!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Builds the "dropped-svelte" fixture:
///   commit 1 (main): add src/Widget.svelte containing the PHRASE
///   commit 2 (main): modify Widget.svelte, keep the PHRASE (a real edit —
///                     the line carrying the phrase is rewritten, so both
///                     added+removed sides mention it; this must NOT be
///                     flagged suspect)
///   commit 3 (main): delete Widget.svelte, add Widget.tsx as a replacement
///                     that does NOT contain the phrase. Subject line is
///                     deliberately neutral ("swap to typed component") —
///                     no mention of "widget", "legacy", or "renderer" — so
///                     subject-substring suspect detection would miss it
///                     entirely, but diff-overlap must still catch it.
///   branch `feature/only-here`: one commit off main's tip containing a
///                     phrase ("branch only marker") that exists nowhere on
///                     main, to prove history-wide reach isn't scoped to the
///                     checked-out branch.
///   stash: one stash entry containing a phrase ("stash only marker") that
///                     exists nowhere in any commit reachable from a branch.
///   working tree: Widget.tsx is left with an uncommitted edit, to exercise
///                     the dirty-file safety gate elsewhere in the suite.
fn make_dropped_svelte_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    fs::create_dir_all(root.join("src")).unwrap();

    // Commit 1: add Widget.svelte with the phrase.
    fs::write(
        root.join("src/Widget.svelte"),
        "<script>\n  // legacy widget renderer\n  export let items = [];\n</script>\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "Add Widget.svelte"]);

    // Commit 2: modify the phrase-bearing line itself (real edit, phrase kept).
    fs::write(
        root.join("src/Widget.svelte"),
        "<script>\n  // legacy widget renderer (v2, still used everywhere)\n  export let items = [];\n  export let title = '';\n</script>\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "Extend Widget.svelte with a title prop"]);

    // Commit 3: delete Widget.svelte, add an unrelated Widget.tsx. Subject
    // deliberately says nothing about widget/legacy/renderer.
    fs::remove_file(root.join("src/Widget.svelte")).unwrap();
    fs::write(
        root.join("src/Widget.tsx"),
        "export function Widget(props: { items: unknown[] }) {\n  return null;\n}\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "swap to typed component"]);

    // Branch reachable only via a non-checked-out ref.
    git(root, &["branch", "feature/only-here"]);
    git(root, &["checkout", "-q", "feature/only-here"]);
    fs::write(root.join("src/branch_only.txt"), "branch only marker\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "content that lives only on a branch"]);
    git(root, &["checkout", "-q", "main"]);

    // Stash entry reachable only via refs/stash.
    fs::write(root.join("src/stash_only.txt"), "stash only marker\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["stash", "push", "-q", "-m", "wip stash marker"]);

    // Leave uncommitted work in the tree (dirty-gate exercise elsewhere).
    fs::write(
        root.join("src/Widget.tsx"),
        "export function Widget(props: { items: unknown[] }) {\n  // uncommitted local tweak\n  return null;\n}\n",
    )
    .unwrap();

    dir
}

fn ingest(root: &Path) -> FactsStore {
    let mut store = FactsStore::open(root).expect("open store");
    ingest_until_fresh(&mut store, &IngestOptions::default()).expect("ingest until fresh");
    store
}

// ---------------------------------------------------------------------------
// Deleted-file discovery + last-good selection
// ---------------------------------------------------------------------------

#[test]
fn excavate_finds_deleted_file_content_by_phrase() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let result = store
        .excavate(Some(PHRASE), None, None, None, 50)
        .expect("excavate");

    assert!(
        !result.candidates.is_empty(),
        "excavate must find candidates for a phrase that only exists in a deleted file"
    );
    assert!(
        result
            .candidates
            .iter()
            .any(|c| c.path == "src/Widget.svelte"),
        "candidates must include the deleted src/Widget.svelte, got: {:#?}",
        result.candidates
    );
    // At least one candidate is explicitly marked as coming from a path
    // that's gone from HEAD.
    assert!(
        result.candidates.iter().any(|c| c.deleted_from_head),
        "at least one candidate must be flagged deleted_from_head"
    );
}

#[test]
fn excavate_last_good_survives_deletion_from_head() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let result = store
        .excavate(Some(PHRASE), None, None, None, 50)
        .expect("excavate");

    let last_good = result
        .last_good
        .as_ref()
        .expect("last_good must be set even though src/Widget.svelte is deleted from HEAD");

    assert_eq!(
        last_good.path, "src/Widget.svelte",
        "last_good must point at the deleted file's own path"
    );
    assert!(
        last_good.phrase_present,
        "last_good candidate must have phrase_present=true"
    );
    // last_good should be commit 2 (the newest commit where the phrase is
    // still present after the commit), not commit 1 or the deleting commit 3.
    assert_eq!(
        last_good.subject, "Extend Widget.svelte with a title prop",
        "last_good must resolve to the newest surviving version, not the add \
         or the delete: got subject {:?}",
        last_good.subject
    );
}

#[test]
fn excavate_plan_encodes_oid_path_restorable_when_absent_from_head() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let result = store
        .excavate(Some(PHRASE), None, None, None, 50)
        .expect("excavate");

    let last_good = result.last_good.as_ref().expect("last_good present");
    let expected_source = format!("{}:{}", last_good.oid, last_good.path);
    assert!(
        result.plan.contains(&expected_source),
        "plan must contain a \"<oid>:<path>\" source for the last_good \
         candidate so it is restorable even though the path is absent from \
         HEAD; plan={:?}",
        result.plan
    );

    // The plan source's oid:path must actually be resolvable via `git show`,
    // proving the restore payload is real and not just a synthesized string.
    let show = Command::new("git")
        .arg("show")
        .arg(&expected_source)
        .current_dir(dir.path())
        .output()
        .expect("git show");
    assert!(
        show.status.success(),
        "git show {expected_source} must succeed: {show:?}"
    );
    let content = String::from_utf8_lossy(&show.stdout);
    assert!(
        content.to_lowercase().contains(&PHRASE.to_lowercase()),
        "content restored from the plan source must contain the phrase"
    );
}

// ---------------------------------------------------------------------------
// Inline snippets: candidates carry the hunk text, capped, with a --show hint
// ---------------------------------------------------------------------------

#[test]
fn excavate_candidates_carry_inline_snippets_with_the_code() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let result = store
        .excavate(Some(PHRASE), None, None, None, 50)
        .expect("excavate");

    // Top candidates must carry inline code — the answer, not just metadata.
    let with_snippets = result
        .candidates
        .iter()
        .filter(|c| c.snippet.is_some())
        .count();
    assert!(
        with_snippets > 0,
        "top excavate candidates must carry inline snippets: {:#?}",
        result.candidates
    );
    // At least one snippet contains the phrase itself (the code body).
    assert!(
        result.candidates.iter().any(|c| c
            .snippet
            .as_deref()
            .is_some_and(|s| s.to_lowercase().contains(&PHRASE.to_lowercase()))),
        "a snippet must contain the searched phrase's code"
    );
    // The deleting commit's snippet is the REMOVED side — the pre-deletion
    // code the user wants back — even though phrase_present is false there.
    let delete_commit = result
        .candidates
        .iter()
        .find(|c| c.subject == "swap to typed component")
        .expect("deleting commit candidate");
    let del_snip = delete_commit
        .snippet
        .as_deref()
        .expect("the deleting commit must carry the removed-side snippet");
    assert!(
        del_snip.to_lowercase().contains(&PHRASE.to_lowercase()),
        "deletion snippet must carry the removed (pre-deletion) code: {del_snip:?}"
    );
}

#[test]
fn excavate_snippets_are_capped_per_candidate() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    // A single large file: the phrase sits in the middle of 500 lines, so an
    // uncapped snippet would be the whole hunk.
    let mut body = String::new();
    for i in 0..500 {
        if i == 250 {
            body.push_str("fn giant() { /* legacy widget renderer */ }\n");
        } else {
            body.push_str(&format!("// filler line {i} with some padding text\n"));
        }
    }
    fs::write(root.join("giant.rs"), &body).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "add giant file"]);
    let store = ingest(root);

    let result = store
        .excavate(Some(PHRASE), None, None, None, 50)
        .expect("excavate");
    let snip = result
        .candidates
        .iter()
        .find_map(|c| c.snippet.as_deref())
        .expect("giant-file candidate must still carry a snippet");

    assert!(
        snip.len() <= 6 * 1024 + 8,
        "snippet must be byte-capped (~6KB), got {} bytes",
        snip.len()
    );
    // 60 content lines max, plus at most two ellipsis markers.
    assert!(
        snip.lines().count() <= 62,
        "snippet must be line-capped (~60 lines), got {}",
        snip.lines().count()
    );
    assert!(
        snip.to_lowercase().contains(&PHRASE.to_lowercase()),
        "capped snippet must stay centered on the phrase match"
    );
    assert!(
        snip.contains('…'),
        "a truncated snippet must mark elided content with …"
    );
}

#[test]
fn excavate_next_step_points_at_show_not_git() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let result = store
        .excavate(Some(PHRASE), None, None, None, 50)
        .expect("excavate");

    assert!(
        result.next.contains("--show"),
        "result.next must tell the agent the follow-up is `excavate --show`: {:?}",
        result.next
    );
    // It names the recommended restore point concretely.
    let lg = result.last_good.as_ref().expect("last_good");
    assert!(
        result.next.contains(&lg.oid) && result.next.contains(&lg.path),
        "result.next must name the last_good oid and path: {:?}",
        result.next
    );
}

// ---------------------------------------------------------------------------
// Suspect detection: diff-content-overlap, not subject-substring
// ---------------------------------------------------------------------------

#[test]
fn excavate_flags_the_deleting_commit_suspect_via_diff_overlap_not_subject() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let result = store
        .excavate(Some(PHRASE), None, None, None, 50)
        .expect("excavate");

    let delete_commit = result
        .candidates
        .iter()
        .find(|c| c.subject == "swap to typed component")
        .expect("the deleting commit must be a candidate (its diff removed phrase-bearing text)");

    // Sanity: the subject genuinely does not mention the phrase or any of
    // its words, so a subject-substring heuristic (the predecessor approach
    // in pixel/src/rescue_cmd.rs) would never flag this commit.
    let subject_lc = delete_commit.subject.to_lowercase();
    for word in ["widget", "legacy", "renderer", "svelte"] {
        assert!(
            !subject_lc.contains(word),
            "fixture invariant broken: subject must not leak the word {word:?}"
        );
    }

    assert!(
        delete_commit.suspect,
        "the deleting commit must be flagged suspect via diff-content overlap \
         even though its subject says nothing about the feature: {:#?}",
        delete_commit
    );
    assert!(
        !delete_commit.phrase_present,
        "the deleting commit removed the phrase, so phrase_present must be false for it"
    );
}

#[test]
fn excavate_does_not_flag_a_same_commit_reformat_as_suspect() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let result = store
        .excavate(Some(PHRASE), None, None, None, 50)
        .expect("excavate");

    // Commit 2 rewrites the phrase-bearing line (removes the old line, adds
    // a new one that still contains the phrase). Diff-overlap detection must
    // NOT call this "suspect" — the phrase was never actually lost.
    let modify_commit = result
        .candidates
        .iter()
        .find(|c| c.subject == "Extend Widget.svelte with a title prop")
        .expect("the modifying commit must be a candidate");

    assert!(
        !modify_commit.suspect,
        "a commit that removes-then-re-adds the phrase on the same line must \
         not be flagged suspect: {:#?}",
        modify_commit
    );
    assert!(
        modify_commit.phrase_present,
        "the modifying commit kept the phrase, so phrase_present must be true"
    );
}

// ---------------------------------------------------------------------------
// History-wide reach: branch-only and stash-only content
// ---------------------------------------------------------------------------

#[test]
fn excavate_finds_content_reachable_only_via_a_non_checked_out_branch() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    // Confirm main (the checked-out branch at ingest time) never had this
    // content — it only exists on feature/only-here.
    let head_log = git(dir.path(), &["log", "--all", "--oneline"]);
    assert!(
        head_log.contains("content that lives only on a branch"),
        "fixture sanity: the branch-only commit must exist in --all history"
    );

    let result = store
        .excavate(Some("branch only marker"), None, None, None, 50)
        .expect("excavate");

    assert!(
        !result.candidates.is_empty(),
        "excavate must find content that exists only on a non-checked-out \
         branch — history-wide reach must not be scoped to the current branch"
    );
    assert!(
        result
            .candidates
            .iter()
            .any(|c| c.path == "src/branch_only.txt"),
        "candidates must include src/branch_only.txt, got: {:#?}",
        result.candidates
    );
}

#[test]
fn excavate_finds_content_reachable_only_via_stash() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let stash_list = git(dir.path(), &["stash", "list"]);
    assert!(
        !stash_list.trim().is_empty(),
        "fixture sanity: a stash entry must exist"
    );

    let result = store
        .excavate(Some("stash only marker"), None, None, None, 50)
        .expect("excavate");

    assert!(
        !result.candidates.is_empty(),
        "excavate must find content that exists only in the stash"
    );
    assert!(
        result
            .candidates
            .iter()
            .any(|c| c.path == "src/stash_only.txt"),
        "candidates must include src/stash_only.txt, got: {:#?}",
        result.candidates
    );
}

// ---------------------------------------------------------------------------
// Dirty-file safety gate stays intact around the fixture's uncommitted edit
// ---------------------------------------------------------------------------

#[test]
fn fixture_leaves_widget_tsx_dirty_for_the_apply_safety_gate() {
    let dir = make_dropped_svelte_repo();
    let status = git(dir.path(), &["status", "--porcelain"]);
    assert!(
        status.lines().any(|l| l.ends_with("src/Widget.tsx")),
        "fixture must leave src/Widget.tsx dirty so the gated-apply safety \
         invariants (refuse-without-a-strategy-flag) have something real to \
         refuse against: status={status:?}"
    );
}

// ---------------------------------------------------------------------------
// --from/--to rev-range narrowing (the flags used to be accepted and
// silently discarded — golden test that they actually restrict results)
// ---------------------------------------------------------------------------

const SUBJ_ADD: &str = "Add Widget.svelte";
const SUBJ_EXTEND: &str = "Extend Widget.svelte with a title prop";
const SUBJ_DELETE: &str = "swap to typed component";

fn subjects(result: &pixel_facts::excavate::ExcavateResult) -> Vec<String> {
    result.candidates.iter().map(|c| c.subject.clone()).collect()
}

#[test]
fn excavate_from_to_narrows_candidates_to_the_rev_range() {
    let dir = make_dropped_svelte_repo();
    let root = dir.path();
    let c1 = git(root, &["rev-parse", "main~2"]).trim().to_string();
    let c2 = git(root, &["rev-parse", "main~1"]).trim().to_string();
    let store = ingest(root);

    // Unbounded baseline: all three phrase-bearing commits are present.
    let all = store.excavate(Some(PHRASE), None, None, None, 50).expect("excavate");
    let s = subjects(&all);
    assert!(s.iter().any(|x| x == SUBJ_ADD), "baseline missing add: {s:?}");
    assert!(s.iter().any(|x| x == SUBJ_EXTEND), "baseline missing extend: {s:?}");
    assert!(s.iter().any(|x| x == SUBJ_DELETE), "baseline missing delete: {s:?}");

    // --to c2: the deleting commit 3 is newer than the bound and must drop.
    let to2 = store
        .excavate(Some(PHRASE), None, None, Some(&c2), 50)
        .expect("excavate --to");
    let s = subjects(&to2);
    assert!(!s.is_empty(), "--to must not empty the result set");
    assert!(
        s.iter().all(|x| x != SUBJ_DELETE),
        "--to <commit2> must exclude the newer deleting commit: {s:?}"
    );
    assert!(s.iter().any(|x| x == SUBJ_ADD), "--to must keep older commits: {s:?}");
    assert!(s.iter().any(|x| x == SUBJ_EXTEND), "--to is inclusive of the bound: {s:?}");

    // --from c2: the add commit 1 is older than the bound and must drop;
    // the bound itself stays included.
    let from2 = store
        .excavate(Some(PHRASE), None, Some(&c2), None, 50)
        .expect("excavate --from");
    let s = subjects(&from2);
    assert!(
        s.iter().all(|x| x != SUBJ_ADD),
        "--from <commit2> must exclude older commits: {s:?}"
    );
    assert!(s.iter().any(|x| x == SUBJ_EXTEND), "--from is inclusive of the bound: {s:?}");
    assert!(s.iter().any(|x| x == SUBJ_DELETE), "--from must keep newer commits: {s:?}");

    // [c1..c2]: inclusive of both ends, excludes the newer deleting commit.
    let mid = store
        .excavate(Some(PHRASE), None, Some(&c1), Some(&c2), 50)
        .expect("excavate --from --to");
    let s = subjects(&mid);
    assert!(s.iter().any(|x| x == SUBJ_ADD), "[from..to] includes the older bound: {s:?}");
    assert!(s.iter().any(|x| x == SUBJ_EXTEND), "[from..to] includes the newer bound: {s:?}");
    assert!(
        s.iter().all(|x| x != SUBJ_DELETE),
        "[from..to] excludes commits past the newer bound: {s:?}"
    );

    // last_good is derived from the FILTERED candidates, so it narrows too:
    // bounded at --to c1 the newest phrase-bearing commit is the add itself.
    let to1 = store
        .excavate(Some(PHRASE), None, None, Some(&c1), 50)
        .expect("excavate --to c1");
    let lg = to1.last_good.as_ref().expect("last_good inside the range");
    assert_eq!(lg.subject, SUBJ_ADD, "last_good must respect the range bound");
}

#[test]
fn excavate_unresolvable_range_ref_is_a_structured_error() {
    let dir = make_dropped_svelte_repo();
    let store = ingest(dir.path());

    let err = store
        .excavate(Some(PHRASE), None, Some("no-such-ref"), None, 50)
        .expect_err("an unresolvable --from ref must be an error, not a silently unfiltered answer");
    let msg = err.to_string();
    assert!(
        msg.contains("does not resolve") && msg.contains("no-such-ref"),
        "error must name the bad ref and say it does not resolve: {msg}"
    );

    let err = store
        .excavate(Some(PHRASE), None, None, Some("also-missing"), 50)
        .expect_err("an unresolvable --to ref must be an error");
    assert!(err.to_string().contains("also-missing"), "error names the ref: {err}");
}

// ---------------------------------------------------------------------------
// Shared diff-content heuristic (rescue's suspect detection calls this)
// ---------------------------------------------------------------------------

#[test]
fn phrase_removed_between_flags_only_actual_removals() {
    use pixel_facts::excavate::phrase_removed_between;
    let kw = vec!["discount".to_string()];
    // Removed: present before, absent after.
    assert_eq!(
        phrase_removed_between("fn apply_discount() {}", "fn nothing() {}", &kw),
        Some("discount".to_string())
    );
    // Kept (e.g. reformatting): not a removal.
    assert_eq!(
        phrase_removed_between("apply_discount()", "apply_discount( )", &kw),
        None
    );
    // Never present: not a removal.
    assert_eq!(phrase_removed_between("", "fn apply_discount() {}", &kw), None);
    // Case-insensitive matching.
    assert_eq!(
        phrase_removed_between("Apply_DISCOUNT here", "gone", &kw),
        Some("discount".to_string())
    );
}

// ---------------------------------------------------------------------------
// Same-commit tie-break: real definition over a doc-comment mention
// ---------------------------------------------------------------------------

const DEF_PHRASE: &str = "register_mcp_server";

/// One commit deletes the phrase from TWO files at once: a doc comment in
/// `a.rs` that merely mentions the identifier, and the actual function
/// definition in `b.rs`. Both candidates tie on (suspect, at, seq) — the
/// bug this reproduces (measured 2026-08-30, docs/bench/agent-ab-2026-08-30
/// -clean-postfix.txt s4-recover): without a definition-vs-mention
/// tiebreaker, incidental SQL row order can surface the doc comment first,
/// forcing the caller to dig past it for the real "find the deleted
/// function" answer.
fn make_same_commit_two_file_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    fs::write(
        root.join("a.rs"),
        format!("/// See also install::{DEF_PHRASE} for the setup path.\npub fn other() {{}}\n"),
    )
    .unwrap();
    fs::write(
        root.join("b.rs"),
        format!("pub fn {DEF_PHRASE}() {{\n    // real implementation\n}}\n"),
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add mcp registration"]);

    fs::write(root.join("a.rs"), "pub fn other() {}\n").unwrap();
    fs::write(root.join("b.rs"), "// removed\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "drop mcp registration"]);

    dir
}

#[test]
fn excavate_ranks_the_real_definition_above_a_same_commit_comment_mention() {
    let dir = make_same_commit_two_file_repo();
    let store = ingest(dir.path());

    let result = store
        .excavate(Some(DEF_PHRASE), None, None, None, 50)
        .expect("excavate");

    assert!(
        result.candidates.len() >= 2,
        "expected candidates from both files: {:#?}",
        result.candidates
    );
    let top = &result.candidates[0];
    assert_eq!(
        top.path, "b.rs",
        "the real function definition (b.rs) must rank first, not the doc-comment \
         mention (a.rs), when both tie on suspect+recency: {:#?}",
        result.candidates
    );
    assert!(top.suspect, "b.rs's deletion must be flagged suspect");
    assert!(
        top.is_definition,
        "b.rs's removed text is a real `pub fn` definition, not a comment mention"
    );

    let comment_candidate = result
        .candidates
        .iter()
        .find(|c| c.path == "a.rs")
        .expect("a.rs must still be a candidate");
    assert!(
        !comment_candidate.is_definition,
        "a.rs's removed text is only a doc-comment mention, not a definition"
    );
}
