#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT


"""Score eval transcripts against scenario rubrics and held-out verifiers.

Claude arms: stream-json (last `result` event carries the answer + metrics).
Codex arms: `exec --json` JSONL (agent_message items, turn.completed usage).
agy arms: stream-json (final `result` event with `response`).
pi arms: the adapter's {status, response} envelope.

Score = sum(must pattern hits) - sum(never penalties), floored at 0.
`answered` is False when the run ended in error (e.g. error_max_turns) —
an unanswered run scores 0 regardless of partial text.

Per row, beside the score: `quality` (an answer task's score/max, an edit
task's held-out verifier verdict as 1/0), wall time and the run identity
from run.sh's `<stem>.run.json`, token usage, and tool-use metrics read
from the transcript — pixel calls, native searches (grep/rg/Grep/Glob) and
the line width of each read that follows a pixel `path:line` hit. A metric
the transcript does not carry is null (unknown), never 0.

Self-contained on purpose: controlled.ts copies this one file to score its
trials.
"""
import argparse, hashlib, json, re, shlex, statistics
from pathlib import Path

UNKNOWN_USAGE = {"input_tokens": None, "gen_tokens": None, "cache_read_tokens": None,
                 "cache_creation_tokens": None, "cost_usd": None}


# --- tool-use metrics ---------------------------------------------------------
SEARCH_PROGRAMS = {"grep", "rg", "egrep", "fgrep", "ag", "ack"}
WRAPPERS = {"rtk", "command", "env", "nice", "time", "timeout", "gtimeout", "xargs", "sudo"}
HIT = re.compile(r"(?<![\w./-])((?:\.{0,2}/)?[\w.-]+(?:/[\w.-]+)*\.[A-Za-z0-9]+):(\d+)")


def unwrap_shell(command: str) -> str:
    """`bash -lc '<cmd>'` (codex) and friends -> `<cmd>`."""
    try:
        argv = shlex.split(command)
    except ValueError:
        return command
    if len(argv) >= 3 and Path(argv[0]).name in ("bash", "sh", "zsh") and argv[1] in ("-c", "-lc", "-ic"):
        return argv[2]
    return command


def segments(command: str) -> list[list[str]]:
    """Each `;`/`&&`/`||`/newline-separated command's first pipeline stage, as argv."""
    out = []
    for part in re.split(r"\n|;|&&|\|\|", unwrap_shell(command)):
        stages = part.split("|")
        # The first stage, plus any later `xargs grep ...` (find | xargs grep).
        chosen = [stages[0]] + [s for s in stages[1:] if s.strip().startswith("xargs")]
        for first in chosen:
            argv = stage_argv(first)
            if argv:
                out.append(argv)
    return out


def stage_argv(stage: str) -> list[str]:
    """One pipeline stage's argv, past env assignments and wrappers (rtk, timeout 30, xargs)."""
    stage = stage.strip().lstrip("({").strip()
    if not stage:
        return []
    try:
        argv = shlex.split(stage)
    except ValueError:
        argv = stage.split()
    while argv and (re.match(r"^\w+=", argv[0]) or Path(argv[0]).name in WRAPPERS
                    or re.fullmatch(r"-\S+|\d+[smh]?", argv[0])):
        argv = argv[1:]
    return argv


def classify_command(command: str) -> set[str]:
    kinds = set()
    for argv in segments(command):
        program = Path(argv[0]).name
        if program in ("pixel", "pixel-dev") and len(argv) > 1 and argv[1] != "run-hook":
            kinds.add("pixel")
        elif program in SEARCH_PROGRAMS or (program == "git" and "grep" in argv[1:3]):
            kinds.add("search")
        elif program == "find" and any(a in ("-name", "-iname", "-path", "-regex") for a in argv):
            kinds.add("find")
    return kinds


