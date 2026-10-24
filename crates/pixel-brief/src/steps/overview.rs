// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The Overview steps (`specs/brief.md`, "Recipes"): what the manifests of a
//! workspace answer in a few lines.
//!
//! * `workspace-overview`: "which crates make up the workspace" is one line per
//!   member plus the README's lead paragraph; a question naming a member gets
//!   that member's description, dependency names and source files.
//! * `overview-answer-evidence`: "what does the `x` crate expose" for a Cargo
//!   member: its crate docs, manifest, facade, tracked files, dependency
//!   purposes and bounded examples.
//! * `cand-k-member-manifest`: "what does the x crate contain / depend on": the
//!   member's manifest with its identity lines cut and every tracked file of
//!   its directory. The rest of the pack drops blocks outside that directory:
//!   the lexical neighbours of a crate question are other crates' files.
//!
//! Workspaces are read from the manifests when the step runs: Cargo
//! `[workspace].members`, package.json `workspaces`, `pnpm-workspace.yaml`,
//! `go.work` and pyproject `[tool.uv.workspace]`. No name is written here.
//! Nothing is printed for a repository that is no workspace or a prompt that
//! asks about neither.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use toml_edit::{DocumentMut, Item};

use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Drop, Outcome, Step};

const MAX_MEMBERS: usize = 40;
const DESC_CHARS: usize = 80;
const LEAD_CHARS: usize = 380;
const NAMES_PER_LINE: usize = 14;
const SRC_NAMES: usize = 16;
/// Tracked paths past which `overview-answer-evidence` says nothing: a
/// repository that large is no single workspace to summarise.
const MAX_TRACKED_PATHS: usize = 4_000;
const MAX_TOTAL: usize = 24_000;
const MAX_DOC_LINES: usize = 60;
const MAX_DOC_CHARS: usize = 240;
const EXAMPLE_LINES: usize = 65;
const MANIFEST_FILES: usize = 24;
const MANIFEST_LINES: usize = 70;
const MANIFEST_NAMES: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];
const MEMBER_NOUNS: &str =
    r"(?:crates?|packages?|modules?|libraries|library|libs?|members?|workspaces?)";

static OVERVIEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(workspaces?|monorepo|crates|packages|members|made up|make up|consists?|overview|layout|structure)\b",
    )
    .expect("the overview-question pattern is a literal")
});
/// Words that make a prompt worth listing the tracked files for: a workspace
/// question names a member noun or asks for the layout. Cheap, and ahead of
/// any file read, so a brief that is about neither pays nothing.
static WORKSPACE_GATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)\b({MEMBER_NOUNS}|monorepo|depend\w*|made up|make up|consists?|layout|structure|overview)\b"
    ))
    .expect("the workspace-gate pattern is a literal")
});
static CRATE_GATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:crate|library)\b").expect("the crate-gate pattern is a literal")
});
static NOUN_AFTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)^(?:'s)?\s+{MEMBER_NOUNS}\b"))
        .expect("the noun-after pattern is a literal")
});
static NOUN_BEFORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#"(?i)\b{MEMBER_NOUNS}\s+[`'"]?"#))
        .expect("the noun-before pattern is a literal")
});
static NAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:the\s+)?([A-Za-z][\w.-]*)\s+(?:crate|package|module|library|lib)\b")
        .expect("the named-member pattern is a literal")
});
static NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(authors|documentation|homepage|repository|keywords|categories|license|license-file|readme|exclude|include|publish|bugs|author|funding|edition(\.workspace)?|rust-version(\.workspace)?)\s*[=:]",
    )
    .expect("the identity-line pattern is a literal")
});
static README: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^readme(\.(md|markdown|rst|txt))?$").expect("the readme pattern is a literal")
});
static RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[=\-~*_]{3,}$").expect("the rule pattern is a literal"));
static TITLE_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[=\-~]{3,}$").expect("the title-rule pattern is a literal"));
static LICENSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(dual-)?licensed").expect("the licence pattern is a literal")
});
static SOURCE_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\.(rs|py|go|ts|tsx|js|jsx|mjs|rb|java|kt|c|h|cc|cpp|hpp)$")
        .expect("the source-extension pattern is a literal")
});
static GO_MODULE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^module\s+(\S+)").expect("the go module pattern is a literal")
});
static GO_REQUIRE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?ms)^require\s*\((.*?)^\)").expect("the go require pattern is a literal")
});
static GO_REQUIRE_ONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^require\s+([^\s(]\S*)\s+\S+(.*)$").expect("the go require pattern is a literal")
});
static GO_USE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?ms)^use\s*\((.*?)^\)").expect("the go use pattern is a literal")
});
static GO_USE_ONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^use\s+(\S+)$").expect("the go use pattern is a literal"));

