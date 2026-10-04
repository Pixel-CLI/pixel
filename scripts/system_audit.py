#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Real CLI audit in disposable repositories/homes. Never uses a live remote/browser.

Build the candidate first; pass its absolute path. The report identifies that
binary and records exact argv, exit codes and assertions, not help-based coverage.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


class Audit:
    def __init__(self, pixel, root):
        self.pixel = pixel
        self.root = root
        self.repo = root / "repo"
        self.repo.mkdir()
        self.home = root / "home"
        self.home.mkdir()
        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        for name in ("claude", "agent-browser", "devin", "codex"):
            fake = fake_bin / name
            fake.write_text("#!/bin/sh\nprintf 'audit substitute: external executor unavailable\\n' >&2\nexit 127\n")
            fake.chmod(0o700)
        self.env = {
            **os.environ,
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / "config"),
            "XDG_DATA_HOME": str(self.home / "data"),
            "XDG_CACHE_HOME": str(self.home / "cache"),
            "PIXEL_DAEMON_AUTO_START": "0",
            "PIXEL_METRICS": "0",
            "PIXEL_FLOW_DIR": str(self.home / "flows"),
            "PATH": str(fake_bin) + os.pathsep + os.environ.get("PATH", ""),
            "PIXEL_TASK_BOUNDARY": "0",
            "PIXEL_TASK_CONTEXT": "1",
            "PIXEL_POST_COMPACTION": "1",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Audit fixture",
            "GIT_AUTHOR_EMAIL": "fixture@example.test",
            "GIT_COMMITTER_NAME": "Audit fixture",
            "GIT_COMMITTER_EMAIL": "fixture@example.test",
        }
        for key in ("ANTHROPIC_API_KEY", "PIXEL_RECALL_HOME", "PIXEL_HOME", "GIT_DIR", "GIT_WORK_TREE"):
            self.env.pop(key, None)
        self.results = []

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, env=self.env, text=True, stderr=subprocess.PIPE).strip()

    def call(self, leaf, args, *, contains=None, json_output=False, exit_code=0, input_text=None):
        started = time.monotonic()
        output = subprocess.run([str(self.pixel), *args], cwd=self.repo, env=self.env,
                                input=input_text, capture_output=True, text=True, timeout=30)
        error = None
        try:
            assert output.returncode == exit_code, f"expected exit {exit_code}, got {output.returncode}: {output.stderr}"
            if contains is not None:
                assert contains in output.stdout, f"missing {contains!r}: {output.stdout[:1200]}"
            if json_output == "ndjson":
                assert output.stdout.strip(), "expected at least one log record"
                for line in output.stdout.splitlines():
                    json.loads(line)
            elif json_output:
                json.loads(output.stdout)
        except (AssertionError, json.JSONDecodeError) as failure:
            error = str(failure)
        row = {"leaf": leaf, "argv": args, "exit_code": output.returncode,
               "seconds": round(time.monotonic() - started, 4),
               "assertion": {"contains": contains, "json": json_output, "exit": exit_code},
               "status": "FAIL" if error else "PASS", "error": error}
        self.results.append(row)
        print(json.dumps(row), flush=True)
        return output.stdout

    def fixture(self):
        self.git("init", "-b", "main")
        (self.repo / "lib.rs").write_text('pub fn login_user(name: &str) -> bool { !name.is_empty() }\npub fn main_entry() { login_user("fixture"); }\n')
        (self.repo / "removed.md").write_text("removed_manual_history evidence\n")
        (self.repo / ".gitignore").write_text(".pixel/\n.env\n")
        self.git("add", ".")
        self.git("commit", "-m", "fixture: original login and manual")
        self.base = self.git("rev-parse", "HEAD")
        self.git("mv", "removed.md", "manual.md")
        self.git("commit", "-m", "fixture: rename manual")
        self.git("rm", "manual.md")
        self.git("commit", "-m", "fixture: delete manual")
        self.tip = self.git("rev-parse", "HEAD")

    def retrieval(self):
        self.call("build-index", ["build-index", "--history"])
        assert (self.repo / ".pixel").is_dir(), "index did not create repository state"
        self.call("rebuild-graph", ["rebuild-graph", "--json"], contains="symbols", json_output=True)
        self.call("status", ["status", "--json"], json_output=True)
        self.call("index-stats", ["index-stats"], contains="files")
        self.call("search-content", ["search-content", "login_user", "--no-daemon"], contains="login_user")
        self.call("search-like-rg", ["search-like-rg", "grep", "--", "-n", "login_user", "lib.rs"], contains="login_user")
        self.call("run-recipe", ["run-recipe", "login_user", "--no-daemon", "--json"], contains="login_user", json_output=True)
        self.call("find-symbol", ["find-symbol", "login_user", "--json"], contains="login_user", json_output=True)
        self.call("list-signatures", ["list-signatures", "lib.rs", "--json"], contains="login_user", json_output=True)
        self.call("repo-map", ["repo-map", "--json"], contains="login_user", json_output=True)
        self.call("pack-context", ["pack-context", "lib.rs#login_user#function", "--budget", "300", "--json"], contains="login_user", json_output=True)
        self.call("scope-task", ["scope-task", "fix login_user", "--json", "--no-manifest"], contains="lib.rs", json_output=True)
        self.call("find-code", ["find-code", "login user", "--json"], contains="login_user", json_output=True)
        for leaf, args in [
            ("impact", ["impact", "login_user", "--json"]),
            ("who-calls", ["who-calls", "login_user", "--role", "callers", "--json"]),
            ("call-path", ["call-path", "main_entry", "login_user", "--json"]),
            ("list-flows", ["list-flows", "--json"]),
            ("list-areas", ["list-areas", "--json"]),
        ]:
            self.call(leaf, args, json_output=True)
        for leaf, args in [
            ("note set", ["note", "set", "lib.rs", "login_user", "audit note", "--json"]),
            ("note get", ["note", "get", "lib.rs", "login_user", "--json"]),
            ("note list", ["note", "list", "lib.rs", "--json"]),
            ("note rm", ["note", "rm", "lib.rs", "login_user", "--json"]),
        ]:
            self.call(leaf, args, json_output=True)
        with (self.repo / "lib.rs").open("a") as source:
            source.write('pub fn changed_function() {}\n')
        self.call("what-changed", ["what-changed", "--json"], contains="lib.rs", json_output=True)
        self.call("repo-state", ["repo-state", "--json"], contains="lib.rs", json_output=True)
        self.call("review-changes", ["review-changes", "--json"], contains="lib.rs", json_output=True)
        self.call("diff", ["diff", "HEAD", "--json"], contains="changed_function", json_output=True)
        self.call("commit-history", ["commit-history", "--json"], contains="fixture", json_output=True)
        self.call("search-history", ["search-history", "removed_manual_history", "--json"], contains="removed_manual_history", json_output=True)
        self.call("file-history", ["file-history", "--file", "removed.md", "--json"], contains="removed.md", json_output=True)
        self.call("dig-history", ["dig-history", "--phrase", "removed_manual_history", "--json"], contains="removed_manual_history", json_output=True)
        self.call("plan-rollback", ["plan-rollback", "login", "--file", "lib.rs", "--json"], contains="lib.rs", json_output=True)
        self.call("who-wrote", ["who-wrote", "lib.rs", "--json"], contains="fixture", json_output=True)
        self.call("list-branches", ["list-branches", "--json"], contains="main", json_output=True)
        self.call("record-event", ["record-event", "read", "--file", "lib.rs", "--detail", "audit fixture", "--json"], json_output=True)
        self.call("action-log", ["action-log", "--json"], json_output="ndjson")
        self.call("token-savings", ["token-savings", "--json"], json_output=True)

    def mutations(self):
        remote = self.root / "remote.git"
        self.git("init", "--bare", str(remote))
        self.git("remote", "add", "origin", str(remote))
        self.git("push", "-u", "origin", "main")
        self.call("new-branch", ["new-branch", "audit", "--request-id", "audit-branch", "--json"], contains="audit", json_output=True)
        self.git("switch", "audit")
        self.call("commit", ["commit", "--message", "fixture: preserve changed function", "--files", "lib.rs", "--request-id", "audit-publish", "--json"], json_output=True)
        published = self.git("rev-parse", "HEAD")
        assert published != self.tip
        self.call("commit", ["commit", "--message", "fixture: preserve changed function", "--files", "lib.rs", "--request-id", "audit-publish", "--json"], json_output=True)
        assert self.git("rev-parse", "HEAD") == published, "idempotent publish made a second commit"
        self.call("push", ["push", "origin", "HEAD:refs/heads/audit", "--request-id", "audit-push", "--json"], json_output=True)
        assert self.git("--git-dir", str(remote), "rev-parse", "refs/heads/audit") == published
        with (self.repo / "lib.rs").open("a") as source:
            source.write("pub fn shipped_function() {}\n")
        self.call("commit-and-push", ["commit-and-push", "origin", "HEAD:refs/heads/audit", "--message", "fixture: ship second function", "--files", "lib.rs", "--request-id", "audit-ship", "--json"], json_output=True)
        shipped = self.git("rev-parse", "HEAD")
        assert shipped != published
        assert self.git("--git-dir", str(remote), "rev-parse", "refs/heads/audit") == shipped
        self.call("fetch", ["fetch", "origin", "--json"], json_output=True)
        self.call("sync-branch", ["sync-branch", "--strategy", "report", "--push", "none", "--json"], json_output=True)
        self.git("switch", "-c", "audit-update", self.tip)
        self.call("fast-forward", ["fast-forward", "--expected-head", self.tip, "--target-oid", shipped, "--request-id", "audit-update", "--json"], json_output=True)
        assert self.git("rev-parse", "HEAD") == shipped
        before = (self.repo / "lib.rs").read_bytes()
        self.call("squash-branch", ["squash-branch", "--onto", self.tip, "--expected-head", shipped, "--request-id", "audit-rewrite", "--json"], json_output=True)
        assert (self.repo / "lib.rs").read_bytes() == before
        assert self.git("rev-list", "--count", f"{self.tip}..HEAD") == "1"

    def environment(self):
        env_file = self.repo / ".env"
        original = b"# preserved fixture\nUNRELATED=fixture_secret_not_for_output\nEXISTING=before\n"
        env_file.write_bytes(original)
        for leaf, args in [
            ("edit-env inventory", ["edit-env", "inventory", "--json"]),
            ("edit-env set", ["edit-env", "set", "--file", ".env", "--key", "EXISTING", "--value", "after", "--json"]),
            ("edit-env check", ["edit-env", "check", "--file", ".env", "--require", "UNRELATED", "--json"]),
            ("edit-env snapshots", ["edit-env", "snapshots", "--file", ".env", "--json"]),
        ]:
            output = self.call(leaf, args, json_output=True)
            assert "fixture_secret_not_for_output" not in output, "environment value leaked"
        assert env_file.read_bytes() == original.replace(b"EXISTING=before", b"EXISTING=after")
        self.call("edit-env restore", ["edit-env", "restore", "--file", ".env", "--json"], json_output=True)
        assert env_file.read_bytes() == original, "snapshot did not restore exact bytes"

    def sniper(self):
        error = {"surface": "reported", "message": "audit_error real boundary", "run_id": "audit-run"}
        recorded = json.loads(self.call("list-errors report", ["list-errors", "report", "--json"], json_output=True, input_text=json.dumps(error)))
        error_id = str(recorded["id"])
        for event in [
            {"type": "run", "run_id": "audit-run", "pid": 123, "port": 4321},
            {"type": "event", "kind": "hmr-update", "run_id": "audit-run", "data": {"files": ["lib.rs"]}},
            {"type": "event", "kind": "test-pass", "run_id": "audit-run"},
        ]:
            self.call("list-errors report", ["list-errors", "report", "--json"], json_output=True, input_text=json.dumps(event))
        for leaf, args, text in [
            ("list-errors last", ["list-errors", "last", "--json"], "audit_error"),
            ("list-errors since", ["list-errors", "since", "0", "--json"], "audit_error"),
            ("list-errors show", ["list-errors", "show", error_id, "--json"], "audit_error"),
            ("list-errors query", ["list-errors", "query", "audit_error", "--json"], "audit_error"),
            ("list-errors hmr", ["list-errors", "hmr", "--json"], "lib.rs"),
            ("list-errors env", ["list-errors", "env", "--json"], "audit-run"),
            ("list-errors test", ["list-errors", "test", "--json"], "test-pass"),
            ("list-errors cursor", ["list-errors", "cursor", "--json"], error_id),
            ("list-errors gc", ["list-errors", "gc", "--json"], None),
        ]:
            self.call(leaf, args, contains=text, json_output=True)
        self.call("list-errors run", ["list-errors", "run", "--", "/bin/sh", "-c", "printf audit_wrapper_failure >&2; exit 9"], exit_code=9)
        # Search indexes the error message; the captured tail is separate extra data.
        self.call("list-errors query", ["list-errors", "query", "exited 9", "--json"], contains="audit_wrapper_failure", json_output=True)

    def tasks(self):
        task = json.loads(self.call("task-state begin", ["task-state", "begin", "fix login_user boundary", "--session", "audit-session", "--provider", "codex", "--json"], json_output=True))["task_id"]
        for leaf in ["prepare", "status", "events"]:
            self.call(f"task-state {leaf}", ["task-state", leaf, task, "--json"], contains=task, json_output=True)
        self.call("task-state show", ["task-state", "show", "--session", "audit-session", "--json"], json_output=True)
        self.call("task-state reset", ["task-state", "reset", "--session", "audit-session", "--json"], json_output=True)

    def hooks(self):
        # Prior mutation fixtures rewrote HEAD; refresh history before saving a
        # current-head manifest. Post-compaction correctly refuses stale hints.
        self.call("build-index", ["build-index", "--history"])
        self.call("prepare-repo", ["prepare-repo", "--json"], json_output=True)
        self.call("scope-task", ["scope-task", "fix login_user", "--json"], contains="lib.rs", json_output=True)
        payload = {"cwd": str(self.repo), "session_id": "hook-audit", "hook_event_name": "PreToolUse", "tool_name": "shell", "tool_input": {"command": "grep -n login_user lib.rs"}}
        self.call("run-hook guard", ["run-hook", "guard", "--provider", "codex"], input_text=json.dumps(payload), contains="pixel search-like-rg", json_output=True)
        foreign = self.root / "foreign-hook.sh"
        foreign.write_text("printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"audit foreign context\"}}'\n")
        backup = self.root / "foreign-hook.json"
        backup.write_text(json.dumps({"version": 1, "provider": "codex", "pre_tool_use": [{"matcher": "shell", "hooks": [{"type": "command", "command": f'/bin/sh "{foreign}"'}]}], "managed_pre_tool_use": []}))
        backup.chmod(0o600)
        self.call("run-hook composed-guard", ["run-hook", "composed-guard", "--backup", str(backup)], input_text=json.dumps(payload), contains="audit foreign context", json_output=True)
        self.call("run-hook session-start", ["run-hook", "session-start"], input_text="{}", contains="capabilities", json_output=True)
        # This hook only queries an already-running daemon; warm the disposable one.
        try:
            self.call("run-hook prompt-submit", ["run-hook", "prompt-submit", "--provider", "codex"], input_text=json.dumps({"cwd": str(self.repo), "session_id": "hook-audit", "prompt": "Fix login_user validation", "hook_event_name": "UserPromptSubmit"}), contains="lib.rs", json_output=True)
            # A question creates a real session packet without an automatic coding handoff.
            self.call("run-hook prompt-submit", ["run-hook", "prompt-submit", "--provider", "claude"], input_text=json.dumps({"cwd": str(self.repo), "session_id": "display-session", "prompt": "How does login_user validation work?", "hook_event_name": "UserPromptSubmit"}), contains="lib.rs", json_output=True)
            self.call("task-state show", ["task-state", "show", "--session", "display-session", "--json"], contains="display-session", json_output=True)
            self.call("task-state reset", ["task-state", "reset", "--session", "display-session", "--json"], json_output=True)
            reset = self.call("task-state show", ["task-state", "show", "--session", "display-session", "--json"], json_output=True)
            assert json.loads(reset)["status"] == "absent" and json.loads(reset)["task_id"] is None, "reset retained the populated session packet"
        finally:
            self.call("daemon stop", ["daemon", "stop"])
        self.call("run-hook post-compaction", ["run-hook", "post-compaction"], input_text=json.dumps({"cwd": str(self.repo), "session_id": "hook-audit", "hook_event_name": "PostCompaction"}), contains="lib.rs", json_output=True)
        self.call("run-hook post-tool-use", ["run-hook", "post-tool-use", "--provider", "claude"], input_text=json.dumps({"cwd": str(self.repo), "tool_name": "Edit", "tool_input": {"file_path": str(self.repo / "lib.rs")}}), contains="PostToolUse", json_output=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pixel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    pixel = args.pixel.resolve(strict=True)
    report = {"candidate": str(pixel)}
    with tempfile.TemporaryDirectory(prefix="pixel-system-audit-") as temporary:
        # Freeze the executable so concurrent rebuilds cannot mix candidate versions.
        frozen = Path(temporary) / "pixel-candidate"
        shutil.copy2(pixel, frozen)
        report["sha256"] = hashlib.sha256(frozen.read_bytes()).hexdigest()
        audit = Audit(frozen, Path(temporary))
        try:
            audit.fixture()
            audit.retrieval()
            audit.mutations()
            audit.environment()
            audit.sniper()
            audit.hooks()
            audit.tasks()
        except Exception as error:
            audit.results.append({"leaf": "integration invariant", "status": "FAIL", "error": str(error)})
            raise
        finally:
            report["results"] = audit.results
            report["passed"] = sum(row["status"] == "PASS" for row in audit.results)
            report["failed"] = sum(row["status"] == "FAIL" for row in audit.results)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
    if report["failed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
