// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Record types for the sniper error sink — THE shapes shared by store,
//! query layer and CLI.

use serde::{Deserialize, Serialize};

/// Every capture surface an error record can originate from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Surface {
    BrowserWindow,
    BrowserRejection,
    ErrorBoundary,
    BrowserConsole,
    ServerConsole,
    NodeUncaught,
    NodeUnhandled,
    /// serde's kebab-case would render this as "http5xx"; the wire name is
    /// "http-5xx" everywhere (as_str/parse and the JS adapters).
    #[serde(rename = "http-5xx", alias = "http5xx")]
    Http5xx,
    ViteTransform,
    Vitest,
    Tsc,
    Minitest,
    Rspec,
    Rubocop,
    RunWrapper,
    Reported,
}

impl Surface {
    pub fn as_str(&self) -> &'static str {
        match self {
            Surface::BrowserWindow => "browser-window",
            Surface::BrowserRejection => "browser-rejection",
            Surface::ErrorBoundary => "error-boundary",
            Surface::BrowserConsole => "browser-console",
            Surface::ServerConsole => "server-console",
            Surface::NodeUncaught => "node-uncaught",
            Surface::NodeUnhandled => "node-unhandled",
            Surface::Http5xx => "http-5xx",
            Surface::ViteTransform => "vite-transform",
            Surface::Vitest => "vitest",
            Surface::Tsc => "tsc",
            Surface::Minitest => "minitest",
            Surface::Rspec => "rspec",
            Surface::Rubocop => "rubocop",
            Surface::RunWrapper => "run-wrapper",
            Surface::Reported => "reported",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "browser-window" => Surface::BrowserWindow,
            "browser-rejection" => Surface::BrowserRejection,
            "error-boundary" => Surface::ErrorBoundary,
            "browser-console" => Surface::BrowserConsole,
            "server-console" => Surface::ServerConsole,
            "node-uncaught" => Surface::NodeUncaught,
            "node-unhandled" => Surface::NodeUnhandled,
            "http-5xx" => Surface::Http5xx,
            "vite-transform" => Surface::ViteTransform,
            "vitest" => Surface::Vitest,
            "tsc" => Surface::Tsc,
            "minitest" => Surface::Minitest,
            "rspec" => Surface::Rspec,
            "rubocop" => Surface::Rubocop,
            "run-wrapper" => Surface::RunWrapper,
            "reported" => Surface::Reported,
            _ => return None,
        })
    }
}

/// Package provenance attached to a stack frame by the enrichment phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePackage {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// More than one entry means the package exists at multiple physical
    /// paths — the duplicate-module smoking gun.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dup_paths: Vec<String>,
}

/// One stack frame: raw text plus optional parsed / source-mapped locations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub raw: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub func: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapped_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapped_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapped_column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkg: Option<FramePackage>,
}

impl Frame {
    /// Best-known source location, source-mapped when available.
    pub fn best_location(&self) -> Option<(&str, u32, Option<u32>)> {
        if let Some(file) = &self.mapped_file {
            return Some((file, self.mapped_line.unwrap_or(0), self.mapped_column));
        }
        self.file
            .as_deref()
            .map(|file| (file, self.line.unwrap_or(0), self.column))
    }
}

/// HTTP request context for `http-5xx` records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_excerpt: Option<String>,
}

/// Input for recording one error occurrence (the ingest shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorInput {
    pub surface: Surface,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<Vec<Frame>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Milliseconds since epoch; defaults to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
}

/// One stored (possibly deduplicated) error row. `id` is THE cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRecord {
    pub id: i64,
    pub first_ts: i64,
    pub last_ts: i64,
    pub count: i64,
    pub run_id: Option<String>,
    pub surface: Surface,
    pub kind: Option<String>,
    pub message: String,
    pub stack_raw: Option<String>,
    pub frames: Option<Vec<Frame>>,
    pub values: Option<serde_json::Value>,
    pub http: Option<serde_json::Value>,
    pub extra: Option<serde_json::Value>,
    pub dedup_hash: String,
}

/// Non-error lifecycle event kinds ("did my edit land?").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    ServerStart,
    HmrUpdate,
    FullReload,
    DepOptimized,
    TestPass,
    BuildOk,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::ServerStart => "server-start",
            EventKind::HmrUpdate => "hmr-update",
            EventKind::FullReload => "full-reload",
            EventKind::DepOptimized => "dep-optimized",
            EventKind::TestPass => "test-pass",
            EventKind::BuildOk => "build-ok",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "server-start" => EventKind::ServerStart,
            "hmr-update" => EventKind::HmrUpdate,
            "full-reload" => EventKind::FullReload,
            "dep-optimized" => EventKind::DepOptimized,
            "test-pass" => EventKind::TestPass,
            "build-ok" => EventKind::BuildOk,
            _ => return None,
        })
    }
}

/// Input for recording one lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventInput {
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
}

/// One stored lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: i64,
    pub ts: i64,
    pub run_id: Option<String>,
    pub kind: EventKind,
    pub data: Option<serde_json::Value>,
}

/// Input for registering a dev-server / wrapper run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInput {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vite_dep_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_since_last_run: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
}

/// One stored run row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub started_at: i64,
    pub pid: Option<i64>,
    pub port: Option<i64>,
    pub git_head: Option<String>,
    pub lockfile_hash: Option<String>,
    pub vite_dep_hash: Option<String>,
    pub fingerprint: Option<serde_json::Value>,
    pub changed_since_last_run: Option<Vec<String>>,
}

/// The one-record ingest envelope `gitpixel sniper report` reads from stdin.
/// A record without a `type` field is an error record; `"type": "event"` and
/// `"type": "run"` route to the other tables.
#[derive(Debug, Clone)]
pub enum ReportEnvelope {
    Error(ErrorInput),
    Event(EventInput),
    Run(RunInput),
}

impl ReportEnvelope {
    pub fn parse(value: serde_json::Value) -> Result<Self, String> {
        let tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error")
            .to_owned();
        let payload = match value {
            serde_json::Value::Object(mut map) => {
                map.remove("type");
                serde_json::Value::Object(map)
            }
            other => other,
        };
        match tag.as_str() {
            "error" => serde_json::from_value(payload)
                .map(ReportEnvelope::Error)
                .map_err(|e| format!("bad error record: {e}")),
            "event" => serde_json::from_value(payload)
                .map(ReportEnvelope::Event)
                .map_err(|e| format!("bad event record: {e}")),
            "run" => serde_json::from_value(payload)
                .map(ReportEnvelope::Run)
                .map_err(|e| format!("bad run record: {e}")),
            other => Err(format!("unknown record type {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_parse_round_trips_every_name() {
        for name in [
            "browser-window",
            "browser-rejection",
            "error-boundary",
            "browser-console",
            "server-console",
            "node-uncaught",
            "node-unhandled",
            "http-5xx",
            "vite-transform",
            "vitest",
            "tsc",
            "minitest",
            "rspec",
            "rubocop",
            "run-wrapper",
            "reported",
        ] {
            assert_eq!(Surface::parse(name).map(|s| s.as_str()), Some(name));
        }
        assert_eq!(Surface::parse("nope"), None);
    }
}