/// The tracked files of the repository and a reader of them.
pub(super) struct Tree<'a> {
    pub(super) files: &'a [String],
    pub(super) read: &'a dyn Fn(&str) -> Option<String>,
}

/// Whether a block head names a place inside the directory `member`: a path
/// under it, or the directory itself.
pub(super) fn inside(head: &str, member: &str) -> bool {
    head.strip_prefix(member)
        .is_some_and(|rest| rest.starts_with(['/', ' ']))
}

/// The Overview steps that apply to the question.
pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    let typed = ctx.typed;
    let workspace = ctx.wants(
        Step::WorkspaceOverview,
        &[QuestionKind::History, QuestionKind::Bugfix],
    ) && (ctx.kind == QuestionKind::Overview || WORKSPACE_GATE.is_match(typed));
    let answer = ctx.wants(
        Step::OverviewAnswerEvidence,
        &[
            QuestionKind::History,
            QuestionKind::Bugfix,
            QuestionKind::Review,
        ],
    ) && CRATE_GATE.is_match(typed);
    let manifest = ctx.wants(
        Step::MemberManifest,
        &[
            QuestionKind::History,
            QuestionKind::Bugfix,
            QuestionKind::Flow,
        ],
    ) && NAMED.is_match(typed);
    if !(workspace || answer || manifest) {
        return;
    }
    let Some(files) = ctx.files() else {
        return;
    };
    let read = |path: &str| ctx.read(path);
    let tree = Tree { files, read: &read };
    let before = out.lead.len() + out.append.len();
    if workspace {
        let _timer = ctx.timer(Step::WorkspaceOverview);
        out.lead.extend(workspace_overview(typed, &tree));
    }
    if answer {
        let _timer = ctx.timer(Step::OverviewAnswerEvidence);
        out.lead.extend(answer_evidence(typed, &tree));
    }
    if manifest {
        let _timer = ctx.timer(Step::MemberManifest);
        if let Some((member, found)) = member_manifest(typed, &tree) {
            out.append.extend(found);
            out.drops.push(Drop::Outside(member));
        }
    }
    out.rescuing |= out.lead.len() + out.append.len() > before;
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// `text` on one line: whitespace collapsed, cut at a word to `limit`
/// characters with `...` when longer.
fn one_line(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let cut: String = text.chars().take(limit).collect();
    let word = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}...", word.trim_end_matches([',', ';', ':', '-']))
}

/// The first `limit` characters of `text`.
fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Whether `ch` continues a name in a manifest or a prompt (`[\w-]`).
fn name_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '-'
}

/// Whether `haystack` holds `needle` as a whole name: not preceded or followed
/// by a name character.
fn has_name(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(start, found)| {
        haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !name_char(ch))
            && haystack[start + found.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !name_char(ch))
    })
}

/// `fnmatch` for `*` (any run of characters, `/` included) and `?`.
fn fnmatch(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|ch| *ch == '*')
}

/// The directories of `dirs` the workspace globs match; `!x` excludes.
fn expand(patterns: &[String], dirs: &[String]) -> Vec<String> {
    let (mut keep, mut drop) = (Vec::new(), Vec::new());
    for pattern in patterns {
        let negated = pattern.starts_with('!');
        let glob = pattern
            .trim_start_matches('!')
            .trim_matches('/')
            .trim_start_matches("./")
            .replace("**", "*");
        if negated {
            drop.push(glob);
        } else {
            keep.push(glob);
        }
    }
    dirs.iter()
        .filter(|dir| {
            keep.iter().any(|glob| fnmatch(glob, dir))
                && !drop.iter().any(|glob| fnmatch(glob, dir))
        })
        .cloned()
        .collect()
}

/// The `- item` entries under the top-level YAML key `packages`.
fn yaml_packages(text: &str) -> Vec<String> {
    let (mut out, mut inside) = (Vec::new(), false);
    for line in text.lines() {
        let trimmed = line.trim_start();
        if line.starts_with("packages") && line["packages".len()..].trim_start().starts_with(':') {
            inside = true;
        } else if inside && trimmed.starts_with('-') {
            let item = trimmed[1..].trim_start();
            let item = item.split(" #").next().unwrap_or("").trim();
            out.push(item.trim_matches(['\'', '"']).to_string());
        } else if inside && !line.trim().is_empty() && !line.starts_with([' ', '#']) {
            inside = false;
        }
    }
    out
}

