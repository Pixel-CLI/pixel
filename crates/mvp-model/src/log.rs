// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! One line of the local invocation log, `.pixel-mvp/log.jsonl` (C-17):
//! the command, its duration, its outcome, the bytes it printed, for the
//! chain (the prompt hook and `minimap`) the paths it emitted, and for a
//! command that ran plugins each run's closed code (C-31). Nothing else:
//! the record has no field that could hold a prompt, a file's contents,
//! an error's text or anything a plugin printed, so none can reach the log
//! by mistake.
//!
//! The JSON is written here by hand, field by field, so its shape is the
//! schema: the keys below in this order, `paths` only when the command
//! emitted a minimap, `plugins` only when it ran a plugin. A line with
//! `plugins` is version 2; every other line is version 1, byte for byte
//! the line it was before plugins were logged, so a reader of version 1
//! reads each line it can and tells by `v` the lines it cannot.

use crate::plugin::{Capability, Code};

/// The schema version of a log line that holds no plugin run.
pub const LOG_VERSION_1: u32 = 1;
/// The schema version of a line that holds `plugins`: version 1 and that
/// key, last.
pub const LOG_VERSION: u32 = 2;

/// The log in a state directory, and the log before its last rotation:
/// the store writes them, and `uninstall` takes the user's back with the
/// state directory it made (ADR-0024).
pub const LOG_FILES: [&str; 2] = ["log.jsonl", "log.jsonl.1"];

/// The commands and hooks that log, a closed list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Classify,
    Config,
    HookPrompt,
    HookSessionStart,
    Index,
    Install,
    Minimap,
    Plugin,
    /// `pixel-mvp <name>`: a `command` plugin, whose name is not logged.
    PluginCommand,
    Status,
    Uninstall,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::Classify => "classify",
            Event::Config => "config",
            Event::HookPrompt => "hook prompt",
            Event::HookSessionStart => "hook session-start",
            Event::Index => "index",
            Event::Install => "install",
            Event::Minimap => "minimap",
            Event::Plugin => "plugin",
            Event::PluginCommand => "plugin command",
            Event::Status => "status",
            Event::Uninstall => "uninstall",
        }
    }
}

/// How an invocation ended, a closed list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It did its work and printed what it had.
    Ok,
    /// It had nothing useful to print: a prompt step 0 skips, no index,
    /// no pick.
    Empty,
    /// It failed; its message went to stderr, never here.
    Error,
    /// The hook's deadline passed, so it printed nothing (C-8).
    Timeout,
}

impl Outcome {
    /// The outcome of a run that printed `picks` entries (`None` when it
    /// printed nothing), ran `late`, or `failed`.
    pub fn of(failed: bool, late: bool, picks: Option<usize>) -> Outcome {
        match (failed, late, picks) {
            (true, _, _) => Outcome::Error,
            (false, true, _) => Outcome::Timeout,
            (false, false, Some(n)) if n > 0 => Outcome::Ok,
            _ => Outcome::Empty,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Empty => "empty",
            Outcome::Error => "error",
            Outcome::Timeout => "timeout",
        }
    }
}

/// A failure of the closed list tester builds report
/// (`design/telemetry.md`, "Two streams"), as a command notes it. The log
/// line has no field for it: only the reporter reads it, and a build
/// without the reporter drops it (C-21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The deadline passed, so the hook printed nothing (C-8).
    HookTimeout,
    /// The hook's payload was a JSON object the adapter found no prompt in.
    AdapterParse,
    /// A corrupt or incompatible index was rebuilt (C-19).
    IndexRebuild,
    /// A step of `install` or `uninstall` failed: the step's name.
    Install(&'static str),
}

/// One plugin run, as the log holds it (C-31): the plugin's name, which
/// has a plugin name's shape or is no run, the capability it was run
/// for, its closed outcome, its time, and how many of its picks the
/// printed minimap showed. Nothing it printed, on stdout or stderr.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRun {
    name: String,
    pub capability: Capability,
    /// `Ok` when its answer was used; else why it was dropped.
    pub result: Result<(), Code>,
    /// From its start to its answer, its exit or its kill.
    pub duration_ms: u64,
    /// Its picks the printed minimap showed: 0 when it printed none.
    pub picks: usize,
}

