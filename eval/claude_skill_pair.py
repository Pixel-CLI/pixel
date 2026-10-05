#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""One isolated Claude native-vs-project-skill pair with a no-model preflight."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

FLAG = re.compile(r"(?m)^disable-model-invocation:\s*(true|false)\s*$")
CONTEXT_FILES = {"AGENTS.md", "CLAUDE.md", "CLAUDE.local.md", "settings.json",
                 "settings.local.json", ".mcp.json", "plugin.json"}
MANAGED = (Path("/Library/Application Support/ClaudeCode/managed-settings.json"),
           Path("/Library/Managed Preferences/com.anthropic.claudecode.plist"),
           Path.home() / ".claude/managed-settings.json")


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def directory_digest(root: Path) -> str:
    result = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix().encode()
        result.update(relative + b"\0")
        if path.is_symlink():
            result.update(b"link\0" + os.readlink(path).encode() + b"\0")
        elif path.is_file():
            result.update(b"file\0" + path.read_bytes() + b"\0")
        elif path.is_dir():
            result.update(b"dir\0")
    return result.hexdigest()


def invocation_variant(text: str, enabled: bool) -> str:
    if len(list(FLAG.finditer(text))) != 1:
        raise ValueError("skill needs exactly one disable-model-invocation field")
    return FLAG.sub("disable-model-invocation: " + ("false" if enabled else "true"),
                    text, count=1)


def command(argv: list[str], *, cwd: Path | None = None, env=None,
            timeout: int = 120) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(argv, cwd=cwd, env=env, text=True, capture_output=True,
                              timeout=timeout, check=False)
    except subprocess.TimeoutExpired as error:
        def text(value) -> str:
            return value.decode(errors="replace") if isinstance(value, bytes) else (value or "")
        return subprocess.CompletedProcess(argv, 124, text(error.stdout), text(error.stderr))


def git(repo: Path, *args: str) -> str:
    result = command(["git", "-C", str(repo), *args])
    if result.returncode:
        raise RuntimeError("git preflight failed: " + result.stderr.strip())
    return result.stdout.strip()


def stage_archive(repo: Path, commit: str, destination: Path) -> None:
    result = subprocess.run(["git", "-C", str(repo), "archive", commit],
                            capture_output=True, check=False)
    if result.returncode:
        raise RuntimeError("could not archive pinned source commit")
    with tarfile.open(fileobj=io.BytesIO(result.stdout), mode="r:") as archive:
        archive.extractall(destination, filter="data")


def context_manifest(root: Path) -> list[dict[str, str]]:
    rows = []
    for path in sorted(root.rglob("*")):
        if path.is_file() and path.name in CONTEXT_FILES:
            if path.name != "plugin.json" or ".claude-plugin" in path.parts:
                rows.append({"path": path.relative_to(root).as_posix(),
                             "sha256": digest(path.read_bytes())})
    return rows


def verify_project_config(project: Path) -> None:
    claude = project / ".claude"
    if claude.exists():
        unexpected = [
            path.relative_to(project).as_posix()
            for path in claude.rglob("*")
            if path.is_file() and path != claude / "skills/pixel-impact/SKILL.md"
        ]
        if unexpected:
            raise RuntimeError("project Claude settings/hooks/plugins found: " + ", ".join(unexpected))
    ancestors = [
        str(parent / name) for parent in project.parents
        for name in ("AGENTS.md", "CLAUDE.md", "CLAUDE.local.md")
        if (parent / name).exists()
    ]
    if ancestors:
        raise RuntimeError("ancestor prompt files would contaminate the run: " + ", ".join(ancestors))
    for path in MANAGED:
        if path.exists():
            raise RuntimeError("managed Claude settings prevent an isolated run: " + str(path))


def version_and_help(claude: str) -> tuple[str, str]:
    version = command([claude, "--version"])
    help_text = command([claude, "--help"])
    if version.returncode or help_text.returncode:
        raise RuntimeError("Claude version/help preflight failed")
    for option in ("--setting-sources", "--permission-mode", "--permission-prompts",
                   "--disallowedTools", "--max-turns", "--max-budget-usd",
                   "--no-session-persistence", "--effort"):
        if option not in help_text.stdout:
            raise RuntimeError("installed Claude lacks required CLI option " + option)
    return version.stdout.strip().splitlines()[0], digest(help_text.stdout.encode())


def _credential_expiry_seconds(value):
    if not isinstance(value, (int, float)):
        return None
    return value / 1000 if value > 10_000_000_000 else value


