// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use super::*;
use std::time::{Duration, Instant};

use crate::chain::{CallerHit, Evidence, Found, SymbolHit};
use crate::steps::testing::{Repo, texts};
use crate::steps::{Outcome, Steps, blocks as run_steps};

const WORKSPACE: &[(&str, &str)] = &[
    ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
    (
        "README.md",
        "# Demo\n\n[![ci](x)](y)\n\nDemo is a toolkit for parsing and formatting things.\n\nSecond paragraph.\n",
    ),
    (
        "crates/alpha/Cargo.toml",
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nauthors = [\"Someone\"]\nlicense = \"MIT\"\ndescription = \"Alpha parses things\"\n\n[dependencies]\nbeta = { path = \"../beta\" }\nserde = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n\n[features]\nextra = []\n",
    ),
    (
        "crates/alpha/src/lib.rs",
        "//! Alpha is the facade.\n//! It parses.\n\npub struct Api;\n",
    ),
    ("crates/alpha/tests/it.rs", "fn t() {}\n"),
    ("crates/alpha/examples/demo.rs", "fn main() {}\n"),
    (
        "crates/beta/Cargo.toml",
        "[package]\nname = \"beta\"\ndescription = \"Beta formats\"\n\n[dependencies]\nregex = \"1\"\n",
    ),
    ("crates/beta/src/lib.rs", "//! Beta formats text.\n"),
    (
        "crates/gamma-core/Cargo.toml",
        "[package]\nname = \"gamma-core\"\n",
    ),
    ("crates/gamma-core/src/main.rs", "fn main() {}\n"),
];

#[test]
fn a_workspace_question_gets_one_line_per_member_and_the_readme_lead() {
    let repo = Repo::new(WORKSPACE);
    let blocks = workspace_overview("which crates make up the workspace?", &repo.tree());
    assert_eq!(
        texts(&blocks),
        vec![
            "### workspace — 3 members (cargo)\n\
             crates/alpha — Alpha parses things\n\
             crates/beta — Beta formats\n\
             crates/gamma-core",
            "### README — lead\n\
             Demo is a toolkit for parsing and formatting things.",
        ]
    );
}

#[test]
fn a_named_member_gets_its_description_dependencies_and_source_files() {
    let repo = Repo::new(WORKSPACE);
    let blocks = workspace_overview("what does the alpha crate depend on", &repo.tree());
    assert_eq!(
        texts(&blocks),
        vec![
            "### crates/alpha — Alpha parses things\n\
             depends on (workspace): beta\n\
             depends on (external): serde\n\
             dev (external): tempfile\n\
             files: src/lib.rs"
        ]
    );
}

#[test]
fn a_package_name_with_a_dash_is_named_without_a_noun() {
    let repo = Repo::new(WORKSPACE);
    let blocks = workspace_overview("how is the gamma-core crate built", &repo.tree());
    assert_eq!(
        texts(&blocks),
        vec!["### crates/gamma-core\nfiles: src/main.rs"]
    );
    // A bare short name is not enough: `beta` alone may be any word.
    assert!(workspace_overview("is beta fast", &repo.tree()).is_empty());
    assert_eq!(
        texts(&workspace_overview("how is gamma-core built", &repo.tree())),
        vec!["### crates/gamma-core\nfiles: src/main.rs"],
        "the full package name needs no noun"
    );
}

#[test]
fn a_prompt_about_neither_the_workspace_nor_a_member_gets_nothing() {
    let repo = Repo::new(WORKSPACE);
    assert!(workspace_overview("fix the typo in the parser", &repo.tree()).is_empty());
}

#[test]
fn a_repository_with_fewer_than_two_members_is_no_workspace() {
    let repo = Repo::new(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/one/Cargo.toml", "[package]\nname = \"one\"\n"),
    ]);
    assert!(workspace_overview("which crates make up the workspace?", &repo.tree()).is_empty());
    let plain = Repo::new(&[("Cargo.toml", "[package]\nname = \"solo\"\n")]);
    assert!(workspace_overview("what is the layout", &plain.tree()).is_empty());
}

