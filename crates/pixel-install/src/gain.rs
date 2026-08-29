//! Gain ledger — a jsonl ledger of telemetry events with a source tag.
//!
//! Minimal Rust port of usable-git's gain ledger (`reference/usable-git/.../
//! gain/{ledger,estimate,report}.ts`). Events are appended with a `source`
//! tag so a `pixel migrate` can carry the ledger across a state rebuild while
//! recording where each event came from.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The ledger file name under the pixel state root.
pub const LEDGER_FILE: &str = "gain-v1.jsonl";
/// The salt file used to hash repository identities.
pub const SALT_FILE: &str = "gain-v1.salt";

/// Source tag for events appended by the current pixel binary.
pub const SOURCE_PIXEL: &str = "pixel";
/// Source tag for events carried over from a `.gitpixel/` migration.
pub const SOURCE_MIGRATED: &str = "migrated";

#[derive(Debug, Error)]
pub enum GainError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("corrupt ledger line {line}: {reason}")]
    CorruptLine { line: usize, reason: String },
}

pub type Result<T> = std::result::Result<T, GainError>;

/// One telemetry event appended to the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GainEvent {
    pub version: String,
    pub timestamp: String,
    pub operation: String,
    pub client: String,
    pub transport: String,
    pub result_code: String,
    pub repository_hash: String,
    pub envelope_bytes: u64,
    pub raw_equivalent_bytes: u64,
    pub agent_ops_raw: u64,
    pub agent_ops_actual: u64,
    pub git_subprocesses_raw: u64,
    pub git_subprocesses_actual: u64,
    pub duration_ms: u64,
    pub tokens_saved: i64,
    /// Which tool appended this event (`pixel` or `migrated`).
    pub source: String,
}

/// Input for appending one event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GainEventInput {
    pub operation: String,
    pub client: String,
    pub transport: String,
    pub result_code: String,
    pub envelope_bytes: u64,
    pub raw_equivalent_bytes: u64,
    pub agent_ops_raw: u64,
    pub agent_ops_actual: u64,
    pub git_subprocesses_raw: u64,
    pub git_subprocesses_actual: u64,
    pub duration_ms: u64,
    pub tokens_saved: i64,
    /// Source tag; defaults to [`SOURCE_PIXEL`].
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String {
    SOURCE_PIXEL.to_string()
}

/// The gain ledger: append-only jsonl under the pixel state root.
pub struct GainLedger {
    directory: PathBuf,
    file: PathBuf,
    salt_file: PathBuf,
}

/// Resolve the pixel state root: `PIXEL_STATE_ROOT` > `XDG_STATE_HOME` >
/// `~/.local/state`, joined with `pixel`.
pub fn resolve_state_root() -> PathBuf {
    if let Ok(root) = std::env::var("PIXEL_STATE_ROOT")
        && !root.is_empty()
    {
        return PathBuf::from(root);
    }
    if let Ok(root) = std::env::var("XDG_STATE_HOME")
        && !root.is_empty()
    {
        return PathBuf::from(root).join("pixel");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".local").join("state").join("pixel")
}

impl GainLedger {
    /// Open (creating if needed) the ledger under the env-resolved state root.
    pub fn open() -> Result<GainLedger> {
        Self::open_at(&resolve_state_root())
    }

    /// Open with an explicit state root (tests / migrate).
    pub fn open_at(state_root: &Path) -> Result<GainLedger> {
        let directory = state_root.to_path_buf();
        fs::create_dir_all(&directory)?;
        Ok(GainLedger {
            directory: directory.clone(),
            file: directory.join(LEDGER_FILE),
            salt_file: directory.join(SALT_FILE),
        })
    }

    pub fn path(&self) -> &Path {
        &self.file
    }

