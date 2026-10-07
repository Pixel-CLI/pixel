// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Documentation drift: the docs and the agent prompts name commands, and
//! the binary is the only source of truth for which commands exist.
//!
//! Two contracts, both against `pixel --help` of the binary built from this
//! checkout:
//!
//! - every backticked `` `pixel <name>` `` in the repository docs and in the
//!   bundled agent prompts and the website's comparison and answer pages,
//!   and every `<code>pixel <name>` in the website's landing page and its
//!   objections, alternatives and answers data, names a real subcommand (a
//!   removed or renamed command cannot linger in prose);
//! - every subcommand appears in ARCHITECTURE.md's `## Command surface`
//!   table, in `pixel --help` order (a new command cannot ship
//!   undocumented);
//! - ARCHITECTURE.md's `## Crates` table has one row per workspace member,
//!   and its `Depends on` cell matches that member's `Cargo.toml`;
//! - the docs page's per-project list and `pixel install --help` name exactly
//!   the files `pixel install --repo` writes (`REPO_ARTIFACTS`).
//!
//! `pixel doctor` already dry-runs the *installed* prompt's command lines
//! against the parser at run time; this test does it for the tree at build
//! time, where a PR can still fix it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Subcommand names from the `Commands:` block of `pixel --help`.
fn subcommands() -> BTreeSet<String> {
    subcommands_in_help_order().into_iter().collect()
}

/// Subcommand names from the `Commands:` block of `pixel --help`, in the
/// order clap prints them.
fn subcommands_in_help_order() -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_pixel"))
        .arg("--help")
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let block = text
        .split("Commands:")
        .nth(1)
        .and_then(|rest| rest.split("Options:").next())
        .expect("clap help has Commands: then Options:");
    let names: Vec<String> = block
        .lines()
        .filter_map(|l| l.strip_prefix("  "))
        .filter(|l| !l.starts_with(' '))
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect();
    assert!(names.len() > 20, "help parsing broke: {names:?}");
    names
}

/// Every `` `pixel <name>`` reference in `text` (backticked only: prose such
/// as "the pixel binary" is not a command).
fn referenced_commands(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (i, _) in text.match_indices("`pixel ") {
        let rest = &text[i + "`pixel ".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
            .collect();
        let next = rest.chars().nth(name.chars().count());
        // `pixel foo`, `pixel foo --flag`, `pixel foo <arg>` … but not
        // `pixel foo/bar`, `pixel-foo`, or a flag such as `pixel --help`.
        if name.starts_with(|c: char| c.is_ascii_lowercase())
            && matches!(next, Some(' ' | '`' | '\n'))
        {
            out.insert(name);
        }
    }
    out
}

const DOCS: &[&str] = &[
    "README.md",
    "ARCHITECTURE.md",
    "CONTRIBUTING.md",
    "docs/manual-setup.md",
    "website/content/docs.md",
    "website/content/benchmarks.md",
    "crates/pixel-install/assets/pixel-agent-prompt.md",
    "crates/pixel-install/assets/pixel-subagent-prompt.md",
    "AGENTS.md",
    ".agents/rules/test-campaigns.md",
    ".agents/rules/measuring.md",
    ".agents/rules/change-propagation.md",
    ".agents/rules/architecture-doc.md",
    ".agents/rules/graph-resolver.md",
    "scripts/README.md",
    "website/data/agents.toml",
    "website/static/llms.txt",
];

/// The per-agent pages, `website/content/for/*.md`, found on disk so a new
/// page is checked the day it lands, relative to the repository root.
fn agent_pages() -> Vec<String> {
    let dir = repo_root().join("website/content/for");
    let mut pages: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .map(|name| format!("website/content/for/{name}"))
        .collect();
    pages.sort();
    assert!(pages.len() > 1, "no agent page found: {pages:?}");
    pages
}

/// The site's HTML sources: the landing page, the objections it and its
/// FAQPage JSON-LD render, the alternatives its cards and the `/vs/`
/// pages render, and the answers the `/answers/` pages render. They quote
/// commands as `<code>pixel …</code>`, which `html_code_as_backticks` turns
/// into the Markdown form.
const SITE_HTML: &[&str] = &[
    "website/layouts/index.html",
    "website/data/objections.toml",
    "website/data/alternatives.toml",
    "website/data/answers.toml",
];

/// The comparison pages, one Markdown file per tool.
const VS_PAGES_DIR: &str = "website/content/vs";

/// The question pages, one Markdown file per question.
const ANSWER_PAGES_DIR: &str = "website/content/answers";

/// Every `<dir>/*.md` (the section's `_index.md` included), read from the
/// tree so a new page is checked without an edit here, relative to the
/// repository root.
fn section_pages(root: &Path, dir: &str) -> Vec<String> {
    let mut pages: Vec<String> = std::fs::read_dir(root.join(dir))
        .unwrap_or_else(|e| panic!("{dir}: {e}"))
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .map(|name| format!("{dir}/{name}"))
        .collect();
    pages.sort();
    pages
}

/// Every `website/content/vs/*.md`.
fn comparison_pages(root: &Path) -> Vec<String> {
    section_pages(root, VS_PAGES_DIR)
}

/// Every `website/content/answers/*.md`.
fn answer_pages(root: &Path) -> Vec<String> {
    section_pages(root, ANSWER_PAGES_DIR)
}

#[test]
fn answer_pages_are_listed_from_the_tree() {
    let pages = answer_pages(&repo_root());
    assert!(
        pages.contains(&format!("{ANSWER_PAGES_DIR}/_index.md"))
            && pages.contains(&format!("{ANSWER_PAGES_DIR}/grep-or-code-index.md")),
        "{pages:?}"
    );
    assert!(pages.iter().all(|p| p.ends_with(".md")), "{pages:?}");
}

#[test]
fn comparison_pages_are_listed_from_the_tree() {
    let pages = comparison_pages(&repo_root());
    // The index and the GitNexus page exist since the directory does; a
    // listing without them means the path or the filter broke.
    assert!(
        pages.contains(&format!("{VS_PAGES_DIR}/_index.md"))
            && pages.contains(&format!("{VS_PAGES_DIR}/gitnexus.md")),
        "{pages:?}"
    );
    assert!(pages.iter().all(|p| p.ends_with(".md")), "{pages:?}");
}

/// `<code>pixel install</code>` read as `` `pixel install` ``, so HTML goes
/// through the same `referenced_commands` as Markdown.
fn html_code_as_backticks(text: &str) -> String {
    text.replace("<code>", "`").replace("</code>", "`")
}

#[test]
fn html_code_spans_count_as_quoted_commands() {
    let html = "<p><code>pixel search-content</code> keeps grep's syntax; \
                <code>pixel install --repo</code> adds a guard. The pixel binary.</p>";
    let got: Vec<String> = referenced_commands(&html_code_as_backticks(html))
        .into_iter()
        .collect();
    assert_eq!(got, ["install", "search-content"]);
}

#[test]
fn every_documented_pixel_command_exists() {
    // `ai-cli-readify` ships behind the `readify` feature (issue #602): the
    // prod binary's help omits it, while the docs keep documenting it.
    #[cfg(not(feature = "readify"))]
    let known = {
        let mut known = subcommands();
        known.insert("ai-cli-readify".to_string());
        known
    };
    #[cfg(feature = "readify")]
    let known = subcommands();
    let root = repo_root();
    let mut stale = Vec::new();
    let vs_pages = comparison_pages(&root);
    let ans_pages = answer_pages(&root);
    let markdown = DOCS
        .iter()
        .map(|doc| ((*doc).to_string(), false))
        .chain(vs_pages.iter().map(|doc| (doc.clone(), false)))
        .chain(ans_pages.iter().map(|doc| (doc.clone(), false)))
        .chain(agent_pages().into_iter().map(|doc| (doc, false)));
    let html = SITE_HTML.iter().map(|doc| ((*doc).to_string(), true));
    let mut vs_names = 0;
    let mut ans_names = 0;
    for (doc, is_html) in markdown.chain(html) {
        let text =
            std::fs::read_to_string(root.join(&doc)).unwrap_or_else(|e| panic!("{doc}: {e}"));
        let text = if is_html {
            html_code_as_backticks(&text)
        } else {
            text
        };
        let names = referenced_commands(&text);
        // Every site HTML source quotes commands today: none found means the
        // HTML form changed and this check went blind, not that the file is
        // clean.
        assert!(
            !is_html || !names.is_empty(),
            "{doc}: no <code>pixel …</code> found"
        );
        if vs_pages.contains(&doc) {
            vs_names += names.len();
        }
        if ans_pages.contains(&doc) {
            ans_names += names.len();
        }
        for name in names {
            if !known.contains(&name) {
                stale.push(format!("{doc}: `pixel {name}`"));
            }
        }
    }
    // The comparison pages quote commands today: none across all of them
    // means the listing went blind.
    assert!(vs_names > 0, "{VS_PAGES_DIR}: no `pixel …` found");
    assert!(ans_names > 0, "{ANSWER_PAGES_DIR}: no `pixel …` found");
    assert!(
        stale.is_empty(),
        "documented commands the binary rejects:\n{}",
        stale.join("\n")
    );
}

/// The production part of a Rust file: everything before its first
/// `#[cfg(test)]` whose next non-attribute line declares a `mod`. A
/// `#[cfg(test)]` on a lone item (a `static` lock, a helper) does not end it.
/// The cut is the one `pixel-git`'s boundary test makes, and its
/// `test_modules_follow_every_production_item` keeps every file's test
/// modules after its last production item, so nothing after it is
/// production.
fn production_part(source: &str) -> &str {
    let lines: Vec<&str> = source.lines().collect();
    let mut offset = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" {
            let next = lines[i + 1..]
                .iter()
                .find(|l| !l.trim_start().starts_with("#["))
                .map_or("", |l| l.trim_start());
            let item = next
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub ");
            if item.starts_with("mod ") {
                return &source[..offset];
            }
        }
        offset += line.len() + 1;
    }
    source
}

