#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Arena entrypoint: per-arm repo preparation (indexing + codex wiring), then
# the scored codex run. $ARM_TOOL selects the arm; $PROMPT/$OUT are the task.
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
REPO_DIR="${ARENA_REPO_DIR:-/repo}"
if [ -n "${ARENA_CODEX_CALLER_FACTS:-}" ] && [ "${ARENA_CODEX_CALLER_FACTS}" != "0" ]; then
  echo "ARENA_CODEX_CALLER_FACTS is retired" >&2
  exit 2
fi
cd "$REPO_DIR" || exit 1
REVIEWED_PIXEL_HOOKS="${ARENA_REVIEWED_PIXEL_HOOKS:-0}"
SKILL_PILOT="${ARENA_SKILL_PILOT:-0}"
case "$REVIEWED_PIXEL_HOOKS:$SKILL_PILOT" in
  0:0|1:0|0:1) ;;
  *) echo "skill-only runs cannot use reviewed Pixel hooks" >&2; exit 2 ;;
esac
HOOK_AUDIT_KEY="${ARENA_RUN_ID:-standalone}-${REP:-1}"
HOOK_AUDIT_READY="/out/hook-audit-${ARM_TOOL:-raw}-${HOOK_AUDIT_KEY}.ready"
mark_hook_audit_failure() {
  local rc=$?
  if [ "$REVIEWED_PIXEL_HOOKS" = "1" ] && [ ! -e "$HOOK_AUDIT_READY" ]; then
    touch "/out/hook-audit-${ARM_TOOL:-raw}-${HOOK_AUDIT_KEY}.failed"
  fi
  return "$rc"
}
trap mark_hook_audit_failure EXIT

