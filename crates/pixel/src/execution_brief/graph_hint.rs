// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The code graph's answer to a native search. When an agent's `grep`, `rg`
//! or Grep tool call names a symbol the graph knows, the post-tool-use hook
//! adds that symbol's definitions and callers to the tool result, so graph
//! retrieval reaches the agent without the agent composing a `pixel` call.
//!
//! Daemon only, within [`WINDOW`]: a hook never opens the graph itself. A
//! search that names a symbol while no daemon answers gets no hint and starts
//! one in the background, under the same switches as a session start.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use pixel_daemon::Request;
use regex::Regex;
use serde_json::Value;

use super::evidence;

/// The whole hint's budget, every daemon request included.
const WINDOW: Duration = Duration::from_millis(800);
/// Symbols looked up per search: an alternation of many names is a scan, not
/// a lookup.
const MAX_SYMBOLS: usize = 3;
const MAX_DEFINITIONS: usize = 3;
const MAX_CALLERS: usize = 8;
const HINT_HEAD: &str = "[PIXEL:GRAPH] Pixel's code graph already resolved the symbol(s) this \
search names. Use these definitions and call sites instead of searching for them again; \
read only the lines you need:";

/// A name right after a declaration keyword: `func ensureAuth`, `type Config`.
static DECLARED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:func|fn|def|class|type|interface|struct|enum|trait|impl|const|let|var|function)\s+([A-Za-z_]\w{2,})",
    )
    .expect("valid regex")
});
/// A code-shaped word: camelCase, PascalCase with two humps, or snake_case.
static CODE_SHAPED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([a-z]+[A-Z]\w*|[A-Z][a-z0-9]+[A-Z]\w*|[a-z][a-z0-9]*_[a-z0-9_]+)\b")
        .expect("valid regex")
});

/// The hint for one finished tool call, `None` when the call is not a search,
/// names no symbol the graph defines, or the daemon does not answer in time.
pub(crate) fn for_tool_call(root: &Path, tool: &str, input: &Value) -> Option<String> {
    let names = searched_symbols(tool, input);
    if names.is_empty() {
        return None;
    }
    let deadline = Instant::now() + WINDOW;
    if evidence::connect(root, deadline).is_none() {
        // No daemon: start one (as a session start does) so the next search
        // gets its hint; this one goes without.
        let _ = super::autostart::warm(root);
        return None;
    }
    let blocks: Vec<String> = names
        .iter()
        .filter_map(|name| symbol_block(root, name, deadline))
        .collect();
    (!blocks.is_empty()).then(|| format!("{HINT_HEAD}\n{}", blocks.join("\n")))
}

