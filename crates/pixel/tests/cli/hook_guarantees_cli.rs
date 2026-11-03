// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The hook guarantees pixel-mvp proves on its own hook (its contract's C-26
//! and C-32), proven on v1's hook entry points (ADR-0036, step 4):
//!
//! - a hook exits 0 on every failure it handles: malformed, binary, nested,
//!   oversized or hostile payloads, a repository never indexed, broken or
//!   hostile state under `.pixel/`, no daemon, argument errors. A host blocks
//!   the prompt or the tool call on a non-zero exit (Claude Code on exit 2,
//!   which is clap's), so one bad byte must never cost the user their turn;
//! - on its own stdout a hook prints exactly one JSON object on one line, or
//!   nothing: never a log line, a metrics box, a warning or a partial object,
//!   which a host parsing stdout as its protocol would reject or, worse, read
//!   as the answer;
//! - it answers in time when its host leaves stdin open, instead of waiting
//!   for the host to kill it.
//!
//! The entry points are the ones `pixel setup` registers, `pixel run-hook
//! task-event` (every provider and event) and `pixel run-hook metrics`, and
//! `pixel brief`, which the OpenCode plugin runs as its
//! prompt-submit hook and whose stdout it injects as the brief.
//!
//! What pixel-mvp's C-26 says beyond this does not port: v1's task gate
//! denies an edit or a stop on an enforced session by design, which
//! pixel-mvp never does, and v1 has no record of a hook the host killed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use serde_json::{Map, Value, json};

use crate::support::{Scratch, git, pixel_command};

/// Every `--provider` of `run-hook task-event`.
const PROVIDERS: [&str; 6] = ["claude", "codex", "pi", "devin", "gemini", "antigravity"];

/// Every `--event` of `run-hook task-event`.
const EVENTS: [&str; 12] = [
    "session-start",
    "prompt-submit",
    "pre-tool-use",
    "post-tool-use",
    "tool-failure",
    "stop",
    "session-end",
    "interrupt",
    "subagent-start",
    "subagent-stop",
    "model-response",
    "user-bash",
];

/// The timeout `pixel setup` registers with every host (`HOOK_TIMEOUT`): a
/// hook still running then is killed, and a killed hook exits with no answer.
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// The shared stdin cap (`hook_input::MAX_HOOK_INPUT`).
const MAX_HOOK_INPUT: usize = 1_048_576;

/// The prompt that gets a brief in [`indexed`].
const RENAME: &str = "handleError in packages/ui/handleError.ts is being renamed to reportError. Which files change?";

/// A repository with a function, its callers and a generated file.
fn fixture(tag: &str) -> Scratch {
    let root = Scratch::for_test("hook-guarantees-cli", tag);
    std::fs::create_dir_all(root.join("packages/ui")).unwrap();
    std::fs::create_dir_all(root.join("apps/web")).unwrap();
    std::fs::write(
        root.join("packages/ui/handleError.ts"),
        "export function handleError(e: Error): string {\n  return \"boom \" + e.message;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("apps/web/page.tsx"),
        [
            "import { handleError } from \"../../packages/ui/handleError\";",
            "export function Page() {",
            "  try { return 1; } catch (e) { return handleError(e as Error); }",
            "}",
            "export function Other() { return handleError(new Error(\"x\")); }",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    // A tree of its own, so the shared base-shard cache never answers for it.
    std::fs::write(root.join("fixture.txt"), format!("{tag}\n")).unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "fixture"]);
    root
}

/// [`fixture`] with the text index and the graph built.
fn indexed(tag: &str) -> Scratch {
    let root = fixture(tag);
    for args in [
        &["search-content", "-F", "handleError", "-l", "--no-daemon"][..],
        &["rebuild-graph"],
    ] {
        let out = pixel_command()
            .args(args)
            .args(["--metrics", "off"])
            .arg(&*root)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
    }
    root
}

/// One finished hook.
struct Run {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
    took: Duration,
}

