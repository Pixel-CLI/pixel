// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `workspace-overview`: two questions a manifest answers in a few lines.
//!
//! * "What does X do and which crates/packages make up the workspace?": one
//!   line per workspace member (path, package name when it differs, one-line
//!   description) and the README's lead paragraph, cut to a few sentences.
//! * "<member> questions": that member's description, its internal and
//!   external dependency names (names only) and its tracked source files.
//!
//! Ported from `probes/workspace-overview/probe`. Workspaces are read from
//! the manifests when the step runs: Cargo `[workspace].members`, package.json
//! `workspaces`, `pnpm-workspace.yaml`, `go.work` (a `use` directory loses
//! every dot, as the script does) and pyproject `[tool.uv.workspace]`. No name
//! is written here. Read-only, sorted output, nothing printed when the
//! repository is no workspace or the prompt asks about neither. Like the
//! script, a `package.json` of a member that is not JSON ends the step with
//! nothing printed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;
use toml_edit::{DocumentMut, Item, TableLike};

use super::text::{last_segment, splitlines, universal_newlines};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "workspace-overview",
    when_kinds: &[],
    skip_kinds: &["history", "impact"],
    when_prompt: Some(
        r"(?i)\b(crates?|workspace|monorepo|packages?|members?|modules?|depend\w*|made up|make up|consists?)\b",
    ),
    when_gate_closed: true,
    budget_ms: 1500,
    mode: ProbeMode::Lead,
    run,
};

const MAX_MEMBERS: usize = 40;
const DESC_CHARS: usize = 80;
const LEAD_CHARS: usize = 380;
const NAMES_PER_LINE: usize = 14;
const SRC_NAMES: usize = 16;
/// Bytes of a manifest or a README that are read; the script has no cap, but
/// a file this large is none of them.
const READ_LIMIT: u64 = 2_000_000;
const MEMBER_NOUNS: &str =
    r"(?:crates?|packages?|modules?|libraries|library|libs?|members?|workspaces?)";

static OVERVIEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(workspaces?|monorepo|crates|packages|members|made up|make up|consists?|overview|layout|structure)\b",
    )
    .expect("the overview-question pattern is a literal")
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
/// The script's `\S+(?!.*// indirect)`, which Rust's regex has no lookahead
/// for: the rest of the line is captured and checked.
static GO_REQUIRE_ONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^require\s+([^\s(]\S*)\s+\S+(.*)$")
        .expect("the go require pattern is a literal")
});
static GO_USE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?ms)^use\s*\((.*?)^\)").expect("the go use pattern is a literal")
});
static GO_USE_ONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^use\s+(\S+)$").expect("the go use pattern is a literal"));

fn run(ctx: &Ctx) -> String {
    let Some(files) = ctx.files() else {
        return String::new();
    };
    let Some((kind, members)) = discover(ctx, files) else {
        return String::new();
    };
    let real = members.iter().filter(|m| m.path != ".").count();
    if real < 2 {
        return String::new();
    }
    let named = named_members(ctx.prompt, &members);
    let mut lines: Vec<String> = Vec::new();
    if !named.is_empty() {
        for member in named.into_iter().take(2) {
            lines.extend(member_block(member, &members, files));
        }
    } else if OVERVIEW.is_match(ctx.prompt) {
        lines.push(format!("### workspace — {real} members ({kind})"));
        for member in &members {
            lines.push(overview_line(member));
        }
        let lead = readme_lead(ctx, files);
        if !lead.is_empty() {
            lines.push("### README — lead".to_owned());
            lines.push(lead);
        }
    }
    if lines.is_empty() || ctx.timed_out() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

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

/// One element of a `fnmatch` pattern.
enum Token {
    /// `*`: any run of characters, `/` included.
    Any,
    /// `?`.
    One,
    Literal(char),
    /// `[seq]` (false) or `[!seq]` (true), with its ranges.
    Class(bool, Vec<(char, char)>),
}

/// `fnmatch.fnmatchcase`'s reading of `pattern`.
fn tokens(pattern: &str) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let ch = chars[at];
        at += 1;
        match ch {
            '*' => {
                if !matches!(out.last(), Some(Token::Any)) {
                    out.push(Token::Any);
                }
            }
            '?' => out.push(Token::One),
            '[' => {
                let mut end = at;
                if chars.get(end) == Some(&'!') {
                    end += 1;
                }
                if chars.get(end) == Some(&']') {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end >= chars.len() {
                    out.push(Token::Literal('['));
                    continue;
                }
                let mut set = &chars[at..end];
                at = end + 1;
                let negated = set.first() == Some(&'!');
                if negated {
                    set = &set[1..];
                }
                let mut ranges = Vec::new();
                let mut item = 0;
                while item < set.len() {
                    if set.get(item + 1) == Some(&'-') && item + 2 < set.len() {
                        ranges.push((set[item], set[item + 2]));
                        item += 3;
                    } else {
                        ranges.push((set[item], set[item]));
                        item += 1;
                    }
                }
                out.push(Token::Class(negated, ranges));
            }
            other => out.push(Token::Literal(other)),
        }
    }
    out
}

