// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Locked task transactions with an authoritative append-only journal.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::*;
use crate::structural::StructuralContext;
use crate::{Error, Result, digest, now_ms, policy, runner, sandbox, snapshot};

/// Journal bounds fail explicitly; truncation never grants completion.
const MAX_JOURNAL_BYTES: u64 = 67_108_864;
const MAX_EVENTS: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Commit {
    request_id: String,
    input_id: String,
    previous: String,
    checksum: String,
    event: TrajectoryEvent,
    task: Task,
}

impl Commit {
    fn checksum(&self) -> Result<String> {
        digest(&(
            &self.request_id,
            &self.input_id,
            &self.previous,
            &self.event,
            &self.task,
        ))
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    limits: StoreLimits,
}

#[derive(Debug, Clone, Copy)]
struct StoreLimits {
    journal_bytes: u64,
    events: usize,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            journal_bytes: MAX_JOURNAL_BYTES,
            events: MAX_EVENTS,
        }
    }
}

struct TaskLock(File);

impl Drop for TaskLock {
    fn drop(&mut self) {
        // Explicit unlock releases the transaction even while another test's
        // freshly forked child briefly inherits the descriptor before exec.
        let _ = FileExt::unlock(&self.0);
    }
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
            limits: StoreLimits::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn directory(&self, task_id: &str) -> Result<PathBuf> {
        valid_id(task_id)?;
        Ok(self.root.join(".pixel/tasks").join(task_id))
    }

    fn lock(&self, task_id: &str) -> Result<TaskLock> {
        let directory = self.directory(task_id)?;
        pixel_ops::durable::ensure_dir(&directory)?;
        if !directory
            .canonicalize()?
            .starts_with(self.root.join(".pixel/tasks").canonicalize()?)
        {
            return Err(Error::Invalid("task directory escapes task store".into()));
        }
        let lock = pixel_git::nofollow::open_lock(&directory.join("lock"))?;
        lock.try_lock_exclusive()
            .map_err(|_| Error::Busy(task_id.into()))?;
        Ok(TaskLock(lock))
    }

    fn read_commits(&self, task_id: &str) -> Result<Vec<Commit>> {
        let path = self.directory(task_id)?.join("journal.jsonl");
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() > self.limits.journal_bytes {
            return Err(Error::Corrupt("task journal exceeds supported size".into()));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut commits: Vec<Commit> = Vec::new();
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if line.last() != Some(&b'\n') {
                break;
            }
            let record: Commit =
                serde_json::from_slice(line).map_err(|error| Error::Corrupt(error.to_string()))?;
            let expected_previous = commits.last().map_or("", |prior| prior.checksum.as_str());
            let expected_revision = commits.last().map_or(1, |prior| prior.task.revision + 1);
            if record.task.task_id != task_id
                || record.task.version != SCHEMA_VERSION
                || record.task.revision != expected_revision
                || record.previous != expected_previous
                || record.checksum != record.checksum()?
            {
                return Err(Error::Corrupt(
                    "task identity, sequence or checksum mismatch".into(),
                ));
            }
            commits.push(record);
            if commits.len() > self.limits.events {
                return Err(Error::Corrupt("task journal event limit exceeded".into()));
            }
        }
        if !bytes.is_empty() && commits.is_empty() {
            return Err(Error::Corrupt("no complete task journal record".into()));
        }
        Ok(commits)
    }

    fn legacy(&self, task_id: &str) -> Result<Task> {
        let bytes = match fs::read(self.directory(task_id)?.join("task.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(task_id.into()));
            }
            Err(error) => return Err(error.into()),
        };
        let old: Value =
            serde_json::from_slice(&bytes).map_err(|error| Error::Corrupt(error.to_string()))?;
        if old["version"] != 1 || old["task_id"].as_str() != Some(task_id) {
            return Err(Error::Corrupt(
                "unsupported task ledger without journal".into(),
            ));
        }
        let objective = old["spec"]["objective"]
            .as_str()
            .ok_or_else(|| Error::Corrupt("legacy objective missing".into()))?;
        let contract = TaskContract {
            objective: objective.into(),
            ..TaskContract::default()
        };
        contract.validate()?;
        Ok(Task {
            version: SCHEMA_VERSION,
            task_id: task_id.into(),
            provider: old["provider"].as_str().unwrap_or("unknown").into(),
            session_id: old["session_id"].as_str().map(str::to_string),
            attempt_id: format!("{task_id}:legacy"),
            revision: 0,
            phase: Phase::Contracted,
            created_ms: old["created_unix"]
                .as_u64()
                .unwrap_or_default()
                .saturating_mul(1000),
            updated_ms: now_ms(),
            contract,
            source: None,
            observations: Vec::new(),
            receipts: Vec::new(),
            review: None,
            claims: old["model_claims"]
                .as_array()
                .map_or_else(Vec::new, |claims| {
                    claims
                        .iter()
                        .filter_map(|claim| claim["text"].as_str().map(str::to_string))
                        .collect()
                }),
            budget: CorrectionBudget::default(),
            running: None,
            legacy_status: Some(old["status"].as_str().unwrap_or("unknown").into()),
        })
    }