def _oauth_payload(value: dict) -> dict:
    oauth = value.get("claudeAiOauth") if isinstance(value, dict) else None
    if not isinstance(oauth, dict) or not oauth.get("accessToken") or not oauth.get("refreshToken"):
        raise RuntimeError("Claude OAuth credential object is unavailable")
    refresh_expiry = _credential_expiry_seconds(oauth.get("refreshTokenExpiresAt"))
    if refresh_expiry is not None and refresh_expiry <= time.time():
        raise RuntimeError("Claude OAuth refresh credential is expired")
    return oauth


def _read_credential_file(path: Path) -> dict:
    if not path.is_file():
        raise FileNotFoundError(path)
    if path.stat().st_mode & 0o077:
        raise RuntimeError("Claude credential file permissions must be private (0600)")
    try:
        value = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError("Claude credential file is unreadable or invalid") from error
    return _oauth_payload(value)


def credential_source(config_dir: Path, explicit_file: Path | None = None) -> str:
    """Identify the configured credential location without reading its contents."""
    candidates = []
    if explicit_file is not None:
        candidates.append(("explicit-file", explicit_file.expanduser()))
    candidates.append(("config-file", config_dir.expanduser() / ".credentials.json"))
    seen = set()
    for source, path in candidates:
        identity = str(path.absolute())
        if identity in seen:
            continue
        seen.add(identity)
        try:
            mode = path.stat().st_mode
        except FileNotFoundError:
            continue
        if not stat.S_ISREG(mode):
            raise RuntimeError("Claude credential path must be a regular file: " + str(path))
        return source
    if sys.platform == "darwin":
        return "macos-keychain"
    raise RuntimeError("Claude OAuth credentials are unavailable in the isolated config")


def load_oauth_credentials(
    config_dir: Path,
    explicit_file: Path | None = None,
    *,
    source: str | None = None,
) -> dict:
    source = source or credential_source(config_dir, explicit_file)
    if source == "explicit-file":
        return _read_credential_file(explicit_file.expanduser())
    if source == "config-file":
        return _read_credential_file(config_dir.expanduser() / ".credentials.json")

    if sys.platform != "darwin":
        raise RuntimeError("Claude OAuth credentials are unavailable in the isolated config")

    config_path = config_dir.expanduser().resolve()
    suffix = digest(str(config_path).encode())[:8]
    service = f"Claude Code-credentials-{suffix}"
    safe_env = {"HOME": str(Path.home()), "PATH": "/usr/bin:/bin", "LANG": "en_US.UTF-8"}
    try:
        result = subprocess.run(
            ["/usr/bin/security", "find-generic-password", "-s", service, "-g"],
            capture_output=True, text=True, env=safe_env, timeout=10, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("macOS Keychain credential lookup failed") from error
    if result.returncode:
        raise RuntimeError("macOS Keychain has no Claude OAuth credential for this config")
    match = re.search(r'password:\s*(?:0x[0-9a-fA-F]+\s*)?("(?:\\.|[^"\\])*")',
                      result.stderr + "\n" + result.stdout, re.DOTALL)
    if not match:
        raise RuntimeError("macOS Keychain entry has no readable Claude OAuth payload")
    try:
        raw = json.loads(match.group(1))
        value = json.loads(raw) if isinstance(raw, str) else raw
    except json.JSONDecodeError as error:
        raise RuntimeError("macOS Keychain Claude OAuth payload is invalid") from error
    return _oauth_payload(value)


def write_isolated_credentials(config_dir: Path, oauth: dict) -> None:
    path = config_dir / ".credentials.json"
    payload = json.dumps({"claudeAiOauth": oauth}).encode()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload)


def credential_receipt(source: str) -> dict[str, object]:
    """Return provenance only; credential contents never enter the receipt."""
    return {
        "credential_source": source,
        # Loading validates a non-empty OAuth payload before returning.
        "oauth_credentials_present": True,
        "credential_hash_saved": False,
        "ANTHROPIC_API_KEY_forwarded": False,
    }


def token_totals(row: dict) -> dict:
    input_fields = ("input_tokens", "cache_read_tokens", "cache_creation_tokens")
    input_values = [row.get(key) for key in input_fields]
    total_input = sum(input_values) if all(value is not None for value in input_values) else None
    output = row.get("output_tokens")
    return {
        "total_input_tokens": total_input,
        "gross_tokens_proxy": total_input + output if total_input is not None and output is not None else None,
    }


