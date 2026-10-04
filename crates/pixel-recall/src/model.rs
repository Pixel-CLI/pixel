// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Unified transcript model every source adapter normalizes into.

/// Where a timestamp came from — outputs always disclose this because some
/// sources (Cursor CLI) have no per-record timestamps at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsSource {
    Iso,
    UnixMs,
    Mtime,
    Absent,
}

impl TsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TsSource::Iso => "iso",
            TsSource::UnixMs => "unixms",
            TsSource::Mtime => "mtime",
            TsSource::Absent => "absent",
        }
    }

    pub fn parse(s: &str) -> TsSource {
        match s {
            "iso" => TsSource::Iso,
            "unixms" => TsSource::UnixMs,
            "mtime" => TsSource::Mtime,
            _ => TsSource::Absent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// Who actually authored a user turn: a human at the keyboard, or harness
/// machinery (system reminders, task notifications, agent handoffs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentSource {
    Human,
    Orchestrator,
}

impl IntentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            IntentSource::Human => "human",
            IntentSource::Orchestrator => "orchestrator",
        }
    }
}

/// One extracted conversation turn — the retrieval unit.
#[derive(Debug, Clone)]
pub struct UnifiedTurn {
    pub role: Role,
    /// Only meaningful for user turns.
    pub intent_source: Option<IntentSource>,
    /// Unix milliseconds, when known.
    pub ts: Option<i64>,
    pub text: String,
    pub truncated: bool,
    /// Provenance into the source file (JSONL sources): byte offset of the
    /// originating record line and its length.
    pub source_byte_start: Option<u64>,
    pub source_byte_len: Option<u64>,
}

/// Session-level metadata accompanying a batch of turns.
#[derive(Debug, Clone)]
pub struct UnifiedSession {
    pub agent: &'static str,
    /// Stable id within the agent's own store (file stem, db session id).
    pub source_session_id: String,
    /// File or database the session came from.
    pub source_path: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// Source-provided title when one exists; otherwise the ingester derives
    /// one from the first human user turn.
    pub title: Option<String>,
    pub ts_source: TsSource,
    pub is_subagent: bool,
    /// The parent's `source_session_id`, for subagent transcripts.
    pub parent_source_session_id: Option<String>,
}

pub const TOOL_RESULT_CAP: usize = 4096;
pub const TOOL_INPUT_CAP: usize = 2048;
pub const TITLE_CAP: usize = 120;

/// Truncate at a char boundary, flagging whether anything was cut.
pub fn cap_text(s: &str, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s.to_string(), false);
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// First line of a prompt, capped, for derived session titles.
pub fn derive_title(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    cap_text(first.trim(), TITLE_CAP).0
}

/// Parse an RFC3339/ISO-8601 timestamp into unix milliseconds.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let odt =
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    Some((odt.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Format unix ms as a compact UTC string for terminal output.
pub fn format_ms(ms: i64) -> String {
    let odt = match time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000) {
        Ok(v) => v,
        Err(_) => return "?".to_string(),
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        odt.year(),
        odt.month() as u8,
        odt.day(),
        odt.hour(),
        odt.minute()
    )
}