prep_semble()  { :; }                     # indexes lazily on first query
prep_graft()   { graft init --yes; }
prep_stacklit() {
  npx -y stacklit init
  npx -y stacklit generate
  npx -y stacklit derive > /root/.codex/AGENTS.md
}
prep_gitnexus() { gitnexus setup && gitnexus analyze /repo; }
prep_gortex()  {
  gortex daemon start
  gortex track /repo
  # the daemon must acknowledge the repo before the MCP server is useful;
  # a blind sleep hung the first round
  for _ in $(seq 1 24); do
    gortex repos 2>/dev/null | grep -q "/repo" && break
    sleep 5
  done
  codex mcp add gortex -- gortex mcp
}
verify_pixel_brief() {
  local output_file receipt_file
  output_file=$(mktemp)
  receipt_file="/out/pixel-brief-hook-${REP:-1}.json"
  # `run-hook prompt-submit` is a retired no-op verb (RetiredHookCmd); the live
  # brief path is the task-event hook `pixel install` registers for Codex's
  # UserPromptSubmit. The probe prompt carries a strong code signal
  # (`README.md` names code by its source extension, `tested` is a code word)
  # so an indexed repository can answer with a real [PIXEL:BRIEF] block; a
  # vague prompt would legitimately abstain with `{}`.
  if ! printf '%s' '{"session_id":"arena-brief-'"${REP:-1}"'","prompt":"where is README.md and how is the project tested?","cwd":"'"$REPO_DIR"'","hook_event_name":"UserPromptSubmit"}' \
    | pixel run-hook task-event --provider codex --event prompt-submit > "$output_file"; then
    rm -f "$output_file"
    echo "Pixel prompt-submit hook invocation failed" >&2
    return 1
  fi
  python3 - "$output_file" "$receipt_file" "${REP:-1}" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

output_path, receipt_path, rep = sys.argv[1:]
try:
    raw_response = Path(output_path).read_text().strip()
    response = json.loads(raw_response) if raw_response else {}
    hook_output = response.get("hookSpecificOutput") or {}
    context = hook_output.get("additionalContext", "")
    # Same validity contract as hook_audit.py's run_hook: an absent
    # hookSpecificOutput is Codex's valid abstention shape (`{}`), so the hook
    # plumbing is verified separately from whether this prompt found evidence.
    valid = (
        bool(raw_response)
        and (
            "hookSpecificOutput" not in response
            or (
                hook_output.get("hookEventName") == "UserPromptSubmit"
                and ("additionalContext" not in hook_output or isinstance(context, str))
            )
        )
    )
    emitted = isinstance(context, str) and bool(context.strip())
    receipt = {
        "arm": "pixel",
        "rep": rep,
        "provider": "codex",
        "event": "prompt-submit",
        "response_valid": valid,
        "emitted_context": emitted,
        "hook_event_name": hook_output.get("hookEventName"),
        "brief_present": "[PIXEL:BRIEF]" in context if isinstance(context, str) else False,
        "execution_route_present": "[PIXEL:EXECUTION_ROUTE]" in context if isinstance(context, str) else False,
        "context_bytes": len(context.encode()) if isinstance(context, str) else 0,
        "context_sha256": hashlib.sha256(context.encode()).hexdigest() if isinstance(context, str) else None,
    }
except (OSError, ValueError, TypeError):
    receipt = {
        "arm": "pixel",
        "rep": rep,
        "provider": "codex",
        "event": "prompt-submit",
        "response_valid": False,
        "emitted_context": False,
        "hook_event_name": None,
        "brief_present": False,
        "context_bytes": 0,
        "context_sha256": None,
    }
Path(receipt_path).write_text(json.dumps(receipt, indent=2) + "\n")
if not receipt["response_valid"]:
    raise SystemExit("Pixel prompt-submit hook did not return a valid Codex envelope")
print("=== Pixel prompt-submit context ===")
print(context)
print("=== End Pixel prompt-submit context ===")
PY
  rm -f "$output_file"
}
install_live_brief_hook() {
  local codex_home hook_file receipt_file
  codex_home="${CODEX_HOME:-$HOME/.codex}"
  hook_file="$codex_home/hooks.json"
  receipt_file="/out/pixel-hook-${REP:-1}.jsonl"
  mkdir -p "$codex_home"
  python3 - "$hook_file" "$receipt_file" <<'PY'
import json
import sys
from pathlib import Path

hook_path, receipt_path = map(Path, sys.argv[1:])
try:
    config = json.loads(hook_path.read_text()) if hook_path.exists() else {}
except (OSError, ValueError):
    config = {}
hooks = config.setdefault("hooks", {})
groups = hooks.setdefault("UserPromptSubmit", [])
# The live brief verb (the retired `run-hook prompt-submit` accepted anything
# and emitted nothing); hook_audit.py allowlists exactly this argv.
command = (
    "python3 /usr/local/lib/arena-hook-audit.py --receipt "
    + str(receipt_path)
    + " -- /usr/local/bin/pixel run-hook task-event --provider codex --event prompt-submit"
)
if not any(command in json.dumps(group) for group in groups):
    groups.append({"hooks": [{"type": "command", "command": command}]})
hook_path.write_text(json.dumps(config, indent=2) + "\n")
PY
}
prep_pixel() {
  local prep_start_ms prep_end_ms graph_db setup_receipt
  if [ "${PIXEL_ARENA_PREP_GRAPH:-0}" = "1" ]; then
    prep_start_ms=$(date +%s%3N) || return $?
  fi
  pixel install || return $?
 if [ "${ARENA_REVIEWED_PIXEL_HOOKS:-0}" != "1" ]; then
   pixel install --repo "$REPO_DIR" || return $?
 fi
  # The classify skill is the conditional-routing surface Codex sees;
  # non-interactive installs skip the wizard, so deploy it explicitly.
  pixel config install-helpers 2>/dev/null || true
 if [ "${PIXEL_ARENA_PREP_GRAPH:-0}" = "1" ]; then
    graph_db="$REPO_DIR/.pixel/graph.v2.db"
    setup_receipt="/out/setup-pixel-${REP:-1}.json"
    pixel prepare-repo --no-daemon "$REPO_DIR" || return $?
    if [ ! -f "$graph_db" ]; then
      echo "graph preparation completed without $graph_db" >&2
      return 1
    fi
    prep_end_ms=$(date +%s%3N) || return $?
    prepare_commands='["pixel install"]'
    if [ "${ARENA_REVIEWED_PIXEL_HOOKS:-0}" != "1" ]; then
      prepare_commands='["pixel install", "pixel install --repo /repo"]'
    fi
    prepare_commands="${prepare_commands%]}, \"pixel prepare-repo --no-daemon /repo\"]"
    python3 -c 'import json,sys; graph=sys.argv[1]; json.dump({"arm":"pixel","rep":sys.argv[4],"prepare_commands":json.loads(sys.argv[6]),"duration_ms":int(sys.argv[2])-int(sys.argv[3]),"graph_db":graph,"graph_db_bytes":__import__("pathlib").Path(graph).stat().st_size},open(sys.argv[5],"w"),indent=2); open(sys.argv[5],"a").write("\n")' \
      "$graph_db" "$prep_end_ms" "$prep_start_ms" "${REP:-1}" "$setup_receipt" "$prepare_commands"
  else
    pixel prepare-repo --no-daemon "$REPO_DIR" || return $?
  fi
  if [ "${ARENA_REVIEWED_PIXEL_HOOKS:-0}" != "1" ]; then
    install_live_brief_hook
  fi
  verify_pixel_brief
}

