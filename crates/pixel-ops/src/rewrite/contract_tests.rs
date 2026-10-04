// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for `rewrite` (squash): what each crash point leaves and
//! how the next call with the same request id resumes, how the base is
//! resolved without `--onto`, and the refusals that keep HEAD untouched.

use super::*;
use tempfile::TempDir;

/// Real git through the runner, isolated from the developer's config.
fn git(root: &Path, args: &[&str]) -> String {
    let out = GitRunner::new(root)
        .run_isolated(args)
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8_lossy(&out).trim().to_string()
}

fn commit(root: &Path, name: &str, msg: &str) {
    std::fs::write(root.join(name), msg).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", msg]);
}

/// `main` with one commit, `feature` with two more on top; no remote.
struct Fixture {
    work: TempDir,
    state: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let work = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let root = work.path();
        git(root, &["init", "-q", "-b", "main"]);
        // `rewrite` commits through a plain runner: the repo carries the
        // identity so the developer's global config is never needed.
        git(root, &["config", "user.name", "t"]);
        git(root, &["config", "user.email", "t@t"]);
        commit(root, "base.txt", "base");
        git(root, &["checkout", "-q", "-b", "feature"]);
        commit(root, "one.txt", "wip one");
        commit(root, "two.txt", "wip two");
        Self { work, state }
    }

    fn root(&self) -> &Path {
        self.work.path()
    }

    fn head(&self) -> String {
        git(self.root(), &["rev-parse", "HEAD"])
    }

    fn run(&self, o: &RewriteOptions, probe: Option<RewriteProbe>) -> Result<Value, String> {
        rewrite_with_state(self.root(), o, probe, self.state.path())
    }
}

fn opts(onto: Option<&str>) -> RewriteOptions {
    RewriteOptions {
        onto: onto.map(ToString::to_string),
        message: None,
        push: false,
        remote: "origin".to_string(),
        request_id: format!("rw-{}", uuid::Uuid::new_v4()),
        expected_head: None,
        allow_default_branch: false,
    }
}

fn crash_at(phase: &'static str) -> RewriteProbe {
    Box::new(move |p: &str| {
        if p == phase {
            Err(format!("simulated crash at {p}"))
        } else {
            Ok(())
        }
    })
}

// --- crash points before the mutation window -----------------------------------

#[test]
fn rewrite_should_leave_head_untouched_when_it_crashes_before_the_reset() {
    for phase in [
        "journal:started",
        "backup:written",
        "journal:ref_update_started",
    ] {
        let fx = Fixture::new();
        let before = fx.head();
        let err = fx
            .run(&opts(Some("main")), Some(crash_at(phase)))
            .unwrap_err();
        assert_eq!(err, format!("simulated crash at {phase}"));
        assert_eq!(fx.head(), before, "{phase}: HEAD must not move");
    }
}

#[test]
fn rewrite_should_squash_on_resume_after_a_crash_before_the_backup() {
    for phase in ["journal:started", "backup:written"] {
        let fx = Fixture::new();
        let o = opts(Some("main"));
        fx.run(&o, Some(crash_at(phase))).unwrap_err();
        let result = fx.run(&o, None).unwrap();
        assert_eq!(result["state"], json!("squashed"), "{phase}");
        assert_eq!(result["commits_squashed"], json!(2), "{phase}");
        assert_eq!(git(fx.root(), &["rev-list", "--count", "main..HEAD"]), "1");
    }
}

// --- crash points after the squash commit ----------------------------------------

#[test]
fn rewrite_should_complete_without_pushing_when_resumed_after_the_commit_was_observed() {
    let fx = Fixture::new();
    let old_head = fx.head();
    let mut o = opts(Some("main"));
    o.push = true;
    fx.run(&o, Some(crash_at("journal:commit_observed")))
        .unwrap_err();
    let squashed_head = fx.head();
    assert_ne!(squashed_head, old_head, "the squash commit exists");

    let result = fx.run(&o, None).unwrap();
    assert_eq!(result["state"], json!("squashed"));
    assert_eq!(result["pushed"], json!(false));
    assert_eq!(result["old_head"], json!(old_head));
    assert_eq!(result["new_head"], json!(squashed_head));
    assert_eq!(result["commits_squashed"], json!(2));
    assert_eq!(
        result["backup_ref"],
        json!("refs/pixel/rewrite-backup/feature")
    );
    assert_eq!(
        result["warnings"],
        json!(["resumed after crash; push (if requested) was not attempted"])
    );
    assert_eq!(fx.head(), squashed_head, "resume never re-squashes");
}

#[test]
fn rewrite_should_refuse_to_retry_a_push_that_may_have_started() {
    let fx = Fixture::new();
    let mut o = opts(Some("main"));
    o.push = true;
    fx.run(&o, Some(crash_at("journal:push_started")))
        .unwrap_err();
    let err = fx.run(&o, None).unwrap_err();
    assert!(err.starts_with("NETWORK_AMBIGUITY:"), "{err}");
}