/// The symbol names a search call looks for: the Grep tool's `pattern`, or
/// every command of a shell line run as `grep`, `rg`, `egrep` or `git grep`
/// (an `rtk` prefix allowed); a `grep` used as a filter after a pipe searches
/// command output, not code, and names nothing.
fn searched_symbols(tool: &str, input: &Value) -> Vec<String> {
    let text = match tool {
        "Grep" => input
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::to_owned),
        "Bash" | "bash" | "shell" | "local_shell" | "exec_command" | "unified_exec" => {
            command_text(input).map(|command| {
                command_heads(command)
                    .into_iter()
                    .filter_map(search_stage)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        }
        _ => None,
    };
    text.map_or_default(|text| symbol_names(&text))
}

/// The shell text of a command call; Codex's `["bash", "-lc", "<script>"]`
/// form gives its last word.
fn command_text(input: &Value) -> Option<&str> {
    ["command", "cmd"].iter().find_map(|key| {
        let value = input.get(key)?;
        value
            .as_str()
            .or_else(|| value.as_array()?.last()?.as_str())
    })
}

/// Each command of a shell line (split at unquoted, unescaped `;`, `&`, `&&`,
/// `||` and newlines), cut before its first pipe: what runs on the files, not
/// the filters after it. A grep alternation `"a\|b"` stays in its command.
fn command_heads(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut heads = Vec::new();
    let (mut start, mut piped) = (0, false);
    let (mut quote, mut escaped) = (None, false);
    for (at, ch) in command.char_indices() {
        let was_escaped = escaped;
        escaped = ch == '\\' && !was_escaped;
        if let Some(open) = quote {
            if ch == open && !was_escaped {
                quote = None;
            }
            continue;
        }
        if was_escaped {
            continue;
        }
        let double_bar = ch == '|'
            && (bytes.get(at + 1) == Some(&b'|')
                || at.checked_sub(1).map(|i| bytes[i]) == Some(b'|'));
        match ch {
            '"' | '\'' => quote = Some(ch),
            '|' if !double_bar => {
                if !piped {
                    heads.push(&command[start..at]);
                }
                piped = true;
            }
            ';' | '&' | '\n' | '|' => {
                if !piped {
                    heads.push(&command[start..at]);
                }
                piped = false;
                start = at + 1;
            }
            _ => {}
        }
    }
    if !piped {
        heads.push(&command[start..]);
    }
    heads
}

/// The arguments of one command when it is a search.
fn search_stage(command: &str) -> Option<&str> {
    let stage = command.trim_start();
    let stage = stage.strip_prefix("rtk ").unwrap_or(stage).trim_start();
    let stage = stage.strip_prefix("git ").unwrap_or(stage).trim_start();
    ["grep ", "rg ", "egrep ", "ugrep "]
        .iter()
        .find_map(|tool| stage.strip_prefix(tool))
}

/// Declared names first, then code-shaped words, each once, path segments and
/// file names excluded.
fn symbol_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let words = text
        .split_whitespace()
        .filter(|word| !word.contains('/') && !word.starts_with('-'));
    let searchable = words.collect::<Vec<_>>().join(" ");
    let found = DECLARED
        .captures_iter(&searchable)
        .chain(CODE_SHAPED.captures_iter(&searchable))
        .filter_map(|captures| captures.get(1));
    for name in found {
        let name = name.as_str();
        if !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
        if names.len() == MAX_SYMBOLS {
            break;
        }
    }
    names
}

/// The definitions and callers of `name`, `None` when the graph defines no
/// symbol of exactly that name.
fn symbol_block(root: &Path, name: &str, deadline: Instant) -> Option<String> {
    let found = ask(
        root,
        &Request::Symbol {
            name: name.to_owned(),
            stale_ok: true,
        },
        deadline,
    )?;
    let mut definitions: Vec<&Value> = found
        .get("symbols")?
        .as_array()?
        .iter()
        .filter(|symbol| symbol.get("name").and_then(Value::as_str) == Some(name))
        .collect();
    definitions.dedup_by_key(|symbol| symbol.get("uid").cloned());
    let first = definitions.first()?;
    let places: Vec<String> = definitions
        .iter()
        .take(MAX_DEFINITIONS)
        .map(|symbol| place(symbol))
        .collect();
    let mut block = format!("`{name}` defined: {}", places.join("; "));
    let uid = first.get("uid").and_then(Value::as_str)?;
    let callers = ask(
        root,
        &Request::Uses {
            uid_or_name: uid.to_owned(),
            role: "callers".to_owned(),
            offset: None,
            stale_ok: true,
        },
        deadline,
    );
    let edges = callers
        .as_ref()
        .and_then(|data| data.get("edges"))
        .and_then(Value::as_array);
    match edges {
        None => block.push_str("\n  callers: not answered in time"),
        Some(edges) if edges.is_empty() => block.push_str("\n  callers: none in the graph"),
        Some(edges) => {
            let _ = write!(block, "\n  callers ({}):", edges.len());
            for edge in edges.iter().take(MAX_CALLERS) {
                let Some(caller) = edge.get("symbol") else {
                    continue;
                };
                let caller_name = caller.get("name").and_then(Value::as_str).unwrap_or("?");
                let _ = write!(block, "\n  {caller_name} {}", place(caller));
                if let Some(line) = edge.get("site_line").and_then(Value::as_i64) {
                    let _ = write!(block, " (call at line {line})");
                }
            }
        }
    }
    Some(block)
}

/// `path:start-end` of a graph symbol row.
fn place(symbol: &Value) -> String {
    let path = symbol.get("path").and_then(Value::as_str).unwrap_or("?");
    let start = symbol
        .get("start_line")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let end = symbol
        .get("end_line")
        .and_then(Value::as_i64)
        .unwrap_or(start);
    format!("{path}:{start}-{end}")
}