#[test]
fn the_root_package_and_inherited_descriptions_are_listed() {
    let repo = Repo::new(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\"]\n[workspace.package]\ndescription = \"Shared words\"\n[package]\nname = \"root\"\ndescription = \"The root\"\n",
        ),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\ndescription.workspace = true\n",
        ),
        ("b/Cargo.toml", "[package]\nname = \"bee\"\n"),
    ]);
    let blocks = workspace_overview("describe the workspace layout", &repo.tree());
    assert_eq!(
        texts(&blocks),
        vec![
            "### workspace — 2 members (cargo)\n\
             . [root] (root) — The root\n\
             a — Shared words\n\
             b [bee]"
        ]
    );
}

#[test]
fn npm_and_pnpm_workspaces_are_read_from_their_manifests() {
    let npm = Repo::new(&[
        (
            "package.json",
            "{\"name\":\"root\",\"workspaces\":[\"packages/*\"]}",
        ),
        (
            "packages/ui/package.json",
            "{\"name\":\"@acme/ui\",\"description\":\"Widgets\",\"dependencies\":{\"@acme/core\":\"1\",\"react\":\"18\"}}",
        ),
        (
            "packages/core/package.json",
            "{\"name\":\"@acme/core\",\"devDependencies\":{\"vitest\":\"1\"}}",
        ),
    ]);
    assert_eq!(
        texts(&workspace_overview(
            "what packages make up this monorepo",
            &npm.tree()
        )),
        vec![
            "### workspace — 2 members (npm)\n\
             packages/core [@acme/core]\n\
             packages/ui [@acme/ui] — Widgets"
        ]
    );
    let pnpm = Repo::new(&[
        (
            "pnpm-workspace.yaml",
            "packages:\n  - 'apps/*'\n  - \"!apps/old\"\nother: 1\n",
        ),
        ("apps/web/package.json", "{\"name\":\"web\"}"),
        ("apps/api/package.json", "{\"name\":\"api\"}"),
        ("apps/old/package.json", "{\"name\":\"old\"}"),
    ]);
    assert_eq!(
        texts(&workspace_overview(
            "what is the workspace layout",
            &pnpm.tree()
        )),
        vec!["### workspace — 2 members (npm)\napps/api\napps/web"]
    );
}

#[test]
fn go_and_uv_workspaces_are_read_from_their_manifests() {
    let go = Repo::new(&[
        (
            "go.work",
            "go 1.22\n\nuse (\n\t./svc.a // the first\n\t./tool\n)\n",
        ),
        (
            "svc.a/go.mod",
            "module example.com/svc\n\nrequire (\n\tgithub.com/x/y v1\n\tgithub.com/z/w v2 // indirect\n)\n",
        ),
        ("tool/go.mod", "module example.com/tool\n"),
    ]);
    assert_eq!(
        texts(&workspace_overview(
            "what modules make up the workspace",
            &go.tree()
        )),
        vec![
            "### workspace — 2 members (go)\n\
             svc.a [example.com/svc]\n\
             tool [example.com/tool]"
        ]
    );
    let uv = Repo::new(&[
        (
            "pyproject.toml",
            "[tool.uv.workspace]\nmembers = [\"libs/*\"]\nexclude = [\"libs/old\"]\n",
        ),
        (
            "libs/a/pyproject.toml",
            "[project]\nname = \"a\"\ndependencies = [\"requests[socks]>=2\"]\n",
        ),
        (
            "libs/b/pyproject.toml",
            "[project]\nname = \"b\"\ndescription = \"Bee\"\n",
        ),
        ("libs/old/pyproject.toml", "[project]\nname = \"old\"\n"),
    ]);
    assert_eq!(
        texts(&workspace_overview(
            "what is the workspace layout",
            &uv.tree()
        )),
        vec!["### workspace — 2 members (python)\nlibs/a\nlibs/b — Bee"]
    );
}

