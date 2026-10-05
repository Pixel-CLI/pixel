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
import unicodedata
from pathlib import Path

FLAG = re.compile(r"(?m)^disable-model-invocation:\s*(true|false)\s*$")
CONTEXT_FILES = {"AGENTS.md", "CLAUDE.md", "CLAUDE.local.md", "settings.json",
                 "settings.local.json", ".mcp.json", "plugin.json"}
MANAGED = (Path("/Library/Application Support/ClaudeCode/managed-settings.json"),
           Path("/Library/Managed Preferences/com.anthropic.claudecode.plist"),
           Path.home() / ".claude/managed-settings.json")
GATEWAY_ENV_KEYS = (
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
)
ACCOUNTING_FILES = {".pixel/actions.jsonl", ".pixel/calls.json"}
SKILL_TREATMENT_FILE = ".claude/skills/pixel-impact/SKILL.md"
PIXEL_IMPACT_TOOL_PERMISSION = "Bash(pixel impact * --no-refresh *)"


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def directory_digest(root: Path, *, ignored_regular_files: set[str] | None = None) -> str:
    result = hashlib.sha256()
    ignored = ACCOUNTING_FILES | (ignored_regular_files or set())
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if relative in ignored and path.is_file() and not path.is_symlink():
            continue
        result.update(relative.encode() + b"\0")
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
                   "--allowedTools", "--disallowedTools", "--max-budget-usd",
                   "--no-session-persistence", "--effort"):
        if option not in help_text.stdout:
            raise RuntimeError("installed Claude lacks required CLI option " + option)
    max_turns_probe = command([claude, "--max-turns"], timeout=5)
    probe_message = max_turns_probe.stderr + "\n" + max_turns_probe.stdout
    if max_turns_probe.returncode == 0 or not re.search(
        r"option ['\"]--max-turns(?: <[^>]+>)?['\"] argument missing", probe_message
    ):
        raise RuntimeError("installed Claude lacks required CLI option --max-turns")
    return version.stdout.strip().splitlines()[0], digest(help_text.stdout.encode())


def verify_preflight_snapshot(project: Path, pixel: str, manifest: dict) -> int:
    if digest(Path(pixel).read_bytes()) != manifest["pixel_binary_sha256"]:
        raise RuntimeError("Pixel binary changed since preflight")
    if digest((project / ".pixel/graph.v2.db").read_bytes()) != manifest["graph_sha256"]:
        raise RuntimeError("prepared graph changed since preflight")
    source_digest = directory_digest(
        project, ignored_regular_files={SKILL_TREATMENT_FILE}
    )
    if source_digest != manifest["project_source_sha256"]:
        raise RuntimeError("prepared source changed since preflight")
    started = time.monotonic()
    query = command(manifest["pre_model_graph_argv"], cwd=project, timeout=5)
    query_ms = round((time.monotonic() - started) * 1000)
    if query.returncode or digest(query.stdout.encode()) != manifest["pre_model_graph_query_sha256"]:
        raise RuntimeError("no-refresh graph query changed since preflight")
    if digest(Path(pixel).read_bytes()) != manifest["pixel_binary_sha256"]:
        raise RuntimeError("Pixel binary changed during pre-arm graph verification")
    if digest((project / ".pixel/graph.v2.db").read_bytes()) != manifest["graph_sha256"]:
        raise RuntimeError("prepared graph changed during pre-arm graph verification")
    if directory_digest(project, ignored_regular_files={SKILL_TREATMENT_FILE}) != manifest[
        "project_source_sha256"
    ]:
        raise RuntimeError("prepared source changed during pre-arm graph verification")
    return query_ms


def claude_run_argv(claude: str, prompt: str, model: str, effort: str,
                    max_turns: int, max_budget_usd: float) -> list[str]:
    return [
        claude, "-p", prompt,
        "--model", model, "--effort", effort,
        "--setting-sources", "project",
        "--permission-mode", "plan", "--permission-prompts", "none",
        "--disallowedTools", "Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch", "Agent",
        "--allowedTools", PIXEL_IMPACT_TOOL_PERMISSION,
        "--no-chrome", "--no-session-persistence",
        "--max-turns", str(max_turns),
        "--max-budget-usd", str(max_budget_usd),
        "--output-format", "stream-json", "--verbose",
    ]