/// Commands a runtime string tells the agent to run: `` `pixel <name>`` or
/// `"pixel <name>` on a line of production code. Comments and the test
/// modules ([`production_part`]) are skipped: tests name old spellings on
/// purpose (alias and hook-compatibility fixtures).
fn runtime_command_mentions(source: &str) -> BTreeSet<String> {
    let production = production_part(source);
    let code: String = production
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .map(|l| format!("{l}\n"))
        .collect();
    let mut out = referenced_commands(&code);
    // A quoted `"pixel …` is a command only when it names one: "pixel is the
    // engine" is prose, "pixel rescue --apply" is a pre-rename spelling.
    out.extend(
        referenced_commands(&code.replace("\"pixel ", "`pixel "))
            .into_iter()
            .filter(|name| pixel_proto::commands::renamed_to(name).is_some()),
    );
    out
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The CLI prints follow-up commands for the agent to type (a revert plan, a
/// `--show` follow-up, a hint to rebuild the index). A pre-rename spelling
/// there still runs through its alias today and fails once the aliases go.
#[test]
fn every_command_production_code_prints_is_a_current_subcommand() {
    let known = subcommands();
    let crates = repo_root().join("crates");
    let mut sources = Vec::new();
    for krate in std::fs::read_dir(&crates).unwrap().map(Result::unwrap) {
        let src = krate.path().join("src");
        if src.is_dir() {
            rust_sources(&src, &mut sources);
        }
    }
    assert!(sources.len() > 50, "source walk broke: {}", sources.len());
    let mut stale = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path).unwrap();
        for name in runtime_command_mentions(&text) {
            if !known.contains(&name) {
                let shown = path.strip_prefix(&crates).unwrap_or(path).display();
                stale.push(format!("{shown}: pixel {name}"));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "production code prints commands the help does not list:\n{}",
        stale.join("\n")
    );
}

#[test]
fn runtime_command_mentions_skip_comments_and_tests() {
    let source = [
        "/// `pixel rescue` is the old name",
        "fn f() -> String { format!(\"pixel plan-rollback --apply {oid} .\") }",
        "const HINT: &str = \"run `pixel build-index .` first\";",
        "const OLD: &str = \"pixel rescue backup\";",
        "const PROSE: &str = \"pixel is the engine\";",
        "#[cfg(test)]",
        "mod tests { const OLD: &str = \"pixel hook guard\"; }",
    ]
    .join("\n");
    let got: Vec<String> = runtime_command_mentions(&source).into_iter().collect();
    assert_eq!(got, ["build-index", "rescue"]);

    // A `#[cfg(test)]` on a lone item is not the start of the tests:
    // `pixel/src/main.rs` opens on a `#[cfg(test)] static` lock, and cutting
    // there hid the rest of the file from this check (#528).
    let lone_item_first = [
        "#[cfg(test)]",
        "pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());",
        "const HINT: &str = \"run `pixel find-symbol` next\";",
        "#[cfg(test)]",
        "#[path = \"cmd_tests.rs\"]",
        "mod cmd_tests;",
        "const AFTER: &str = \"run `pixel status` next\";",
    ]
    .join("\n");
    let got: Vec<String> = runtime_command_mentions(&lone_item_first)
        .into_iter()
        .collect();
    assert_eq!(got, ["find-symbol"]);
}

/// The body of ARCHITECTURE.md's `## <heading>` section, up to the next
/// `## ` heading.
fn architecture_section(heading: &str) -> String {
    let arch = std::fs::read_to_string(repo_root().join("ARCHITECTURE.md")).unwrap();
    arch.split(&format!("\n## {heading}\n"))
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap_or_else(|| panic!("ARCHITECTURE.md has a `## {heading}` section"))
        .to_string()
}

/// The table cells of every `| … |` row of `text` below a `| --- |`
/// separator, trimmed, in document order.
fn table_rows(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix('|')?.strip_suffix('|'))
        .map(|row| {
            row.split(" | ")
                .map(|c| c.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|cells| !cells.iter().all(|c| c.chars().all(|ch| ch == '-')))
        .collect()
}

/// The command each row of the `## Command surface` table documents, in
/// table order.
fn command_table_order(section: &str) -> Vec<String> {
    table_rows(section)
        .iter()
        .filter_map(|cells| cells.first()?.strip_prefix("`pixel ")?.strip_suffix('`'))
        .map(str::to_string)
        .collect()
}

#[test]
fn the_architecture_command_table_lists_every_subcommand_in_help_order() {
    // The table promises `pixel --help` order: a reader scanning both side
    // by side, or a diff of the two, only works while they agree row for row.
    // `ai-cli-readify` ships behind the `readify` feature (issue #602) and
    // is absent from the prod binary's help when the feature is off.
    #[cfg(not(feature = "readify"))]
    let documented = {
        let mut documented = command_table_order(&architecture_section("Command surface"));
        documented.retain(|c| c != "ai-cli-readify");
        documented
    };
    #[cfg(feature = "readify")]
    let documented = command_table_order(&architecture_section("Command surface"));
    let known = subcommands();
    let missing: Vec<&String> = known.iter().filter(|c| !documented.contains(*c)).collect();
    assert!(
        missing.is_empty(),
        "subcommands missing from ARCHITECTURE.md `## Command surface`: {missing:?}"
    );
    assert_eq!(
        documented,
        subcommands_in_help_order(),
        "ARCHITECTURE.md `## Command surface` must list one row per subcommand, in `pixel --help` order"
    );
}

#[test]
fn command_table_order_reads_the_first_cell_of_each_row() {
    let section = [
        "| Command | Does |",
        "| --- | --- |",
        "| `pixel status` | Index `pixel build-index` status |",
        "| `pixel build-index` | Build |",
        "prose naming `pixel doctor`",
    ]
    .join("\n");
    assert_eq!(command_table_order(&section), ["status", "build-index"]);
}

/// A workspace member's internal dependencies, as its `Cargo.toml` declares
/// them: `(package name, normal pixel-* deps, dev pixel-* deps)`.
fn member_deps(root: &Path, member: &str) -> (String, BTreeSet<String>, BTreeSet<String>) {
    let text = std::fs::read_to_string(root.join(member).join("Cargo.toml")).unwrap();
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    let name = doc["package"]["name"].as_str().unwrap().to_string();
    let internal = |table: &str| -> BTreeSet<String> {
        doc.get(table)
            .and_then(toml_edit::Item::as_table_like)
            .map(|deps| {
                deps.iter()
                    .map(|(dep, _)| dep.to_string())
                    .filter(|dep| dep.starts_with("pixel-") && *dep != name)
                    .collect()
            })
            .unwrap_or_default()
    };
    (
        name.clone(),
        internal("dependencies"),
        internal("dev-dependencies"),
    )
}

/// The `Depends on (pixel crates)` cell of the `## Crates` table, read as
/// `(normal deps, dev deps when the cell lists them)`. `libraries` is every
/// library crate, which the `every library crate except …` form starts from.
fn documented_deps(
    cell: &str,
    libraries: &BTreeSet<String>,
) -> (BTreeSet<String>, Option<BTreeSet<String>>) {
    let backticked = |text: &str| -> BTreeSet<String> {
        text.split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    };
    let short_names = |text: &str| -> BTreeSet<String> {
        text.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty() && *name != "none")
            .map(|name| format!("pixel-{name}"))
            .collect()
    };
    if let Some(except) = cell.strip_prefix("every library crate except") {
        let excluded = backticked(except);
        return (libraries.difference(&excluded).cloned().collect(), None);
    }
    match cell.split_once("(dev:") {
        Some((normal, dev)) => (
            short_names(normal),
            Some(short_names(dev.trim_end_matches(')'))),
        ),
        None => (short_names(cell), None),
    }
}

#[test]
fn documented_deps_reads_every_form_of_the_depends_cell() {
    let libraries: BTreeSet<String> = ["pixel-git", "pixel-index", "pixel-bench"]
        .map(str::to_string)
        .into();
    let set =
        |names: &[&str]| -> BTreeSet<String> { names.iter().map(|n| (*n).to_string()).collect() };
    assert_eq!(documented_deps("none", &libraries), (set(&[]), None));
    assert_eq!(
        documented_deps("graph, git", &libraries),
        (set(&["pixel-graph", "pixel-git"]), None)
    );
    assert_eq!(
        documented_deps("index (dev: daemon, proto)", &libraries),
        (
            set(&["pixel-index"]),
            Some(set(&["pixel-daemon", "pixel-proto"]))
        )
    );
    assert_eq!(
        documented_deps(
            "every library crate except `pixel-git` (reached elsewhere) and `pixel-bench`",
            &libraries
        ),
        (set(&["pixel-index"]), None)
    );
}

#[test]
fn the_architecture_crate_table_matches_the_workspace_manifests() {
    // The crate map is what a contributor reads before touching two crates:
    // a missing crate or a wrong edge sends them to the wrong layer (the
    // table once credited `pixel-facts` with an index dependency it never
    // had, and left out the graph one `pixel-recall` uses).
    let root = repo_root();
    let workspace: toml_edit::DocumentMut = std::fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let members: Vec<(String, BTreeSet<String>, BTreeSet<String>)> =
        workspace["workspace"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| member_deps(&root, m.as_str().unwrap()))
            .collect();
    let libraries: BTreeSet<String> = members
        .iter()
        .map(|(name, _, _)| name.clone())
        .filter(|name| name != "pixel-cli")
        .collect();

    let section = architecture_section("Crates");
    let rows: Vec<(String, String)> = table_rows(&section)
        .into_iter()
        .filter(|cells| cells.len() == 3 && cells[0].starts_with('`'))
        .map(|cells| {
            let name = cells[0].split('`').nth(1).unwrap().to_string();
            (name, cells[2].clone())
        })
        .collect();

    let documented: BTreeSet<&String> = rows.iter().map(|(name, _)| name).collect();
    let actual: BTreeSet<&String> = members.iter().map(|(name, _, _)| name).collect();
    assert_eq!(
        rows.len(),
        actual.len(),
        "ARCHITECTURE.md `## Crates` must have exactly one row per workspace member"
    );
    assert_eq!(
        documented, actual,
        "ARCHITECTURE.md `## Crates` must have exactly one row per workspace member (set)"
    );

    for (name, cell) in &rows {
        let (_, normal, dev) = members.iter().find(|(n, _, _)| n == name).unwrap();
        let (doc_normal, doc_dev) = documented_deps(cell, &libraries);
        assert_eq!(
            &doc_normal, normal,
            "ARCHITECTURE.md `## Crates`: `{name}` depends on {normal:?} in its Cargo.toml, the table says `{cell}`"
        );
        if let Some(doc_dev) = doc_dev {
            assert_eq!(
                &doc_dev, dev,
                "ARCHITECTURE.md `## Crates`: `{name}` dev-depends on {dev:?} in its Cargo.toml, the table says `{cell}`"
            );
        }
    }
}

/// The `(old, new)` rows of the first `| Old name | New name |` table in
/// `text`, in document order.
fn rename_table_rows(text: &str) -> Vec<(String, String)> {
    let Some((_, after)) = text.split_once("| Old name | New name |") else {
        return Vec::new();
    };
    after
        .lines()
        .skip(2) // the rest of the header line, then the `| --- |` separator
        .map_while(|line| {
            let cells: Vec<&str> = line
                .trim()
                .strip_prefix('|')?
                .strip_suffix('|')?
                .split('|')
                .map(|cell| cell.trim().trim_matches('`'))
                .collect();
            match cells.as_slice() {
                [old, new] => Some(((*old).to_string(), (*new).to_string())),
                _ => None,
            }
        })
        .collect()
}

#[test]
fn renamed_command_tables_list_exactly_the_accepted_aliases() {
    // The rename doc and the changelog tell users which old names still work;
    // the CLI registers those aliases from `RENAMED_COMMANDS`. A table that
    // drops a row or keeps a stale one sends a user to a name that fails.
    let expected: Vec<(String, String)> = pixel_proto::commands::RENAMED_COMMANDS
        .iter()
        .map(|(old, new)| ((*old).to_string(), (*new).to_string()))
        .collect();
    let root = repo_root();
    for doc in ["docs/renamed-commands.md", "CHANGELOG.md"] {
        let text = std::fs::read_to_string(root.join(doc)).unwrap();
        assert_eq!(
            rename_table_rows(&text),
            expected,
            "{doc}: the `| Old name | New name |` table must match RENAMED_COMMANDS row for row"
        );
    }
}

#[test]
fn rename_table_rows_stop_at_the_end_of_the_table() {
    let text = [
        "intro",
        "| Old name | New name |",
        "| --- | --- |",
        "| `ready` | `prepare-repo` |",
        "| `hook` | `run-hook` |",
        "",
        "| `after` | `blank line` |",
    ]
    .join("\n");
    assert_eq!(
        rename_table_rows(&text),
        vec![
            ("ready".to_string(), "prepare-repo".to_string()),
            ("hook".to_string(), "run-hook".to_string()),
        ]
    );
    assert!(rename_table_rows("no table here").is_empty());
}

#[test]
fn referenced_commands_reads_only_backticked_command_names() {
    let text = "Run `pixel search-content foo` then `pixel impact`.\n`pixel-cli` is the crate; the pixel binary; `pixel` alone; `pixel foo/bar`; `pixel --help` is a flag.";
    let got: Vec<String> = referenced_commands(text).into_iter().collect();
    assert_eq!(got, ["impact", "search-content"]);
}

/// Rule ids (`M-…`) named in `text`, wildcards such as `M-FFI-*` excluded:
/// those name a family, not one heading.
fn referenced_rule_ids(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (i, _) in text.match_indices("M-") {
        if i > 0 && text.as_bytes()[i - 1].is_ascii_alphanumeric() {
            continue; // `SOM-…`, `M-` inside a word
        }
        let id: String = text[i..]
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '-' || *c == '*')
            .collect();
        let id = id.trim_end_matches('-');
        if id.len() > 2 && !id.ends_with('*') {
            out.insert(id.to_string());
        }
    }
    out
}

/// Rule ids that head a `## <title> (M-ID)` section of `guidelines.txt`.
fn guideline_rule_ids(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|l| l.starts_with("## "))
        .filter_map(|l| {
            let open = l.rfind("(M-")?;
            let close = l[open..].find(')')?;
            Some(l[open + 1..open + close].to_string())
        })
        .collect()
}