#[test]
fn workspace_globs_match_like_fnmatch_and_a_bang_excludes() {
    let dirs: Vec<String> = ["crates/a", "crates/b", "tools/x", "crates/deep/c"]
        .map(String::from)
        .to_vec();
    let patterns = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        expand(&patterns(&["crates/*"]), &dirs),
        vec!["crates/a", "crates/b", "crates/deep/c"]
    );
    assert_eq!(
        expand(&patterns(&["./crates/**", "!crates/b"]), &dirs),
        vec!["crates/a", "crates/deep/c"]
    );
    assert_eq!(expand(&patterns(&["tools/?"]), &dirs), vec!["tools/x"]);
    assert!(fnmatch("a*c", "abbbc") && !fnmatch("a*c", "abbbd") && fnmatch("*", ""));
}

#[test]
fn a_description_is_cut_at_a_word_with_an_ellipsis() {
    assert_eq!(one_line("  spaced \n out  ", 80), "spaced out");
    // The last word of the cut goes, complete or not, and a trailing
    // punctuation mark with it.
    assert_eq!(
        one_line("alpha beta, gamma delta epsilon", 17),
        "alpha beta..."
    );
    assert_eq!(one_line("alpha beta, gamma", 11), "alpha...");
    assert_eq!(one_line("unbroken", 4), "unbr...");
}

#[test]
fn the_readme_lead_skips_headings_badges_and_tables_and_stops_at_the_paragraph() {
    let lead = |text: &str| {
        let repo = Repo::new(&[("README.md", text)]);
        readme_lead(&repo.tree())
    };
    assert_eq!(
        lead("Title\n=====\n\n<p>logo</p>\n\nFirst line\ncontinues here.\n\nNext paragraph.\n"),
        "First line continues here."
    );
    assert_eq!(
        lead("# T\n\n| a | b |\n\n- bullet\n\nLicensed under MIT.\n"),
        ""
    );
    let long = format!("{}. {}", "word ".repeat(30).trim_end(), "tail ".repeat(60));
    let cut = lead(&long);
    assert!(cut.ends_with("word."), "{cut}");
    let unbroken = lead(&"x".repeat(500));
    assert_eq!(unbroken, format!("{}...", "x".repeat(380)));
}

#[test]
fn a_long_list_of_dependencies_and_files_says_how_many_more() {
    let mut files: Vec<(String, String)> = vec![
        (
            "Cargo.toml".into(),
            "[workspace]\nmembers = [\"a\", \"b\"]\n".into(),
        ),
        ("b/Cargo.toml".into(), "[package]\nname = \"b\"\n".into()),
    ];
    let deps: String = (0..16).map(|n| format!("d{n:02} = \"1\"\n")).collect();
    files.push((
        "a/Cargo.toml".into(),
        format!("[package]\nname = \"a\"\n[dependencies]\n{deps}"),
    ));
    for n in 0..20 {
        files.push((format!("a/src/m{n:02}.rs"), String::new()));
    }
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let repo = Repo::new(&borrowed);
    let blocks = workspace_overview("what does the a package depend on", &repo.tree());
    let text = &texts(&blocks)[0];
    assert!(
        text.contains("depends on (external): d00, d01, d02, d03, d04, d05, d06, d07, d08, d09, d10, d11, d12, d13 +2 more"),
        "{text}"
    );
    assert!(
        text.contains("files: src/m00.rs") && text.contains("+4 more"),
        "{text}"
    );
}

#[test]
fn a_member_manifest_cuts_identity_lines_and_lists_the_directory() {
    let repo = Repo::new(WORKSPACE);
    let (member, blocks) =
        member_manifest("what does the alpha crate contain?", &repo.tree()).unwrap();
    assert_eq!(member, "crates/alpha");
    assert_eq!(
        texts(&blocks),
        vec![
            "### crates/alpha/Cargo.toml:1-16 — manifest of alpha (identity lines cut)\n\
             1| [package]\n\
             2| name = \"alpha\"\n\
             3| version = \"0.1.0\"\n\
             6| description = \"Alpha parses things\"\n\
             7| \n\
             8| [dependencies]\n\
             9| beta = { path = \"../beta\" }\n\
             10| serde = \"1\"\n\
             11| \n\
             12| [dev-dependencies]\n\
             13| tempfile = \"3\"\n\
             14| \n\
             15| [features]\n\
             16| extra = []",
            "### crates/alpha/ — 4 tracked files\n\
             Cargo.toml, examples/demo.rs, src/lib.rs, tests/it.rs, +0 more"
                .replace(", +0 more", "")
                .as_str(),
        ]
    );
    assert!(member_manifest("what does the nowhere crate contain?", &repo.tree()).is_none());
    assert!(member_manifest("what does this crate contain?", &repo.tree()).is_none());
}