/// `pixel <args>` in `root` with `stdin` written to it, the pipe then held
/// open for `hold` (a host that does not close it), and `env` added. A hook
/// still running after twice the host's timeout fails the test instead of
/// hanging it.
fn run(root: &Path, args: &[&str], stdin: &[u8], hold: Duration, env: &[(&str, &str)]) -> Run {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = command.spawn().unwrap();
    let pid = child.id();
    let mut pipe = child.stdin.take().unwrap();
    let body = stdin.to_vec();
    // Detached: a hook that stops reading early must not block the test on a
    // full pipe, and a held pipe outlives the hook on purpose.
    std::thread::spawn(move || {
        let _ = pipe.write_all(&body);
        let _ = pipe.flush();
        std::thread::sleep(hold);
    });
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(child.wait_with_output());
    });
    let Ok(output) = receive.recv_timeout(HOOK_TIMEOUT * 2) else {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
        panic!(
            "`pixel {}` still ran after {:?}",
            args.join(" "),
            HOOK_TIMEOUT * 2
        );
    };
    let output = output.unwrap();
    Run {
        code: output.status.code(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        took: started.elapsed(),
    }
}

/// The one JSON object `stdout` holds on one line, or `None` for nothing;
/// an error for anything else.
fn one_json_line_or_nothing(stdout: &[u8]) -> Result<Option<Map<String, Value>>, String> {
    if stdout.is_empty() {
        return Ok(None);
    }
    let text = std::str::from_utf8(stdout).map_err(|err| format!("not UTF-8: {err}"))?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.contains(['\n', '\r']) {
        return Err(format!("more than one line: {text:?}"));
    }
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(object)) => Ok(Some(object)),
        Ok(other) => Err(format!("not an object: {other}")),
        Err(err) => Err(format!("{err}: {line:?}")),
    }
}

/// The guarantees every hook run owes, whatever its input: exit 0 within the
/// host's timeout, one JSON object on one line or nothing on stdout.
fn assert_hook_contract(run: &Run, what: &str) -> Option<Map<String, Value>> {
    assert_eq!(run.code, Some(0), "{what}: exit; stderr: {}", run.stderr);
    assert!(
        run.took < HOOK_TIMEOUT,
        "{what}: took {:?}, past the host's timeout",
        run.took
    );
    one_json_line_or_nothing(&run.stdout).unwrap_or_else(|err| panic!("{what}: stdout {err}"))
}

/// The additional context a hook answer carries to the model, if any.
fn context(answer: &Map<String, Value>) -> Option<&str> {
    answer
        .get("hookSpecificOutput")
        .and_then(|output| output.get("additionalContext"))
        .and_then(Value::as_str)
}

/// Deterministic bytes of any value: a fuzz case that reproduces.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed.to_be_bytes()[0]
        })
        .collect()
}

/// Malformed and hostile payloads, named, each aimed at `root`'s repository
/// where it names one.
fn hostile_payloads(root: &Path) -> Vec<(&'static str, Vec<u8>)> {
    let cwd = root.to_string_lossy().into_owned();
    let tokens: Vec<String> = (0..10_000).map(|i| format!("Tok{i}")).collect();
    vec![
        ("empty", Vec::new()),
        ("blank", b" \n\t\r\n".to_vec()),
        ("not json", b"not json".to_vec()),
        ("noise", noise(4096, 7)),
        (
            "invalid utf-8",
            b"\xff\xfe{\"prompt\":\"\xc3\x28\"}".to_vec(),
        ),
        (
            "truncated object",
            br#"{"hook_event_name":"PreToolUse","tool_na"#.to_vec(),
        ),
        (
            "trailing garbage",
            br#"{"prompt":"x"} {"prompt":"y"}"#.to_vec(),
        ),
        (
            "nested 100k deep",
            format!("{}{}", "[".repeat(100_000), "]".repeat(100_000)).into_bytes(),
        ),
        ("array", b"[1, 2, 3]".to_vec()),
        ("string", b"\"UserPromptSubmit\"".to_vec()),
        ("number", b"1e999".to_vec()),
        ("null", b"null".to_vec()),
        (
            "every field of the wrong type",
            json!({
                "prompt": 42, "cwd": {"a": 1}, "session_id": [1, 2],
                "hook_event_name": null, "tool_name": 7, "tool_input": "rm -rf /",
                "transcript_path": false, "workspacePaths": "x", "conversationId": 3,
            })
            .to_string()
            .into_bytes(),
        ),
        (
            "a nul and a lone surrogate",
            format!(
                r#"{{"prompt":"fix\u0000 handleError \ud800","cwd":{},"session_id":"\ud800"}}"#,
                json!(cwd)
            )
            .into_bytes(),
        ),
        (
            "a session id that climbs out of .pixel",
            json!({
                "prompt": RENAME, "cwd": cwd, "session_id": "../../../../../../tmp/x",
                "hook_event_name": "UserPromptSubmit",
            })
            .to_string()
            .into_bytes(),
        ),
        (
            "cwd elsewhere",
            json!({"prompt": RENAME, "cwd": "/nonexistent/pixel", "session_id": "s"})
                .to_string()
                .into_bytes(),
        ),
        (
            "an edit outside the repository",
            json!({
                "hook_event_name": "PreToolUse", "tool_name": "Edit", "cwd": cwd,
                "tool_input": {"file_path": "/etc/passwd", "old_string": "a", "new_string": "b"},
                "session_id": "s",
            })
            .to_string()
            .into_bytes(),
        ),
        (
            "10 000 distinct tokens",
            json!({"prompt": tokens.join(" "), "cwd": cwd, "session_id": "s"})
                .to_string()
                .into_bytes(),
        ),
        (
            "one byte over the cap",
            json!({"prompt": "a".repeat(MAX_HOOK_INPUT), "cwd": cwd})
                .to_string()
                .into_bytes(),
        ),
    ]
}