fn ask(root: &Path, request: &Request, deadline: Instant) -> Option<Value> {
    let mut stream = evidence::connect(root, deadline)?;
    let response = crate::roundtrip_by(&mut stream, request, Some(evidence::unix_ms_at(deadline)))?;
    crate::unwrap_response(response).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> Vec<String> {
        searched_symbols("Bash", &json!({ "command": command }))
    }

    #[test]
    fn a_shell_search_names_its_declared_symbol_first() {
        assert_eq!(
            bash(r#"rtk grep -rn "func ensureOmniAuth" apps/cli"#),
            ["ensureOmniAuth"]
        );
        assert_eq!(
            bash(r#"grep -n "type GuideStateCommandResult struct" -A10 internal/x.go | head"#),
            ["GuideStateCommandResult"]
        );
        assert_eq!(
            bash("git grep -n prepare_launcher_update"),
            ["prepare_launcher_update"]
        );
        assert_eq!(
            bash("rg -n 'useEntitlement\\|PaywallPage' src"),
            ["useEntitlement", "PaywallPage"]
        );
    }

    #[test]
    fn an_alternation_inside_quotes_stays_in_the_search() {
        // The live session that found this: the hook cut the command at the
        // quoted `\|` and named nothing.
        assert_eq!(
            bash(
                r#"rtk grep -rn "ensureOmniAuth(\|handleOmniAuthFailure(" apps/cli --include=*.go & echo"#
            ),
            ["ensureOmniAuth", "handleOmniAuthFailure"]
        );
        assert_eq!(command_heads(r"grep 'a|b' x | head"), [r"grep 'a|b' x "]);
        assert_eq!(command_heads(r"grep a\|b x; ls"), [r"grep a\|b x", " ls"]);
        assert_eq!(command_heads("grep x"), ["grep x"]);
        assert_eq!(
            command_heads("a | grep b && c || d & e"),
            ["a ", "", " c ", "", " d ", " e"]
        );
    }

    #[test]
    fn a_codex_argv_command_is_read_from_its_script() {
        let input = json!({ "command": ["bash", "-lc", "rg -n 'fn resolve_calls' crates"] });
        assert_eq!(searched_symbols("shell", &input), ["resolve_calls"]);
        assert_eq!(
            searched_symbols("exec_command", &json!({ "cmd": "rg loadUser" })),
            ["loadUser"]
        );
    }

    #[test]
    fn a_search_later_in_a_chain_is_read_but_not_a_filter() {
        // The live EntityType session: every search came after `cd … && echo … &&`.
        assert_eq!(
            bash(r#"cd services/x && echo "== users" && rg -n 'EntityType' . | head -40"#),
            ["EntityType"]
        );
        assert_eq!(
            bash("rg -n loadUser src; grep -rn saveUser lib"),
            ["loadUser", "saveUser"]
        );
        assert!(bash("cd src && ls | grep loadUser").is_empty());
    }

    #[test]
    fn the_grep_tool_pattern_is_read() {
        let names = searched_symbols(
            "Grep",
            &json!({ "pattern": "export function useEntitlement" }),
        );
        assert_eq!(names, ["useEntitlement"]);
    }

    #[test]
    fn a_filter_grep_paths_and_plain_words_name_nothing() {
        assert!(
            bash("git log --oneline | grep fixAuth").is_empty(),
            "grep after a pipe filters output"
        );
        assert!(bash("ls src/hooks | grep -i entitlement").is_empty());
        assert!(bash(r#"grep -rn "docker build" apps/my_app/Dockerfile"#).is_empty());
        assert!(bash("cat src/useEntitlement.ts").is_empty(), "not a search");
        assert!(searched_symbols("Read", &json!({ "file_path": "src/a_b.rs" })).is_empty());
    }

    #[test]
    fn at_most_three_names_each_once() {
        assert_eq!(
            bash("rg 'fooBar|fooBar|bazQux|oneTwo|threeFour'"),
            ["fooBar", "bazQux", "oneTwo"]
        );
    }

    #[test]
    fn no_daemon_means_no_hint() {
        let dir = tempfile::tempdir().unwrap();
        let input = json!({ "command": "rg -n ensureOmniAuth" });
        assert_eq!(for_tool_call(dir.path(), "Bash", &input), None);
    }

    #[test]
    fn place_reads_path_and_lines() {
        let row = json!({ "path": "src/a.go", "start_line": 3, "end_line": 9 });
        assert_eq!(place(&row), "src/a.go:3-9");
    }
}