#[test]
fn a_member_manifest_names_what_the_listing_left_out() {
    let mut files = vec![("w/Cargo.toml".to_string(), "[package]\n".to_string())];
    files.extend((0..30).map(|n| (format!("w/f{n:02}.txt"), String::new())));
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let repo = Repo::new(&borrowed);
    let (_, blocks) = member_manifest("the w crate", &repo.tree()).unwrap();
    let listing = &texts(&blocks)[1];
    assert!(
        listing.starts_with("### w/ — 31 tracked files\nCargo.toml, f00.txt"),
        "{listing}"
    );
    assert!(listing.ends_with(", +7 more"), "{listing}");
}

#[test]
fn the_rest_of_the_pack_is_cut_to_the_member_directory() {
    assert!(inside("crates/alpha/src/lib.rs:1-9 — x", "crates/alpha"));
    assert!(inside("crates/alpha/ — 4 tracked files", "crates/alpha"));
    assert!(!inside("crates/alphabet/src/lib.rs:1-9", "crates/alpha"));
    assert!(!inside("crates/beta/Cargo.toml:1-2", "crates/alpha"));
}

#[test]
fn a_crate_question_gets_its_docs_manifest_facade_files_dependencies_and_examples() {
    let mut files: Vec<(&str, &str)> = WORKSPACE.to_vec();
    files.push(("AGENTS.md", "Build with cargo.\nTest with nextest.\n"));
    let repo = Repo::new(&files);
    let blocks = answer_evidence("what does the alpha crate expose?", &repo.tree());
    let heads: Vec<&str> = blocks.iter().map(Block::head).collect();
    assert_eq!(
        heads,
        vec![
            "Repository instructions",
            "crates/alpha/src/lib.rs — crate purpose and public surface",
            "crates/alpha/Cargo.toml — line-numbered package manifest",
            "crates/alpha/src/lib.rs — line-numbered crate facade",
            "Tracked files in crates/alpha (4 total)",
            "crates/alpha/examples/demo.rs — example source",
        ]
    );
    let all = texts(&blocks);
    assert_eq!(
        all[0],
        "### Repository instructions\nAGENTS.md:\nBuild with cargo.\nTest with nextest."
    );
    assert_eq!(
        all[1],
        "### crates/alpha/src/lib.rs — crate purpose and public surface\n\
         Alpha parses things\nAlpha is the facade.\nIt parses."
    );
    assert!(
        all[2].contains("\n9| beta = { path = \"../beta\" }"),
        "{}",
        all[2]
    );
    assert_eq!(
        all[3],
        "### crates/alpha/src/lib.rs — line-numbered crate facade\n\
         1| //! Alpha is the facade.\n2| //! It parses.\n3| \n4| pub struct Api;"
    );
    assert_eq!(
        all[4],
        "### Tracked files in crates/alpha (4 total)\n\
         Cargo.toml\nexamples/demo.rs\nsrc/lib.rs\ntests/it.rs\n\
         dependencies:\n\
         - beta — Beta formats\n  crates/beta/src/lib.rs — leading module documentation\n  Beta formats text.\n\
         - serde\n\
         development dependencies:\n\
         - tempfile"
    );
    assert_eq!(
        all[5],
        "### crates/alpha/examples/demo.rs — example source\n1| fn main() {}"
    );
}

#[test]
fn a_crate_question_without_agents_files_says_so() {
    let repo = Repo::new(WORKSPACE);
    let blocks = answer_evidence("what does the alpha crate expose?", &repo.tree());
    assert_eq!(
        blocks[0].text(),
        "### Repository instructions\nNo tracked AGENTS.md instruction files are present."
    );
}