fn token_takes(token: &Token, ch: char) -> bool {
    match token {
        Token::Any => false,
        Token::One => true,
        Token::Literal(literal) => *literal == ch,
        Token::Class(negated, ranges) => {
            ranges.iter().any(|(low, high)| (*low..=*high).contains(&ch)) != *negated
        }
    }
}

/// `fnmatch.fnmatchcase(text, pattern)`.
fn fnmatch(pattern: &str, text: &str) -> bool {
    let tokens = tokens(pattern);
    let text: Vec<char> = text.chars().collect();
    let (mut at, mut ch) = (0, 0);
    // Where the last `*` stands, and the text position it has reached.
    let mut star: Option<(usize, usize)> = None;
    while ch < text.len() {
        if let Some(Token::Any) = tokens.get(at) {
            star = Some((at, ch));
            at += 1;
        } else if tokens.get(at).is_some_and(|t| token_takes(t, text[ch])) {
            at += 1;
            ch += 1;
        } else if let Some((star_at, star_ch)) = star {
            at = star_at + 1;
            ch = star_ch + 1;
            star = Some((star_at, star_ch + 1));
        } else {
            return false;
        }
    }
    tokens[at..].iter().all(|t| matches!(t, Token::Any))
}

/// The directories of `dirs` the workspace globs match; `!x` excludes.
fn expand(patterns: &[String], dirs: &[String]) -> Vec<String> {
    let (mut keep, mut drop) = (Vec::new(), Vec::new());
    for pattern in patterns {
        let glob = pattern.trim_start_matches('!').trim_matches('/');
        let glob = glob.strip_prefix("./").unwrap_or(glob).replace("**", "*");
        if pattern.starts_with('!') {
            drop.push(glob);
        } else {
            keep.push(glob);
        }
    }
    dirs.iter()
        .filter(|dir| {
            keep.iter().any(|glob| fnmatch(glob, dir)) && !drop.iter().any(|glob| fnmatch(glob, dir))
        })
        .cloned()
        .collect()
}

