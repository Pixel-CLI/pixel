# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Exercise arena.sh's receipt protocol and abort cleanup without an agent.

No model, no Herdr, no Pixel binary: a fixture Codex rollout feeds
`arena.sh receipt`, and a fake `herdr` (with `pgrep`, `pkill`, `lsof` and
`sleep` stubbed) feeds `arena.sh abort`. Needs bash and jq, as arena.sh does.
Run: python3 .agents/skills/arena/offline-protocol-test.py
"""

import json
import os
import subprocess
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ARENA = HERE / "arena.sh"


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def stub(path: Path, body: str) -> None:
    path.write_text(f"#!/bin/sh\n{body}", encoding="utf-8")
    path.chmod(0o700)


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="arena-offline-") as temp:
        scratch = Path(temp)
        env = dict(os.environ, ARENA_SCRATCH=str(scratch))

        # The hook payload the pixel arm's prompt-submit hook receives.
        payload = subprocess.run(
            ["bash", "-c", 'source "$1" help >/dev/null; hook_payload "$2" "$3"', "arena-offline",
             str(ARENA), "/tmp/arena-fixture-ws", "fixture prompt"],
            env=env, text=True, capture_output=True, check=True,
        ).stdout
        assert json.loads(payload.splitlines()[-1]) == {
            "hook_event_name": "UserPromptSubmit", "session_id": "arena",
            "cwd": "/tmp/arena-fixture-ws", "prompt": "fixture prompt",
        }, payload

        # A receipt rebuilt from a fixture rollout: brief, calls, tokens, time.
        run = scratch / "runs" / "fixture"
        pixel_home = run / "pixel" / "codex-home"
        no_pixel_home = run / "no-pixel" / "codex-home"
        for home in (pixel_home, no_pixel_home):
            (home / "sessions").mkdir(parents=True)
        rollout = pixel_home / "sessions" / "rollout-fixture.jsonl"
        entries = [
            {"type": "event_msg", "timestamp": "2026-10-24T10:00:00Z", "payload": {"type": "task_started"}},
            {"type": "response_item", "payload": {"type": "message", "content": [{"text": "[PIXEL:BRIEF] fixture brief"}]}},
            {"type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "cmd: \"rg target\""}},
            {"type": "response_item", "payload": {"type": "local_shell_call", "action": {"command": ["cat", "file.rs"]}}},
            {"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"text": "Fixture answer."}]}},
            {"type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": 11, "output_tokens": 7}}}},
            {"type": "event_msg", "timestamp": "2026-10-24T10:00:09Z", "payload": {"type": "task_complete"}},
        ]
        rollout.write_text("".join(json.dumps(item) + "\n" for item in entries), encoding="utf-8")
        write_json(run / "run.json", {
            "run": "fixture", "case": 1, "name": "lookup", "prompt": "fixture prompt",
            "commit": "a" * 40, "pixel_binary_sha256": "b" * 64,
            "pixel_home": str(pixel_home), "no_pixel_home": str(no_pixel_home),
        })
        result = subprocess.run([str(ARENA), "receipt", "fixture"], env=env, text=True, capture_output=True, check=True)
        receipt = json.loads((run / "receipt.json").read_text(encoding="utf-8"))
        pixel = receipt["arms"]["pixel"]
        assert pixel["brief"] == "[PIXEL:BRIEF] fixture brief", pixel["brief"]
        assert [call["call"] for call in pixel["tool_calls"]] == ["rg target", "cat file.rs"], pixel["tool_calls"]
        assert pixel["tokens"]["input_tokens"] == 11 and pixel["tokens"]["output_tokens"] == 7, pixel["tokens"]
        assert pixel["codex_seconds"] == 9, pixel["codex_seconds"]
        assert pixel["final_answer"] == "Fixture answer."
        assert receipt["isolation"]["pixel_arm_received_brief"] is True
        assert receipt["isolation"]["no_pixel_arm_received_brief"] is False
        assert receipt["isolation"]["ok"] is True
        assert receipt["arms"]["no-pixel"]["tool_calls"] == []
        assert "tool calls        2 | 0" in result.stdout + result.stderr, result.stdout + result.stderr
        assert (run / "brief.md").read_text(encoding="utf-8").strip() == "[PIXEL:BRIEF] fixture brief"

        # abort runs against a fake herdr; nothing real is touched.
        outside_run = scratch / "outside-case-1"
        outside_auth = outside_run / "pixel" / "codex-home" / "auth.json"
        outside_auth.parent.mkdir(parents=True)
        outside_auth.write_text("keep", encoding="utf-8")
        fake_bin = scratch / "bin"
        fake_bin.mkdir()
        herdr_log = scratch / "herdr.log"
        stub(fake_bin / "herdr", 'printf \'%s\\n\' "$*" >>"$ARENA_HERDR_LOG"\nprintf \'%s\\n\' \'{"result":{"panes":[]}}\'\n')
        for tool in ("pgrep", "pkill", "lsof", "sleep"):
            stub(fake_bin / tool, "exit 1\n")
        abort_env = dict(env, PATH=f"{fake_bin}:{os.environ['PATH']}", ARENA_HERDR_LOG=str(herdr_log))

        # A path-shaped or unknown case is refused before any Herdr call or delete.
        for bad in ("../outside-case", "nonesuch"):
            refused = subprocess.run([str(ARENA), "abort", bad], env=abort_env, text=True, capture_output=True)
            assert refused.returncode != 0, bad
            assert not herdr_log.exists(), bad
            assert outside_auth.read_text(encoding="utf-8") == "keep", bad

        # A catalog case aborts its own run: the copied logins go, the rest stays.
        owned = scratch / "runs" / "lookup-20261024T100000Z"
        copies = [owned / arm / "codex-home" / "auth.json" for arm in ("pixel", "no-pixel")]
        for copy in copies:
            copy.parent.mkdir(parents=True)
            copy.write_text("owned copy", encoding="utf-8")
        write_json(owned / "run.json", {"name": "lookup", "pixel_pane": "p1", "no_pixel_pane": "p2"})
        aborted = subprocess.run([str(ARENA), "abort", "lookup"], env=abort_env, text=True, capture_output=True, check=True)
        assert "abort complete" in aborted.stderr, aborted.stderr
        assert not any(copy.exists() for copy in copies)
        assert (owned / "run.json").exists() and outside_auth.read_text(encoding="utf-8") == "keep"
        assert "pane list" in herdr_log.read_text(encoding="utf-8")

        # An unknown command is a usage error.
        unknown = subprocess.run([str(ARENA), "bogus"], env=env, text=True, capture_output=True)
        assert unknown.returncode == 2, unknown.returncode

    print("Arena offline protocol checks passed: hook payload, receipt, abort refusal (2), abort cleanup, usage error.")


if __name__ == "__main__":
    main()
