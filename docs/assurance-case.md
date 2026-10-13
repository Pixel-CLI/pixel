# Pixel assurance case

This document argues why Pixel meets the security requirements it states,
and where it does not (OpenSSF Best Practices `assurance_case`). It does not
repeat the analysis: [threat-model.md](threat-model.md) holds the assets,
actors, trust boundaries and threats, each with the code that mitigates it
and the risk left over. This file connects those facts to the claims and
gives the two arguments the threat model does not: which secure design
principles the code applies, and how the common implementation weaknesses of
a local CLI written in Rust are countered.

## 1. The claims

The top-level claim: **used as documented, Pixel does not give a repository,
an agent, another local user or a network peer more than the user already
gave them.** It breaks down into the security requirements of
[SECURITY.md](../SECURITY.md), "Security model":

| # | Requirement | Threats it answers |
| --- | --- | --- |
| R1 | A repository's content cannot make Pixel read, write or run anything outside that repository, beyond what the user's own `git` would run | T5, T6, T7, T8, T9, T23 |
| R2 | Another local user cannot reach the daemon or read Pixel's state | T2, T15 |
| R3 | A hook never runs a command from its payload, rewrites or approves a command, or blocks a host through a stale registration | T10 |
| R4 | Nothing leaves the machine without an explicit command or setting, and a key goes only to the endpoint it was configured for, over TLS | T16, T17, T18 |
| R5 | `pixel install` changes only what it manages, and can be undone | T14 |
| R6 | A release is built from the tagged source by the project's workflow, and a user can verify it | T20, T21, T22 |

The claims hold for the trust model of threat-model.md, section 2: the user
and the maintainers are trusted, same-user processes are trusted by the
operating system and not separated by Pixel (T1), and the agent can be
steered by what it reads (T12).

## 2. Threat model and trust boundaries

[threat-model.md](threat-model.md), sections 2 to 4: the actors and their
trust, the seven boundaries B1 to B7 (hostile repository, agent, same-user
process, installed hooks, network, contributor-to-release), the entry points
behind each boundary, and the 24 threats with a STRIDE category, a
mitigation, a status (mitigated, partial, accepted) and a residual.

## 3. Secure design principles

The design principles of Saltzer and Schroeder, and where Pixel applies
them. Each line names the mechanism; the threat it belongs to gives the code.

- **Least privilege.** The daemon socket is 0600 in a per-user directory
  (T2); state under `.pixel/`, flows and the global config are 0700/0600
  (T15); the task layer's git snapshots run with global and system configs,
  hooks and `core.fsmonitor` disabled (`GitRunner::run_isolated`, section
  3.6); workflows default to read-only tokens and grant write per job, and
  the build that signs a release holds no secret (T21, T22).
- **Fail-safe defaults.** The retired hook verbs answer nothing, so a stale
  registration cannot block the agent, and `task-event` never executes a
  command from its payload (T10); a `.pixel/` that is a
  link or tracked by git is refused, not repaired (T6); a key is never sent
  over plain `http://` to a non-loopback host (T16); network commands are
  opt-in (T17).
- **Complete mediation.** One spawner for git, `GitRunner`, held by a test
  that fails on any other `Command::new("git")` (T8); one confinement check,
  `pixel_git::repo_path::confine`, for every path read back from the index or
  the graph (T6).
- **Economy of mechanism.** The one hook `pixel install` registers,
  `task-event`, interprets no shell (section 3.3); 0.7.0 removed the MCP
  servers and the task worker runtime, so the CLI and that hook are the whole
  surface.
- **Open design.** The code, this case and the threat model are public;
  nothing depends on an attacker not knowing how a check works.
- **Separation of privilege.** Granting write access or a secret needs both
  maintainers' agreement in a pull request (GOVERNANCE.md, "Granting
  access"); releases are signed by a workflow separate from the one holding
  the tap token (T21).
- **Least common mechanism.** Each daemon serves one repository root (T1);
  the global configuration, which holds keys and endpoints, is never
  overridden by a repository's configuration (section 3.7, T16).
- **Psychological acceptability.** `pixel install` wires every agent in one
  command and `pixel doctor` reports what is wrong with the fix to run (T14);
  release verification is one documented `gh attestation verify` command
  (SECURITY.md, "Verifying a release").
- **Input validation at the boundary.** Arguments are parsed into typed
  values by clap; daemon requests are deserialised into the typed
  `pixel_proto::Op`, hook payloads are parsed as JSON and read field by
  field, and request lines
  and hook inputs are capped (3.2, T10); refs go
  through `validate_ref` (T8); shard files are bounds-checked before use
  (T5).