impl PluginRun {
    /// The run of the plugin `name`, or `None` when `name` is not a
    /// plugin's name ([`crate::plugin::is_name`]).
    pub fn new(
        name: &str,
        capability: Capability,
        result: Result<(), Code>,
        duration_ms: u64,
        picks: usize,
    ) -> Option<PluginRun> {
        crate::plugin::is_name(name).then(|| PluginRun {
            name: name.to_owned(),
            capability,
            result,
            duration_ms,
            picks,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `ok`, or the closed code of why its answer was dropped.
    pub fn outcome(&self) -> &'static str {
        match self.result {
            Ok(()) => "ok",
            Err(code) => code.as_str(),
        }
    }
}

/// One invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// When it started, in seconds since the Unix epoch, UTC.
    pub at: i64,
    /// The version of pixel-mvp that ran.
    pub version: &'static str,
    pub event: Event,
    pub duration_ms: u64,
    pub outcome: Outcome,
    /// Bytes printed on stdout.
    pub bytes: u64,
    /// The harness table row the hook answered for, an id of the table.
    pub harness: Option<String>,
    /// Entries in the minimap, and the files it left out as stale.
    pub picks: Option<usize>,
    pub stale: Option<usize>,
    /// The repository paths the minimap emitted, in its order.
    pub paths: Option<Vec<String>>,
    /// The plugins it ran, in name order (the host collects them so);
    /// none, and the key is left out.
    pub plugins: Vec<PluginRun>,
}

impl Record {
    /// The record as one line of JSON, without its newline.
    pub fn json(&self) -> String {
        let mut out = String::with_capacity(160);
        let version = if self.plugins.is_empty() {
            LOG_VERSION_1
        } else {
            LOG_VERSION
        };
        out.push_str("{\"v\":");
        out.push_str(&version.to_string());
        out.push_str(",\"at\":\"");
        out.push_str(&crate::utc(self.at));
        out.push_str("\",\"pixel\":");
        string(&mut out, self.version);
        out.push_str(",\"command\":");
        string(&mut out, self.event.as_str());
        out.push_str(",\"duration_ms\":");
        out.push_str(&self.duration_ms.to_string());
        out.push_str(",\"outcome\":");
        string(&mut out, self.outcome.as_str());
        out.push_str(",\"bytes\":");
        out.push_str(&self.bytes.to_string());
        if let Some(harness) = &self.harness {
            out.push_str(",\"harness\":");
            string(&mut out, harness);
        }
        if let Some(picks) = self.picks {
            out.push_str(",\"picks\":");
            out.push_str(&picks.to_string());
        }
        if let Some(stale) = self.stale {
            out.push_str(",\"stale\":");
            out.push_str(&stale.to_string());
        }
        if let Some(paths) = &self.paths {
            out.push_str(",\"paths\":[");
            for (n, path) in paths.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                string(&mut out, path);
            }
            out.push(']');
        }
        if !self.plugins.is_empty() {
            out.push_str(",\"plugins\":[");
            for (n, run) in self.plugins.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":");
                string(&mut out, run.name());
                out.push_str(",\"capability\":");
                string(&mut out, run.capability.as_str());
                out.push_str(",\"outcome\":");
                string(&mut out, run.outcome());
                out.push_str(",\"duration_ms\":");
                out.push_str(&run.duration_ms.to_string());
                out.push_str(",\"picks\":");
                out.push_str(&run.picks.to_string());
                out.push('}');
            }
            out.push(']');
        }
        out.push('}');
        out
    }
}