#[test]
fn every_rule_id_in_the_skill_should_head_a_guideline_when_upstream_is_refreshed() {
    let root = repo_root().join(".agents/skills/rust-guidelines");
    let skill = std::fs::read_to_string(root.join("SKILL.md")).unwrap();
    let guidelines = std::fs::read_to_string(root.join("guidelines.txt")).unwrap();
    let known = guideline_rule_ids(&guidelines);
    assert!(
        known.len() >= 80,
        "guidelines.txt heading parsing broke: {}",
        known.len()
    );
    let named = referenced_rule_ids(&skill);
    assert!(named.len() >= 40, "SKILL.md id parsing broke: {named:?}");
    let unknown: Vec<&String> = named.iter().filter(|id| !known.contains(*id)).collect();
    assert!(
        unknown.is_empty(),
        "SKILL.md names rule ids that are not headings of guidelines.txt (renamed or removed upstream? run scripts/refresh-guidelines.sh and update SKILL.md): {unknown:?}"
    );
}

#[test]
fn referenced_rule_ids_should_skip_wildcards_and_embedded_matches() {
    let text =
        "Apply M-PANIC-ON-BUG and (M-FROM-ERROR). Not M-FFI-*, not SOM-THING, `M-DI-HIERARCHY`.";
    let got: Vec<String> = referenced_rule_ids(text).into_iter().collect();
    assert_eq!(got, ["M-DI-HIERARCHY", "M-FROM-ERROR", "M-PANIC-ON-BUG"]);
}