prep_skill_pilot() {
  local start end graph_db setup_receipt
  start=$(date +%s%3N) || return $?
  graph_db="$REPO_DIR/.pixel/graph.v2.db"
  pixel prepare-repo --no-daemon "$REPO_DIR" || return $?
  if [ ! -f "$graph_db" ]; then
    echo "skill-pilot preparation completed without $graph_db" >&2
    return 1
  fi
  end=$(date +%s%3N) || return $?
  setup_receipt="/out/setup-${ARM_TOOL:-raw}-${REP:-1}.json"
  python3 -c 'import json,sys; from pathlib import Path; db=Path(sys.argv[1]); json.dump({"arm":sys.argv[4],"rep":sys.argv[5],"prepare_commands":["pixel prepare-repo --no-daemon /repo"],"duration_ms":int(sys.argv[3])-int(sys.argv[2]),"graph_db_bytes":db.stat().st_size},open(sys.argv[6],"w"),indent=2); open(sys.argv[6],"a").write("\n")' \
    "$graph_db" "$start" "$end" "${ARM_TOOL:-raw}" "${REP:-1}" "$setup_receipt"
}

if [ "$SKILL_PILOT" = "1" ]; then
  prep_skill_pilot
  prep_rc=$?
else
case "${ARM_TOOL:-raw}" in
  raw)      : ;;
  semble)   prep_semble ;;
  graft)    prep_graft ;;
  stacklit) prep_stacklit ;;
  gitnexus) prep_gitnexus ;;
  gortex)   prep_gortex ;;
  pixel)    prep_pixel ;;
esac
prep_rc=$?
fi
if [ "$prep_rc" -ne 0 ]; then
  echo "arm preparation failed: arm=${ARM_TOOL:-raw} rc=$prep_rc" >&2
  exit "$prep_rc"
fi

if [ "$REVIEWED_PIXEL_HOOKS" = "1" ]; then
  if ! python3 /usr/local/lib/arena-hook-audit.py audit \
    --arm "${ARM_TOOL:-raw}" --codex-home "${CODEX_HOME:-$HOME/.codex}" \
    --repo "$REPO_DIR" --rep "${REP:-1}" \
    --receipt "/out/hook-audit-${ARM_TOOL:-raw}-${REP:-1}.json"; then
    exit 1
  fi
  touch "$HOOK_AUDIT_READY"
  for _ in $(seq 1 240); do
    if [ -e "/out/hook-audit-raw-${HOOK_AUDIT_KEY}.failed" ] || \
       [ -e "/out/hook-audit-pixel-${HOOK_AUDIT_KEY}.failed" ]; then
      touch "/out/hook-audit-${ARM_TOOL:-raw}-${HOOK_AUDIT_KEY}.failed"
      echo "paired hook audit failed; refusing to start Codex" >&2
      exit 1
    fi
    if [ -e "/out/hook-audit-raw-${HOOK_AUDIT_KEY}.ready" ] && \
       [ -e "/out/hook-audit-pixel-${HOOK_AUDIT_KEY}.ready" ]; then
      break
    fi
    sleep 1
  done
  if [ ! -e "/out/hook-audit-raw-${HOOK_AUDIT_KEY}.ready" ] || \
     [ ! -e "/out/hook-audit-pixel-${HOOK_AUDIT_KEY}.ready" ]; then
    touch "/out/hook-audit-${ARM_TOOL:-raw}-${HOOK_AUDIT_KEY}.failed"
    echo "timed out waiting for both paired hook audits; refusing to start Codex" >&2
    exit 1
  fi