/// The package name of a requirement such as `requests[socks]>=2`.
fn dep_name(spec: &str) -> &str {
    spec.trim()
        .split(|ch: char| ch.is_whitespace() || "<>=!~;[(@".contains(ch))
        .next()
        .unwrap_or("")
}

fn parse_toml(tree: &Tree, path: &str) -> Option<DocumentMut> {
    (tree.read)(path)?.parse().ok()
}

fn table(item: Option<&Item>) -> Option<&dyn toml_edit::TableLike> {
    item.and_then(Item::as_table_like)
}

fn strings(item: Option<&Item>) -> Vec<String> {
    item.and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn manifest_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn last_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

/// One member of a workspace. A dependency maps to whether the manifest says
/// it is a path dependency of the same repository.
#[derive(Debug, Default)]
struct Member {
    path: String,
    name: String,
    desc: Option<String>,
    deps: BTreeMap<String, bool>,
    dev: BTreeMap<String, bool>,
}

fn cargo_member(tree: &Tree, dir: &str, workspace_package: Option<&str>) -> Option<Member> {
    let doc = parse_toml(tree, &manifest_path(dir, "Cargo.toml"))?;
    let package = table(doc.get("package"))?;
    // `description.workspace = true` inherits the workspace's.
    let desc = package
        .get("description")
        .and_then(|item| match item.as_str() {
            Some(text) => Some(text),
            None if item.is_table_like() => workspace_package,
            None => None,
        })
        .map(ToString::to_string);
    let mut member = Member {
        path: if dir.is_empty() { "." } else { dir }.to_string(),
        name: package
            .get("name")
            .and_then(Item::as_str)
            .map_or_else(|| dir.to_string(), ToString::to_string),
        desc,
        ..Member::default()
    };
    for (key, into_deps) in [
        ("dependencies", true),
        ("build-dependencies", true),
        ("dev-dependencies", false),
    ] {
        let Some(deps) = table(doc.get(key)) else {
            continue;
        };
        for (name, value) in deps.iter() {
            let internal = value
                .as_table_like()
                .is_some_and(|t| t.contains_key("path"));
            if into_deps {
                member.deps.insert(name.to_string(), internal);
            } else {
                member.dev.insert(name.to_string(), internal);
            }
        }
    }
    Some(member)
}

fn npm_member(tree: &Tree, dir: &str) -> Option<Member> {
    let text = (tree.read)(&manifest_path(dir, "package.json"))?;
    let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = doc.get("name")?.as_str().filter(|name| !name.is_empty())?;
    let keys = |field: &str| -> Vec<String> {
        doc.get(field)
            .and_then(|value| value.as_object())
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    };
    let deps = ["dependencies", "peerDependencies", "optionalDependencies"]
        .into_iter()
        .flat_map(keys)
        .map(|key| (key, false))
        .collect();
    let dev = keys("devDependencies")
        .into_iter()
        .map(|key| (key, false))
        .collect();
    Some(Member {
        path: if dir.is_empty() { "." } else { dir }.to_string(),
        name: name.to_string(),
        desc: doc
            .get("description")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        deps,
        dev,
    })
}

fn go_member(tree: &Tree, dir: &str) -> Option<Member> {
    let text = (tree.read)(&manifest_path(dir, "go.mod"))?;
    let module = GO_MODULE.captures(&text)?[1].to_string();
    let mut deps = BTreeMap::new();
    for block in GO_REQUIRE_BLOCK.captures_iter(&text) {
        for line in block[1].lines() {
            if !line.contains("// indirect")
                && let Some(name) = line.split_whitespace().next()
            {
                deps.insert(name.to_string(), false);
            }
        }
    }
    for line in text.lines() {
        if let Some(caps) = GO_REQUIRE_ONE.captures(line)
            && !caps[2].contains("// indirect")
        {
            deps.insert(caps[1].to_string(), false);
        }
    }
    Some(Member {
        path: if dir.is_empty() { "." } else { dir }.to_string(),
        name: module,
        desc: None,
        deps,
        dev: BTreeMap::new(),
    })
}

fn python_member(tree: &Tree, dir: &str) -> Option<Member> {
    let doc = parse_toml(tree, &manifest_path(dir, "pyproject.toml"))?;
    let project = table(doc.get("project"))?;
    let name = project
        .get("name")
        .and_then(Item::as_str)
        .filter(|name| !name.is_empty())?;
    let deps = strings(project.get("dependencies"))
        .iter()
        .map(|spec| (dep_name(spec).to_string(), false))
        .collect();
    let mut dev = BTreeMap::new();
    if let Some(groups) = table(doc.get("dependency-groups")) {
        for (_, group) in groups.iter() {
            for spec in strings(Some(group)) {
                dev.insert(dep_name(&spec).to_string(), false);
            }
        }
    }
    Some(Member {
        path: if dir.is_empty() { "." } else { dir }.to_string(),
        name: name.to_string(),
        desc: project
            .get("description")
            .and_then(Item::as_str)
            .map(ToString::to_string),
        deps,
        dev,
    })
}

/// The ecosystem and members of the repository's workspace, or `("", [])`.
fn discover(tree: &Tree) -> (&'static str, Vec<Member>) {
    let dirs = |manifest: &str| -> Vec<String> {
        let suffix = format!("/{manifest}");
        let mut dirs: Vec<String> = tree
            .files
            .iter()
            .filter(|file| file.as_str() == manifest || file.ends_with(&suffix))
            .map(|file| parent(file).to_string())
            .filter(|dir| !dir.is_empty())
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    };
    let excluded = |picked: Vec<String>, exclude: &[String]| -> Vec<String> {
        picked
            .into_iter()
            .filter(|dir| {
                !exclude
                    .iter()
                    .any(|glob| fnmatch(glob.trim_matches('/'), dir))
            })
            .collect()
    };

    if let Some(doc) = parse_toml(tree, "Cargo.toml")
        && let Some(workspace) = table(doc.get("workspace"))
    {
        let patterns = strings(workspace.get("members"));
        if !patterns.is_empty() {
            let workspace_package = table(workspace.get("package"))
                .and_then(|package| package.get("description"))
                .and_then(Item::as_str);
            let picked = excluded(
                expand(&patterns, &dirs("Cargo.toml")),
                &strings(workspace.get("exclude")),
            );
            let mut members: Vec<Member> = cargo_member(tree, "", workspace_package)
                .into_iter()
                .collect();
            members.extend(
                picked
                    .iter()
                    .take(MAX_MEMBERS)
                    .filter_map(|dir| cargo_member(tree, dir, workspace_package)),
            );
            return ("cargo", members);
        }
    }

    let npm = (tree.read)("package.json")
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|doc| {
            let workspaces = doc.get("workspaces")?.clone();
            let list = workspaces.get("packages").unwrap_or(&workspaces);
            Some(
                list.as_array()?
                    .iter()
                    .filter_map(|value| value.as_str().map(ToString::to_string))
                    .collect::<Vec<_>>(),
            )
        })
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| yaml_packages(&(tree.read)("pnpm-workspace.yaml").unwrap_or_default()));
    if !npm.is_empty() {
        let members = expand(&npm, &dirs("package.json"))
            .iter()
            .take(MAX_MEMBERS)
            .filter_map(|dir| npm_member(tree, dir))
            .collect();
        return ("npm", members);
    }

    if let Some(doc) = parse_toml(tree, "pyproject.toml")
        && let Some(uv) = table(doc.get("tool"))
            .and_then(|tool| table(tool.get("uv")))
            .and_then(|uv| table(uv.get("workspace")))
    {
        let patterns = strings(uv.get("members"));
        if !patterns.is_empty() {
            let picked = excluded(
                expand(&patterns, &dirs("pyproject.toml")),
                &strings(uv.get("exclude")),
            );
            let mut members: Vec<Member> = python_member(tree, "").into_iter().collect();
            members.extend(
                picked
                    .iter()
                    .take(MAX_MEMBERS)
                    .filter_map(|dir| python_member(tree, dir)),
            );
            return ("python", members);
        }
    }

    let work = (tree.read)("go.work").unwrap_or_default();
    let mut uses: Vec<String> = GO_USE_ONE
        .captures_iter(&work)
        .map(|caps| caps[1].to_string())
        .collect();
    for block in GO_USE_BLOCK.captures_iter(&work) {
        uses.extend(block[1].lines().filter_map(|line| {
            let item = line.split("//").next().unwrap_or("").trim();
            (!item.is_empty()).then(|| item.to_string())
        }));
    }
    // A block's opening `use (` also matches the one-line form.
    uses.retain(|used| used != "(");
    if !uses.is_empty() {
        let members = uses
            .iter()
            .take(MAX_MEMBERS)
            .filter_map(|used| {
                // `use .` is the root module; a dot inside a directory name
                // stays (Pixel's probe stripped every dot).
                let dir = used.trim_start_matches("./").trim_matches('/');
                go_member(tree, if dir == "." { "" } else { dir })
            })
            .collect();
        return ("go", members);
    }
    ("", Vec::new())
}