#[test]
fn a_crate_question_needs_exactly_one_member_and_a_readable_library_root() {
    let repo = Repo::new(WORKSPACE);
    // Two members named: no single crate to describe.
    assert!(answer_evidence("compare the alpha crate with beta", &repo.tree()).is_empty());
    // None named.
    assert!(answer_evidence("what does the library expose?", &repo.tree()).is_empty());
    // A member that is only a prefix of another name is not named.
    assert!(answer_evidence("what does gamma expose?", &repo.tree()).is_empty());
    // gamma-core has no `src/lib.rs`.
    assert!(answer_evidence("what does the gamma-core crate expose?", &repo.tree()).is_empty());
    // The directory name names it too, whatever the package is called.
    let renamed = Repo::new(&[
        ("pkg/Cargo.toml", "[package]\nname = \"other-name\"\n"),
        ("pkg/src/lib.rs", "//! Doc.\n"),
    ]);
    assert_eq!(
        answer_evidence("the pkg library", &renamed.tree())[1].head(),
        "pkg/src/lib.rs — crate purpose and public surface"
    );
}

#[test]
fn the_manifest_names_the_library_root_that_replaces_the_default() {
    let repo = Repo::new(&[
        (
            "member/Cargo.toml",
            "[package]\nname = \"member\"\n[lib]\npath = \"src/api.rs\"\n",
        ),
        (
            "member/src/api.rs",
            "//! The configured library target.\npub struct Api;\n",
        ),
        (
            "member/src/lib.rs",
            "//! Not the selected target.\npub struct Private;\n",
        ),
    ]);
    let blocks = answer_evidence("the member crate", &repo.tree());
    let text = texts(&blocks).join("\n");
    assert!(
        text.contains("member/src/api.rs — line-numbered crate facade"),
        "{text}"
    );
    assert!(
        text.contains("pub struct Api") && !text.contains("Private"),
        "{text}"
    );

    let default = Repo::new(&[
        ("member/Cargo.toml", "[package]\nname = \"member\"\n"),
        (
            "member/src/lib.rs",
            "//! The public crate facade.\npub struct Api;\n",
        ),
        (
            "member/src/internal.rs",
            "//! Private details.\npub(crate) struct Internal;\n",
        ),
    ]);
    let text = texts(&answer_evidence("the member crate", &default.tree())).join("\n");
    assert!(
        text.contains("pub struct Api") && !text.contains("Internal;"),
        "{text}"
    );
}

#[test]
fn a_repository_of_thousands_of_files_gets_no_crate_answer() {
    let mut files = vec![
        (
            "member/Cargo.toml".to_string(),
            "[package]\nname = \"member\"\n".to_string(),
        ),
        ("member/src/lib.rs".to_string(), "//! Doc.\n".to_string()),
    ];
    files.extend((0..MAX_TRACKED_PATHS).map(|n| (format!("vendor/f{n}.txt"), String::new())));
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let repo = Repo::new(&borrowed);
    assert!(answer_evidence("the member crate", &repo.tree()).is_empty());
}

#[test]
fn the_crate_answer_stops_once_its_character_budget_is_used() {
    let long_doc: String = (0..100)
        .map(|_| format!("//! {}\n", "x".repeat(170)))
        .collect();
    let example: String = (0..65)
        .map(|_| format!("let _ = \"{}\";\n", "y".repeat(170)))
        .collect();
    let repo = Repo::new(&[
        ("member/Cargo.toml", "[package]\nname = \"member\"\n"),
        ("member/src/lib.rs", long_doc.as_str()),
        ("member/examples/a.rs", example.as_str()),
        ("member/examples/b.rs", example.as_str()),
        ("member/examples/c.rs", example.as_str()),
    ]);
    let blocks = answer_evidence("the member crate", &repo.tree());
    let heads: Vec<&str> = blocks.iter().map(Block::head).collect();
    assert_eq!(
        heads.last(),
        Some(&"evidence truncated at the configured character budget"),
        "{heads:?}"
    );
    assert!(
        !heads.contains(&"member/examples/c.rs — example source"),
        "{heads:?}"
    );
}