/// Every provider and every event, over every hostile payload: each payload
/// meets every provider on one event and every event on one provider, which
/// keeps the run near three hundred hooks instead of twelve hundred.
#[test]
fn task_event_should_exit_0_with_one_json_line_or_nothing_on_hostile_payloads() {
    let root = indexed("hostile");
    for (at, (name, payload)) in hostile_payloads(&root).iter().enumerate() {
        let pairs = PROVIDERS
            .iter()
            .map(|provider| (*provider, EVENTS[at % EVENTS.len()]))
            .chain(
                EVENTS
                    .iter()
                    .map(|event| (PROVIDERS[at % PROVIDERS.len()], *event)),
            );
        for (provider, event) in pairs {
            let args = [
                "run-hook",
                "task-event",
                "--provider",
                provider,
                "--event",
                event,
            ];
            let hook = run(&root, &args, payload, Duration::ZERO, &[]);
            assert_hook_contract(&hook, &format!("{provider}/{event}/{name}"));
        }
    }
}

/// The prompt-submit hook over payloads a property generator makes: objects
/// with the protocol's keys of any shape, any JSON, any bytes, and — as often
/// as all of those — a payload in the repository whose prompt gets a brief,
/// so answers are checked and not only silences.
#[test]
fn task_event_should_exit_0_with_one_json_line_or_nothing_on_random_payloads() {
    let root = indexed("random");
    let cwd = root.to_string_lossy().into_owned();
    let config = ProptestConfig {
        cases: 64,
        // A failure prints its case; nothing is written into the source tree.
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    let (cases, briefed) = (std::cell::Cell::new(0_u32), std::cell::Cell::new(0_u32));
    proptest!(config, |(
        provider in prop::sample::select(PROVIDERS.to_vec()),
        event in prop::sample::select(EVENTS.to_vec()),
        input in payload(cwd.clone()),
    )| {
        let (named, bytes) = input;
        // A named prompt is a prompt: it rides the prompt event of a
        // provider that briefs.
        let (provider, event) = if named {
            (["claude", "codex", "gemini", "devin"][cases.get() as usize % 4], "prompt-submit")
        } else {
            (provider, event)
        };
        let args = ["run-hook", "task-event", "--provider", provider, "--event", event];
        let hook = run(&root, &args, &bytes, Duration::ZERO, &[]);
        let shown = String::from_utf8_lossy(&bytes[..bytes.len().min(160)]).into_owned();
        let what = format!("{provider}/{event}: {shown}");
        prop_assert_eq!(hook.code, Some(0), "{}: {}", what, hook.stderr);
        prop_assert!(hook.took < HOOK_TIMEOUT, "{}: {:?}", what, hook.took);
        let answer = one_json_line_or_nothing(&hook.stdout)
            .map_err(|err| TestCaseError::fail(format!("{what}: {err}")))?;
        cases.set(cases.get() + 1);
        if named && answer.as_ref().and_then(context).is_some_and(|text| text.starts_with("[PIXEL:BRIEF]")) {
            briefed.set(briefed.get() + 1);
        }
    });
    // About half the cases are named; most of those must get their brief, or
    // the run proved only that silence is well formed.
    let (cases, briefed) = (cases.get(), briefed.get());
    assert!(
        briefed * 4 >= cases,
        "{briefed} of {cases} payloads got a brief"
    );
}

/// What a host may send, and whether it is a payload of the repository whose
/// prompt names its code.
fn payload(cwd: String) -> impl Strategy<Value = (bool, Vec<u8>)> {
    let in_repo = cwd.clone();
    let around = prop_oneof![
        Just(String::new()),
        Just("<system-reminder>handleError</system-reminder>".to_owned()),
        any::<String>().prop_map(|text| text.chars().take(40).collect::<String>()),
    ];
    let named = (around, any::<u32>()).prop_map(move |(around, session)| {
        let fields = json!({
            "prompt": format!("{RENAME} {around}"),
            "cwd": in_repo,
            "session_id": format!("random-{session}"),
            "hook_event_name": "UserPromptSubmit",
        });
        (true, fields.to_string().into_bytes())
    });
    let place = prop_oneof![
        Just(Value::from(cwd)),
        Just(Value::from("/nonexistent/pixel")),
        Just(Value::from("")),
        any::<i64>().prop_map(Value::from),
    ];
    let object = (
        prop::option::of(value()),
        prop::option::of(place),
        prop::option::of(value()),
        prop::option::of(value()),
        prop::option::of(value()),
        prop::option::of(value()),
    )
        .prop_map(|(prompt, cwd, event, tool, input, session)| {
            let mut fields = Map::new();
            for (key, value) in [
                ("prompt", prompt),
                ("cwd", cwd),
                ("hook_event_name", event),
                ("tool_name", tool),
                ("tool_input", input),
                ("session_id", session),
            ] {
                if let Some(value) = value {
                    fields.insert(key.to_owned(), value);
                }
            }
            (false, Value::Object(fields).to_string().into_bytes())
        });
    prop_oneof![
        3 => named,
        1 => object,
        1 => value().prop_map(|value| (false, value.to_string().into_bytes())),
        1 => prop::collection::vec(any::<u8>(), 0..2048).prop_map(|bytes| (false, bytes)),
    ]
}

/// Any JSON value a few levels deep, its strings sometimes ones a hook reads
/// as a tool or as code.
fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(|f| json!(f)),
        any::<String>().prop_map(Value::from),
        Just(Value::from("Edit")),
        Just(Value::from("git push --force")),
    ];
    leaf.prop_recursive(3, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::from),
            prop::collection::btree_map(any::<String>(), inner, 0..6)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