// ---------------------------------------------------------------------------
// workspace-overview
// ---------------------------------------------------------------------------

/// The lead paragraph of the top-level README, cut to a few sentences.
fn readme_lead(tree: &Tree) -> String {
    let Some(name) = tree
        .files
        .iter()
        .find(|file| !file.contains('/') && README.is_match(file))
    else {
        return String::new();
    };
    let text = (tree.read)(name).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let (mut paragraph, mut started) = (Vec::new(), false);
    for (index, line) in lines.iter().enumerate() {
        let text = line.trim();
        let next = lines.get(index + 1).map_or("", |next| next.trim());
        let title = TITLE_RULE.is_match(next) && paragraph.is_empty();
        let skip = text.is_empty()
            || ["#", "![", "[![", "<", ">", "|", "```", "* ", "- ", "["]
                .iter()
                .any(|prefix| text.starts_with(prefix))
            || RULE.is_match(text)
            || title
            || LICENSED.is_match(text);
        if skip {
            if started {
                break;
            }
            continue;
        }
        started = true;
        paragraph.push(text);
    }
    let text = paragraph.join(" ");
    if text.chars().count() <= LEAD_CHARS {
        return text;
    }
    let cut: String = text.chars().take(LEAD_CHARS).collect();
    let end = [cut.rfind(". "), cut.rfind("? ")]
        .into_iter()
        .flatten()
        .max();
    match end {
        Some(end) if cut[..end].chars().count() > 120 => cut[..=end].to_string(),
        _ => format!(
            "{}...",
            cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head)
        ),
    }
}