/// The `- item` entries under the top-level YAML key `packages`.
fn yaml_packages(text: &str) -> Vec<String> {
    let (mut out, mut inside) = (Vec::new(), false);
    for line in splitlines(text) {
        if line
            .strip_prefix("packages")
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
        {
            inside = true;
        } else if inside && let Some(item) = line.trim_start().strip_prefix('-') {
            let item = item.trim_start().split(" #").next().unwrap_or("").trim();
            out.push(item.trim_matches(['\'', '"']).to_owned());
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

/// The text of a tracked file as Python's `open()` reads it, `""` when it
/// cannot be read.
fn read(ctx: &Ctx, path: &str) -> String {
    ctx.read(path, READ_LIMIT)
        .map_or_else(String::new, |text| universal_newlines(&text))
}

fn parse_toml(ctx: &Ctx, path: &str) -> Option<DocumentMut> {
    read(ctx, path).parse().ok()
}

fn table(item: Option<&Item>) -> Option<&dyn TableLike> {
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
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}


/// One member of a workspace. A dependency maps to whether the manifest says
/// it is a path dependency of the same repository.
struct Member {
    path: String,
    name: String,
    desc: Option<String>,
    deps: BTreeMap<String, bool>,
    dev: BTreeMap<String, bool>,
}

/// The member's path as the blocks spell it: `.` for the root.
fn member_path(dir: &str) -> String {
    if dir.is_empty() { "." } else { dir }.to_owned()
}

/// A description the script prints: an empty one is none.
fn description(text: Option<&str>) -> Option<String> {
    text.filter(|text| !text.is_empty()).map(ToString::to_string)
}

fn cargo_member(ctx: &Ctx, dir: &str, workspace_desc: Option<&str>) -> Option<Member> {
    let doc = parse_toml(ctx, &manifest_path(dir, "Cargo.toml"))?;
    let package = table(doc.get("package")).filter(|package| !package.is_empty())?;
    // `description.workspace = true` inherits the workspace's.
    let desc = match package.get("description") {
        Some(item) if item.is_table_like() => workspace_desc,
        other => other.and_then(Item::as_str),
    };
    let mut member = Member {
        path: member_path(dir),
        name: package
            .get("name")
            .and_then(Item::as_str)
            .unwrap_or(dir)
            .to_owned(),
        desc: description(desc),
        deps: BTreeMap::new(),
        dev: BTreeMap::new(),
    };
    for (key, dev) in [
        ("dependencies", false),
        ("build-dependencies", false),
        ("dev-dependencies", true),
    ] {
        let Some(deps) = table(doc.get(key)) else {
            continue;
        };
        for (name, value) in deps.iter() {
            let internal = value
                .as_table_like()
                .is_some_and(|dep| dep.contains_key("path"));
            let into = if dev { &mut member.dev } else { &mut member.deps };
            into.insert(name.to_owned(), internal);
        }
    }
    Some(member)
}

/// The manifest of the package in `dir` is no JSON: the script ends there.
struct NotJson;

fn npm_member(ctx: &Ctx, dir: &str) -> Result<Option<Member>, NotJson> {
    let text = read(ctx, &manifest_path(dir, "package.json"));
    let doc: serde_json::Value = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).map_err(|_| NotJson)?
    };
    let Some(name) = doc.get("name").and_then(|name| name.as_str()).filter(|name| !name.is_empty())
    else {
        return Ok(None);
    };
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
    Ok(Some(Member {
        path: member_path(dir),
        name: name.to_owned(),
        desc: description(doc.get("description").and_then(|value| value.as_str())),
        deps,
        dev,
    }))
}

fn go_member(ctx: &Ctx, dir: &str) -> Option<Member> {
    let text = read(ctx, &manifest_path(dir, "go.mod"));
    let module = GO_MODULE.captures(&text)?[1].to_owned();
    let mut deps = BTreeMap::new();
    for block in GO_REQUIRE_BLOCK.captures_iter(&text) {
        for line in splitlines(&block[1]) {
            if !line.contains("// indirect")
                && let Some(name) = line.split_whitespace().next()
            {
                deps.insert(name.to_owned(), false);
            }
        }
    }
    for caps in GO_REQUIRE_ONE.captures_iter(&text) {
        if !caps[2].contains("// indirect") {
            deps.insert(caps[1].to_owned(), false);
        }
    }
    Some(Member {
        path: member_path(dir),
        name: module,
        desc: None,
        deps,
        dev: BTreeMap::new(),
    })
}

fn python_member(ctx: &Ctx, dir: &str) -> Option<Member> {
    let doc = parse_toml(ctx, &manifest_path(dir, "pyproject.toml"))?;
    let project = table(doc.get("project"))?;
    let name = project
        .get("name")
        .and_then(Item::as_str)
        .filter(|name| !name.is_empty())?;
    let deps = strings(project.get("dependencies"))
        .iter()
        .map(|spec| (dep_name(spec).to_owned(), false))
        .collect();
    let mut dev = BTreeMap::new();
    for (_, group) in table(doc.get("dependency-groups"))
        .map(|groups| groups.iter().collect::<Vec<_>>())
        .unwrap_or_default()
    {
        for spec in strings(Some(group)) {
            dev.insert(dep_name(&spec).to_owned(), false);
        }
    }
    Some(Member {
        path: member_path(dir),
        name: name.to_owned(),
        desc: description(project.get("description").and_then(Item::as_str)),
        deps,
        dev,
    })
}

/// The directories holding a `manifest`, root excluded, sorted.
fn manifest_dirs(files: &[String], manifest: &str) -> Vec<String> {
    let suffix = format!("/{manifest}");
    let dirs: BTreeSet<&str> = files
        .iter()
        .filter(|file| file.as_str() == manifest || file.ends_with(&suffix))
        .map(|file| file.rsplit_once('/').map_or("", |(dir, _)| dir))
        .filter(|dir| !dir.is_empty())
        .collect();
    dirs.into_iter().map(ToString::to_string).collect()
}

/// `picked` without the directories an `exclude` glob matches.
fn without(picked: Vec<String>, exclude: &[String]) -> Vec<String> {
    picked
        .into_iter()
        .filter(|dir| {
            !exclude
                .iter()
                .any(|glob| fnmatch(glob.trim_matches('/'), dir))
        })
        .collect()
}

/// The ecosystem and members of the repository's workspace, `("", [])` when
/// it has none, `None` when the script would have ended without output.
fn discover(ctx: &Ctx, files: &[String]) -> Option<(&'static str, Vec<Member>)> {
    if let Some(doc) = parse_toml(ctx, "Cargo.toml")
        && let Some(workspace) = table(doc.get("workspace"))
    {
        let patterns = strings(workspace.get("members"));
        if !patterns.is_empty() {
            let workspace_desc = table(workspace.get("package"))
                .and_then(|package| package.get("description"))
                .and_then(Item::as_str);
            let picked = without(
                expand(&patterns, &manifest_dirs(files, "Cargo.toml")),
                &strings(workspace.get("exclude")),
            );
            let mut members: Vec<Member> = cargo_member(ctx, "", workspace_desc)
                .into_iter()
                .collect();
            for dir in picked.iter().take(MAX_MEMBERS) {
                if ctx.timed_out() {
                    return None;
                }
                members.extend(cargo_member(ctx, dir, workspace_desc));
            }
            return Some(("cargo", members));
        }
    }

    let package_json = serde_json::from_str::<serde_json::Value>(&read(ctx, "package.json")).ok();
    let from_json = package_json.and_then(|doc| {
        let workspaces = doc.get("workspaces")?;
        let list = workspaces.get("packages").unwrap_or(workspaces);
        Some(
            list.as_array()?
                .iter()
                .filter_map(|value| value.as_str().map(ToString::to_string))
                .collect::<Vec<_>>(),
        )
    });
    let patterns = from_json
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| yaml_packages(&read(ctx, "pnpm-workspace.yaml")));
    if !patterns.is_empty() {
        let mut members = Vec::new();
        for dir in expand(&patterns, &manifest_dirs(files, "package.json")).iter().take(MAX_MEMBERS) {
            if ctx.timed_out() {
                return None;
            }
            members.extend(npm_member(ctx, dir).ok()?);
        }
        return Some(("npm", members));
    }

    if let Some(doc) = parse_toml(ctx, "pyproject.toml")
        && let Some(uv) = table(doc.get("tool"))
            .and_then(|tool| table(tool.get("uv")))
            .and_then(|uv| table(uv.get("workspace")))
    {
        let patterns = strings(uv.get("members"));
        if !patterns.is_empty() {
            let picked = without(
                expand(&patterns, &manifest_dirs(files, "pyproject.toml")),
                &strings(uv.get("exclude")),
            );
            let mut members: Vec<Member> = python_member(ctx, "").into_iter().collect();
            for dir in picked.iter().take(MAX_MEMBERS) {
                if ctx.timed_out() {
                    return None;
                }
                members.extend(python_member(ctx, dir));
            }
            return Some(("python", members));
        }
    }

    let work = read(ctx, "go.work");
    let mut uses: Vec<String> = GO_USE_ONE
        .captures_iter(&work)
        .map(|caps| caps[1].to_owned())
        .collect();
    for block in GO_USE_BLOCK.captures_iter(&work) {
        uses.extend(splitlines(&block[1]).into_iter().filter_map(|line| {
            let item = line.split("//").next().unwrap_or("").trim();
            (!item.is_empty()).then(|| item.to_owned())
        }));
    }
    let mut members = Vec::new();
    for used in uses.iter().take(MAX_MEMBERS) {
        if ctx.timed_out() {
            return None;
        }
        let dir = used.strip_prefix("./").unwrap_or(used).trim_matches('/');
        members.extend(go_member(ctx, &dir.replace('.', "")));
    }
    Some((if members.is_empty() { "" } else { "go" }, members))
}