## 4. Common implementation weaknesses

The weaknesses of the CWE Top 25 that apply to a local Rust CLI with a
daemon, hooks and network clients, and how each is countered. Web-only
weaknesses (cross-site scripting, cross-site request forgery) do not apply:
Pixel serves no web content.

| Weakness | Countered by | Evidence |
| --- | --- | --- |
| Memory corruption: out-of-bounds read/write, use after free (CWE-787, 125, 416) | Rust's ownership and bounds checks; every `unsafe` block carries a `// SAFETY:` comment (`undocumented_unsafe_blocks` lint, Cargo.toml); the C tree-sitter grammars are fuzzed under AddressSanitizer | `fuzz/` (`graph_extract`), T5 residual names the remaining C surface and the mapped-shard `SIGBUS` |
| OS command and argument injection (CWE-78, 88) | git is spawned with an argument vector, never a shell; `validate_ref` and `--end-of-options` for refs, `--` before pathspecs; `task-event` never executes the hook's command | T8, T10; `crates/pixel-git/tests/boundary.rs`, `ref_guard.rs` tests |
| Path traversal and link following (CWE-22, 59) | `repo_path::confine` on stored paths; `O_NOFOLLOW` and `SQLITE_OPEN_NOFOLLOW` under `.pixel/`; the walker does not follow links | T6, GHSA-c9f5-vxc4-wjph |
| SQL injection (CWE-89) | SQLite values are bound parameters; the identifiers spliced into a statement are constants of the code | `pixel-graph`, `pixel-facts`, `pixel-recall` stores |
| Deserialisation of untrusted data (CWE-502) | serde into typed structures or plain JSON values, with no code execution on load; malformed input is an error | T4, T5 (`malformed_shard_rejected_gracefully`) |
| Uncontrolled resource consumption (CWE-400, 770) | file size cap (`MAX_FILE_BYTES`), graph file cap, request line, connection deadline and per-connection request caps on the daemon, git timeouts and output caps, stdout cap | sections 3.1, 3.2, 3.4, 3.6; T3 residual |
| Integer overflow when sizing reads (CWE-190) | `Shard::open` checks every section length with checked arithmetic before indexing | T5 |
| Incorrect default permissions (CWE-276, 732) | modes set on the open descriptor, 0600/0700 on every file holding state or secrets | T2, T15 |
| Cleartext storage or logging of secrets (CWE-312, 532) | keys only in the 0600 global config or the environment; `logged_args` masks `config remote-key` values in the action log | T15 (its residual lists the arguments not masked yet) |
| Cleartext transmission and certificate validation (CWE-319, 295) | TLS with webpki roots through `ureq`; a key never travels over plain HTTP off loopback | T16 |
| Download of code without integrity check (CWE-494) | release archives checked against their `.sha256`, signed SLSA provenance on every archive and `install.sh` | T20, T21; T18 residual: models and the Ollaya installer are not pinned |
| Time-of-check to time-of-use (CWE-367) | files are created fresh and renamed into place rather than checked and then opened | T6, T14 residual names the configs still written with `fs::write` |

## 5. Assurance that the arguments stay true

- **Tests that fail when a mitigation goes.** The critical paths of
  threat-model.md section 5 each name their tests; mutation testing fails a
  pull request whose diff leaves a surviving mutant, so a check that can be
  deleted without a test failing does not merge (CONTRIBUTING.md, "Mutation
  testing").
- **Analysis on every change.** CodeQL, Clippy with warnings denied,
  `cargo deny` and fuzzing run in CI (threat-model.md section 5); SECURITY.md
  sets the remediation threshold for their findings.
- **Review.** Every change to `main` goes through a pull request and its
  required checks (GOVERNANCE.md).
- **Upkeep.** A pull request that crosses a trust boundary updates the
  threat model in the same change, the release skill reads each changelog
  against it, and a fixed advisory updates its threat (threat-model.md
  section 6). A change that invalidates an argument here updates this file
  in the same pull request.

## 6. What the case does not cover

The partial and accepted threats of threat-model.md section 4 are the known
gaps; their residuals are part of this case, not exceptions to it. In short:
same-user processes are not separated (T1), a repository's own git
configuration and hooks run as they would for the user (T7, T9, T23),
prompt injection is the model's to resist (T12), and downloaded models are
not pinned (T18).