def read_width(command: str):
    """(path, width) for a shell read of one file; width None = whole file."""
    for argv in segments(command):
        program = Path(argv[0]).name
        rest = [a for a in argv[1:]]
        if program == "sed" and "-n" in rest:
            spec = next((a for a in rest if re.fullmatch(r"\d+,\d+p", a)), None)
            files = [a for a in rest if not a.startswith("-") and a != spec]
            if spec:
                a, b = map(int, spec[:-1].split(","))
                return (files[-1] if files else None), b - a + 1
        if program == "nl":
            stage = unwrap_shell(command)
            m = re.search(r"sed -n ['\"]?(\d+),(\d+)p", stage)
            files = [a for a in rest if not a.startswith("-")]
            if files:
                return files[-1], (int(m.group(2)) - int(m.group(1)) + 1) if m else None
        if program in ("head", "tail"):
            n = None
            for i, a in enumerate(rest):
                if a == "-n" and i + 1 < len(rest) and rest[i + 1].lstrip("+").isdigit():
                    n = int(rest[i + 1].lstrip("+"))
                elif re.fullmatch(r"-n?\d+", a):
                    n = int(re.sub(r"\D", "", a))
            files = [a for a in rest if not a.startswith("-") and not a.lstrip("+").isdigit()]
            if files:
                return files[-1], n if n is not None else 10
        if program in ("cat", "bat", "less") and rest:
            files = [a for a in rest if not a.startswith("-")]
            if len(files) == 1:
                return files[0], None
    return None


def norm(path: str) -> str:
    return path[2:] if path.startswith("./") else path


def hit_paths(output: str) -> set[str]:
    return {norm(h) for h, _ in HIT.findall(output or "")}


def matches_hit(path: str, hits: set[str]) -> bool:
    path = norm(path)
    return any(path == h or path.endswith("/" + h) for h in hits)


def tool_metrics(calls: list[dict]) -> dict:
    """calls: [{"kind": "bash"|"read"|"grep"|"glob"|"edit"|"other", "command", "path",
    "limit", "output"}] in transcript order."""
    pixel = search = find = 0
    hits: set[str] = set()
    widths = []
    for call in calls:
        kind = call["kind"]
        if kind in ("grep", "glob"):
            search += 1
            continue
        if kind == "pixel-tool":
            pixel += 1
            hits |= hit_paths(call.get("output"))
            continue
        if kind == "read":
            if hits and call.get("path") and matches_hit(call["path"], hits):
                widths.append(call.get("limit"))
            continue
        if kind != "bash":
            continue
        command = call.get("command") or ""
        kinds = classify_command(command)
        if "pixel" in kinds:
            pixel += 1
            hits |= hit_paths(call.get("output"))
        if "search" in kinds:
            search += 1
        if "find" in kinds:
            find += 1
        if "pixel" not in kinds and hits:
            read = read_width(command)
            if read and read[0] and matches_hit(read[0], hits):
                widths.append(read[1])
    bounded = [w for w in widths if w is not None]
    return {
        "tool_calls": len(calls),
        "pixel_calls": pixel,
        "native_search_calls": search,
        "find_calls": find,
        "pixel_hit_reads": len(widths),
        "pixel_hit_read_widths": widths,
        "pixel_hit_full_reads": sum(1 for w in widths if w is None),
        "pixel_hit_read_median_lines": statistics.median(bounded) if bounded else None,
    }


def text_of(content) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(text_of(c.get("text") if isinstance(c, dict) and "text" in c
                                 else c.get("content") if isinstance(c, dict) else c)
                         for c in content if c is not None)
    return "" if content is None else str(content)


