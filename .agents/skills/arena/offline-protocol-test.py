# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Exercise Arena's receipt protocol and auth-copy cleanup without an agent."""

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


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="arena-offline-") as temp:
        scratch = Path(temp)
        run = scratch / "runs" / "fixture"
        pixel_home = run / "pixel" / "codex-home"
        no_pixel_home = run / "no-pixel" / "codex-home"
        source_auth = scratch / "source-auth.json"
        source_auth.write_text('{"fixture":"source"}', encoding="utf-8")
        for home in (pixel_home, no_pixel_home):
            (home / "sessions").mkdir(parents=True)
            (home / "auth.json").write_text('{"fixture":"copy"}', encoding="utf-8")

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

        env = dict(os.environ, ARENA_SCRATCH=str(scratch))
        stub_pixel = scratch / "pixel-stub"
        stub_pixel.write_text(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >\"$ARENA_STUB_ARGS\"\ncat >\"$ARENA_STUB_PAYLOAD\"\n",
            encoding="utf-8",
        )
        stub_pixel.chmod(0o700)
        env.update(ARENA_STUB_ARGS=str(scratch / "stub-args"), ARENA_STUB_PAYLOAD=str(scratch / "stub-payload"))
        subprocess.run(
            ["bash", "-c", 'source "$1"; hook_payload "$2" "$3" | "$4" hook prompt-submit --host codex',
             "arena-offline", str(ARENA), "/tmp/arena-fixture-ws", "fixture prompt", str(stub_pixel)],
            env=env, text=True, capture_output=True, check=True,
        )
        assert (scratch / "stub-args").read_text(encoding="utf-8").splitlines() == [
            "hook", "prompt-submit", "--host", "codex",
        ]
        assert json.loads((scratch / "stub-payload").read_text(encoding="utf-8")) == {
            "hook_event_name": "UserPromptSubmit", "session_id": "arena",
            "cwd": "/tmp/arena-fixture-ws", "prompt": "fixture prompt",
        }

        result = subprocess.run(
            [str(ARENA), "receipt", "fixture"], env=env,
            text=True, capture_output=True, check=True,
        )
        receipt = json.loads((run / "receipt.json").read_text(encoding="utf-8"))
        pixel = receipt["arms"]["pixel"]
        assert pixel["brief"] == "[PIXEL:BRIEF] fixture brief"
        assert [call["call"] for call in pixel["tool_calls"]] == ["rg target", "cat file.rs"]
        assert pixel["tokens"]["input_tokens"] == 11
        assert pixel["tokens"]["output_tokens"] == 7
        assert pixel["codex_seconds"] == 9
        assert pixel["final_answer"] == "Fixture answer."
        assert receipt["isolation"]["pixel_arm_received_brief"] is True
        assert receipt["isolation"]["no_pixel_arm_received_brief"] is False
        assert "tool calls        2 | 0" in result.stdout
        assert not (pixel_home / "auth.json").exists()
        assert not (no_pixel_home / "auth.json").exists()
        assert source_auth.read_text(encoding="utf-8") == '{"fixture":"source"}'
        assert rollout.exists() and (run / "brief.md").exists()

        # A malformed run manifest cannot turn cleanup into deletion elsewhere.
        outside_auth = scratch / "outside-auth.json"
        outside_auth.write_text("keep", encoding="utf-8")
        write_json(run / "run.json", {"run": "fixture", "pixel_home": str(scratch), "no_pixel_home": ""})
        # The guard is exercised by the real command; missing transcripts yield an empty receipt.
        subprocess.run([str(ARENA), "receipt", "fixture"], env=env, text=True, capture_output=True, check=True)
        assert outside_auth.read_text(encoding="utf-8") == "keep"

        # Traversal abort input is rejected before any Herdr or process command.
        outside_run = scratch / "outside-case-1"
        outside_auth_copy = outside_run / "pixel" / "codex-home" / "auth.json"
        outside_auth_copy.parent.mkdir(parents=True)
        outside_auth_copy.write_text("keep", encoding="utf-8")
        fake_bin = scratch / "bin"
        fake_bin.mkdir()
        herdr_log = scratch / "herdr.log"
        herdr = fake_bin / "herdr"
        herdr.write_text(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$ARENA_HERDR_LOG\"\n"
            "printf '%s\\n' '{\"result\":{\"panes\":[]}}'\n",
            encoding="utf-8",
        )
        herdr.chmod(0o700)
        for tool in ("pgrep", "pkill", "lsof", "sleep"):
            stub = fake_bin / tool
            stub.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
            stub.chmod(0o700)
        abort_env = dict(env, PATH=f"{fake_bin}:{os.environ['PATH']}", ARENA_HERDR_LOG=str(herdr_log))
        traversal = subprocess.run(
            [str(ARENA), "abort", "../outside-case"], env=abort_env,
            text=True, capture_output=True,
        )
        assert traversal.returncode != 0
        assert not herdr_log.exists()
        assert outside_auth_copy.read_text(encoding="utf-8") == "keep"

        # A valid catalog case still aborts its owned run and preserves source auth.
        owned = scratch / "runs" / "lookup-20261024T100000Z"
        owned_pixel = owned / "pixel" / "codex-home"
        owned_no_pixel = owned / "no-pixel" / "codex-home"
        for home in (owned_pixel, owned_no_pixel):
            home.mkdir(parents=True)
            (home / "auth.json").write_text("owned copy", encoding="utf-8")
        write_json(owned / "run.json", {
            "name": "lookup", "pixel_home": str(owned_pixel), "no_pixel_home": str(owned_no_pixel),
        })
        valid_abort = subprocess.run(
            [str(ARENA), "abort", "lookup"], env=abort_env,
            text=True, capture_output=True, check=True,
        )
        assert "abort complete" in valid_abort.stderr
        assert not (owned_pixel / "auth.json").exists()
        assert not (owned_no_pixel / "auth.json").exists()
        assert outside_auth_copy.read_text(encoding="utf-8") == "keep"
        assert "pane list" in herdr_log.read_text(encoding="utf-8")

        # Legacy commands from the superseded protocol are rejected.
        for old_command in ("prepare-repo", "build-index", "run-hook"):
            rejected = subprocess.run([str(ARENA), old_command], env=env, text=True, capture_output=True)
            assert rejected.returncode == 2, (old_command, rejected.returncode, rejected.stderr)

    print("Arena offline protocol checks passed: receipt, cleanup ownership, abort traversal/valid case, legacy rejection (3).")


if __name__ == "__main__":
    main()
