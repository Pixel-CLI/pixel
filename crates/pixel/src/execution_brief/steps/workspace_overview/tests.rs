// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `probes/workspace-overview` has no test file: these are the cases its
//! docstring promises, run on the step.

use super::super::testing::{repo, run};
use super::*;

fn ask(files: &[(&str, &str)], prompt: &str) -> String {
    let dir = repo(files);
    run(&STEP, dir.path(), prompt, "")
}

const CARGO_ROOT: &str = "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\ndescription = \"The shared words\"\n";
const CORE: &str = "[package]\nname = \"acme-core\"\ndescription.workspace = true\n\n[dependencies]\nserde = \"1\"\n";
const CLI: &str = "[package]\nname = \"acme-cli\"\ndescription = \"The command line\"\n\n[dependencies]\nacme-core = { path = \"../core\" }\nclap = \"4\"\nacme_util = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n";
const UTIL: &str = "[package]\nname = \"acme_util\"\ndescription = \"Small helpers\"\n";

fn cargo() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", CARGO_ROOT),
        ("README.md", "# Acme\n\nAcme builds things. It is small.\n\nSecond paragraph.\n"),
        ("crates/core/Cargo.toml", CORE),
        ("crates/core/src/lib.rs", ""),
        ("crates/cli/Cargo.toml", CLI),
        ("crates/cli/src/main.rs", ""),
        ("crates/cli/tests/it.rs", ""),
        ("crates/util/Cargo.toml", UTIL),
    ]
}

#[test]
fn the_step_applies_to_a_workspace_question_and_not_to_history_or_impact() {
    let asked = "which crates make up the workspace";
    assert!(STEP.applies(None, asked, true));
    assert!(STEP.applies(Some("overview"), asked, false));
    assert!(!STEP.applies(Some("history"), asked, false));
    assert!(!STEP.applies(Some("impact"), asked, false));
    assert!(!STEP.applies(None, "where is the parser", true));
}