/// The metrics relay, under no provider and every one, on both its events:
/// a payload it cannot use is nothing on stdout, and a usable one is one
/// object.
#[test]
fn metrics_relay_should_exit_0_with_one_json_line_or_nothing_on_hostile_payloads() {
    let root = indexed("metrics");
    let mut payloads = hostile_payloads(&root);
    payloads.push((
        "a pixel call in a shell tool",
        json!({
            "tool_name": "Bash", "cwd": &*root,
            "tool_input": {"command": "pixel search-content -F handleError ."},
            "toolCall": {"name": "run_command", "args": {"CommandLine": "pixel x"}},
            "conversationId": "../../../../tmp/escape",
        })
        .to_string()
        .into_bytes(),
    ));
    for (name, payload) in &payloads {
        for provider in [
            None,
            Some("claude"),
            Some("codex"),
            Some("devin"),
            Some("antigravity"),
        ] {
            for event in ["post-tool-use", "post-invocation"] {
                let mut args = vec!["run-hook", "metrics", "--event", event];
                if let Some(provider) = provider {
                    args.extend(["--provider", provider]);
                }
                let hook = run(&root, &args, payload, Duration::ZERO, &[]);
                assert_hook_contract(&hook, &format!("{provider:?}/{event}/{name}"));
            }
        }
    }
}

/// Breaks the fixture it is given; names the runtime directory the daemon's
/// socket is looked for in when it moved it.
type Breaking = fn(&Path) -> Option<PathBuf>;