    pub fn status(&self, task_id: &str) -> Result<Task> {
        let commits = self.read_commits(task_id)?;
        commits.last().map_or_else(
            || self.legacy(task_id),
            |last| self.hydrate(last.task.clone()),
        )
    }

    fn hydrate(&self, mut task: Task) -> Result<Task> {
        if let Some(source) = &mut task.source {
            if source.content_id.len() != 64
                || !source
                    .content_id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(Error::Corrupt("invalid source manifest identity".into()));
            }
            let path = self
                .root
                .join(".pixel/tasks/source-manifests")
                .join(format!("{}.json", source.content_id));
            let files: Vec<SourceFile> = serde_json::from_slice(&fs::read(path)?)?;
            if snapshot::source_identity(&files, &source.head, &source.index_id, &source.refs_id)?
                != source.content_id
            {
                return Err(Error::Corrupt("source manifest checksum mismatch".into()));
            }
            source.files = files;
        }
        Ok(task)
    }

    fn persist_source(&self, task: &Task) -> Result<()> {
        if let Some(source) = &task.source {
            if snapshot::source_identity(
                &source.files,
                &source.head,
                &source.index_id,
                &source.refs_id,
            )? != source.content_id
            {
                return Err(Error::Corrupt(
                    "cannot persist mismatched source manifest".into(),
                ));
            }
            let directory = self.root.join(".pixel/tasks/source-manifests");
            pixel_ops::durable::ensure_dir(&directory)?;
            let path = directory.join(format!("{}.json", source.content_id));
            if !pixel_ops::durable::write_new_durably(&path, &serde_json::to_vec(&source.files)?)? {
                let existing: Vec<SourceFile> = serde_json::from_slice(&fs::read(path)?)?;
                if existing != source.files {
                    return Err(Error::Corrupt("immutable source manifest differs".into()));
                }
            }
        }
        Ok(())
    }

    pub fn events(&self, task_id: &str) -> Result<Vec<TrajectoryEvent>> {
        Ok(self
            .read_commits(task_id)?
            .into_iter()
            .map(|commit| commit.event)
            .collect())
    }

    /// Only the exact provider/session pair participates in automatic binding.
    pub fn find_session(&self, provider: &str, session: &str) -> Result<Option<Task>> {
        valid_id(provider)?;
        valid_id(session)?;
        let entries = match fs::read_dir(self.root.join(".pixel/tasks")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut found: Option<Task> = None;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if !entry.path().join("task.json").exists()
                && !entry.path().join("journal.jsonl").exists()
            {
                continue;
            }
            let task = self.status(&id)?;
            if task.provider == provider && task.session_id.as_deref() == Some(session) {
                if found.as_ref().is_some_and(|old| {
                    old.updated_ms == task.updated_ms && old.task_id != task.task_id
                }) {
                    return Err(Error::Blocked("ambiguous active session task".into()));
                }
                if found
                    .as_ref()
                    .is_none_or(|old| task.updated_ms.cmp(&old.updated_ms).is_gt())
                {
                    found = Some(task);
                }
            }
        }
        Ok(found)
    }

    pub fn begin(
        &self,
        contract: TaskContract,
        provider: &str,
        session: Option<&str>,
        request_id: &str,
    ) -> Result<Task> {
        contract.validate()?;
        valid_id(provider)?;
        valid_id(request_id)?;
        if let Some(session) = session {
            valid_id(session)?;
        }
        let task_id = format!("task-{}", &digest(&(provider, session, request_id))?[..24]);
        let _lock = self.lock(&task_id)?;
        let commits = self.read_commits(&task_id)?;
        let input_id = digest(&("begin", &contract, provider, session))?;
        if let Some(existing) = replay(&commits, request_id, &input_id)? {
            return self.hydrate(existing);
        }
        if !commits.is_empty() {
            return Err(Error::Idempotency(request_id.into()));
        }
        let now = now_ms();
        let task = Task {
            version: SCHEMA_VERSION,
            task_id,
            provider: provider.into(),
            session_id: session.map(str::to_string),
            attempt_id: uuid::Uuid::new_v4().to_string(),
            revision: 0,
            phase: Phase::Contracted,
            created_ms: now,
            updated_ms: now,
            contract,
            source: None,
            observations: Vec::new(),
            receipts: Vec::new(),
            review: None,
            claims: Vec::new(),
            budget: CorrectionBudget::default(),
            running: None,
            legacy_status: None,
        };
        self.commit(task, &commits, request_id, &input_id, "begun", Value::Null)
    }

    pub fn update(
        &self,
        task_id: &str,
        expected_revision: u64,
        request_id: &str,
        action: Action,
    ) -> Result<Task> {
        valid_id(request_id)?;
        let _lock = self.lock(task_id)?;
        let commits = self.read_commits(task_id)?;
        let input_id = digest(&action)?;
        if let Some(task) = replay(&commits, request_id, &input_id)? {
            return self.hydrate(task);
        }
        let mut task = commits.last().map_or_else(
            || self.legacy(task_id),
            |last| self.hydrate(last.task.clone()),
        )?;
        if task.revision != expected_revision {
            return Err(Error::Conflict {
                expected: expected_revision,
                actual: task.revision,
            });
        }
        let mut data = serde_json::to_value(&action)?;
        let kind = match action {
            Action::SetContract {
                contract,
                human_authorized,
            } => {
                nonterminal(&task)?;
                contract.validate()?;
                if !human_authorized && !contract.preserves(&task.contract) {
                    return Err(Error::Blocked(
                        "contract weakening requires human authorization".into(),
                    ));
                }
                if task.running.is_some() {
                    return Err(Error::Busy("verification still owns the contract".into()));
                }
                task.contract = contract;
                task.source = None;
                task.observations.clear();
                task.review = None;
                task.phase = Phase::Contracted;
                task.legacy_status = None;
                "contracted"
            }
            Action::Prepare { mut observations } => {
                nonterminal(&task)?;
                let source = snapshot::capture(&self.root, &task.contract, false)?;
                for observation in &mut observations {
                    if observation.source_id.is_empty() {
                        observation.source_id.clone_from(&source.content_id);
                    }
                    if observation.source_id != source.content_id {
                        return Err(Error::Blocked(
                            "preparation observation came from another source snapshot".into(),
                        ));
                    }
                }
                task.source = Some(source);
                task.observations = observations;
                task.phase = Phase::Prepared;
                "prepared"
            }
            Action::Edited => {
                nonterminal(&task)?;
                task.phase = Phase::Editing;
                task.source = None;
                task.observations.clear();
                task.review = None;
                "edited"
            }
            Action::Review { passed, findings } => {
                nonterminal(&task)?;
                let source = snapshot::capture(&self.root, &task.contract, true)?;
                if task
                    .source
                    .as_ref()
                    .is_none_or(|prior| prior.content_id != source.content_id)
                {
                    return Err(Error::Blocked(
                        "prepare changed source before review".into(),
                    ));
                }
                task.review = Some(ReviewReceipt {
                    source_id: source.content_id,
                    contract_id: task.contract.id()?,
                    passed,
                    findings,
                });
                task.phase = Phase::Reviewing;
                "reviewed"
            }
            Action::Finish => {
                let decision = self.current_decision(&task, Gate::Finish)?;
                if !decision.allowed {
                    return Err(Error::Blocked(decision.reasons.join("; ")));
                }
                task.phase = Phase::Complete;
                "completed"
            }
            Action::Cancel => {
                task.phase = Phase::Cancelled;
                "cancelled"
            }
            Action::Correction { state_key } => {
                nonterminal(&task)?;
                if task.budget.continuations >= MAX_CORRECTIONS {
                    return Err(Error::Blocked(
                        "automatic continuation budget exhausted".into(),
                    ));
                }
                let repeats = if task.budget.last_state.as_deref() == Some(&state_key) {
                    task.budget.same_state_repeats.saturating_add(1)
                } else {
                    1
                };
                if repeats > MAX_REPEATED_STATE {
                    return Err(Error::Blocked(
                        "same-state continuation limit exhausted".into(),
                    ));
                }
                task.budget.continuations += 1;
                task.budget.same_state_repeats = repeats;
                task.budget.last_state = Some(state_key);
                "correction"
            }
            Action::Observe { event } => {
                data = serde_json::to_value(event)?;
                "observation"
            }
            Action::Claim { text } => {
                if text.len() > 16_384 || task.claims.len() >= 128 {
                    return Err(Error::Invalid("model claim limits exceeded".into()));
                }
                task.claims.push(text);
                "model_claim"
            }
            Action::Recover => {
                if let Some(run) = &task.running {
                    if process_alive(run.owner_pid) {
                        return Err(Error::Busy("verification process is still alive".into()));
                    }
                    sandbox::check_recovery(
                        &self
                            .directory(task_id)?
                            .join(format!("run-{}.json", run.run_id)),
                    )?;
                    task.running = None;
                    if task.phase != Phase::Cancelled {
                        task.phase = Phase::Incomplete;
                    }
                }
                "recovered"
            }
        };
        self.commit(task, &commits, request_id, &input_id, kind, data)
    }

    pub fn decision(&self, task_id: &str, gate: Gate) -> Result<Decision> {
        self.current_decision(&self.status(task_id)?, gate)
    }

    /// Freeze all live gate inputs, including executable validity, for replay.
    pub fn decision_input(&self, task_id: &str, gate: Gate) -> Result<(Task, String)> {
        self.evaluate_input(&self.status(task_id)?, gate)
    }

    fn current_decision(&self, task: &Task, gate: Gate) -> Result<Decision> {
        let (evaluated, source_id) = self.evaluate_input(task, gate)?;
        Ok(policy::decide(&evaluated, gate, Some(&source_id)))
    }

    fn evaluate_input(&self, task: &Task, gate: Gate) -> Result<(Task, String)> {
        let source = snapshot::capture(&self.root, &task.contract, gate == Gate::Finish)?;
        let mut evaluated = task.clone();
        for check in &task.contract.checks {
            let identity = match check.kind {
                CheckKind::Argv => runner::contract_check_identity(check, &task.contract).ok(),
                _ => Some(runner::structural_check_identity(check).unwrap_or_default()),
            };
            for receipt in evaluated
                .receipts
                .iter_mut()
                .filter(|receipt| receipt.check_id == check.id)
            {
                if identity.as_deref() != Some(receipt.check_digest.as_str()) {
                    receipt.outcome = CheckOutcome::Unavailable;
                }
            }
        }
        Ok((evaluated, source.content_id))
    }

    pub fn verify(&self, task_id: &str, check_ids: &[String], request_id: &str) -> Result<Task> {
        self.verify_with_context(task_id, check_ids, request_id, None)
    }

    /// Verify with the structural facts the caller gathered (`diff-in-scope`,
    /// `graph-resolves`, `tests-touched`); `None` leaves every structural
    /// check `Unavailable`.
    pub fn verify_with_context(
        &self,
        task_id: &str,
        check_ids: &[String],
        request_id: &str,
        context: Option<&StructuralContext>,
    ) -> Result<Task> {
        valid_id(request_id)?;
        let input_id = digest(&("verify", check_ids))?;
        let (task, checks, run_id) = {
            let _lock = self.lock(task_id)?;
            let commits = self.read_commits(task_id)?;
            if let Some(task) = replay(&commits, request_id, &input_id)? {
                return self.hydrate(task);
            }
            let mut task = self.hydrate(
                commits
                    .last()
                    .ok_or_else(|| Error::NotFound(task_id.into()))?
                    .task
                    .clone(),
            )?;
            nonterminal(&task)?;
            if task.running.is_some() {
                return Err(Error::Busy(
                    "verification is already running or interrupted".into(),
                ));
            }
            let source = snapshot::capture(&self.root, &task.contract, true)?;
            if task
                .source
                .as_ref()
                .is_none_or(|prior| prior.content_id != source.content_id)
            {
                return Err(Error::Blocked(
                    "prepare current source before verification".into(),
                ));
            }
            let checks: Vec<_> = task
                .contract
                .checks
                .iter()
                .filter(|check| check_ids.is_empty() || check_ids.contains(&check.id))
                .cloned()
                .collect();
            if checks.is_empty()
                || check_ids
                    .iter()
                    .any(|id| !checks.iter().any(|check| &check.id == id))
            {
                return Err(Error::Invalid("no checks or unknown check id".into()));
            }
            let run_id = uuid::Uuid::new_v4().to_string();
            task.source = Some(source.clone());
            task.phase = Phase::Verifying;
            task.running = Some(VerificationRun {
                run_id: run_id.clone(),
                request_id: request_id.into(),
                source_id: source.content_id,
                owner_pid: std::process::id(),
                started_ms: now_ms(),
            });
            let started_request = format!("run:{run_id}");
            let task = self.commit(
                task,
                &commits,
                &started_request,
                &input_id,
                "verification_started",
                json!({"run_id":run_id,"checks":check_ids}),
            )?;
            (task, checks, run_id)
        };
        let source = task
            .source
            .as_ref()
            .expect("verification admission captures source");
        let lease = self.directory(task_id)?.join(format!("run-{run_id}.json"));
        let result = runner::verify_with_lease(
            source,
            &task.contract,
            &checks,
            &run_id,
            Some(&lease),
            context,
        );
        let _lock = self.lock(task_id)?;
        let commits = self.read_commits(task_id)?;
        let mut current = self.hydrate(
            commits
                .last()
                .ok_or_else(|| Error::NotFound(task_id.into()))?
                .task
                .clone(),
        )?;
        if current
            .running
            .as_ref()
            .is_none_or(|run| run.run_id != run_id)
        {
            return Err(Error::Conflict {
                expected: task.revision,
                actual: current.revision,
            });
        }
        current.running = None;
        let data = match result {
            Ok(receipts) => {
                let data = serde_json::to_value(&receipts)?;
                current.receipts.extend(receipts);
                data
            }
            Err(error) => {
                let message = error.to_string();
                for check in &checks {
                    let check_digest = match check.kind {
                        CheckKind::Argv => runner::contract_check_identity(check, &task.contract)
                            .unwrap_or_default(),
                        _ => runner::structural_check_identity(check).unwrap_or_default(),
                    };
                    current.receipts.push(VerificationReceipt {
                        run_id: run_id.clone(),
                        check_id: check.id.clone(),
                        source_id: source.content_id.clone(),
                        contract_id: task.contract.id()?,
                        check_digest,
                        outcome: CheckOutcome::Unavailable,
                        exit_code: None,
                        started_ms: task.updated_ms,
                        finished_ms: now_ms(),
                        duration_ms: now_ms().saturating_sub(task.updated_ms),
                        stdout_sha256: String::new(),
                        stderr_sha256: String::new(),
                        stdout_bytes: 0,
                        stderr_bytes: 0,
                        execution_root: String::new(),
                        diagnostic: Some(message.clone()),
                        structural: None,
                    });
                }
                json!({"error":message,"outcome":"unavailable"})
            }
        };
        if current.phase != Phase::Cancelled {
            current.phase = Phase::Reviewing;
        }
        self.commit(
            current,
            &commits,
            request_id,
            &input_id,
            "verification_finished",
            data,
        )
    }

    fn commit(
        &self,
        mut task: Task,
        commits: &[Commit],
        request_id: &str,
        input_id: &str,
        kind: &str,
        data: Value,
    ) -> Result<Task> {
        if commits.len() >= self.limits.events {
            return Err(Error::Unavailable("task event limit exceeded".into()));
        }
        task.revision = commits.last().map_or(1, |prior| prior.task.revision + 1);
        task.updated_ms = now_ms();
        self.persist_source(&task)?;
        let mut journal_task = task.clone();
        if let Some(source) = &mut journal_task.source {
            source.files.clear();
        }
        let event = TrajectoryEvent {
            id: request_id.into(),
            kind: kind.into(),
            attempt_id: task.attempt_id.clone(),
            occurred_ms: task.updated_ms,
            data,
        };
        let mut record = Commit {
            request_id: request_id.into(),
            input_id: input_id.into(),
            previous: commits
                .last()
                .map_or_else(String::new, |prior| prior.checksum.clone()),
            checksum: String::new(),
            event,
            task: journal_task,
        };
        record.checksum = record.checksum()?;
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        let directory = self.directory(&task.task_id)?;
        let mut journal = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("journal.jsonl"))?;
        let mut prior = Vec::new();
        journal.read_to_end(&mut prior)?;
        let committed_len = prior
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |offset| offset + 1);
        if committed_len as u64 + line.len() as u64 > self.limits.journal_bytes {
            return Err(Error::Unavailable(
                "task journal byte limit exceeded".into(),
            ));
        }
        journal.set_len(committed_len as u64)?;
        journal.seek(SeekFrom::End(0))?;
        journal.write_all(&line)?;
        journal.sync_all()?;
        File::open(&directory)?.sync_all()?;
        // Journal is authoritative: this view may be rebuilt after a crash.
        pixel_ops::durable::write_durably(
            &directory.join("task.json"),
            &serde_json::to_vec_pretty(&task)?,
        )?;
        Ok(task)
    }
}