def resolve_executable(value: str | None, name: str) -> Path:
    candidate = value or shutil.which(name)
    if candidate is None:
        raise RuntimeError(f"{name} executable not found; add it to PATH or pass --{name}")
    try:
        path = Path(candidate).expanduser().resolve(strict=True)
    except OSError as error:
        raise RuntimeError(f"{name} executable does not exist or cannot be resolved: {candidate}") from error
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"{name} executable is not an executable file: {path}")
    return path


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


def credential_source(config_dir: Path | None, explicit_file: Path | None = None) -> str:
    """Identify the configured credential location without reading its contents."""
    candidates = []
    if explicit_file is not None:
        candidates.append(("explicit-file", explicit_file.expanduser()))
    config_path = config_dir.expanduser() if config_dir is not None else Path.home() / ".claude"
    candidates.append(("config-file", config_path / ".credentials.json"))
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
    config_dir: Path | None,
    explicit_file: Path | None = None,
    *,
    source: str | None = None,
) -> dict:
    source = source or credential_source(config_dir, explicit_file)
    if source == "explicit-file":
        return _read_credential_file(explicit_file.expanduser())
    if source == "config-file":
        config_path = config_dir.expanduser() if config_dir is not None else Path.home() / ".claude"
        return _read_credential_file(config_path / ".credentials.json")

    if sys.platform != "darwin":
        raise RuntimeError("Claude OAuth credentials are unavailable in the isolated config")

    import pwd

    service = "Claude Code-credentials"
    if config_dir is not None:
        config_path = unicodedata.normalize("NFC", str(config_dir.expanduser()))
        service += "-" + digest(config_path.encode())[:8]
    try:
        account = os.environ.get("USER") or pwd.getpwuid(os.getuid()).pw_name
    except (KeyError, OSError):
        account = "claude-code-user"
    if re.fullmatch(r"[a-zA-Z0-9._-]+", account) is None:
        account = "claude-code-user"
    safe_env = {"HOME": str(Path.home()), "PATH": "/usr/bin:/bin", "LANG": "en_US.UTF-8"}
    try:
        result = subprocess.run(
            ["/usr/bin/security", "find-generic-password", "-a", account, "-s", service, "-w"],
            capture_output=True, env=safe_env, timeout=10, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("macOS Keychain credential lookup failed") from error
    if result.returncode:
        raise RuntimeError("macOS Keychain has no Claude OAuth credential for this config")
    try:
        value = json.loads(result.stdout)
    except (json.JSONDecodeError, UnicodeDecodeError):
        raise RuntimeError("macOS Keychain Claude OAuth payload is invalid") from None
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


def _gateway_file_identity(path: Path, info: os.stat_result) -> dict[str, object]:
    return {
        "resolved_path": str(path),
        "device": info.st_dev,
        "inode": info.st_ino,
        "size": info.st_size,
        "mtime_ns": info.st_mtime_ns,
        "ctime_ns": info.st_ctime_ns,
    }


def load_gateway_settings_snapshot(path: Path) -> tuple[dict[str, str], dict[str, object]]:
    """Load allowlisted gateway env entries and non-secret source identity."""
    try:
        resolved = path.expanduser().resolve(strict=True)
        before = resolved.stat()
        if not stat.S_ISREG(before.st_mode):
            raise RuntimeError("gateway settings must be a regular file")
        contents = resolved.read_text()
        after = resolved.stat()
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        raise RuntimeError("gateway settings are unreadable or invalid") from None
    source = _gateway_file_identity(resolved, before)
    if source != _gateway_file_identity(resolved, after):
        raise RuntimeError("gateway settings changed while being read")
    try:
        value = json.loads(contents)
    except json.JSONDecodeError:
        raise RuntimeError("gateway settings are unreadable or invalid") from None
    settings_env = value.get("env") if isinstance(value, dict) else None
    if not isinstance(settings_env, dict):
        raise RuntimeError("gateway settings must contain an env object")
    selected = {
        key: settings_env[key]
        for key in GATEWAY_ENV_KEYS
        if key in settings_env
    }
    for required in ("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"):
        if not isinstance(selected.get(required), str) or not selected[required].strip():
            raise RuntimeError("gateway settings require ANTHROPIC_AUTH_TOKEN and ANTHROPIC_BASE_URL")
    if any(not isinstance(item, str) for item in selected.values()):
        raise RuntimeError("allowlisted gateway settings must be strings")
    return selected, source


def load_gateway_settings(path: Path) -> dict[str, str]:
    return load_gateway_settings_snapshot(path)[0]


def claude_run_environment(pixel: str, home: Path, root: Path, config: Path,
                           gateway_env: dict[str, str] | None = None) -> dict[str, str]:
    env = {
        "PATH": f"{Path(pixel).parent}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(home),
        "TMPDIR": str(root),
        "LANG": os.environ.get("LANG", "en_US.UTF-8"),
        "CLAUDE_CONFIG_DIR": str(config),
    }
    if gateway_env is not None:
        env.update(gateway_env)
    return env


def reported_model_identity(row: dict) -> dict[str, object]:
    usage = row.get("resolved_model_usage")
    usage_ids = sorted(usage) if isinstance(usage, dict) else []
    return {
        "cli_init_model": row.get("cli_init_model"),
        "cli_model_usage_ids": usage_ids,
    }


def model_selection_receipt(alias: str, gateway_env: dict[str, str] | None) -> dict[str, object]:
    alias_key = {
        "sonnet": "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "opus": "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "haiku": "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    }.get(alias)
    return {
        "requested_cli_model": alias,
        "selection_source": "explicit --model CLI argument",
        "configured_alias_override_key": (
            alias_key if gateway_env is not None and alias_key in gateway_env else None
        ),
        "configured_default_model_present": gateway_env is not None and "ANTHROPIC_MODEL" in gateway_env,
    }


def require_matching_reported_models(results: list[dict]) -> dict[str, object] | None:
    if len(results) != 2 or not all(row.get("result_found") and not row.get("is_error") for row in results):
        return None
    identities = [reported_model_identity(row) for row in results]
    if not any(identity["cli_init_model"] or identity["cli_model_usage_ids"] for identity in identities):
        raise RuntimeError("Claude CLI did not report model identity for both arms")
    if identities[0] != identities[1]:
        raise RuntimeError("Claude CLI-reported model identity differs between arms")
    return {
        "status": "matched",
        "source": "Claude CLI init/modelUsage; gateway backend identity is not attested",
        "identity": identities[0],
    }


def redact_gateway_values(text: str, gateway_env: dict[str, str] | None) -> str:
    if gateway_env is None:
        return text
    sensitive_values = [gateway_env.get(key, "") for key in (
        "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "ANTHROPIC_CUSTOM_HEADERS",
    )]
    custom_headers = gateway_env.get("ANTHROPIC_CUSTOM_HEADERS", "")
    for line in custom_headers.splitlines():
        _, separator, value = line.partition(":")
        if separator and value.strip():
            sensitive_values.append(value.strip())
    escaped_values = {
        escaped
        for value in sensitive_values
        if value
        for escaped in (
            value,
            json.dumps(value, ensure_ascii=False)[1:-1],
            json.dumps(value, ensure_ascii=True)[1:-1],
        )
    }
    for value in sorted(escaped_values, key=len, reverse=True):
        if value:
            text = text.replace(value, "<redacted-gateway-setting>")
    return text


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
    claude = str(resolve_executable(args.claude, "claude"))
    pixel = str(resolve_executable(args.pixel, "pixel"))
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
    auth_mode = getattr(args, "auth_mode", "oauth")
    gateway_settings = getattr(args, "gateway_settings", None)
    gateway_env = None
    gateway_source = None
    if auth_mode == "configured-gateway":
        if not gateway_settings:
            raise RuntimeError("--gateway-settings is required for configured-gateway auth")
        gateway_env, gateway_source = load_gateway_settings_snapshot(Path(gateway_settings))
        credential_source_name = "configured-gateway-settings"
        auth_receipt = {
            "credential_source": credential_source_name,
            "auth_mode": auth_mode,
            "configured_environment_keys": sorted(gateway_env),
            "gateway_settings_source": gateway_source,
            "ANTHROPIC_API_KEY_forwarded": False,
        }
    else:
        if gateway_settings:
            raise RuntimeError("--gateway-settings requires --auth-mode configured-gateway")
        auth_config_dir = Path(args.auth_config_dir).expanduser() if args.auth_config_dir else None
        credentials_file = Path(args.credentials_file).expanduser() if args.credentials_file else None
        credential_source_name = credential_source(auth_config_dir, credentials_file)
        oauth_credentials = load_oauth_credentials(
            auth_config_dir, credentials_file, source=credential_source_name
        )
        auth_receipt = {
            "auth_mode": auth_mode,
            **credential_receipt(credential_source_name),
        }
    version, help_sha = version_and_help(claude)
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
    pixel_version = command([pixel, "--version"])
    if pixel_version.returncode:
        raise RuntimeError("Pixel version preflight failed")
    started = time.monotonic()
    graph_query_argv = [pixel, "impact", args.symbol, "--no-refresh", "--depth", "2",
                        "--json", "--metrics", "off"]
    impact = command(graph_query_argv, cwd=project, timeout=5)
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
        "pre_model_graph_argv": graph_query_argv,
        "pre_model_graph_query_sha256": digest(impact.stdout.encode()),
        "pre_model_graph_query_ms": lookup_ms,
        "project_source_sha256": directory_digest(
            project, ignored_regular_files={SKILL_TREATMENT_FILE}
        ),
        "pre_model_graph_result_count": len(graph_result) if isinstance(graph_result, list) else 1,
        "claude_binary": claude,
        "claude_version": version,
        "claude_help_sha256": help_sha,
        "model_alias": args.model,
        "model_selection": model_selection_receipt(args.model, gateway_env),
        "auth_mode": auth_mode,
        "auth_environment_keys": sorted(gateway_env) if gateway_env is not None else [],
        "model_identity_policy": "CLI-reported init/modelUsage only; gateway backend identity is not attested",
        "effort": args.effort,
        "max_turns": args.max_turns,
        "max_budget_usd_per_arm": args.max_budget_usd,
        "permission_mode": "plan",
        "setting_sources": "project",
        "allowed_tools": [PIXEL_IMPACT_TOOL_PERMISSION],
        "config_policy": ("ephemeral config with allowlisted gateway environment only; no host settings, skills, hooks, plugins, or MCP copied"
                          if gateway_env is not None else
                          "ephemeral config containing OAuth credentials only; no host settings, skills, hooks, plugins, or MCP copied"),
        **auth_receipt,
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
    cli_init_model = None
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
        elif event.get("type") == "system" and event.get("subtype") == "init":
            model = event.get("model")
            if isinstance(model, str):
                cli_init_model = model
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
        "cli_init_model": cli_init_model,
        "skill_invocations": skill_uses,
        "pixel_impact_calls": pixel_calls,
        "tool_use_count": len(events),
    }


def execute(args: argparse.Namespace) -> list[dict]:
    out = Path(args.results_dir).resolve(strict=True)
    manifest = json.loads((out / "preflight.json").read_text())
    claude = str(resolve_executable(args.claude, "claude"))
    pixel = str(resolve_executable(args.pixel, "pixel"))
    version, _ = version_and_help(claude)
    if version != manifest["claude_version"]:
        raise RuntimeError("Claude version changed since preflight")
    project = Path(manifest["workspace"])
    skill_file = project / ".claude/skills/pixel-impact/SKILL.md"
    auth_mode = getattr(args, "auth_mode", "oauth")
    gateway_settings = getattr(args, "gateway_settings", None)
    if auth_mode != manifest.get("auth_mode", "oauth"):
        raise RuntimeError("Claude auth mode changed since preflight")
    gateway_env = None
    gateway_source = None
    if auth_mode == "configured-gateway":
        if not gateway_settings:
            raise RuntimeError("--gateway-settings is required for configured-gateway auth")
        gateway_env, gateway_source = load_gateway_settings_snapshot(Path(gateway_settings))
        if gateway_source != manifest.get("gateway_settings_source"):
            raise RuntimeError("configured gateway settings changed since preflight")
        if sorted(gateway_env) != manifest.get("auth_environment_keys"):
            raise RuntimeError("configured gateway environment keys changed since preflight")
    else:
        auth_config_dir = Path(args.auth_config_dir).expanduser() if args.auth_config_dir else None
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
            if gateway_env is None:
                write_isolated_credentials(config, oauth_credentials)
            env = claude_run_environment(pixel, isolated_home, root, config, gateway_env)
            graph_check_ms = verify_preflight_snapshot(project, pixel, manifest)
            argv = claude_run_argv(
                claude, scenario["prompt"], args.model, args.effort,
                args.max_turns, args.max_budget_usd,
            )
            before_tree = directory_digest(project)
            started = time.monotonic()
            proc = command(argv, cwd=project, env=env, timeout=args.timeout)
            wall_ms = round((time.monotonic() - started) * 1000)
            proc_stdout = redact_gateway_values(proc.stdout, gateway_env)
            proc_stderr = redact_gateway_values(proc.stderr, gateway_env)
            transcript = out / f"{scenario['id']}-{arm}.claude.jsonl"
            transcript.write_text(proc_stdout)
            transcript.chmod(0o600)
            stderr_path = out / f"{arm}.stderr"
            stderr_path.write_text(proc_stderr)
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
            if gateway_env is not None and isinstance(row.get("answer"), str):
                row["answer"] = redact_gateway_values(row["answer"], gateway_env)
            row.update({"arm": arm, "exit_code": proc.returncode, "wall_ms": wall_ms,
                        "graph_check_ms": graph_check_ms,
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
                "graph_check_ms": graph_check_ms,
                "exit_code": proc.returncode, "timed_out": False,
            }, indent=2) + "\n")
            run_path.chmod(0o600)
            if directory_digest(project) != before_tree:
                raise RuntimeError(arm + " run changed the read-only experiment snapshot")
            if proc.returncode or not row["result_found"] or row["is_error"]:
                break
    pair_path = out / "pair.json"
    model_identity = None
    model_identity_error = None
    if auth_mode == "configured-gateway":
        try:
            model_identity = require_matching_reported_models(results)
        except RuntimeError as error:
            model_identity_error = error
    if auth_mode == "configured-gateway" and len(results) == 2 and all(
        row.get("result_found") and not row.get("is_error") for row in results
    ):
        (out / "model-identity.json").write_text(json.dumps({
            "schema_version": 1,
            "source": "Claude CLI init/modelUsage; gateway backend identity is not attested",
            "status": model_identity["status"] if model_identity else "mismatch-or-unavailable",
            "identity_by_arm": {
                row["arm"]: reported_model_identity(row) for row in results
            },
        }, indent=2) + "\n")
        (out / "model-identity.json").chmod(0o600)
    pair_path.write_text(json.dumps(results, indent=2) + "\n")
    pair_path.chmod(0o600)
    # The source snapshot is reproducible from the pinned Git commit and graph hash.
    shutil.rmtree(manifest["workspace_parent"], ignore_errors=True)
    if model_identity_error is not None:
        raise model_identity_error
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
    parser.add_argument("--pixel", default=shutil.which("pixel"))
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--auth-config-dir", default=os.environ.get(
        "CLAUDE_SECURESTORAGE_CONFIG_DIR", os.environ.get("CLAUDE_CONFIG_DIR")),
        help="Claude credential scope; defaults to its configured scope or the unscoped login")
    parser.add_argument("--credentials-file")
    parser.add_argument("--auth-mode", choices=("oauth", "configured-gateway"), default="oauth",
                        help="credential source; OAuth remains the default")
    parser.add_argument("--gateway-settings",
                        help="settings JSON for configured-gateway mode; only allowlisted env fields are loaded")
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
