#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Real CLI contracts in disposable homes/repos; no host daemons or network models."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def audit(binary):
    outcomes = []
    with tempfile.TemporaryDirectory(prefix="pixel-recall-audit-") as directory:
        base = Path(directory)
        home, repo, fake = (base / name for name in ("home", "repo", "fake"))
        for path in (home, repo, fake):
            path.mkdir()
        # Upgrade's process-control boundary must never reach real pkill.
        (fake / "pkill").write_text("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AUDIT_PKILL_LOG\"\nexit 0\n")
        (fake / "pkill").chmod(0o755)
        bad_model = base / "invalid-local-model"
        bad_model.mkdir()
        env = {
            **os.environ, "HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_CACHE_HOME": str(home / ".cache"), "XDG_DATA_HOME": str(home / ".local/share"),
            "CODEX_HOME": str(home / ".codex"), "CLAUDE_CONFIG_DIR": str(home / ".claude"),
            "GIT_CONFIG_GLOBAL": str(home / ".gitconfig"), "GIT_CONFIG_NOSYSTEM": "1",
            "PIXEL_METRICS": "0", "PIXEL_RECALL_MODEL_REPO": str(bad_model),
            "PIXEL_RECALL_MODEL": "potion", "HF_HUB_OFFLINE": "1",
            "AUDIT_PKILL_LOG": str(base / "pkill.log"),
            "PATH": str(fake) + os.pathsep + os.environ["PATH"],
        }
        for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GITPIXEL_RECALL_DIR"):
            env.pop(key, None)

        def run(args, expected=0, contains=None):
            result = subprocess.run([str(binary), *args], cwd=repo, env=env,
                                    text=True, capture_output=True, timeout=30)
            output = result.stdout + result.stderr
            assert (result.returncode == 0) == (expected == 0), (args, result.returncode, output)
            if contains:
                assert contains in output, (args, output)
            outcomes.append({"command": args, "exit": result.returncode,
                             "outcome": "pass", "output": output[-2000:]})
            return result

        subprocess.run(["git", "init", "-q", str(repo)], check=True, env=env)
        for key, value in (("user.name", "Pixel Audit"), ("user.email", "audit@example.invalid")):
            subprocess.run(["git", "-C", str(repo), "config", key, value], check=True, env=env)
        (repo / "fixture.rs").write_text("pub fn manual_fixture() -> bool { true }\n")
        run(["commit", "-m", "test: disposable audit fixture", "--files", "fixture.rs", "--request-id", "fixture-seed"])
        run(["build-index"])
        transcript = home / ".claude/projects/audit/session-audit.jsonl"
        transcript.parent.mkdir(parents=True)
        turns = [
            {"type": "user", "cwd": str(repo), "timestamp": "2026-09-12T12:00:00Z",
             "message": {"content": "Find the quartzneedle manual setup instructions"}},
            {"type": "assistant", "timestamp": "2026-09-12T12:00:01Z",
             "message": {"content": [{"type": "text", "text": "quartzneedle manual setup preserves existing configuration"}]}},
        ]
        transcript.write_text("\n".join(json.dumps(row) for row in turns) + "\n{malformed record}\n")
        run(["recall", "index", "--source", "claude", "--stats"])
        run(["recall", "search", "quartzneedle", "--json"], contains="quartzneedle")
        run(["recall", "sessions", "--json"], contains="session-audit")
        run(["recall", "show", "claude:session-audit", "--json"], contains="quartzneedle")
        run(["recall", "ask", "quartzneedle manual", "--lexical-only", "--json"], contains="quartzneedle")
        run(["recall", "context", "quartzneedle", "--lexical-only", "--budget", "1000"], contains="quartzneedle")
        run(["recall", "maxtest", "quartzneedle,manual", "--json"], contains="quartzneedle")
        exported = base / "exported"
        run(["recall", "export", "--out", str(exported)])
        assert any("quartzneedle" in p.read_text() for p in exported.iterdir())
        run(["recall", "status", "--json"])
        run(["recall", "setup"], expected=1, contains="model")
        run(["recall", "embed"], expected=1, contains="model")
        with transcript.open("a") as stream:
            stream.write(json.dumps({**turns[0], "message": {"content": "incremental quartzappend evidence"}}) + "\n")
        run(["recall", "index", "--source", "claude", "--stats"])
        run(["recall", "search", "quartzappend", "--json"], contains="quartzappend")

        # Both daemon families are isolated by explicit repo / temporary HOME.
        for prefix, suffix in ((["daemon"], [str(repo)]), (["recall", "daemon"], [])):
            try:
                run([*prefix, "start", *suffix], contains="daemon")
                run([*prefix, "status", *suffix], contains="running")
            finally:
                run([*prefix, "stop", *suffix], contains="daemon")

        settings = home / ".claude/settings.json"
        settings.write_text(json.dumps({"unrelatedSetting": {"preserve": True}}))
        run(["install", "--json"])
        run(["install", "--json"])
        assert json.loads(settings.read_text())["unrelatedSetting"] == {"preserve": True}
        # The fixture never starts a daemon nor builds the graph and history,
        # so those checks are red by design; the install wiring must be green.
        run(["doctor", str(repo), "--json", "--skip", "daemon.health",
             "--skip", "graph.freshness", "--skip", "facts.freshness"])
        run(["uninstall", "--json", "--binary-path", str(home / "unused-binary")])
        assert json.loads(settings.read_text())["unrelatedSetting"] == {"preserve": True}
        old = repo / ".gitpixel"
        old.mkdir()
        (old / "cache").write_text("obsolete disposable state")
        preserved = repo / ".pixel/user-state.json"
        preserved.write_text('{"preserve":true}')
        migrated = json.loads(run(["migrate", str(repo), "--json"]).stdout)
        assert migrated["new_state_rebuilt"] is False
        assert migrated["new_state_directory_prepared"] is True
        assert preserved.read_text() == '{"preserve":true}'
        assert not old.exists() and (repo / ".pixel").is_dir()

        release = repo / "target/release/pixel"
        release.parent.mkdir(parents=True)
        release.write_text("#!/bin/sh\necho upgrade-fixture\n")
        release.chmod(0o755)
        installed = home / "upgrade-pixel"
        installed.write_bytes(b"previous binary")
        run(["self-update", "--build", "exit 7", "--install-path", str(installed)], expected=1)
        assert installed.read_bytes() == b"previous binary"
        run(["self-update", "--build", "true", "--install-path", str(installed)])
        assert installed.read_bytes() == release.read_bytes()
        pkill = base / "pkill.log"
        assert not pkill.exists(), "upgrade attempted global pkill: " + pkill.read_text()
    return outcomes


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    report = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "checks": audit(binary)}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"{len(report['checks'])} isolated boundary checks passed; report: {args.output}")