/// The lead paragraph of the top-level README, cut to a few sentences.
fn readme_lead(ctx: &Ctx, files: &[String]) -> String {
    let Some(name) = files
        .iter()
        .find(|file| !file.contains('/') && README.is_match(file))
    else {
        return String::new();
    };
    let text = read(ctx, name);
    let lines = splitlines(&text);
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
    let joined = paragraph.join(" ");
    let chars: Vec<char> = joined.chars().collect();
    if chars.len() <= LEAD_CHARS {
        return joined;
    }
    let cut = &chars[..LEAD_CHARS];
    let sentence_end = |stop: char| {
        cut.windows(2)
            .rposition(|pair| pair[0] == stop && pair[1] == ' ')
    };
    match sentence_end('.').max(sentence_end('?')) {
        Some(end) if end > 120 => cut[..=end].iter().collect(),
        _ => {
            let words = cut.iter().rposition(|ch| *ch == ' ').map_or(cut, |at| &cut[..at]);
            format!("{}...", words.iter().collect::<String>())
        }
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

/// A package name as a manifest of the workspace could spell it.
fn folded(name: &str) -> String {
    name.replace('_', "-").to_lowercase()
}

fn member_block(member: &Member, members: &[Member], files: &[String]) -> Vec<String> {
    let member_names: BTreeSet<String> = members.iter().map(|m| folded(&m.name)).collect();
    let split = |table: &BTreeMap<String, bool>| -> (Vec<String>, Vec<String>) {
        table
            .iter()
            .map(|(name, internal)| (name, *internal || member_names.contains(&folded(name))))
            .fold((Vec::new(), Vec::new()), |mut acc, (name, inner)| {
                (if inner { &mut acc.0 } else { &mut acc.1 }).push(name.clone());
                acc
            })
    };
    let (inner, outer) = split(&member.deps);
    let (dev_inner, dev_outer) = split(&member.dev);
    let at = member.path.as_str();
    let title = if member.name == last_segment(at) {
        at.to_owned()
    } else {
        format!("{at} [{}]", member.name)
    };
    let head = match &member.desc {
        Some(desc) => format!("### {title} — {}", one_line(desc, 100)),
        None => format!("### {title}"),
    };
    let mut out = vec![head];
    out.extend(
        [
            names_line("depends on (workspace)", &inner),
            names_line("depends on (external)", &outer),
            names_line("dev (workspace)", &dev_inner),
            names_line("dev (external)", &dev_outer),
        ]
        .into_iter()
        .flatten(),
    );
    let prefix = if at == "." { String::new() } else { format!("{at}/") };
    let src: Vec<&str> = files
        .iter()
        .filter_map(|file| file.strip_prefix(prefix.as_str()).map(|rest| (file, rest)))
        .filter(|(file, rest)| {
            !["tests/", "test/", "benches/", "examples/"]
                .iter()
                .any(|dir| rest.starts_with(dir))
                && SOURCE_FILE.is_match(file)
                && (at != "." || rest.matches('/').count() <= 1)
        })
        .map(|(_, rest)| rest)
        .collect();
    if !src.is_empty() {
        let more = src.len().saturating_sub(SRC_NAMES);
        let mut shown = src[..src.len() - more].join(", ");
        if more > 0 {
            shown.push_str(&format!(" +{more} more"));
        }
        out.push(format!("files: {shown}"));
    }
    out
}

/// The line of the overview that stands for `member`.
fn overview_line(member: &Member) -> String {
    let desc = member.desc.as_deref();
    if member.path == "." {
        let tail = desc.map_or_else(String::new, |d| format!(" — {}", one_line(d, 70)));
        return format!(". [{}] (root){tail}", member.name);
    }
    let tag = if member.name == last_segment(&member.path) {
        String::new()
    } else {
        format!(" [{}]", member.name)
    };
    let tail = desc.map_or_else(String::new, |d| format!(" — {}", one_line(d, DESC_CHARS)));
    format!("{}{tag}{tail}", member.path)
}

/// The members of `members` the prompt names: by full package name when that
/// name is distinctive (`grep-cli`, `@scope/pkg`), or by a short name beside
/// a noun such as crate or package (`the grep crate`, `crate grep`).
fn named_members<'a>(prompt: &str, members: &'a [Member]) -> Vec<&'a Member> {
    let found = |pattern: &str| Regex::new(pattern).is_ok_and(|rx| rx.is_match(prompt));
    members
        .iter()
        .filter(|member| member.path != ".")
        .filter(|member| {
            let base = last_segment(&member.path);
            [member.name.as_str(), base].into_iter().any(|short| {
                if short.is_empty() {
                    return false;
                }
                let pat = regex::escape(short);
                // The script's lookbehind `(?<![\w/@.-])` and lookahead
                // `(?![\w-])`, as a character that is not one of them or an
                // end of the text.
                let distinctive = short == member.name
                    && (short.contains(['-', '/', '_']))
                    && found(&format!(r"(?:^|[^\w/@.-]){pat}(?:[^\w-]|$)"));
                distinctive
                    || found(&format!(
                        r#"(?i)(?:^|[^\w-]){pat}(?:'s)?\s+{MEMBER_NOUNS}\b|\b{MEMBER_NOUNS}\s+[`'"]?{pat}(?:[^\w-]|$)"#
                    ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