fn names_line(label: &str, names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let mut names = names.to_vec();
    names.sort();
    let more = names.len().saturating_sub(NAMES_PER_LINE);
    let shown = names[..names.len() - more].join(", ");
    Some(if more > 0 {
        format!("{label}: {shown} +{more} more")
    } else {
        format!("{label}: {shown}")
    })
}

fn member_block(member: &Member, members: &[Member], files: &[String]) -> Block {
    let member_names: Vec<String> = members
        .iter()
        .map(|m| m.name.replace('_', "-").to_lowercase())
        .collect();
    let split = |table: &BTreeMap<String, bool>| -> (Vec<String>, Vec<String>) {
        table
            .iter()
            .fold((Vec::new(), Vec::new()), |mut acc, (name, internal)| {
                let inner =
                    *internal || member_names.contains(&name.replace('_', "-").to_lowercase());
                (if inner { &mut acc.0 } else { &mut acc.1 }).push(name.clone());
                acc
            })
    };
    let (inner, outer) = split(&member.deps);
    let (dev_inner, dev_outer) = split(&member.dev);
    let where_ = member.path.as_str();
    let title = if member.name == last_segment(where_) {
        where_.to_string()
    } else {
        format!("{where_} [{}]", member.name)
    };
    let head = match &member.desc {
        Some(desc) => format!("{title} — {}", one_line(desc, 100)),
        None => title,
    };
    let mut body: Vec<String> = [
        names_line("depends on (workspace)", &inner),
        names_line("depends on (external)", &outer),
        names_line("dev (workspace)", &dev_inner),
        names_line("dev (external)", &dev_outer),
    ]
    .into_iter()
    .flatten()
    .collect();
    let prefix = if where_ == "." {
        String::new()
    } else {
        format!("{where_}/")
    };
    let src: Vec<&str> = files
        .iter()
        .filter_map(|file| file.strip_prefix(prefix.as_str()))
        .filter(|rest| {
            !["tests/", "test/", "benches/", "examples/"]
                .iter()
                .any(|dir| rest.starts_with(dir))
                && SOURCE_FILE.is_match(rest)
                && (where_ != "." || rest.matches('/').count() <= 1)
        })
        .collect();
    if !src.is_empty() {
        let more = src.len().saturating_sub(SRC_NAMES);
        let mut shown = src[..src.len() - more].join(", ");
        if more > 0 {
            shown.push_str(&format!(" +{more} more"));
        }
        body.push(format!("files: {shown}"));
    }
    Block::new(head, body)
}