def preflight(args: argparse.Namespace) -> dict:
    setup_started = time.monotonic()
    repo = Path(args.repo).resolve(strict=True)
    out = Path(args.results_dir).resolve()
    skill = Path(args.skill).resolve(strict=True)
    scenario_path = Path(args.scenario).resolve(strict=True)
    if out.exists() and any(out.iterdir()):
        raise RuntimeError("results directory must be empty: " + str(out))
    out.mkdir(parents=True, exist_ok=True, mode=0o700)
    commit = git(repo, "rev-parse", args.revision)
    scenario = json.loads(scenario_path.read_text())
    if scenario.get("mode", "answer") != "answer":
        raise RuntimeError("only answer scenarios are accepted")
    prompt = scenario["prompt"]
    source_skill = skill.read_text()
    baseline_skill = invocation_variant(source_skill, False)
    candidate_skill = invocation_variant(source_skill, True)
    if invocation_variant(candidate_skill, False) != baseline_skill:
        raise RuntimeError("candidate changes skill content beyond invocation metadata")
    auth_config_dir = Path(args.auth_config_dir).expanduser()
    credentials_file = Path(args.credentials_file).expanduser() if args.credentials_file else None
    credential_source_name = credential_source(auth_config_dir, credentials_file)
    oauth_credentials = load_oauth_credentials(
        auth_config_dir, credentials_file, source=credential_source_name
    )
    version, help_sha = version_and_help(args.claude)
    workspace_parent = Path(tempfile.mkdtemp(prefix="pixel-claude-pair-workspace-"))
    project = workspace_parent / "project"
    project.mkdir()
    stage_archive(repo, commit, project)
    graph_source = repo / ".pixel"
    if not (graph_source / "graph.v2.db").is_file():
        raise RuntimeError("prepared graph is missing")
    shutil.copytree(graph_source, project / ".pixel", ignore=shutil.ignore_patterns(
        "actions.jsonl", "*.lock", "tasks", "task-hook-observations*", "*.db-shm"))
    skill_file = project / ".claude/skills/pixel-impact/SKILL.md"
    skill_file.parent.mkdir(parents=True)
    skill_file.write_text(baseline_skill)
    verify_project_config(project)
    pixel = str(Path(args.pixel).resolve(strict=True))
    pixel_version = command([pixel, "--version"])
    if pixel_version.returncode:
        raise RuntimeError("Pixel version preflight failed")
    started = time.monotonic()
    impact = command([pixel, "impact", args.symbol, "--no-refresh", "--depth", "2",
                      "--json", "--metrics", "off"], cwd=project, timeout=5)
    lookup_ms = round((time.monotonic() - started) * 1000)
    if impact.returncode:
        raise RuntimeError("no-refresh graph preflight failed: " + impact.stderr.strip())
    try:
        graph_result = json.loads(impact.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError("no-refresh graph query returned invalid JSON") from error
    if not graph_result:
        raise RuntimeError("no-refresh graph query returned no candidates")
    manifest = {
        "schema_version": 1,
        "repo_commit": commit,
        "repo_git_tree": git(repo, "rev-parse", f"{commit}^{{tree}}"),
        "scenario": scenario["id"],
        "scenario_sha256": digest(scenario_path.read_bytes()),
        "prompt_sha256": digest(prompt.encode()),
        "skill_source_sha256": digest(skill.read_bytes()),
        "skill_disabled_sha256": digest(baseline_skill.encode()),
        "skill_enabled_sha256": digest(candidate_skill.encode()),
        "skill_body_same_after_metadata_normalization": True,
        "single_treatment": "disable-model-invocation true -> false",
        "project_context_manifest": context_manifest(project),
        "only_project_context_delta": [".claude/skills/pixel-impact/SKILL.md"],
        "pixel_binary": pixel,
        "pixel_version": pixel_version.stdout.strip().splitlines()[0],
        "pixel_binary_sha256": digest(Path(pixel).read_bytes()),
        "graph_sha256": digest((project / ".pixel/graph.v2.db").read_bytes()),
        "pre_model_graph_query": f"pixel impact {args.symbol} --no-refresh --depth 2 --json --metrics off",
        "pre_model_graph_query_sha256": digest(impact.stdout.encode()),
        "pre_model_graph_query_ms": lookup_ms,
        "pre_model_graph_result_count": len(graph_result) if isinstance(graph_result, list) else 1,
        "claude_binary": str(Path(args.claude).resolve(strict=True)),
        "claude_version": version,
        "claude_help_sha256": help_sha,
        "model_alias": args.model,
        "effort": args.effort,
        "max_turns": args.max_turns,
        "max_budget_usd_per_arm": args.max_budget_usd,
        "permission_mode": "plan",
        "setting_sources": "project",
        "config_policy": "ephemeral config containing OAuth credentials only; no host settings, skills, hooks, plugins, or MCP copied",
        **credential_receipt(credential_source_name),
        "workspace": str(project),
        "workspace_parent": str(workspace_parent),
        "setup_ms": round((time.monotonic() - setup_started) * 1000),
        "preflight_only": True,
    }
    (out / "preflight.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (out / "pre-model-impact.json").write_text(impact.stdout)
    return manifest


def parse_stream(path: Path) -> dict:
    events = []
    event_positions = {}
    final = None
    for line in path.read_text(errors="replace").splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "assistant":
            for block in event.get("message", {}).get("content", []):
                if block.get("type") == "tool_use":
                    event_id = block.get("id")
                    if isinstance(event_id, str) and event_id:
                        # Stream snapshots may repeat a tool block; retain its latest full form.
                        if event_id in event_positions:
                            events[event_positions[event_id]] = block
                        else:
                            event_positions[event_id] = len(events)
                            events.append(block)
                    else:
                        events.append(block)
        elif event.get("type") == "result":
            final = event
    skill_uses = [b.get("input") for b in events if b.get("name") == "Skill"]
    pixel_calls = []
    for block in events:
        if block.get("name") == "Bash":
            shell = str(block.get("input", {}).get("command", ""))
            if re.search(r"(?:^|[;&|\s])pixel\s+impact\b", shell):
                pixel_calls.append(shell)
    usage = final.get("usage") if final else None
    if not isinstance(usage, dict):
        usage = {}
    return {
        "result_found": final is not None,
        "is_error": final.get("is_error") if final else None,
        "subtype": final.get("subtype") if final else None,
        "answer": final.get("result") if final else None,
        "input_tokens": usage.get("input_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "cache_read_tokens": usage.get("cache_read_input_tokens"),
        "cache_creation_tokens": usage.get("cache_creation_input_tokens"),
        "total_cost_usd": final.get("total_cost_usd") if final else None,
        "resolved_model_usage": final.get("modelUsage") if final else None,
        "skill_invocations": skill_uses,
        "pixel_impact_calls": pixel_calls,
        "tool_use_count": len(events),
    }


def execute(args: argparse.Namespace) -> list[dict]:
    out = Path(args.results_dir).resolve(strict=True)
    manifest = json.loads((out / "preflight.json").read_text())
    version, _ = version_and_help(args.claude)
    if version != manifest["claude_version"]:
        raise RuntimeError("Claude version changed since preflight")
    project = Path(manifest["workspace"])
    skill_file = project / ".claude/skills/pixel-impact/SKILL.md"
    auth_config_dir = Path(args.auth_config_dir).expanduser()
    credentials_file = Path(args.credentials_file).expanduser() if args.credentials_file else None
    current_credential_source = credential_source(auth_config_dir, credentials_file)
    oauth_credentials = load_oauth_credentials(
        auth_config_dir, credentials_file, source=current_credential_source
    )
    if current_credential_source != manifest.get("credential_source"):
        raise RuntimeError("Claude credential source changed since preflight")
    scenario = json.loads(Path(args.scenario).read_text())
    if digest(Path(args.scenario).read_bytes()) != manifest["scenario_sha256"]:
        raise RuntimeError("scenario changed since preflight")
    if digest(Path(args.skill).read_bytes()) != manifest["skill_source_sha256"]:
        raise RuntimeError("skill changed since preflight")
    if args.model != manifest["model_alias"] or args.effort != manifest["effort"]:
        raise RuntimeError("model profile changed since preflight")
    if any((out / f"{scenario['id']}-{arm}.claude.jsonl").exists() for arm in ("raw", "skill")):
        raise RuntimeError("refusing to overwrite a previous model attempt")
    results = []
    with tempfile.TemporaryDirectory(prefix="pixel-claude-pair-") as temp:
        root = Path(temp)
        isolated_home = root / "home"
        isolated_home.mkdir()
        for arm in ("raw", "skill"):
            enabled = arm == "skill"
            body = invocation_variant(Path(args.skill).read_text(), enabled)
            skill_file.write_text(body)
            config = root / ("config-" + arm)
            config.mkdir(mode=0o700)
            write_isolated_credentials(config, oauth_credentials)
            env = {
                "PATH": f"{Path(args.pixel).resolve().parent}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                "HOME": str(isolated_home),
                "TMPDIR": str(root),
                "LANG": os.environ.get("LANG", "en_US.UTF-8"),
                "CLAUDE_CONFIG_DIR": str(config),
            }
            argv = [
                str(Path(args.claude).resolve()), "-p", scenario["prompt"],
                "--model", args.model, "--effort", args.effort,
                "--setting-sources", "project",
                "--permission-mode", "plan", "--permission-prompts", "none",
                "--disallowedTools", "Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch", "Agent",
                "--no-chrome", "--no-session-persistence",
                "--max-turns", str(args.max_turns),
                "--max-budget-usd", str(args.max_budget_usd),
                "--output-format", "stream-json", "--verbose",
            ]
            before_tree = directory_digest(project)
            started = time.monotonic()
            proc = command(argv, cwd=project, env=env, timeout=args.timeout)
            wall_ms = round((time.monotonic() - started) * 1000)
            transcript = out / f"{scenario['id']}-{arm}.claude.jsonl"
            transcript.write_text(proc.stdout)
            transcript.chmod(0o600)
            stderr_path = out / f"{arm}.stderr"
            stderr_path.write_text(proc.stderr)
            stderr_path.chmod(0o600)
            command_path = out / f"{arm}.command.json"
            command_path.write_text(json.dumps({
                "argv": [argv[0], "-p", "<scenario prompt>", *argv[3:]],
                "environment_keys": sorted(env),
                "exit_code": proc.returncode,
                "wall_ms": wall_ms,
                "skill_sha256": digest(body.encode()),
            }, indent=2) + "\n")
            command_path.chmod(0o600)
            row = parse_stream(transcript)
            row.update({"arm": arm, "exit_code": proc.returncode, "wall_ms": wall_ms,
                        "skill_sha256": digest(body.encode()), "rep": 1,
                        "model": args.model, "cli_version": manifest["claude_version"],
                        "commit": manifest["repo_commit"]})
            row.update(token_totals(row))
            results.append(row)
            run_path = out / f"{scenario['id']}-{arm}.claude.run.json"
            run_path.write_text(json.dumps({
                "schema_version": 1, "scenario": scenario["id"], "arm": arm, "cli": "claude",
                "rep": 1, "model": args.model, "cli_version": manifest["claude_version"],
                "commit": manifest["repo_commit"], "wall_ms": wall_ms,
                "exit_code": proc.returncode, "timed_out": False,
            }, indent=2) + "\n")
            run_path.chmod(0o600)
            if directory_digest(project) != before_tree:
                raise RuntimeError(arm + " run changed the read-only experiment snapshot")
            if proc.returncode or not row["result_found"] or row["is_error"]:
                break
    pair_path = out / "pair.json"
    pair_path.write_text(json.dumps(results, indent=2) + "\n")
    pair_path.chmod(0o600)
    # The source snapshot is reproducible from the pinned Git commit and graph hash.
    shutil.rmtree(manifest["workspace_parent"], ignore_errors=True)
    score = command([sys.executable, str(Path(__file__).with_name("score.py")),
                     "--results", str(out), "--scenarios-dir", str(Path(args.scenario).resolve().parent),
                     "raw", "skill"], timeout=120)
    (out / "score.txt").write_text(score.stdout + score.stderr)
    if score.returncode:
        raise RuntimeError("existing eval scorer failed: " + score.stderr.strip())
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("preflight", "run"))
    parser.add_argument("--repo", required=True)
    parser.add_argument("--revision", default="HEAD")
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--skill", required=True)
    parser.add_argument("--pixel", default="/Users/livio/.local/bin/pixel")
    parser.add_argument("--claude", default="/Users/livio/.local/bin/claude")
    parser.add_argument("--auth-config-dir", default=str(Path.home() / ".claude"))
    parser.add_argument("--credentials-file")
    parser.add_argument("--results-dir", required=True)
    parser.add_argument("--symbol", default="transferPageToGhost")
    parser.add_argument("--model", default="sonnet")
    parser.add_argument("--effort", default="medium",
                        choices=("low", "medium", "high", "xhigh", "max"))
    parser.add_argument("--max-turns", type=int, default=10)
    parser.add_argument("--max-budget-usd", type=float, default=1.0)
    parser.add_argument("--timeout", type=int, default=1200)
    args = parser.parse_args()
    # Never pass an ambient banned API key to the Claude child process.
    os.environ.pop("ANTHROPIC_API_KEY", None)
    try:
        if args.mode == "preflight":
            manifest = preflight(args)
            manifest.pop("workspace", None)
            print(json.dumps(manifest, indent=2))
            return 0
        results = execute(args)
        print(json.dumps([{key: value for key, value in row.items() if key != "answer"}
                          for row in results], indent=2))
        return 0 if len(results) == 2 and all(row["result_found"] for row in results) else 1
    except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
        print("claude_skill_pair: " + str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