#[test]
fn guideline_rule_ids_should_read_only_heading_ids() {
    let text = "## Panic on bug (M-PANIC-ON-BUG) { #M-PANIC-ON-BUG }\nSee M-FROM-ERROR in prose.\n### Sub (M-NOT-A-RULE)\n";
    let got: Vec<String> = guideline_rule_ids(text).into_iter().collect();
    assert_eq!(got, ["M-PANIC-ON-BUG"]);
}

/// Every backticked `` `<repo>/<path>` `` in `text`, without the prefix.
fn repo_paths(text: &str) -> BTreeSet<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|token| token.strip_prefix("<repo>/"))
        .map(str::to_string)
        .collect()
}

/// Every backticked relative path (`` `.codex/hooks.json` ``) in `text`.
fn dot_paths(text: &str) -> BTreeSet<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|token| {
            (*token == "AGENTS.md")
                || (token.starts_with('.') && token.contains('/') && !token.contains(' '))
        })
        .map(str::to_string)
        .collect()
}

fn repo_artifact_paths() -> BTreeSet<String> {
    pixel_install::install::REPO_ARTIFACTS
        .iter()
        .map(|artifact| artifact.path.to_string())
        .collect()
}

/// The README once sent readers to `.devin/hooks.json` and `.pi/agent/`,
/// files the agents never read, and left out the Claude file the install
/// really writes: the list, now on the docs page, is checked against the code.
#[test]
fn docs_per_project_list_should_name_exactly_the_repo_install_files() {
    let docs = std::fs::read_to_string(repo_root().join("website/content/docs.md")).unwrap();
    assert_eq!(repo_paths(&docs), repo_artifact_paths());
}