fi
# This receipt is captured after arm setup, immediately before Codex runs, so
# the arena can verify the static instructions visible to each arm.
python3 /usr/local/lib/arena-context-manifest.py \
  --repo "$REPO_DIR" --codex-home "${CODEX_HOME:-$HOME/.codex}" \
  > "/out/context-${ARM_TOOL:-raw}-${REP:-1}.json"
touch "/out/arena-ready-${ARM_TOOL:-raw}-${REP:-1}"

# MCP arms: register the tool's stdio server with codex
case "${ARM_TOOL:-raw}" in
  semble)   codex mcp add semble -- uvx --from 'semble[mcp]' semble ;;
  graft)    codex mcp add graft -- graft mcp ;;
  stacklit) codex mcp add stacklit -- npx -y stacklit serve ;;
  gitnexus) : ;;   # gitnexus setup wrote its own MCP entries
  gortex)   : ;;   # registered in prep_gortex
  pixel)    : ;;   # pixel wires itself via pixel install
esac

if [ "${ARENA_WATCH_ONLY:-0}" = "1" ]; then
  while :; do sleep 3600; done
fi

IFS=' ' read -r -a TASK_LIST <<< "${TASKS:-s1-hook-install s2-vector-recall s3-rename-impact}"
MODEL_ARGS=()
[ -n "${CODEX_MODEL:-}" ] && MODEL_ARGS+=(-m "$CODEX_MODEL")
[ -n "${CODEX_EFFORT:-}" ] && MODEL_ARGS+=(-c model_reasoning_effort="$CODEX_EFFORT")
overall_rc=0
for task in "${TASK_LIST[@]}"; do
  prompt=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['prompt'])" "/prompts/$task.json")
  tag="${ARM_TOOL}-${task}-${REP}"
  t0=$(date +%s)
  # The container is the sandbox: docker's default seccomp blocks codex's
  # bubblewrap namespaces, so read-only mode would fail every command.
  codex exec --json --sandbox danger-full-access --skip-git-repo-check \
    "${MODEL_ARGS[@]}" "$prompt" \
    > "/out/$tag.jsonl" 2> "/out/$tag.stderr"
  rc=$?
  t1=$(date +%s)
  echo $((t1 - t0)) > "/out/$tag.seconds"
  if [ "$rc" -ne 0 ]; then
    echo "arm=$ARM_TOOL task=$task rc=$rc" >&2
    touch "/out/$tag.failed"
    overall_rc=1
  fi
done
if [ "$REVIEWED_PIXEL_HOOKS" = "1" ] && [ "${ARM_TOOL:-raw}" = "pixel" ]; then
  hook_receipt="/out/pixel-hook-${REP:-1}.jsonl"
  if ! python3 - "$hook_receipt" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
try:
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
except (OSError, ValueError):
    rows = []
if not rows or any(
    row.get("response_valid") is not True
    or row.get("emitted_context") is not True
    for row in rows
):
    sys.exit(1)
PY
  then
    task="${TASK_LIST[0]:-unknown}"
    echo "Pixel prompt-hook receipt is missing or invalid; excluding this run" >&2
    touch "/out/pixel-${task}-${REP:-1}.failed"
    overall_rc=1
  fi
fi
exit "$overall_rc"