#[test]
fn an_overview_lists_every_member_and_the_readme_lead() {
    let out = ask(&cargo(), "which crates make up the workspace");
    assert_eq!(
        out,
        [
            "### workspace — 3 members (cargo)",
            "crates/cli [acme-cli] — The command line",
            "crates/core [acme-core] — The shared words",
            "crates/util [acme_util] — Small helpers",
            "### README — lead",
            "Acme builds things. It is small.",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn a_repository_that_is_no_workspace_or_has_one_member_says_nothing() {
    let one = [
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/core/Cargo.toml", CORE),
    ];
    assert_eq!(ask(&one, "which crates make up the workspace"), "");
    let plain = [("Cargo.toml", "[package]\nname = \"solo\"\n")];
    assert_eq!(ask(&plain, "which crates make up the workspace"), "");
    assert_eq!(ask(&cargo(), "where is the parser"), "");
}

#[test]
fn the_root_package_leads_the_overview_with_its_own_line() {
    let mut files = cargo();
    files[0] = (
        "Cargo.toml",
        "[package]\nname = \"acme\"\ndescription = \"The suite\"\n\n[workspace]\nmembers = [\"crates/*\"]\n",
    );
    let out = ask(&files, "what is the workspace layout");
    assert!(
        out.starts_with(
            "### workspace — 3 members (cargo)\n. [acme] (root) — The suite\ncrates/cli [acme-cli] — The command line\n"
        ),
        "{out}"
    );
}

#[test]
fn a_member_named_beside_a_noun_gets_its_manifest_facts() {
    let out = ask(&cargo(), "what does the acme-cli crate depend on");
    assert_eq!(
        out,
        [
            "### crates/cli [acme-cli] — The command line",
            "depends on (workspace): acme-core, acme_util",
            "depends on (external): clap",
            "dev (external): tempfile",
            "files: src/main.rs",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn a_short_name_needs_a_noun_beside_it_and_a_package_name_does_not() {
    let by_noun = ask(&cargo(), "explain the util crate");
    assert!(by_noun.starts_with("### crates/util [acme_util] — Small helpers\n"), "{by_noun}");
    let quoted = ask(&cargo(), "what is crate `util` for");
    assert!(quoted.starts_with("### crates/util"), "{quoted}");
    let possessive = ask(&cargo(), "Util's crate is odd");
    assert!(possessive.starts_with("### crates/util"), "{possessive}");
    // `util` alone, or inside another word, names nothing.
    assert_eq!(ask(&cargo(), "where is the util code"), "");
    assert_eq!(ask(&cargo(), "the utility crate"), "");
    // A distinctive full name is enough, unless it is part of a longer one.
    let by_name = ask(&cargo(), "read acme-core please");
    assert!(by_name.starts_with("### crates/core [acme-core] — The shared words\n"), "{by_name}");
    assert_eq!(ask(&cargo(), "read acme-core-extra please"), "");
    assert_eq!(ask(&cargo(), "read @acme-core please"), "");
}

#[test]
fn at_most_two_named_members_are_described() {
    let out = ask(&cargo(), "compare the cli crate, the core crate and the util crate");
    assert_eq!(out.matches("### ").count(), 2, "{out}");
}

#[test]
fn many_files_and_names_are_cut_with_a_count() {
    let mut deps = String::from("[package]\nname = \"big\"\n\n[dependencies]\n");
    for n in 0..16 {
        deps.push_str(&format!("dep{n:02} = \"1\"\n"));
    }
    let mut files: Vec<(String, String)> = vec![
        ("Cargo.toml".into(), "[workspace]\nmembers = [\"big\", \"small\"]\n".into()),
        ("big/Cargo.toml".into(), deps),
        ("small/Cargo.toml".into(), "[package]\nname = \"small\"\n".into()),
    ];
    for n in 0..18 {
        files.push((format!("big/src/m{n:02}.rs"), String::new()));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let out = ask(&refs, "the big crate");
    assert!(
        out.contains("depends on (external): dep00, dep01, dep02, dep03, dep04, dep05, dep06, dep07, dep08, dep09, dep10, dep11, dep12, dep13 +2 more\n"),
        "{out}"
    );
    assert!(out.contains("files: src/m00.rs, src/m01.rs"), "{out}");
    assert!(out.ends_with("src/m15.rs +2 more\n"), "{out}");
}

#[test]
fn npm_workspaces_are_read_from_package_json_and_from_pnpm() {
    let json = [
        ("package.json", "{\"name\": \"root\", \"workspaces\": [\"packages/*\", \"!packages/skip\"]}"),
        ("packages/a/package.json", "{\"name\": \"@x/a\", \"description\": \"Alpha\", \"dependencies\": {\"@x/b\": \"1\", \"left-pad\": \"1\"}, \"devDependencies\": {\"jest\": \"1\"}}"),
        ("packages/b/package.json", "{\"name\": \"@x/b\"}"),
        ("packages/skip/package.json", "{\"name\": \"@x/skip\"}"),
        ("packages/a/index.ts", ""),
    ];
    let out = ask(&json, "which packages are in this monorepo");
    assert_eq!(
        out,
        "### workspace — 2 members (npm)\npackages/a [@x/a] — Alpha\npackages/b [@x/b]\n"
    );
    let named = ask(&json, "what does the @x/a package use");
    assert_eq!(
        named,
        [
            "### packages/a [@x/a] — Alpha",
            "depends on (workspace): @x/b",
            "depends on (external): left-pad",
            "dev (external): jest",
            "files: index.ts",
            "",
        ]
        .join("\n")
    );
    let pnpm = [
        ("package.json", "{\"name\": \"root\"}"),
        ("pnpm-workspace.yaml", "packages:\n  - 'apps/*' # the apps\n  - \"libs/*\"\ncatalog:\n  - nope\n"),
        ("apps/web/package.json", "{\"name\": \"web\"}"),
        ("libs/ui/package.json", "{\"name\": \"ui\"}"),
    ];
    assert_eq!(
        ask(&pnpm, "list the packages"),
        "### workspace — 2 members (npm)\napps/web\nlibs/ui\n"
    );
}

#[test]
fn a_member_package_json_that_is_not_json_ends_the_step_like_the_script() {
    let files = [
        ("package.json", "{\"workspaces\": [\"p/*\"]}"),
        ("p/a/package.json", "{\"name\": \"a\"}"),
        ("p/b/package.json", "{\"name\": \"b\"}"),
        ("p/c/package.json", "{broken"),
    ];
    assert_eq!(ask(&files, "list the packages"), "");
}

#[test]
fn go_work_and_uv_workspaces_are_read_too() {
    let go = [
        ("go.work", "go 1.22\n\nuse (\n\t./api // the api\n\t./svc\n)\n"),
        ("api/go.mod", "module example.com/api\n\nrequire (\n\tgithub.com/a/b v1.0.0\n\tgithub.com/c/d v1.0.0 // indirect\n)\n\nrequire github.com/e/f v2.0.0\nrequire github.com/g/h v3.0.0 // indirect\n"),
        ("svc/go.mod", "module example.com/svc\n"),
        ("api/main.go", ""),
    ];
    assert_eq!(
        ask(&go, "what modules make up this workspace"),
        "### workspace — 2 members (go)\napi [example.com/api]\nsvc [example.com/svc]\n"
    );
    let named = ask(&go, "what does the api module depend on");
    assert_eq!(
        named,
        "### api [example.com/api]\ndepends on (external): github.com/a/b, github.com/e/f\nfiles: main.go\n"
    );
    let uv = [
        ("pyproject.toml", "[project]\nname = \"top\"\n\n[tool.uv.workspace]\nmembers = [\"pkgs/*\"]\nexclude = [\"pkgs/old\"]\n"),
        ("pkgs/a/pyproject.toml", "[project]\nname = \"pkg-a\"\ndescription = \"A\"\ndependencies = [\"requests[socks]>=2\", \"pkg-b ; python_version > '3'\"]\n\n[dependency-groups]\ndev = [\"pytest>=8\", {include-group = \"x\"}]\n"),
        ("pkgs/b/pyproject.toml", "[project]\nname = \"pkg-b\"\n"),
        ("pkgs/old/pyproject.toml", "[project]\nname = \"pkg-old\"\n"),
    ];
    assert_eq!(
        ask(&uv, "what packages does the workspace have"),
        "### workspace — 2 members (python)\n. [top] (root)\npkgs/a [pkg-a] — A\npkgs/b [pkg-b]\n"
    );
    assert_eq!(
        ask(&uv, "what does the pkg-a package depend on"),
        "### pkgs/a [pkg-a] — A\ndepends on (workspace): pkg-b\ndepends on (external): requests\ndev (external): pytest\n"
    );
}

#[test]
fn the_readme_lead_skips_titles_badges_and_licences_and_stops_at_the_paragraph() {
    let readme = "Acme\n====\n\n[![ci](x)](y)\n<p align=center>\n\nLicensed under MIT.\n\nFirst line\nof the lead.\n\nNot this one.\n";
    let mut files = cargo();
    files[1] = ("README.md", readme);
    let out = ask(&files, "workspace overview");
    assert!(out.ends_with("### README — lead\nFirst line of the lead.\n"), "{out}");
    files[1] = ("README.txt", "- a list\n* another\n");
    assert!(!ask(&files, "workspace overview").contains("### README"));
}

#[test]
fn a_long_lead_is_cut_at_the_last_sentence_inside_380_characters_or_at_a_word() {
    let sentence = "Acme is a very long sentence about nothing in particular, ";
    let long = format!("{}end here. {}", sentence.repeat(3), sentence.repeat(6));
    let mut files = cargo();
    let text = format!("{long}\n");
    files[1] = ("README.md", &text);
    let out = ask(&files, "workspace overview");
    let lead = out.split("### README — lead\n").nth(1).unwrap();
    assert!(lead.ends_with("end here.\n"), "{lead}");
    let words = format!("{}\n", "word ".repeat(100));
    files[1] = ("README.md", &words);
    let out = ask(&files, "workspace overview");
    let lead = out.split("### README — lead\n").nth(1).unwrap();
    assert_eq!(lead, format!("{}...\n", "word ".repeat(76).trim_end()));
}

#[test]
fn one_line_cuts_at_a_word_and_drops_trailing_punctuation() {
    assert_eq!(one_line("a  b\n c", 10), "a b c");
    assert_eq!(one_line("alpha, beta gamma", 10), "alpha...");
    assert_eq!(one_line("alpha-beta; gamma", 11), "alpha-beta...");
    assert_eq!(one_line("abcdefghij", 5), "abcde...");
    assert_eq!(one_line("abcde", 5), "abcde");
}

#[test]
fn fnmatch_reads_stars_marks_and_sets_like_python() {
    assert!(fnmatch("crates/*", "crates/a/b"), "a star crosses a slash");
    assert!(fnmatch("c?ates", "crates"));
    assert!(!fnmatch("c?ates", "cates"));
    assert!(fnmatch("pkg[ab]", "pkga"));
    assert!(!fnmatch("pkg[ab]", "pkgc"));
    assert!(fnmatch("pkg[!ab]", "pkgc"));
    assert!(!fnmatch("pkg[!ab]", "pkga"));
    assert!(fnmatch("v[0-9]", "v7"));
    assert!(!fnmatch("v[0-9]", "va"));
    assert!(fnmatch("a[]b", "a[]b"), "an unclosed set is a literal bracket");
    assert!(fnmatch("a[b", "a[b"));
    assert!(fnmatch("[]]", "]"));
    assert!(fnmatch("a*b*c", "aXXbYYc"));
    assert!(!fnmatch("a*b*c", "aXXbYY"));
    assert!(fnmatch("*", ""));
    assert!(!fnmatch("a", "ab"));
    assert!(fnmatch("a**", "a"));
}

#[test]
fn expand_applies_negations_and_cleans_the_globs() {
    let dirs: Vec<String> = ["apps/a", "libs/b", "libs/x/y", "tools"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let globs = |items: &[&str]| -> Vec<String> { items.iter().map(ToString::to_string).collect() };
    assert_eq!(expand(&globs(&["./libs/**"]), &dirs), ["libs/b", "libs/x/y"]);
    assert_eq!(expand(&globs(&["apps/", "/tools"]), &dirs), ["tools"]);
    assert_eq!(expand(&globs(&["*", "!libs/*"]), &dirs), ["apps/a", "tools"]);
}

#[test]
fn a_dependency_name_stops_at_the_first_specifier_character() {
    for (spec, name) in [
        ("requests[socks]>=2", "requests"),
        ("  pkg-b ; python_version>'3'", "pkg-b"),
        ("tool @ https://x", "tool"),
        ("a<2", "a"),
        ("a(>=1)", "a"),
        ("plain", "plain"),
    ] {
        assert_eq!(dep_name(spec), name, "{spec}");
    }
}

#[test]
fn yaml_packages_reads_the_dash_items_under_packages_only() {
    let text = "name: x\npackages:\n  - a\n  - 'b' # note\n# comment\n- c\nother:\n  - d\npackages :\n  - e\n";
    assert_eq!(yaml_packages(text), ["a", "b", "c", "e"]);
    assert_eq!(yaml_packages("packages: [a, b]\n"), Vec::<String>::new());
}