#[test]
fn leading_docs_reads_line_and_block_documentation_and_stops_at_code() {
    assert_eq!(
        leading_docs("\n//! One.\n//!\n//! Two.\nuse x;\n//! Three.\n", 60),
        vec!["One.", "Two."]
    );
    assert_eq!(
        leading_docs("/*! Block title\n * more\n * text */\nfn x() {}\n", 60),
        vec!["Block title", "more", "text"]
    );
    assert!(leading_docs("fn x() {}\n//! late\n", 60).is_empty());
    let long = format!("//! {}\n", "y".repeat(300));
    assert_eq!(leading_docs(&long, 60)[0].chars().count(), 237);
    assert_eq!(leading_docs("//! a\n//! b\n//! c\n", 2), vec!["a", "b"]);
}

#[test]
fn the_steps_answer_by_kind_and_each_flag_switches_one_off() {
    let repo = Repo::new(WORKSPACE);
    let deadline = Instant::now() + Duration::from_secs(5);
    let ask = |typed: &str, kind, steps| run_steps(typed, kind, steps, &[], &repo, deadline);

    let overview = ask(
        "which crates make up the workspace?",
        QuestionKind::Overview,
        Steps::default(),
    );
    assert_eq!(overview.lead.len(), 2);
    assert!(overview.append.is_empty() && overview.drops.is_empty());

    let off = Steps::default()
        .without(&["workspace-overview".to_string()])
        .unwrap();
    assert!(
        ask(
            "which crates make up the workspace?",
            QuestionKind::Overview,
            off
        )
        .lead
        .is_empty()
    );

    // The manifest step appends, names the directory to keep, and is not
    // asked of a flow question.
    let contain = ask(
        "what does the alpha crate contain?",
        QuestionKind::Overview,
        Steps::default(),
    );
    assert!(contain.drops.drops_head("crates/beta/src/lib.rs:1-3 — x"));
    assert!(!contain.drops.drops_head("crates/alpha/src/lib.rs:1-3 — x"));
    assert_eq!(contain.append.len(), 2);
    assert!(
        ask(
            "what does the alpha crate contain?",
            QuestionKind::Flow,
            Steps::default()
        )
        .append
        .is_empty()
    );

    // The listing of files waits for a word that asks about members: a
    // bare package name in a lookup question reads nothing.
    assert_eq!(
        ask(
            "how is gamma-core built",
            QuestionKind::Lookup,
            Steps::default()
        ),
        Outcome::default()
    );
    assert_eq!(
        ask(
            "how is gamma-core built",
            QuestionKind::Overview,
            Steps::default()
        )
        .lead
        .len(),
        1
    );

    // A history question gets none of the three.
    assert_eq!(
        ask(
            "what does the alpha crate contain?",
            QuestionKind::History,
            Steps::default()
        ),
        Outcome::default()
    );
    // The crate step is not asked of a review.
    let review = ask(
        "what does the alpha crate expose?",
        QuestionKind::Review,
        Steps::default(),
    );
    assert!(
        review
            .lead
            .iter()
            .all(|b| b.head() != "Repository instructions")
    );
    let lookup = ask(
        "what does the alpha crate expose?",
        QuestionKind::Lookup,
        Steps::default(),
    );
    assert!(
        lookup
            .lead
            .iter()
            .any(|b| b.head() == "Repository instructions")
    );
}

#[test]
fn a_source_that_cannot_list_files_adds_nothing() {
    struct Blind;
    impl Evidence for Blind {
        fn files_with(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("x".into())
        }
        fn concept(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("x".into())
        }
        fn symbols(&self, _: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            Err("x".into())
        }
        fn callers(&self, _: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
            Err("x".into())
        }
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut out = run_steps(
        "which crates make up the workspace?",
        QuestionKind::Overview,
        Steps::default(),
        &[],
        &Blind,
        deadline,
    );
    // Steps that asked and found nothing are timed; their milliseconds are not
    // what this case is about.
    out.timings.clear();
    assert_eq!(out, Outcome::default());
}