#[test]
fn rewrite_should_replay_the_recorded_result_after_a_crash_past_completion() {
    let fx = Fixture::new();
    let o = opts(Some("main"));
    fx.run(&o, Some(crash_at("journal:terminal"))).unwrap_err();
    let head = fx.head();
    let replay = fx.run(&o, None).unwrap();
    assert_eq!(replay["new_head"], json!(head));
    assert_eq!(fx.head(), head);
}

#[test]
fn rewrite_should_record_a_rejected_push_as_stale_remote_without_failing_the_squash() {
    let fx = Fixture::new();
    let mut o = opts(Some("main"));
    o.push = true;
    // `origin` does not exist: the leased push fails, the squash stands.
    let result = fx.run(&o, None).unwrap();
    assert_eq!(result["pushed"], json!(false));
    let push_error = result["push_error"].as_str().unwrap();
    assert!(push_error.starts_with("STALE_REMOTE:"), "{push_error}");
    assert!(
        push_error.contains("<absent>"),
        "never pushed: {push_error}"
    );
    assert_eq!(git(fx.root(), &["rev-list", "--count", "main..HEAD"]), "1");
}

// --- base resolution ------------------------------------------------------------------

#[test]
fn rewrite_should_refuse_an_onto_that_is_not_a_commit() {
    let fx = Fixture::new();
    let before = fx.head();
    let err = fx.run(&opts(Some("no-such-ref")), None).unwrap_err();
    assert_eq!(
        err,
        "REFUSED: --onto no-such-ref does not resolve to a commit"
    );
    assert_eq!(fx.head(), before);
}

#[test]
fn rewrite_should_squash_onto_the_upstream_merge_base_when_no_onto_is_given() {
    let fx = Fixture::new();
    git(fx.root(), &["branch", "--set-upstream-to=main"]);
    let main = git(fx.root(), &["rev-parse", "main"]);
    let result = fx.run(&opts(None), None).unwrap();
    assert_eq!(result["base_oid"], json!(main));
    assert_eq!(result["commits_squashed"], json!(2));
}

#[test]
fn rewrite_should_refuse_when_no_base_can_be_resolved() {
    let fx = Fixture::new();
    let before = fx.head();
    let err = fx.run(&opts(None), None).unwrap_err();
    assert!(
        err.starts_with("REFUSED: no base resolvable for feature"),
        "{err}"
    );
    assert_eq!(fx.head(), before);
}

#[test]
fn rewrite_should_refuse_an_invalid_remote_name() {
    let fx = Fixture::new();
    let mut o = opts(Some("main"));
    o.remote = "-upload-pack=x".to_string();
    assert!(fx.run(&o, None).is_err());
    assert_eq!(git(fx.root(), &["rev-list", "--count", "main..HEAD"]), "2");
}

// --- commit failure -------------------------------------------------------------------

#[test]
fn rewrite_should_restore_the_branch_when_the_squash_commit_fails() {
    let fx = Fixture::new();
    let before = fx.head();
    let mut o = opts(Some("main"));
    // git refuses an empty commit message.
    o.message = Some(String::new());
    let err = fx.run(&o, None).unwrap_err();
    assert!(
        err.starts_with(&format!(
            "git commit failed during squash; branch restored to {before}"
        )),
        "{err}"
    );
    assert_eq!(fx.head(), before);
    assert_eq!(
        git(fx.root(), &["diff", "--cached", "--name-only"]),
        "",
        "nothing left staged"
    );
}

// --- replay identity ------------------------------------------------------------------

#[test]
fn rewrite_should_refuse_reusing_a_request_id_with_different_options() {
    let fx = Fixture::new();
    let o = opts(Some("main"));
    fx.run(&o, None).unwrap();
    let mut changed = o.clone();
    changed.message = Some("another message".to_string());
    let err = fx.run(&changed, None).unwrap_err();
    assert_eq!(
        err,
        format!(
            "idempotency conflict: requestId {} already used with different operation/input",
            o.request_id
        )
    );
}

#[test]
fn rewrite_input_hash_should_change_with_every_option() {
    let base = opts(Some("main"));
    let h = rewrite_input_hash(&base);
    let variants: Vec<RewriteOptions> = vec![
        RewriteOptions {
            onto: Some("other".into()),
            ..base.clone()
        },
        RewriteOptions {
            message: Some("m".into()),
            ..base.clone()
        },
        RewriteOptions {
            push: true,
            ..base.clone()
        },
        RewriteOptions {
            remote: "upstream".into(),
            ..base.clone()
        },
        RewriteOptions {
            expected_head: Some("abc".into()),
            ..base.clone()
        },
        RewriteOptions {
            allow_default_branch: true,
            ..base.clone()
        },
    ];
    for v in variants {
        assert_ne!(rewrite_input_hash(&v), h, "{v:?}");
    }
    let same_but_new_id = RewriteOptions {
        request_id: "other".into(),
        ..base.clone()
    };
    assert_eq!(rewrite_input_hash(&same_but_new_id), h);
}