def claude_calls(events: list[dict]) -> list[dict]:
    calls, by_id, results = [], {}, {}
    for ev in events:
        message = ev.get("message") or {}
        content = message.get("content") if isinstance(message.get("content"), list) else []
        if ev.get("type") == "assistant":
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    name, args = block.get("name") or "", block.get("input") or {}
                    call = {"id": block.get("id"), "name": name}
                    if name == "Bash":
                        call.update(kind="bash", command=args.get("command"))
                    elif name == "Read":
                        call.update(kind="read", path=args.get("file_path"), limit=args.get("limit"))
                    elif name in ("Grep", "Glob"):
                        call.update(kind=name.lower())
                    elif "pixel" in name.lower():
                        call.update(kind="pixel-tool")
                    elif name in ("Edit", "Write", "MultiEdit", "NotebookEdit"):
                        call.update(kind="edit")
                    else:
                        call.update(kind="other")
                    calls.append(call)
                    by_id[call["id"]] = call
        elif ev.get("type") == "user":
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_result":
                    results[block.get("tool_use_id")] = text_of(block.get("content"))
    for call in calls:
        call["output"] = results.get(call["id"], "")
    return calls


def codex_calls(events: list[dict]) -> list[dict]:
    calls = []
    for ev in events:
        if ev.get("type") != "item.completed":
            continue
        item = ev.get("item") or {}
        kind = item.get("type")
        if kind == "command_execution":
            calls.append({"kind": "bash", "command": item.get("command"),
                          "output": item.get("aggregated_output") or ""})
        elif kind == "file_change":
            calls.append({"kind": "edit"})
        elif kind == "mcp_tool_call":
            name = f"{item.get('server', '')}/{item.get('tool', '')}"
            calls.append({"kind": "pixel-tool" if "pixel" in name.lower() else "other",
                          "output": text_of((item.get("result") or {}).get("content"))})
        elif kind in ("web_search",):
            calls.append({"kind": "other"})
    return calls


# --- per-host transcript parsing ----------------------------------------------
def events_of(path: Path) -> list[dict]:
    events = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(ev, dict):
            events.append(ev)
    return events