    /// The salt, creating it (0600) on first use.
    fn salt(&self) -> Result<String> {
        if let Ok(s) = fs::read_to_string(&self.salt_file) {
            return Ok(s.trim().to_string());
        }
        let salt = random_hex(32);
        match fs::write(&self.salt_file, format!("{salt}\n")) {
            Ok(()) => {
                set_mode(&self.salt_file, 0o600);
                Ok(salt)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Ok(fs::read_to_string(&self.salt_file)?.trim().to_string())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Hash a repository identity with the salt (sha256, hex).
    pub fn hash_repository(&self, repository_identity: &str) -> Result<String> {
        let salt = self.salt()?;
        let mut hasher = Sha256::new();
        hasher.update(salt.as_bytes());
        hasher.update([0u8]);
        hasher.update(repository_identity.as_bytes());
        Ok(hex(&hasher.finalize()))
    }

    /// Append one event. Returns the repository hash used.
    pub fn append(&self, input: &GainEventInput) -> Result<String> {
        let repository_hash = self.hash_repository(&input.client)?;
        let event = GainEvent {
            version: "v1".into(),
            timestamp: iso_now(),
            operation: input.operation.clone(),
            client: input.client.clone(),
            transport: input.transport.clone(),
            result_code: input.result_code.clone(),
            repository_hash: repository_hash.clone(),
            envelope_bytes: input.envelope_bytes,
            raw_equivalent_bytes: input.raw_equivalent_bytes,
            agent_ops_raw: input.agent_ops_raw,
            agent_ops_actual: input.agent_ops_actual,
            git_subprocesses_raw: input.git_subprocesses_raw,
            git_subprocesses_actual: input.git_subprocesses_actual,
            duration_ms: input.duration_ms,
            tokens_saved: input.tokens_saved,
            source: input.source.clone(),
        };
        let line = serde_json::to_string(&event)?;
        let mut handle = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;
        use std::io::Write;
        writeln!(handle, "{line}")?;
        handle.sync_all()?;
        Ok(repository_hash)
    }

    /// Read all events in append order.
    pub fn read(&self) -> Result<Vec<GainEvent>> {
        let raw = match fs::read_to_string(&self.file) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for (idx, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: GainEvent = serde_json::from_str(line).map_err(|e| GainError::CorruptLine {
                line: idx + 1,
                reason: e.to_string(),
            })?;
            out.push(event);
        }
        Ok(out)
    }

    /// Read events for one repository identity (by hashed identity).
    pub fn read_for_repository(&self, repository_identity: &str) -> Result<Vec<GainEvent>> {
        let target = self.hash_repository(repository_identity)?;
        Ok(self
            .read()?
            .into_iter()
            .filter(|e| e.repository_hash == target)
            .collect())
    }

    /// Delete the ledger file (used by `gain --reset`).
    pub fn reset(&self) -> Result<()> {
        match fs::remove_file(&self.file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Aggregate a set of events into a report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerAggregate {
    pub total_operations: u64,
    pub total_envelope_bytes: u64,
    pub total_raw_equivalent_bytes: u64,
    pub total_tokens_saved: i64,
    pub total_agent_ops_saved: i64,
    pub total_subprocesses_saved: i64,
    pub avg_savings_pct: f64,
    pub by_operation: Vec<OperationBreakdown>,
    pub by_source: Vec<SourceBreakdown>,
    pub recent: Vec<GainEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationBreakdown {
    pub operation: String,
    pub count: u64,
    pub tokens_saved: i64,
    pub avg_pct: f64,
    pub envelope_bytes: u64,
    pub raw_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceBreakdown {
    pub source: String,
    pub count: u64,
}

/// Aggregate events into a report (port of usable-git `aggregateEvents`).
pub fn aggregate_events(events: &[GainEvent]) -> LedgerAggregate {
    let total_operations = events.len() as u64;
    let mut total_envelope_bytes = 0u64;
    let mut total_raw_equivalent_bytes = 0u64;
    let mut total_tokens_saved = 0i64;
    let mut total_agent_ops_saved = 0i64;
    let mut total_subprocesses_saved = 0i64;

    let mut by_op: Vec<OperationBreakdown> = Vec::new();
    let mut by_source: Vec<SourceBreakdown> = Vec::new();

    for event in events {
        total_envelope_bytes += event.envelope_bytes;
        total_raw_equivalent_bytes += event.raw_equivalent_bytes;
        total_tokens_saved += event.tokens_saved;
        total_agent_ops_saved += event.agent_ops_raw as i64 - event.agent_ops_actual as i64;
        total_subprocesses_saved +=
            event.git_subprocesses_raw as i64 - event.git_subprocesses_actual as i64;

        match by_op.iter_mut().find(|b| b.operation == event.operation) {
            Some(b) => {
                b.count += 1;
                b.tokens_saved += event.tokens_saved;
                b.envelope_bytes += event.envelope_bytes;
                b.raw_bytes += event.raw_equivalent_bytes;
            }
            None => by_op.push(OperationBreakdown {
                operation: event.operation.clone(),
                count: 1,
                tokens_saved: event.tokens_saved,
                envelope_bytes: event.envelope_bytes,
                raw_bytes: event.raw_equivalent_bytes,
                avg_pct: 0.0,
            }),
        }

        match by_source.iter_mut().find(|b| b.source == event.source) {
            Some(b) => b.count += 1,
            None => by_source.push(SourceBreakdown {
                source: event.source.clone(),
                count: 1,
            }),
        }
    }

    for b in &mut by_op {
        b.avg_pct = if b.raw_bytes > 0 {
            (1.0 - b.envelope_bytes as f64 / b.raw_bytes as f64).max(0.0) * 100.0
        } else {
            0.0
        };
    }
    by_op.sort_by(|a, b| b.tokens_saved.cmp(&a.tokens_saved));
    by_source.sort_by(|a, b| b.count.cmp(&a.count));

    let avg_savings_pct = if total_raw_equivalent_bytes > 0 {
        (1.0 - total_envelope_bytes as f64 / total_raw_equivalent_bytes as f64).max(0.0) * 100.0
    } else {
        0.0
    };

    let recent = events.iter().rev().take(20).cloned().collect();

    LedgerAggregate {
        total_operations,
        total_envelope_bytes,
        total_raw_equivalent_bytes,
        total_tokens_saved,
        total_agent_ops_saved,
        total_subprocesses_saved,
        avg_savings_pct,
        by_operation: by_op,
        by_source,
        recent,
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    #[cfg(unix)]
    {
        if let Ok(mut f) = fs::File::open("/dev/urandom") {
            use std::io::Read;
            if f.read_exact(&mut buf).is_ok() {
                return hex(&buf);
            }
        }
    }
    // Fallback (non-unix or urandom failure): hash time + pid + a counter.
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(bytes * 2);
    for _ in 0..bytes {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0));
        h.write_u64(std::process::id() as u64);
        out.push_str(&format!("{:02x}", h.finish() as u8));
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // RFC3339-ish UTC timestamp (second precision is sufficient for a ledger).
    format!("{secs}")
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}