fn replay(commits: &[Commit], request_id: &str, input_id: &str) -> Result<Option<Task>> {
    match commits
        .iter()
        .find(|commit| commit.request_id == request_id)
    {
        Some(commit) if commit.input_id == input_id => Ok(Some(commit.task.clone())),
        Some(_) => Err(Error::Idempotency(request_id.into())),
        None => Ok(None),
    }
}

fn nonterminal(task: &Task) -> Result<()> {
    if task.phase.terminal() {
        Err(Error::Blocked("terminal task cannot be changed".into()))
    } else {
        Ok(())
    }
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    // SAFETY: signal zero probes process existence without delivering a signal.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{contract, repo};

    fn begin(store: &Store, request: &str) -> Task {
        store
            .begin(contract(), "pi", Some("session"), request)
            .unwrap()
    }

    fn rewrite_task(store: &Store, id: &str, change: impl FnOnce(&mut Task)) {
        let mut records = store.read_commits(id).unwrap();
        let record = records.last_mut().unwrap();
        change(&mut record.task);
        record.checksum = record.checksum().unwrap();
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend(serde_json::to_vec(&record).unwrap());
            bytes.push(b'\n');
        }
        fs::write(store.directory(id).unwrap().join("journal.jsonl"), bytes).unwrap();
    }

    #[test]
    fn root_and_lock_ownership_survive_canonical_paths_and_inherited_descriptors() {
        let root = repo();
        let store = Store::open(&root.path().join(".")).unwrap();
        assert_eq!(store.root(), root.path().canonicalize().unwrap());
        let lock = store.lock("task-lock").unwrap();
        let inherited = lock.0.try_clone().unwrap();
        assert!(matches!(store.lock("task-lock"), Err(Error::Busy(_))));
        drop(lock);
        drop(
            store
                .lock("task-lock")
                .expect("explicit unlock must release inherited descriptor ownership"),
        );
        drop(inherited);
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".pixel/tasks/escape"))
            .unwrap();
        assert!(matches!(store.lock("escape"), Err(Error::Invalid(_))));
    }

    #[test]
    fn journal_read_and_append_limits_are_inclusive_and_atomic() {
        let root = repo();
        let mut store = Store::open(root.path()).unwrap();
        assert_eq!(store.limits.events, 10_000);
        assert_eq!(store.limits.journal_bytes, 67_108_864);
        store.limits.events = 2;
        let task = begin(&store, "begin");
        let task = store
            .update(
                &task.task_id,
                task.revision,
                "claim",
                Action::Claim {
                    text: "claim".into(),
                },
            )
            .unwrap();
        assert_eq!(store.status(&task.task_id).unwrap().revision, 2);
        assert!(matches!(
            store.update(
                &task.task_id,
                task.revision,
                "extra",
                Action::Claim {
                    text: "extra".into()
                }
            ),
            Err(Error::Unavailable(_))
        ));
        store.limits.events = 1;
        assert!(matches!(
            store.status(&task.task_id),
            Err(Error::Corrupt(_))
        ));

        let root = repo();
        let mut store = Store::open(root.path()).unwrap();
        let task = begin(&store, "begin");
        let journal = store
            .directory(&task.task_id)
            .unwrap()
            .join("journal.jsonl");
        let before = fs::read(&journal).unwrap();
        store.limits.journal_bytes = before.len() as u64;
        assert_eq!(store.status(&task.task_id).unwrap(), task);
        store.limits.journal_bytes -= 1;
        assert!(matches!(
            store.status(&task.task_id),
            Err(Error::Corrupt(_))
        ));
        store.limits.journal_bytes = MAX_JOURNAL_BYTES;
        store
            .update(
                &task.task_id,
                task.revision,
                "claim",
                Action::Claim {
                    text: "value".into(),
                },
            )
            .unwrap();
        let exact_size = fs::metadata(&journal).unwrap().len();
        fs::write(&journal, &before).unwrap();
        store.limits.journal_bytes = exact_size;
        assert_eq!(
            store
                .update(
                    &task.task_id,
                    task.revision,
                    "claim",
                    Action::Claim {
                        text: "value".into()
                    }
                )
                .unwrap()
                .revision,
            2
        );
        assert_eq!(fs::metadata(&journal).unwrap().len(), exact_size);
        fs::write(&journal, &before).unwrap();
        store.limits.journal_bytes = exact_size - 1;
        assert!(matches!(
            store.update(
                &task.task_id,
                task.revision,
                "claim",
                Action::Claim {
                    text: "value".into()
                }
            ),
            Err(Error::Unavailable(_))
        ));
        assert_eq!(fs::read(journal).unwrap(), before);
    }

    #[test]
    fn missing_state_is_distinct_from_nonmissing_io_and_partial_authority() {
        let root = repo();
        let store = Store::open(root.path()).unwrap();
        assert!(matches!(store.status("missing"), Err(Error::NotFound(_))));
        assert!(store.find_session("pi", "missing").unwrap().is_none());
        let path = store.directory("bad-journal").unwrap();
        fs::create_dir_all(path.join("journal.jsonl")).unwrap();
        assert!(matches!(store.status("bad-journal"), Err(Error::Io(_))));
        let path = store.directory("bad-legacy").unwrap();
        fs::create_dir_all(path.join("task.json")).unwrap();
        assert!(matches!(store.status("bad-legacy"), Err(Error::Io(_))));
        let path = store.directory("partial").unwrap();
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("journal.jsonl"), b"{\"partial\":").unwrap();
        assert!(
            matches!(store.status("partial"), Err(Error::Corrupt(message)) if message == "no complete task journal record")
        );
        let root = repo();
        fs::create_dir_all(root.path().join(".pixel")).unwrap();
        fs::write(root.path().join(".pixel/tasks"), "not a directory").unwrap();
        assert!(matches!(
            Store::open(root.path())
                .unwrap()
                .find_session("pi", "session"),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn journal_open_errors_never_fall_back_to_legacy_state() {
        let root = repo();
        let store = Store::open(root.path()).unwrap();
        let path = store.directory("legacy").unwrap();
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("task.json"),
            serde_json::to_vec(&json!({
                "version": 1,
                "task_id": "legacy",
                "spec": {"objective": "legacy objective"},
                "status": "complete"
            }))
            .unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink("journal.jsonl", path.join("journal.jsonl")).unwrap();
        assert!(matches!(
            store.status("legacy"),
            Err(Error::Io(error)) if error.raw_os_error() == Some(libc::ELOOP)
        ));
        fs::remove_file(path.join("journal.jsonl")).unwrap();
        assert_eq!(
            store.status("legacy").unwrap().contract.objective,
            "legacy objective"
        );
    }

    #[test]
    fn legacy_schema_and_task_identity_are_checked_independently() {
        let root = repo();
        let store = Store::open(root.path()).unwrap();
        let path = store.directory("legacy").unwrap();
        fs::create_dir_all(&path).unwrap();
        for (version, id) in [(2, "legacy"), (1, "other")] {
            fs::write(path.join("task.json"), serde_json::to_vec(&json!({"version":version,"task_id":id,"spec":{"objective":"legacy objective"}})).unwrap()).unwrap();
            assert!(matches!(store.status("legacy"), Err(Error::Corrupt(_))));
        }
        fs::write(path.join("task.json"), serde_json::to_vec(&json!({"version":1,"task_id":"legacy","spec":{"objective":"legacy objective"},"created_unix":12,"provider":"pi","session_id":"session","status":"worker","model_claims":[{"text":"done"}]})).unwrap()).unwrap();
        let task = store.status("legacy").unwrap();
        assert_eq!(task.contract.objective, "legacy objective");
        assert_eq!(task.created_ms, 12_000);
        assert_eq!(task.legacy_status.as_deref(), Some("worker"));
        assert_eq!(task.claims, ["done"]);
        assert_eq!(
            store
                .find_session("pi", "session")
                .unwrap()
                .unwrap()
                .task_id,
            "legacy"
        );
    }

    #[test]
    fn session_binding_chooses_latest_authority_and_refuses_timestamp_ties() {
        let root = repo();
        let store = Store::open(root.path()).unwrap();
        let first = begin(&store, "first");
        let second = begin(&store, "second");
        rewrite_task(&store, &first.task_id, |task| task.updated_ms = 10);
        rewrite_task(&store, &second.task_id, |task| task.updated_ms = 20);
        fs::remove_file(store.directory(&second.task_id).unwrap().join("task.json")).unwrap();
        fs::write(root.path().join(".pixel/tasks/ordinary-file"), "ignore").unwrap();
        fs::create_dir(root.path().join(".pixel/tasks/empty-directory")).unwrap();
        assert_eq!(
            store
                .find_session("pi", "session")
                .unwrap()
                .unwrap()
                .task_id,
            second.task_id
        );
        rewrite_task(&store, &first.task_id, |task| task.updated_ms = 30);
        assert_eq!(
            store
                .find_session("pi", "session")
                .unwrap()
                .unwrap()
                .task_id,
            first.task_id
        );
        rewrite_task(&store, &second.task_id, |task| task.updated_ms = 30);
        assert!(matches!(
            store.find_session("pi", "session"),
            Err(Error::Blocked(_))
        ));
    }

    #[test]
    fn claim_byte_and_count_limits_accept_exact_boundaries_without_partial_writes() {
        let root = repo();
        let store = Store::open(root.path()).unwrap();
        let task = begin(&store, "begin");
        let task = store
            .update(
                &task.task_id,
                task.revision,
                "max-length",
                Action::Claim {
                    text: "x".repeat(16_384),
                },
            )
            .unwrap();
        assert_eq!(task.claims[0].len(), 16_384);
        assert!(matches!(
            store.update(
                &task.task_id,
                task.revision,
                "too-long",
                Action::Claim {
                    text: "x".repeat(16_385)
                }
            ),
            Err(Error::Invalid(_))
        ));
        rewrite_task(&store, &task.task_id, |task| {
            task.claims = vec!["existing".into(); 127]
        });
        let task = store
            .update(
                &task.task_id,
                task.revision,
                "max-count",
                Action::Claim {
                    text: "last".into(),
                },
            )
            .unwrap();
        assert_eq!(task.claims.len(), 128);
        assert_eq!(task.claims.last().map(String::as_str), Some("last"));
        assert!(matches!(
            store.update(
                &task.task_id,
                task.revision,
                "too-many",
                Action::Claim {
                    text: "extra".into()
                }
            ),
            Err(Error::Invalid(_))
        ));
        assert_eq!(store.status(&task.task_id).unwrap(), task);
    }

    #[test]
    fn manifest_id_validation_and_existing_manifest_collision_fail_closed() {
        let root = repo();
        let store = Store::open(root.path()).unwrap();
        let task = begin(&store, "begin");
        let task = store
            .update(
                &task.task_id,
                task.revision,
                "prepare",
                Action::Prepare {
                    observations: vec![],
                },
            )
            .unwrap();
        let original = fs::read(
            store
                .directory(&task.task_id)
                .unwrap()
                .join("journal.jsonl"),
        )
        .unwrap();
        for id in ["a".repeat(63), "g".repeat(64)] {
            fs::write(
                store
                    .directory(&task.task_id)
                    .unwrap()
                    .join("journal.jsonl"),
                &original,
            )
            .unwrap();
            rewrite_task(&store, &task.task_id, |task| {
                task.source.as_mut().unwrap().content_id = id
            });
            assert!(
                matches!(store.status(&task.task_id), Err(Error::Corrupt(message)) if message == "invalid source manifest identity")
            );
        }
        fs::write(
            store
                .directory(&task.task_id)
                .unwrap()
                .join("journal.jsonl"),
            &original,
        )
        .unwrap();
        let mut changed = task.clone();
        changed.source.as_mut().unwrap().files[0].sha256 = "changed".into();
        assert!(matches!(
            store.persist_source(&changed),
            Err(Error::Corrupt(_))
        ));
        let manifest = root
            .path()
            .join(".pixel/tasks/source-manifests")
            .join(format!("{}.json", task.source.as_ref().unwrap().content_id));
        fs::write(manifest, "[]").unwrap();
        assert!(
            matches!(store.persist_source(&task), Err(Error::Corrupt(message)) if message == "immutable source manifest differs")
        );
    }

    #[test]
    fn recovery_distinguishes_live_exited_and_unrepresentable_owners() {
        assert!(process_alive(std::process::id()));
        assert!(process_alive(u32::MAX));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let exited = child.id();
        assert!(child.wait().unwrap().success());
        assert!(!process_alive(exited));
        let root = repo();
        let store = Store::open(root.path()).unwrap();
        let task = begin(&store, "begin");
        rewrite_task(&store, &task.task_id, |task| {
            task.running = Some(VerificationRun {
                run_id: "recoverable".into(),
                request_id: "verify".into(),
                source_id: "source".into(),
                owner_pid: std::process::id(),
                started_ms: 1,
            })
        });
        assert!(matches!(
            store.update(&task.task_id, task.revision, "alive", Action::Recover),
            Err(Error::Busy(_))
        ));
        rewrite_task(&store, &task.task_id, |task| {
            task.running.as_mut().unwrap().owner_pid = exited
        });
        let recovered = store
            .update(&task.task_id, task.revision, "exited", Action::Recover)
            .unwrap();
        assert_eq!(recovered.phase, Phase::Incomplete);
        assert!(recovered.running.is_none());
    }
}