/// The members of `members` the prompt names: by full package name when that
/// name is distinctive (`grep-cli`, `@scope/pkg`), or by a short name beside
/// a noun such as crate or package (`the grep crate`, `crate grep`).
fn named_members<'a>(prompt: &str, members: &'a [Member]) -> Vec<&'a Member> {
    members
        .iter()
        .filter(|member| member.path != ".")
        .filter(|member| {
            let mut shorts = vec![member.name.as_str(), last_segment(&member.path)];
            shorts.dedup();
            shorts.into_iter().any(|short| {
                let distinctive = short == member.name
                    && (short.contains('-') || short.contains('/') || short.contains('_'));
                (distinctive && has_exact_name(prompt, short)) || beside_noun(prompt, short)
            })
        })
        .collect()
}

/// `short` as a whole name: not preceded by `[\w/@.-]` nor followed by `[\w-]`.
fn has_exact_name(prompt: &str, short: &str) -> bool {
    prompt.match_indices(short).any(|(start, found)| {
        prompt[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !(name_char(ch) || "/@.".contains(ch)))
            && prompt[start + found.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !name_char(ch))
    })
}

/// `short` beside a noun, in either order, whatever its case.
fn beside_noun(prompt: &str, short: &str) -> bool {
    let escaped = regex::escape(short);
    let (Ok(name), Ok(anchored)) = (
        Regex::new(&format!("(?i){escaped}")),
        Regex::new(&format!("(?i)^{escaped}")),
    ) else {
        return false;
    };
    let before = name.find_iter(prompt).any(|found| {
        prompt[..found.start()]
            .chars()
            .next_back()
            .is_none_or(|ch| !name_char(ch))
            && NOUN_AFTER.is_match(&prompt[found.end()..])
    });
    before
        || NOUN_BEFORE.find_iter(prompt).any(|noun| {
            let rest = &prompt[noun.end()..];
            anchored.find(rest).is_some_and(|found| {
                rest[found.end()..]
                    .chars()
                    .next()
                    .is_none_or(|ch| !name_char(ch))
            })
        })
}

/// One line per workspace member and the README lead, or the blocks of the
/// members the prompt names.
fn workspace_overview(typed: &str, tree: &Tree) -> Vec<Block> {
    let (kind, members) = discover(tree);
    let real = members.iter().filter(|m| m.path != ".").count();
    if real < 2 {
        return Vec::new();
    }
    let named = named_members(typed, &members);
    if !named.is_empty() {
        return named
            .into_iter()
            .take(2)
            .map(|member| member_block(member, &members, tree.files))
            .collect();
    }
    if !OVERVIEW.is_match(typed) {
        return Vec::new();
    }
    let lines = members
        .iter()
        .map(|member| {
            let desc = member.desc.as_deref();
            if member.path == "." {
                let tail = desc.map_or(String::new(), |d| format!(" — {}", one_line(d, 70)));
                return format!(". [{}] (root){tail}", member.name);
            }
            let tag = if member.name == last_segment(&member.path) {
                String::new()
            } else {
                format!(" [{}]", member.name)
            };
            let tail = desc.map_or(String::new(), |d| format!(" — {}", one_line(d, DESC_CHARS)));
            format!("{}{tag}{tail}", member.path)
        })
        .collect();
    let mut blocks = vec![Block::new(
        format!("workspace — {real} members ({kind})"),
        lines,
    )];
    let lead = readme_lead(tree);
    if !lead.is_empty() {
        blocks.push(Block::new("README — lead".to_string(), vec![lead]));
    }
    blocks
}

// ---------------------------------------------------------------------------
// cand-k-member-manifest
// ---------------------------------------------------------------------------