/// Pixel's own state broken, or planted by a clone: the hook still exits 0
/// with one line on every event, in the host's time. No daemon runs in any
/// of them (`pixel_command` turns auto-start off), which is the daemon-down
/// case; one has a socket nothing listens on.
#[test]
fn task_event_should_exit_0_with_one_json_line_when_pixel_state_is_broken() {
    let mut breakages: Vec<(&str, Breaking)> = vec![
        ("never indexed", |root| {
            std::fs::remove_dir_all(root.join(".pixel")).unwrap();
            None
        }),
        ("graph garbage", |root| {
            for entry in std::fs::read_dir(root.join(".pixel")).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if name.starts_with("graph") {
                    std::fs::write(&path, noise(5000, 11)).unwrap();
                }
            }
            None
        }),
        ("state garbage", |root| {
            std::fs::write(root.join(".pixel/state.json"), "{{{").unwrap();
            std::fs::write(root.join(".pixel/base.shard"), noise(5000, 13)).unwrap();
            None
        }),
        (".pixel a file", |root| {
            std::fs::remove_dir_all(root.join(".pixel")).unwrap();
            std::fs::write(root.join(".pixel"), "x").unwrap();
            None
        }),
    ];
    #[cfg(unix)]
    breakages.extend([
        (
            "a dead daemon socket",
            (|root: &Path| {
                // Where `pixel_daemon::socket_path` looks for this root's
                // daemon: the runtime directory (TMPDIR on macOS,
                // XDG_RUNTIME_DIR elsewhere), named by the root's hash.
                let dir = std::env::temp_dir().join(format!("pxh-{}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                let hash = xxhash_rust::xxh3::xxh3_64(root.to_string_lossy().as_bytes());
                let socket = dir.join(format!("pixel-{hash:016x}.sock"));
                drop(std::os::unix::net::UnixListener::bind(socket).unwrap());
                Some(dir)
            }) as fn(&Path) -> Option<PathBuf>,
        ),
        (".pixel unreadable", |root| {
            use std::os::unix::fs::PermissionsExt;
            let dir = root.join(".pixel");
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            None
        }),
    ]);
    for (name, breaking) in breakages {
        let root = indexed(&format!("broken-{}", name.replace([' ', '.'], "-")));
        let runtime = breaking(&root);
        let runtime_dir = runtime
            .as_deref()
            .map(|dir| dir.to_string_lossy().into_owned());
        let env: Vec<(&str, &str)> = runtime_dir
            .as_deref()
            .map(|dir| vec![("TMPDIR", dir), ("XDG_RUNTIME_DIR", dir)])
            .unwrap_or_default();
        for (provider, event) in [
            ("claude", "session-start"),
            ("claude", "prompt-submit"),
            ("claude", "pre-tool-use"),
            ("claude", "post-tool-use"),
            ("claude", "stop"),
            ("codex", "prompt-submit"),
        ] {
            let payload = json!({
                "session_id": "broken", "prompt": RENAME, "cwd": &*root,
                "hook_event_name": "UserPromptSubmit", "tool_name": "Edit",
                "tool_input": {"file_path": "apps/web/page.tsx"},
            });
            let args = [
                "run-hook",
                "task-event",
                "--provider",
                provider,
                "--event",
                event,
            ];
            let hook = run(
                &root,
                &args,
                payload.to_string().as_bytes(),
                Duration::ZERO,
                &env,
            );
            assert_hook_contract(&hook, &format!("{name}: {provider}/{event}"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                root.join(".pixel"),
                std::fs::Permissions::from_mode(0o755),
            );
        }
        if let Some(dir) = runtime {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// A clone may commit a `.pixel/` whose files block a read: a FIFO where the
/// task ledger keeps its observations. The read never ends, so the gate's
/// decision window is what answers, inside the host's time.
#[cfg(unix)]
#[test]
fn task_event_should_answer_in_time_when_a_clone_plants_a_fifo_in_pixel_state() {
    let root = indexed("fifo");
    let fifo = root.join(".pixel/task-hook-observations.json");
    let _ = std::fs::remove_file(&fifo);
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let payload = json!({"session_id": "fifo", "prompt": RENAME, "cwd": &*root});
    let args = [
        "run-hook",
        "task-event",
        "--provider",
        "claude",
        "--event",
        "prompt-submit",
    ];
    let hook = run(
        &root,
        &args,
        payload.to_string().as_bytes(),
        Duration::ZERO,
        &[],
    );
    assert_hook_contract(&hook, "a FIFO in .pixel");
}

/// A usage error under `run-hook` (a newer install's flag given to this
/// binary, or a retired verb) exits 0 with nothing on stdout and says so on
/// stderr; any other command keeps clap's exit code.
#[test]
fn run_hook_argument_errors_should_exit_0_and_print_nothing() {
    let root = fixture("arguments");
    for args in [
        &["run-hook"][..],
        &["run-hook", "retired-verb"],
        &["run-hook", "task-event"],
        &[
            "run-hook",
            "task-event",
            "--provider",
            "newer-host",
            "--event",
            "stop",
        ],
        &[
            "run-hook",
            "task-event",
            "--provider",
            "claude",
            "--event",
            "newer-event",
        ],
        &[
            "run-hook",
            "task-event",
            "--provider",
            "claude",
            "--event",
            "prompt-submit",
            "--part",
            "9/3",
        ],
        &[
            "run-hook",
            "task-event",
            "--provider",
            "claude",
            "--event",
            "stop",
            "--newer-flag",
        ],
        &["run-hook", "metrics", "--provider", "newer-host"],
        // A global flag before the verb (#1103): `run-hook` is no longer
        // `argv[1]`, and the hook exited 2 before.
        &["--metrics=off", "run-hook", "--bogus"],
        &["--metrics", "off", "run-hook", "task-event"],
        &["-v", "run-hook", "task-event"],
    ] {
        let hook = run(&root, args, b"{}", Duration::ZERO, &[]);
        assert_eq!(hook.code, Some(0), "{args:?}: {}", hook.stderr);
        assert_eq!(
            String::from_utf8_lossy(&hook.stdout),
            "",
            "{args:?}: stdout"
        );
        assert!(
            hook.stderr.contains("error:") || hook.stderr.contains("Usage: pixel run-hook"),
            "{args:?}: the error is said on stderr: {:?}",
            hook.stderr
        );
    }
    for args in [
        &["status", "--newer-flag"][..],
        &["--metrics", "off", "status", "--newer-flag", "run-hook"],
    ] {
        let status = run(&root, args, b"", Duration::ZERO, &[]);
        assert_eq!(status.code, Some(2), "{args:?}: {}", status.stderr);
    }
}

/// A panic in a hook's own work (#1096) exits 0 instead of unwinding to 101:
/// before the response there is nothing on stdout, after it the one response
/// stays alone, and the panic is said on stderr. The debug-only
/// `PIXEL_HOOK_TEST_PANIC` (`hook_input::TEST_PANIC_ENV`) picks the stage.
#[test]
fn a_hook_that_panics_should_still_exit_0_with_one_json_line_or_nothing() {
    let root = fixture("panic");
    let payload = json!({
        "session_id": "panic", "prompt": RENAME, "cwd": &*root,
        "hook_event_name": "UserPromptSubmit",
    })
    .to_string();
    let task_event = [
        "run-hook",
        "task-event",
        "--provider",
        "claude",
        "--event",
        "prompt-submit",
    ];
    for (args, stage, answers) in [
        (&task_event[..], "before-response", false),
        (&task_event[..], "after-response", true),
        (
            &["run-hook", "metrics", "--event", "post-tool-use"][..],
            "before-response",
            false,
        ),
    ] {
        let what = format!("{} at {stage}", args.join(" "));
        let hook = run(
            &root,
            args,
            payload.as_bytes(),
            Duration::ZERO,
            &[("PIXEL_HOOK_TEST_PANIC", stage)],
        );
        let answer = assert_hook_contract(&hook, &what);
        assert_eq!(
            answer.is_some(),
            answers,
            "{what}: stdout {:?}",
            hook.stdout
        );
        assert!(
            hook.stderr
                .contains(&format!("PIXEL_HOOK_TEST_PANIC={stage}")),
            "{what}: the panic is said on stderr: {:?}",
            hook.stderr
        );
    }
}

/// A host that writes its payload and never closes stdin gets the same
/// answer as one that closes it, in about a second instead of at its own
/// timeout; one that writes nothing gets the unavailable envelope, and the
/// metrics relay nothing, in the same time.
#[test]
fn a_hook_whose_stdin_is_left_open_should_answer_in_time() {
    let root = indexed("held");
    let prompt = |session: &str| {
        json!({
            "session_id": session, "prompt": RENAME, "cwd": &*root,
            "hook_event_name": "UserPromptSubmit",
        })
        .to_string()
    };
    let args = [
        "run-hook",
        "task-event",
        "--provider",
        "claude",
        "--event",
        "prompt-submit",
    ];
    let quiet = [("PIXEL_METRICS", "0")];
    let held_for = HOOK_TIMEOUT + Duration::from_secs(2);
    let closed = run(
        &root,
        &args,
        prompt("closed").as_bytes(),
        Duration::ZERO,
        &quiet,
    );
    let held = run(&root, &args, prompt("held").as_bytes(), held_for, &quiet);
    let closed = assert_hook_contract(&closed, "closed stdin").unwrap();
    let held_answer = assert_hook_contract(&held, "stdin left open")
        .unwrap_or_else(|| panic!("stdin left open: no answer; stderr {}", held.stderr));
    assert!(
        held.took < Duration::from_secs(5),
        "stdin left open: answered after {:?}",
        held.took
    );
    assert!(
        context(&closed).is_some_and(|text| text.starts_with("[PIXEL:BRIEF]")),
        "{closed:?}"
    );
    assert_eq!(context(&held_answer), context(&closed));

    let stop = [
        "run-hook",
        "task-event",
        "--provider",
        "claude",
        "--event",
        "stop",
    ];
    let silent = run(&root, &stop, b"", held_for, &quiet);
    let answer = assert_hook_contract(&silent, "nothing written, stdin left open");
    assert!(silent.took < Duration::from_secs(5), "{:?}", silent.took);
    assert!(answer.is_some(), "task-event has no silent path");

    let metrics = ["run-hook", "metrics", "--provider", "claude"];
    let relay = run(&root, &metrics, b"", held_for, &quiet);
    assert_eq!(
        assert_hook_contract(&relay, "metrics, stdin left open"),
        None
    );
    assert!(relay.took < Duration::from_secs(5), "{:?}", relay.took);
}

/// `pixel brief` is the prompt hook of the OpenCode plugin, which injects its
/// stdout whole: stdout is the brief or nothing, never the metrics
/// line (on by default) or a note; `--json` is one object on one line.
#[test]
fn brief_stdout_should_be_the_brief_or_nothing() {
    let root = indexed("brief");
    let unindexed = fixture("brief-unindexed");
    for parts in ["1", "2", "3"] {
        let briefed = run(
            &root,
            &["brief", RENAME, ".", "--parts", parts],
            b"",
            Duration::ZERO,
            &[],
        );
        assert_eq!(briefed.code, Some(0), "{}", briefed.stderr);
        let text = String::from_utf8(briefed.stdout).unwrap();
        assert!(
            text.starts_with("[PIXEL:BRIEF]\n"),
            "--parts {parts}: {text:?}"
        );
        assert!(!text.contains('🟩'), "--parts {parts}: {text:?}");
        assert!(
            briefed.stderr.contains("🟩 pixel brief"),
            "the metrics line is on stderr: {:?}",
            briefed.stderr
        );
    }
    for (dir, prompt) in [
        (&root, "thanks, that is all for today"),
        (&unindexed, RENAME),
    ] {
        let none = run(dir, &["brief", prompt, "."], b"", Duration::ZERO, &[]);
        assert_eq!(none.code, Some(0), "{prompt}: {}", none.stderr);
        assert_eq!(String::from_utf8_lossy(&none.stdout), "", "{prompt}");
    }
    for (dir, prompt, path, code) in [
        (&root, RENAME, ".", Some(0)),
        (&root, "thanks, that is all for today", ".", Some(0)),
        (&unindexed, RENAME, ".", Some(0)),
        (&root, RENAME, "/nonexistent/pixel", Some(1)),
    ] {
        let json = run(
            dir,
            &["brief", "--json", prompt, path],
            b"",
            Duration::ZERO,
            &[],
        );
        assert_eq!(json.code, code, "{prompt} {path}: {}", json.stderr);
        assert!(
            one_json_line_or_nothing(&json.stdout)
                .unwrap_or_else(|err| panic!("{prompt} {path}: {err}"))
                .is_some(),
            "{prompt} {path}: --json always answers"
        );
    }
}