/// `text` as a JSON string: quotes, backslashes and every control
/// character escaped, so a path holding a newline stays on its line.
fn string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            at: 1_760_227_200,
            version: "0.1.0",
            event: Event::HookPrompt,
            duration_ms: 42,
            outcome: Outcome::Ok,
            bytes: 1234,
            harness: Some("row-a".into()),
            picks: Some(2),
            stale: Some(0),
            paths: Some(vec!["app/models/room.rb".into(), "src/main.rs".into()]),
            plugins: Vec::new(),
        }
    }

    fn run(
        name: &str,
        capability: Capability,
        result: Result<(), Code>,
        ms: u64,
        picks: usize,
    ) -> PluginRun {
        PluginRun::new(name, capability, result, ms, picks).expect("a plugin name")
    }

    #[test]
    fn c17_plugin_runs_are_logged_field_by_field_in_their_order_as_version_2() {
        let with = Record {
            plugins: vec![
                run("mail", Capability::ChainStep, Ok(()), 12, 2),
                run("slow-1", Capability::ChainStep, Err(Code::Timeout), 200, 0),
                run("crash", Capability::ChainStep, Err(Code::Exit), 3, 0),
            ],
            ..record()
        };
        assert_eq!(
            with.json(),
            "{\"v\":2,\"at\":\"2025-10-12T00:00Z\",\"pixel\":\"0.1.0\",\"command\":\"hook prompt\",\
             \"duration_ms\":42,\"outcome\":\"ok\",\"bytes\":1234,\"harness\":\"row-a\",\
             \"picks\":2,\"stale\":0,\"paths\":[\"app/models/room.rb\",\"src/main.rs\"],\
             \"plugins\":[{\"name\":\"mail\",\"capability\":\"chain-step\",\"outcome\":\"ok\",\"duration_ms\":12,\"picks\":2},\
             {\"name\":\"slow-1\",\"capability\":\"chain-step\",\"outcome\":\"timeout\",\"duration_ms\":200,\"picks\":0},\
             {\"name\":\"crash\",\"capability\":\"chain-step\",\"outcome\":\"exit\",\"duration_ms\":3,\"picks\":0}]}"
        );
    }

    #[test]
    fn c17_each_plugin_code_and_capability_is_its_closed_string() {
        let codes = [
            (Ok(()), "ok"),
            (Err(Code::Timeout), "timeout"),
            (Err(Code::Exit), "exit"),
            (Err(Code::Malformed), "malformed"),
            (Err(Code::OutsideRepo), "outside-repo"),
            (Err(Code::UnknownSymbol), "unknown-symbol"),
        ];
        for (result, text) in codes {
            let line = Record {
                event: Event::HookSessionStart,
                plugins: vec![run("a", Capability::SessionStart, result, 1, 0)],
                ..record()
            }
            .json();
            assert!(
                line.ends_with(&format!(
                    "\"plugins\":[{{\"name\":\"a\",\"capability\":\"session-start\",\"outcome\":\"{text}\",\"duration_ms\":1,\"picks\":0}}]}}"
                )),
                "{line}"
            );
        }
    }

    #[test]
    fn c17_a_name_outside_the_plugin_name_rules_is_never_a_plugin_run() {
        for name in [
            "",
            "-a",
            "A",
            "a b",
            "a\"b",
            "a/b",
            "../x",
            "a\nb",
            "é",
            &"a".repeat(65),
        ] {
            assert_eq!(
                PluginRun::new(name, Capability::ChainStep, Ok(()), 1, 0),
                None,
                "{name:?}"
            );
        }
        assert!(PluginRun::new(&"a".repeat(64), Capability::ChainStep, Ok(()), 1, 0).is_some());
        assert!(PluginRun::new("pixel-map2", Capability::ChainStep, Ok(()), 1, 0).is_some());
    }

    #[test]
    fn c17_a_line_with_no_plugin_run_is_version_1_as_before() {
        let line = record().json();
        assert!(line.starts_with("{\"v\":1,"), "{line}");
        assert!(!line.contains("plugins"), "{line}");
    }

    #[test]
    fn c17_a_record_is_one_line_in_the_schema_order() {
        assert_eq!(
            record().json(),
            "{\"v\":1,\"at\":\"2025-10-12T00:00Z\",\"pixel\":\"0.1.0\",\"command\":\"hook prompt\",\
             \"duration_ms\":42,\"outcome\":\"ok\",\"bytes\":1234,\"harness\":\"row-a\",\
             \"picks\":2,\"stale\":0,\"paths\":[\"app/models/room.rb\",\"src/main.rs\"]}"
        );
    }

    #[test]
    fn c17_a_hostile_path_stays_one_escaped_line() {
        let mut hostile = record();
        let path = "a\"b\\c\nd\re\tf\u{1}g\u{7f}h\u{2028}i.rb";
        hostile.paths = Some(vec![path.into()]);
        let line = hostile.json();
        assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
        assert!(
            line.chars()
                .all(|c| c as u32 >= 0x20 && c != '\u{7f}' && c != '\u{2028}'),
            "{line}"
        );
        assert!(
            line.ends_with("\"paths\":[\"a\\\"b\\\\c\\nd\\re\\tf\\u0001g\\u007fh\\u2028i.rb\"]}")
        );
    }

    #[test]
    fn c17_a_minimap_of_no_entry_is_empty_and_a_late_one_a_timeout() {
        assert_eq!(Outcome::of(false, false, Some(3)), Outcome::Ok);
        assert_eq!(Outcome::of(false, false, Some(0)), Outcome::Empty);
        assert_eq!(Outcome::of(false, false, None), Outcome::Empty);
        assert_eq!(Outcome::of(false, true, None), Outcome::Timeout);
        assert_eq!(Outcome::of(true, true, Some(3)), Outcome::Error);
    }

    #[test]
    fn c17_fields_a_command_has_not_are_left_out() {
        let index = Record {
            event: Event::Index,
            harness: None,
            picks: None,
            stale: None,
            paths: None,
            ..record()
        };
        let line = index.json();
        for key in ["harness", "picks", "stale", "paths", "plugins"] {
            assert!(!line.contains(&format!("\"{key}\"")), "{line}");
        }
    }
}