/// The manifest of the one member the prompt names (`the x crate`), with its
/// identity lines cut, and every tracked file of its directory. The member's
/// directory is returned so the rest of the pack can be cut to it.
fn member_manifest(typed: &str, tree: &Tree) -> Option<(String, Vec<Block>)> {
    let manifests: Vec<&String> = tree
        .files
        .iter()
        .filter(|file| file.contains('/') && MANIFEST_NAMES.contains(&last_segment(file)))
        .collect();
    let names: Vec<String> = NAMED
        .captures_iter(typed)
        .map(|caps| caps[1].to_lowercase())
        .filter(|name| {
            !["this", "that", "each", "which", "other", "the", "a"].contains(&name.as_str())
        })
        .collect();
    for name in names {
        let Some(hit) = manifests
            .iter()
            .find(|path| last_segment(parent(path)).to_lowercase() == name)
        else {
            continue;
        };
        let member = parent(hit);
        let text = (tree.read)(hit)?;
        let lines: Vec<&str> = text.lines().collect();
        let shown: Vec<String> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !NOISE.is_match(line))
            .take(MANIFEST_LINES)
            .map(|(index, line)| format!("{}| {}", index + 1, clip(line, 140)))
            .collect();
        let prefix = format!("{member}/");
        let inside: Vec<&str> = tree
            .files
            .iter()
            .filter_map(|file| file.strip_prefix(prefix.as_str()))
            .collect();
        let more = inside.len().saturating_sub(MANIFEST_FILES);
        let mut listed: Vec<String> = inside[..inside.len() - more]
            .iter()
            .map(ToString::to_string)
            .collect();
        if more > 0 {
            listed.push(format!("+{more} more"));
        }
        return Some((
            member.to_string(),
            vec![
                Block::new(
                    format!(
                        "{hit}:1-{} — manifest of {name} (identity lines cut)",
                        lines.len()
                    ),
                    shown,
                ),
                Block::new(
                    format!("{member}/ — {} tracked files", inside.len()),
                    vec![listed.join(", ")],
                ),
            ],
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// overview-answer-evidence
// ---------------------------------------------------------------------------

/// The leading `//!` or `/*! */` documentation of a Rust source.
fn leading_docs(text: &str, max_lines: usize) -> Vec<String> {
    let (mut out, mut block): (Vec<String>, bool) = (Vec::new(), false);
    for raw in text.lines().take(max_lines) {
        let line = if raw.chars().count() > MAX_DOC_CHARS {
            format!("{}…", clip(raw, MAX_DOC_CHARS))
        } else {
            raw.to_string()
        };
        let mut stripped = line.trim().to_string();
        if out.is_empty() && stripped.is_empty() {
            continue;
        }
        if out.is_empty() && stripped.starts_with("/*!") {
            block = true;
            stripped = stripped[3..].trim().to_string();
        } else if block {
            if let Some((head, _)) = stripped.split_once("*/") {
                block = false;
                stripped = head.trim_start_matches(['*', ' ']).trim_end().to_string();
            } else {
                stripped = stripped.trim_start_matches(['*', ' ']).to_string();
            }
        } else if let Some(rest) = stripped.strip_prefix("//!") {
            stripped = rest.trim().to_string();
        } else if stripped.is_empty() {
            continue;
        } else {
            break;
        }
        if !stripped.is_empty() {
            out.push(stripped.clone());
        }
        let trimmed = line.trim();
        if !block
            && !stripped.is_empty()
            && !["//!", "/*", "*"]
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
        {
            break;
        }
    }
    out
}

fn numbered(text: &str, limit: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = lines
        .iter()
        .take(limit)
        .enumerate()
        .map(|(index, line)| format!("{}| {}", index + 1, clip(line, 180)))
        .collect();
    if lines.len() > limit {
        out.push(format!("... {} lines omitted", lines.len() - limit));
    }
    out
}

/// A tracked Cargo manifest.
struct Crate {
    path: String,
    doc: DocumentMut,
}

impl Crate {
    fn package(&self) -> Option<&dyn toml_edit::TableLike> {
        table(self.doc.get("package"))
    }

    fn package_str(&self, key: &str) -> Option<&str> {
        self.package()?.get(key)?.as_str()
    }

    /// The library root: the manifest's `[lib].path`, else `src/lib.rs`.
    fn source(&self, tree: &Tree) -> Option<(String, String)> {
        let dir = parent(&self.path);
        let configured = table(self.doc.get("lib"))
            .and_then(|lib| lib.get("path"))
            .and_then(Item::as_str);
        let path = match configured {
            Some(path) => format!("{dir}/{path}"),
            None => format!("{dir}/src/lib.rs"),
        };
        let text = tree.files.contains(&path).then(|| (tree.read)(&path))??;
        Some((path, text))
    }
}

fn answer_evidence(typed: &str, tree: &Tree) -> Vec<Block> {
    if tree.files.len() > MAX_TRACKED_PATHS {
        return Vec::new();
    }
    let crates: Vec<Crate> = tree
        .files
        .iter()
        .filter(|file| file.as_str() == "Cargo.toml" || file.ends_with("/Cargo.toml"))
        .filter_map(|path| {
            Some(Crate {
                path: path.clone(),
                doc: parse_toml(tree, path)?,
            })
        })
        .collect();
    let folded = typed.to_lowercase();
    let named: Vec<&Crate> = crates
        .iter()
        .filter(|candidate| candidate.path != "Cargo.toml")
        .filter(|candidate| {
            let Some(package) = candidate.package_str("name") else {
                return false;
            };
            [package, last_segment(parent(&candidate.path))]
                .iter()
                .any(|name| has_name(&folded, &name.to_lowercase()))
        })
        .collect();
    let [member] = named[..] else {
        return Vec::new();
    };
    let directory = parent(&member.path);

    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let guidance: Vec<&String> = tree
        .files
        .iter()
        .filter(|file| last_segment(file) == "AGENTS.md")
        .collect();
    let mut instructions = Vec::new();
    if guidance.is_empty() {
        instructions.push("No tracked AGENTS.md instruction files are present.".to_string());
    }
    for path in guidance.iter().take(8) {
        if let Some(text) = (tree.read)(path) {
            instructions.push(format!("{path}:"));
            instructions.extend(text.lines().take(12).map(|line| clip(line, MAX_DOC_CHARS)));
        }
    }
    out.push(("Repository instructions".to_string(), instructions));

    let Some((source, main_source)) = member.source(tree) else {
        return Vec::new();
    };
    let mut purpose = vec![
        Some(one_line(
            member.package_str("description").unwrap_or(""),
            160,
        ))
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| "crate-level documentation:".to_string()),
    ];
    purpose.extend(leading_docs(&main_source, MAX_DOC_LINES));
    out.push((
        format!("{source} — crate purpose and public surface"),
        purpose,
    ));
    if let Some(text) = (tree.read)(&member.path) {
        out.push((
            format!("{} — line-numbered package manifest", member.path),
            numbered(&text, 100),
        ));
    }
    out.push((
        format!("{source} — line-numbered crate facade"),
        numbered(&main_source, 100),
    ));

    let prefix = format!("{directory}/");
    let mut member_files: Vec<&str> = tree
        .files
        .iter()
        .filter_map(|file| file.strip_prefix(prefix.as_str()))
        .collect();
    member_files.sort_unstable();
    let mut listing: Vec<String> = member_files
        .iter()
        .take(100)
        .map(ToString::to_string)
        .collect();
    if member_files.len() > 100 {
        listing.push(format!("... {} paths omitted", member_files.len() - 100));
    }
    for (key, label) in [
        ("dependencies", "dependencies"),
        ("dev-dependencies", "development dependencies"),
    ] {
        let Some(deps) = table(member.doc.get(key)) else {
            continue;
        };
        if deps.iter().next().is_some() {
            listing.push(format!("{label}:"));
        }
        let mut names: Vec<(&str, &Item)> = deps.iter().collect();
        names.sort_by_key(|(name, _)| *name);
        for (name, spec) in names {
            let local = crates
                .iter()
                .find(|other| other.package_str("name") == Some(name));
            let optional = spec
                .as_table_like()
                .and_then(|t| t.get("optional"))
                .and_then(Item::as_bool)
                == Some(true);
            let described = local.and_then(|other| other.package_str("description"));
            listing.push(format!(
                "- {name}{}{}",
                if optional { " (optional)" } else { "" },
                described.map_or(String::new(), |d| format!(" — {}", one_line(d, 160)))
            ));
            if let Some(other) = local
                && let Some((dep_source, dep_text)) = other.source(tree)
            {
                let docs = leading_docs(&dep_text, 12);
                if !docs.is_empty() {
                    listing.push(format!("  {dep_source} — leading module documentation"));
                    listing.extend(docs.into_iter().map(|line| format!("  {line}")));
                }
            }
        }
    }
    out.push((
        format!(
            "Tracked files in {directory} ({} total)",
            member_files.len()
        ),
        listing,
    ));

    let examples = format!("{directory}/examples/");
    for example in tree
        .files
        .iter()
        .filter(|file| file.starts_with(&examples) && file.ends_with(".rs"))
        .take(3)
    {
        if let Some(text) = (tree.read)(example) {
            out.push((
                format!("{example} — example source"),
                numbered(&text, EXAMPLE_LINES),
            ));
        }
    }
    bounded(out)
}

/// The sections as blocks, stopping once [`MAX_TOTAL`] characters are used
/// and saying so in a last block. Each block is cut to the pack's own block
/// cap first, so the count is of what the pack would carry.
fn bounded(sections: Vec<(String, Vec<String>)>) -> Vec<Block> {
    let mut used = 0;
    let mut blocks = Vec::new();
    for (head, body) in sections {
        if used >= MAX_TOTAL {
            blocks.push(Block::new(
                "evidence truncated at the configured character budget".to_string(),
                Vec::new(),
            ));
            break;
        }
        let block = Block::new(head, body);
        used += block.text().len() + 1;
        blocks.push(block);
    }
    blocks
}

#[cfg(test)]
mod tests;