def load_result(path: Path, cli: str):
    """Return (answer, metrics) for a transcript, parsed per CLI."""
    controlled = path.with_suffix(".controlled.json")
    if controlled.exists():
        r = json.loads(controlled.read_text())
        if (r.get("schema_version") != 1 or r.get("cli") != cli
                or r.get("transcript_sha256") != hashlib.sha256(path.read_bytes()).hexdigest()):
            return "", {"answered": False, "turns": None, "input_tokens": None,
                        "cost_usd": None, "coverage": "partial"}
        # Backend captures termination and heldout checks outside model context.
        keys = ("answered", "turns", "input_tokens", "gen_tokens", "cost_usd", "rep",
                "comparison_key", "coverage", "model_tool_requests", "duration_ms",
                "verifier_success", "termination", "observed_model_tool_requests",
                "blocked_requests", "retried_requests", "coordinator_calls", "classifier_calls")
        metrics = {key: r.get(key) for key in keys}
        metrics["answered"] = (r.get("answered") is True and r.get("verifier_success") is True
                               and r.get("termination") == "exited" and r.get("exit_code") == 0)
        return r.get("answer") or "", metrics
    answer, metrics = "", {}
    if cli == "codex":
        events = events_of(path)
        texts, turns, usage_seen, turn_failed, stream_error = [], 0, False, False, False
        totals = {"input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0}
        for ev in events:
            if ev.get("type") == "item.completed":
                item = ev.get("item") or {}
                if item.get("type") == "agent_message" and item.get("text"):
                    texts.append(item["text"])
            elif ev.get("type") == "turn.completed":
                turns += 1
                # A completed turn recovers from an earlier transient
                # `error` event (a stream reconnect), not from turn.failed.
                stream_error = False
                usage = ev.get("usage")
                if isinstance(usage, dict):
                    usage_seen = True
                    for key in totals:
                        totals[key] += usage.get(key) or 0
            elif ev.get("type") == "turn.failed":
                turn_failed = True
            elif ev.get("type") == "error":
                stream_error = True
        answer = "\n\n".join(texts)
        # codex emits the message and the turn completion as separate events:
        # an answer without a completed turn is an interrupted trial.
        metrics = {"answered": bool(answer.strip()) and turns >= 1 and not turn_failed
                               and not stream_error,
                   "turns": turns or None,
                   "input_tokens": totals["input_tokens"] if usage_seen else None,
                   "gen_tokens": totals["output_tokens"] if usage_seen else None,
                   # codex counts cached tokens inside input_tokens
                   "cache_read_tokens": totals["cached_input_tokens"] if usage_seen else None,
                   "cache_creation_tokens": None, "cost_usd": None,
                   "transcript_model": None,
                   **tool_metrics(codex_calls(events))}
        metrics["total_input_tokens"] = metrics["input_tokens"]
        return answer, metrics
    if cli == "pi":
        try:
            r = json.loads(path.read_text())
        except json.JSONDecodeError:
            return "", {"answered": False, "turns": None, **UNKNOWN_USAGE}
        return r.get("response") or "", {
            "answered": r.get("status") == "SUCCESS" and bool((r.get("response") or "").strip()),
            "turns": r.get("num_turns"), **UNKNOWN_USAGE}
    events = events_of(path)
    model = None
    for ev in events:
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            model = ev.get("model")
        if ev.get("type") == "result":          # claude
            usage = ev.get("usage") if isinstance(ev.get("usage"), dict) else None
            answer = ev.get("result") or ""
            metrics = {
                "answered": ev.get("subtype") == "success",
                "turns": ev.get("num_turns"),
                "input_tokens": usage.get("input_tokens") if usage else None,
                "gen_tokens": usage.get("output_tokens") if usage else None,
                "cache_read_tokens": usage.get("cache_read_input_tokens") if usage else None,
                "cache_creation_tokens": usage.get("cache_creation_input_tokens") if usage else None,
                "cost_usd": ev.get("total_cost_usd"),
                "host_duration_ms": ev.get("duration_ms"),
            }
        elif "result" in ev and isinstance(ev["result"], dict):  # agy
            r = ev["result"]
            answer = r.get("response") or ""
            metrics = {
                "answered": r.get("status") == "SUCCESS",
                "turns": r.get("num_turns"),
                "input_tokens": (r.get("usage") or {}).get("input_tokens"),
                "cost_usd": None,
            }
    if cli == "claude" and not metrics:
        # Interrupted: no result event. Still count its tool calls, as the
        # codex path does, so adoption numbers keep the run.
        metrics = {"answered": False, "turns": None, **UNKNOWN_USAGE}
    if metrics and cli == "claude":
        parts = [metrics.get(k) for k in ("input_tokens", "cache_read_tokens", "cache_creation_tokens")]
        metrics["total_input_tokens"] = sum(parts) if all(p is not None for p in parts) else None
        metrics["transcript_model"] = model
        metrics.update(tool_metrics(claude_calls(events)))
    return answer, metrics


def score_answer(answer: str, rubric: dict):
    earned, hits, penalties = 0, [], 0
    for m in rubric.get("must", []):
        if re.search(m["pattern"], answer, re.IGNORECASE):
            earned += m["points"]
            hits.append(m["pattern"])
    for n in rubric.get("never", []):
        if re.search(n["pattern"], answer, re.IGNORECASE):
            penalties += n.get("penalty", 1)
    return max(0, earned - penalties), hits, penalties


def sidecar(path: Path, suffix: str):
    side = path.with_name(path.name[: -len(".jsonl")] + suffix)
    return json.loads(side.read_text()) if side.exists() else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True)
    ap.add_argument("--scenarios-dir", required=True)
    ap.add_argument("arms", nargs="*")
    args = ap.parse_args()
    results = Path(args.results)
    rubrics = {p.stem: json.loads(p.read_text())
               for p in Path(args.scenarios_dir).glob("*.json")}
    rows = []
    for f in sorted(results.rglob("*.jsonl")):
        rel = f.relative_to(results)
        if "stale" in rel.parts[:-1] or f.name.endswith(".actions.jsonl"):
            continue
        stem = f.stem                      # <scenario>-<arm>.<cli>
        # Longest id first: `ab-x` must not claim `ab-x-y-baseline.claude`.
        scenario = next((k for k in sorted(rubrics, key=len, reverse=True)
                         if stem.startswith(k + "-")), None)
        if scenario is None:
            continue
        arm, cli = stem[len(scenario) + 1:].rsplit(".", 1)
        if args.arms and arm not in args.arms:
            continue
        rubric = rubrics[scenario]
        answer, metrics = load_result(f, cli)
        if not metrics:
            # A transcript with no terminal result (interrupted run) is a
            # failed trial: score it zero instead of dropping it.
            metrics = {"answered": False, "turns": None, **UNKNOWN_USAGE}
        if not metrics["answered"]:
            score = 0
        else:
            score, hits, penalties = score_answer(answer, rubric)
            metrics["penalties"] = penalties
        maximum = sum(m["points"] for m in rubric.get("must", []))
        rep_dir = next((p for p in rel.parts[:-1] if re.fullmatch(r"rep-\d+", p)), None)
        run = sidecar(f, ".run.json") or {}
        verify = sidecar(f, ".verify.json")
        mode = rubric.get("mode", "answer")
        if mode == "edit":
            verifier_passed = verify.get("passed") if verify else None
            quality = None if verifier_passed is None else (1.0 if verifier_passed else 0.0)
        else:
            verifier_passed = None
            quality = (score / maximum) if maximum else None
        row = {"scenario": scenario, "arm": arm, "cli": cli, "score": score, "max": maximum,
               **metrics,
               "task_class": rubric.get("task_class"), "mode": mode,
               "quality": quality, "verifier_passed": verifier_passed,
               "verifier_wall_ms": verify.get("wall_ms") if verify else None}
        if "rep" not in metrics or metrics.get("rep") is None:
            row["rep"] = run.get("rep", int(rep_dir[4:]) if rep_dir else None)
        row["wall_ms"] = run.get("wall_ms", metrics.get("duration_ms"))
        for key in ("position", "model", "cli_version", "commit", "pixel_version", "exit_code",
                    "timed_out", "files_changed", "payload"):
            if key in run:
                row[key] = run[key]
        rows.append(row)
    rows.sort(key=lambda r: (r["scenario"], r["arm"], r["cli"], r.get("rep") or 0))
    # table
    print(f"{'scenario':<26} {'arm':<9} {'cli':<6} {'rep':>3} {'score':>6} {'q':>4} {'ans':>5} "
          f"{'turns':>5} {'tools':>5} {'pixel':>5} {'grep':>4} {'in_tok':>8} {'wall_s':>6} {'cost':>6}")
    for r in rows:
        q = r.get("quality")
        wall = r.get("wall_ms")
        print(f"{r['scenario'][:26]:<26} {r['arm']:<9} {r['cli']:<6} {str(r.get('rep') or '-'):>3} "
              f"{r['score']:>3}/{r['max']:<2} {('%.2f' % q) if q is not None else '-':>4} "
              f"{str(r['answered']):>5} {str(r['turns']):>5} {str(r.get('tool_calls', '-')):>5} "
              f"{str(r.get('pixel_calls', '-')):>5} {str(r.get('native_search_calls', '-')):>4} "
              f"{str(r.get('input_tokens')):>8} {('%.0f' % (wall / 1000)) if wall is not None else '-':>6} "
              f"{('%.2f' % r['cost_usd']) if r.get('cost_usd') is not None else '-':>6}")
    # per-arm means
    print("\narm means over scenarios:")
    by_arm = {}
    for r in rows:
        by_arm.setdefault((r["cli"], r["arm"]), []).append(r)
    for (cli, arm), rs in sorted(by_arm.items()):
        mean = sum(r["score"] for r in rs) / len(rs)
        turns = sum(r["turns"] for r in rs) if all(r["turns"] is not None for r in rs) else None
        answered = sum(1 for r in rs if r["answered"])
        print(f"  {cli:<6} {arm:<10} mean={mean:5.1f}  answered={answered}/{len(rs)}  total_turns={turns}")
    (results / "scores.json").write_text(json.dumps(rows, indent=2))

if __name__ == "__main__":
    main()