#[test]
fn install_repo_help_should_name_exactly_the_repo_install_files() {
    let out = Command::new(env!("CARGO_BIN_EXE_pixel"))
        .args(["install", "--help"])
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let help = String::from_utf8(out.stdout).unwrap();
    assert_eq!(dot_paths(&help), repo_artifact_paths(), "{help}");
}

#[test]
fn repo_paths_should_read_only_backticked_repo_prefixed_tokens() {
    let text = "`<repo>/.a/b` and <repo>/.c/d, `.e/f`, `<repo>/.g/h`, `AGENTS.md`";
    let got: Vec<String> = repo_paths(text).into_iter().collect();
    assert_eq!(got, [".a/b", ".g/h"]);
    let got: Vec<String> = dot_paths(text).into_iter().collect();
    assert_eq!(got, [".e/f", "AGENTS.md"]);
}

// ---------------------------------------------------------------------------
// website/data/agents.toml: what the site says `pixel install` writes for
// each agent, held to what a real install writes.
// ---------------------------------------------------------------------------

/// `website/data/agents.toml`, parsed.
fn agents_data() -> toml_edit::DocumentMut {
    let path = repo_root().join("website/data/agents.toml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .parse()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The strings of an array field, empty when the field is absent.
fn string_list(item: Option<&toml_edit::Item>) -> Vec<String> {
    item.and_then(toml_edit::Item::as_array)
        .map(|array| {
            array
                .iter()
                .map(|v| v.as_str().expect("a list of strings").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The `field` list of every agent, concatenated in file order.
fn agents_field(data: &toml_edit::DocumentMut, field: &str) -> Vec<String> {
    let agents = data["agent"]
        .as_array_of_tables()
        .expect("agents.toml has [[agent]] tables");
    assert!(
        agents.len() > 5,
        "agents.toml parsing broke: {}",
        agents.len()
    );
    agents
        .iter()
        .flat_map(|agent| string_list(agent.get(field)))
        .collect()
}

/// Every file under `dir`, relative to `base`, `/`-separated.
fn files_under(dir: &Path, base: &Path, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, base, out);
        } else {
            let rel = path.strip_prefix(base).unwrap();
            out.insert(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// How `claimed` (paths, a trailing `/` claiming a whole directory) and
/// `written` (files) disagree: the files no entry claims, then the entries
/// that match no file. Both empty when the data says what the install did.
fn claim_mismatch(claimed: &[String], written: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    let covers =
        |claim: &str, file: &str| claim == file || claim.ends_with('/') && file.starts_with(claim);
    let unclaimed = written
        .iter()
        .filter(|file| !claimed.iter().any(|claim| covers(claim, file)))
        .cloned()
        .collect();
    let unmatched = claimed
        .iter()
        .filter(|claim| !written.iter().any(|file| covers(claim, file)))
        .cloned()
        .collect();
    (unclaimed, unmatched)
}

#[test]
fn claim_mismatch_should_match_files_and_whole_directories() {
    let written: BTreeSet<String> = ["a/x.json", "d/one", "d/sub/two", "loose"]
        .map(String::from)
        .into();
    let claimed = ["a/x.json", "d/", "gone.toml", "e/"].map(String::from);
    let (unclaimed, unmatched) = claim_mismatch(&claimed, &written);
    assert_eq!(unclaimed, ["loose"]);
    assert_eq!(unmatched, ["gone.toml", "e/"]);
    // A directory claim needs its slash: `d` alone names a file never written.
    let (unclaimed, unmatched) = claim_mismatch(&["d".to_string()], &written);
    assert_eq!(unclaimed.len(), 4);
    assert_eq!(unmatched, ["d"]);
}

/// The agent pages tell a reader which files `pixel install` puts in their
/// home. Run the real command into an empty one, with the config directories
/// its Pi step and cleanup steps look for (OpenCode's, Devin's, Antigravity's,
/// Pi's), and hold the data to exactly what landed: a file the install starts
/// writing, stops writing or moves fails here until `website/data/agents.toml`
/// says so.
#[test]
fn agents_data_should_name_exactly_the_files_a_global_install_writes() {
    // Written only when Codex has a retired Pixel block to remove.
    const CONDITIONAL: &[&str] = &[".codex/config.toml"];

    let home = crate::support::Scratch::for_test("docs-drift", "agents-global");
    for dir in [
        ".config/opencode",
        ".config/devin",
        ".gemini/config",
        ".pi/agent",
    ] {
        std::fs::create_dir_all(home.join(dir)).unwrap();
    }
    let out = crate::support::pixel_command()
        .args(["install", "--shell", "zsh", "--json"])
        .env("HOME", &*home)
        .env_remove("CODEX_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert!(out.status.success(), "pixel install: {out:?}");
    let mut written = BTreeSet::new();
    files_under(&home, &home, &mut written);

    let data = agents_data();
    let claimed: Vec<String> = string_list(data.get("shared"))
        .into_iter()
        .chain(agents_field(&data, "global"))
        .map(|path| {
            path.strip_prefix("~/")
                .unwrap_or_else(|| panic!("`{path}`: global paths start at ~/"))
                .to_string()
        })
        .collect();
    let (unclaimed, unmatched) = claim_mismatch(&claimed, &written);
    let unwritten: Vec<_> = unmatched
        .into_iter()
        .filter(|p| !CONDITIONAL.contains(&p.as_str()))
        .collect();
    assert!(
        unclaimed.is_empty() && unwritten.is_empty(),
        "website/data/agents.toml disagrees with `pixel install`:\n\
         written but not listed: {unclaimed:?}\n\
         listed but not written: {unwritten:?}"
    );
}

/// The same contract for `pixel install --repo`: the data's `repo` lists are
/// exactly `REPO_ARTIFACTS`, and a real run into an empty repository writes
/// every one of them but the conditional ones, and nothing outside them.
#[test]
fn agents_data_should_name_exactly_the_files_a_repo_install_writes() {
    // These appear only when legacy hook backups exist, retired Pixel
    // guidance is migrated out of an existing project file, or a retired
    // Pixel callback is removed from an existing hook file: native cleanup
    // never creates an empty one.
    const CONDITIONAL: &[&str] = &[
        ".claude/pixel-rtk-hooks.json",
        ".claude/settings.local.json",
        ".codex/config.toml",
        ".codex/hooks.json",
        ".codex/pixel-composed-guard-backup.json",
        "AGENTS.md",
    ];
    let data = agents_data();
    let mut claimed = agents_field(&data, "repo");
    claimed.extend(string_list(data.get("repo_shared")));
    let listed: BTreeSet<String> = claimed.iter().cloned().collect();
    assert_eq!(
        listed.len(),
        claimed.len(),
        "a repo path listed twice: {claimed:?}"
    );
    assert_eq!(listed, repo_artifact_paths());

    let root = crate::support::Scratch::for_test("docs-drift", "agents-repo");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let out = crate::support::pixel_command()
        .args(["install", "--json", "--repo"])
        .arg(&repo)
        .env("HOME", root.join("home"))
        .output()
        .unwrap();
    assert!(out.status.success(), "pixel install --repo: {out:?}");
    let mut written = BTreeSet::new();
    files_under(&repo, &repo, &mut written);
    // Every command run on a repository appends to its action log; that file
    // is the CLI's, not an agent's.
    written.retain(|file| !file.starts_with(".pixel/"));
    let (unclaimed, unmatched) = claim_mismatch(&claimed, &written);
    assert!(
        unclaimed.is_empty(),
        "`pixel install --repo` wrote files agents.toml does not list: {unclaimed:?}"
    );
    // The pages promise these files to anyone who runs the command, so an
    // install that writes nothing must fail here too. Only the conditional
    // ones may be missing from an empty repository.
    let missing: Vec<&String> = unmatched
        .iter()
        .filter(|path| !CONDITIONAL.contains(&path.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "`pixel install --repo` did not write files agents.toml lists: {missing:?} (wrote {written:?})"
    );
}

/// Repo inventory includes portable files that are rewritten only while
/// removing a retired Pixel block; migration must preserve surrounding text.
#[test]
fn repo_install_should_preserve_and_rewrite_documented_migration_files() {
    let root = crate::support::Scratch::for_test("docs-drift", "agents-repo-migration");
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".codex")).unwrap();
    let codex_path = repo.join(".codex/config.toml");
    let codex_instructions = format!(
        "Keep this Codex instruction.\n{}\nRetired Pixel instructions.\n{}\nKeep this too.\n",
        pixel_install::config::MANAGED_BEGIN,
        pixel_install::config::MANAGED_END,
    );
    std::fs::write(
        &codex_path,
        format!(
            "developer_instructions = '''\n{codex_instructions}'''\n\n[other]\nvalue = \"preserve\"\n"
        ),
    )
    .unwrap();
    let agents_path = repo.join("AGENTS.md");
    std::fs::write(
        &agents_path,
        "Keep this project instruction.\n\n<!-- pixel:warp-retrieval:begin -->\nRetired Pixel-first instructions.\n<!-- pixel:warp-retrieval:end -->\n\nKeep this too.\n",
    )
    .unwrap();

    let out = crate::support::pixel_command()
        .args(["install", "--json", "--repo"])
        .arg(&repo)
        .env("HOME", root.join("home"))
        .env_remove("CODEX_HOME")
        .output()
        .unwrap();
    assert!(out.status.success(), "pixel install --repo: {out:?}");

    let codex = std::fs::read_to_string(&codex_path).unwrap();
    let codex: toml_edit::DocumentMut = codex.parse().unwrap();
    let instructions = codex["developer_instructions"].as_str().unwrap();
    assert!(instructions.contains("Keep this Codex instruction."));
    assert!(instructions.contains("Keep this too."));
    assert!(!instructions.contains(pixel_install::config::MANAGED_BEGIN));
    assert_eq!(codex["other"]["value"].as_str(), Some("preserve"));

    let agents = std::fs::read_to_string(&agents_path).unwrap();
    assert!(agents.contains("Keep this project instruction."));
    assert!(agents.contains("Keep this too."));
    assert!(!agents.contains("pixel:warp-retrieval:"));
}

/// `static/llms.txt` is a static file, so it cannot build its links from
/// `baseURL` as the templates do: it names the site's `/for/` page and one
/// page per agent under the `baseURL` of `hugo.toml`, and a renamed slug, a
/// new agent or a moved site fails here instead of sending an assistant to
/// a 404.
#[test]
fn llms_txt_should_link_every_agent_page_under_the_site_base_url() {
    let root = repo_root();
    let hugo: toml_edit::DocumentMut = std::fs::read_to_string(root.join("website/hugo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let base = hugo["baseURL"].as_str().expect("hugo.toml has a baseURL");
    let llms = std::fs::read_to_string(root.join("website/static/llms.txt")).unwrap();
    let data = agents_data();
    let slugs = data["agent"]
        .as_array_of_tables()
        .unwrap()
        .iter()
        .map(|agent| agent["slug"].as_str().unwrap().to_string());
    let expected: Vec<String> = std::iter::once(format!("{base}for/"))
        .chain(slugs.map(|slug| format!("{base}for/{slug}/")))
        .collect();
    let missing: Vec<&String> = expected
        .iter()
        .filter(|url| !llms.contains(&format!("]({url})")))
        .collect();
    assert!(
        missing.is_empty(),
        "static/llms.txt does not link: {missing:?}"
    );
    let linked = llms.matches(&format!("]({base}for/")).count();
    assert_eq!(
        linked,
        expected.len(),
        "static/llms.txt links a /for/ page no agent has"
    );
}

/// The `title` of a page's YAML front matter (`title: "…"`), which for an
/// `/answers/` page is the question.
fn front_matter_title(text: &str) -> Option<String> {
    let front = text.strip_prefix("---\n")?.split("\n---").next()?;
    front.lines().find_map(|line| {
        let value = line.strip_prefix("title:")?.trim();
        Some(value.trim_matches('"').to_string())
    })
}

#[test]
fn front_matter_title_reads_only_the_front_matter() {
    let page = "---\ntitle: \"Why does my agent read whole files?\"\nanswer: \"x\"\n---\n\ntitle: not this\n";
    assert_eq!(
        front_matter_title(page).as_deref(),
        Some("Why does my agent read whole files?")
    );
    assert_eq!(front_matter_title("title: \"no front matter\"\n"), None);
    assert_eq!(
        front_matter_title("---\nanswer: \"x\"\n---\ntitle: late\n"),
        None
    );
}

/// `static/llms.txt` is not a template, so it links each `/answers/` page by
/// hand: under its question, at the `baseURL` of `hugo.toml`. A renamed
/// page, a reworded question or a new one fails here instead of leaving an
/// assistant with a 404 or a question the page no longer asks.
#[test]
fn llms_txt_should_link_every_answer_page_under_its_question() {
    let root = repo_root();
    let hugo: toml_edit::DocumentMut = std::fs::read_to_string(root.join("website/hugo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let base = hugo["baseURL"].as_str().expect("hugo.toml has a baseURL");
    let llms = std::fs::read_to_string(root.join("website/static/llms.txt")).unwrap();
    let mut expected = vec![format!("]({base}answers/)")];
    for page in answer_pages(&root) {
        let stem = Path::new(&page).file_stem().unwrap().to_string_lossy();
        if stem == "_index" {
            continue;
        }
        let text = std::fs::read_to_string(root.join(&page)).unwrap();
        let title = front_matter_title(&text).unwrap_or_else(|| panic!("{page}: no title"));
        expected.push(format!("[{title}]({base}answers/{stem}/)"));
    }
    let missing: Vec<&String> = expected.iter().filter(|l| !llms.contains(*l)).collect();
    assert!(
        missing.is_empty(),
        "static/llms.txt does not link: {missing:?}"
    );
    assert_eq!(
        llms.matches(&format!("]({base}answers/")).count(),
        expected.len(),
        "static/llms.txt links an /answers/ page that does not exist"
    );
}

// ---------------------------------------------------------------------------
// Over-absolute claims: the docs must not promise what the binary cannot
// guarantee. A claim that is too absolute ("zero commands", "no telemetry",
// "never leaves the machine", "crash-safe", "ship more") contradicts the
// product's own behaviour (a release check, a model-backed classifier, Git
// remotes) and erodes trust when a user finds the boundary.
// ---------------------------------------------------------------------------

/// Files whose claims are checked for over-absolute language.
const HARMONIZED_DOCS: &[&str] = &[
    "README.md",
    "website/layouts/partials/head.html",
    "website/layouts/index.html",
    "website/content/teams.md",
    "website/content/about.md",
    "website/content/docs.md",
    "website/content/legal/privacy-policy.md",
    "website/static/llms.txt",
    "website/og/card.html",
];

/// Claims that are over-absolute and must not appear in the docs. Each entry
/// is (pattern, qualifier that makes it acceptable, why it is over-absolute).
const OVER_ABSOLUTE_CLAIMS: &[(&str, &str, &str)] = &[
    (
        "zero commands to learn",
        "a handful of commands",
        "the product has many commands",
    ),
    (
        "No account. No API key. No telemetry.",
        "No account. No API key. No telemetry beyond an optional release check.",
        "the release check is a network call",
    ),
    (
        "Local and deterministic.",
        "Local and mostly deterministic.",
        "pixel classify is model-backed and non-deterministic",
    ),
    (
        "never leaves the machine",
        "the machine could be a VM or cloud instance",
        "the machine could be a VM or cloud instance",
    ),
    (
        "crash-safe",
        "designed to be crash-safe",
        "crash-safety cannot be guaranteed in all scenarios",
    ),
    (
        "Ship more.",
        "Ship more responsibly",
        "the product does not guarantee shipping more",
    ),
    (
        "sends no telemetry",
        "no telemetry beyond an optional release check",
        "the release check is a network call",
    ),
    (
        "never leave the machine",
        "the machine could be a VM or cloud instance",
        "the machine could be a VM or cloud instance",
    ),
];

/// Check that a doc does not contain an over-absolute claim. A claim is
/// allowed when its specific qualifier appears in the surrounding text.
fn assert_no_over_absolute_claims(doc: &str, text: &str) {
    for (pattern, qualifier, why) in OVER_ABSOLUTE_CLAIMS {
        if text.contains(pattern) {
            assert!(
                text.contains(qualifier),
                "{doc}: over-absolute claim \"{pattern}\" — {why}. \
                 Qualify it with \"{qualifier}\"."
            );
        }
    }
}

#[test]
fn docs_do_not_make_over_absolute_claims() {
    let root = repo_root();
    for doc in HARMONIZED_DOCS {
        let text = std::fs::read_to_string(root.join(doc)).unwrap_or_else(|e| panic!("{doc}: {e}"));
        assert_no_over_absolute_claims(doc, &text);
    }
}

#[test]
fn readme_claims_are_qualified() {
    let text = std::fs::read_to_string(repo_root().join("README.md")).unwrap();
    // The README must not claim "zero commands to learn".
    assert!(
        !text.contains("zero commands to learn"),
        "README: 'zero commands to learn' is over-absolute"
    );
    // The README must not claim "No account. No API key. No telemetry."
    // without qualification.
    assert!(
        !text.contains("No account. No API key. No telemetry."),
        "README: 'No account. No API key. No telemetry.' is over-absolute"
    );
    // The README must not claim "Local and deterministic." without
    // acknowledging that classify is non-deterministic.
    assert!(
        !text.contains("Local and deterministic."),
        "README: 'Local and deterministic.' is over-absolute (classify is model-backed)"
    );
    // The README must not claim "never leaves the machine".
    assert!(
        !text.contains("never leaves the machine"),
        "README: 'never leaves the machine' is over-absolute"
    );
    // The README must not claim "crash-safe" without qualification.
    // The qualified form "designed to be crash-safe" is acceptable.
    assert!(
        !text.contains("crash-safe `pixel commit-and-push`"),
        "README: 'crash-safe `pixel commit-and-push`' is over-absolute"
    );
}

#[test]
fn og_card_does_not_claim_ship_more() {
    let text = std::fs::read_to_string(repo_root().join("website/og/card.html")).unwrap();
    assert!(
        !text.contains("Ship more."),
        "og card: 'Ship more.' is over-absolute"
    );
}

#[test]
fn head_html_telemetry_claim_is_qualified() {
    let text =
        std::fs::read_to_string(repo_root().join("website/layouts/partials/head.html")).unwrap();
    // The metadata value must qualify "No telemetry" with the release-check
    // disclaimer, and must not appear as a standalone "No telemetry:" label.
    for line in text.lines() {
        if line.contains("\"No telemetry:") {
            assert!(
                line.contains("beyond an optional release check"),
                "head.html: 'No telemetry' must be qualified with 'beyond an optional release check'"
            );
        }
    }
}

#[test]
fn website_content_telemetry_claims_are_qualified() {
    let root = repo_root();
    for doc in [
        "website/content/teams.md",
        "website/content/about.md",
        "website/content/docs.md",
        "website/content/legal/privacy-policy.md",
    ] {
        let text = std::fs::read_to_string(root.join(doc)).unwrap_or_else(|e| panic!("{doc}: {e}"));
        // The claim "no telemetry" must be qualified with "beyond an optional
        // release check" or similar.
        if text.contains("no telemetry") || text.contains("No telemetry") {
            assert!(
                text.contains("beyond an optional release check"),
                "{doc}: 'no telemetry' must be qualified with 'beyond an optional release check'"
            );
        }
    }
}

#[test]
fn llms_txt_claims_are_qualified() {
    let text = std::fs::read_to_string(repo_root().join("website/static/llms.txt")).unwrap();
    // llms.txt must not claim "never leaves the machine".
    assert!(
        !text.contains("never leaves the machine"),
        "llms.txt: 'never leaves the machine' is over-absolute"
    );
    // llms.txt must not claim "no telemetry" without qualification.
    if text.contains("no telemetry") || text.contains("No telemetry") {
        assert!(
            text.contains("beyond an optional release check"),
            "llms.txt: 'no telemetry' must be qualified"
        );
    }
}

/// Every `checks` id is a `pixel doctor` check, and every agent check of the
/// doctor belongs to an agent: a page cannot send a reader to `--only` a
/// check that does not exist, nor leave out one that judges its agent.
#[test]
fn agents_data_checks_should_be_exactly_the_agent_checks_of_doctor() {
    // Checks on Pixel's own files, which no single agent owns.
    const NOT_AN_AGENT: &[&str] = &[
        "install.agent-prompt",
        "install.rtk-backup",
        "install.legacy-wrappers",
    ];
    let data = agents_data();
    let mut named = agents_field(&data, "checks");
    named.extend(string_list(data.get("repo_shared_checks")));
    let named_set: BTreeSet<&str> = named.iter().map(String::as_str).collect();
    assert_eq!(
        named_set.len(),
        named.len(),
        "a check listed twice: {named:?}"
    );
    let agent_checks: BTreeSet<&str> = pixel_install::doctor::CHECKS
        .iter()
        .map(|check| check.id)
        .filter(|id| id.starts_with("install.") || id.starts_with("repo."))
        .filter(|id| !NOT_AN_AGENT.contains(id))
        .collect();
    assert_eq!(named_set, agent_checks);
}
