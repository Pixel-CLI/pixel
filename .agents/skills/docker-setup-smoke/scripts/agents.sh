#!/bin/sh
set -eu
# Agent sessions against fake-llm.py, after checks.sh left the personal
# settings restored: Claude Code, Codex and pi each run one session in a
# project where Pixel is installed, and their requests are the evidence.
# shellcheck source=/dev/null
. /etc/pixel-smoke.env
export PATH="$PIXEL_BIN_DIR:$PATH"
export PIXEL_METRICS=0
if [ ! -x "$PIXEL_BIN_DIR/pixel" ]; then
    # checks.sh uninstalled a binary pixel owns; put the same one back.
    cp /tmp/pixel-runner "$PIXEL_BIN_DIR/pixel"
fi
claude --version > /evidence/agent-versions.txt
codex --version >> /evidence/agent-versions.txt
printf 'pi %s\n' "$(pi --version)" >> /evidence/agent-versions.txt
cat /evidence/agent-versions.txt
project="$HOME/agents-project"
cp -R "$HOME/doctor-project" "$project"
rm -rf "$project/.pixel"
cd "$project"
pixel install --shell bash --json > /evidence/agents-install.json
pixel install --repo . --json > /evidence/agents-repo-install.json
pixel prepare-repo . > /evidence/agents-prepare.txt 2>&1
# A compliant model's first call, then a native search the guards may route.
export FAKE_LLM_COMMANDS='pixel search-content -F helper_1 src
grep -rn helper_1 src'
port=8765
serve() {
    FAKE_LLM_LOG="/evidence/$1-requests.jsonl" python3 /checks/fake-llm.py "$port" &
    server=$!
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        curl -fsS "http://127.0.0.1:$port/v1/models" >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    echo "FAIL fake model server did not start for $1" >&2
    exit 1
}
actions() { grep -c "\"command\":\"$1\"" .pixel/actions.jsonl || true; }
hook_runs() { grep -c "\"args\":\"run-hook [a-z-]* --provider $1" .pixel/actions.jsonl || true; }
# The last decision pi's project guard logged for a bash call.
pi_decision() {
    python3 - <<'PY'
import json
try:
    rows = [json.loads(line) for line in open(".pixel/pi-policy.jsonl")]
except OSError:
    rows = []
rows = [r for r in rows if r.get("tool") == "bash"]
print(f"{rows[-1]['kind']}: {rows[-1]['reason']}" if rows else "no decision logged")
PY
}
# One session whose model runs only the native grep: how the guard answers it.
grep_session() {
    agent=$1
    shift
    # A prefix assignment on a function call does not reliably reach its
    # children in every sh, so the exported value is swapped around it.
    saved=$FAKE_LLM_COMMANDS
    FAKE_LLM_COMMANDS='grep -rn helper_1 src'
    serve "$agent"
    FAKE_LLM_COMMANDS=$saved
    "$@" > "/evidence/$agent.out" 2> "/evidence/$agent.err" < /dev/null
    kill "$server"
    wait "$server" 2>/dev/null || true
    grep -q FAKE_LLM_DONE "/evidence/$agent.out"
}
# Evidence for one finished session: prompt delivered, both tool results fed
# back, the model's pixel call logged; the guard outcome is reported.
verify() {
    agent=$1 marker=$2 searches_before=$3 compat_before=$4
    kill "$server"
    wait "$server" 2>/dev/null || true
    grep -q FAKE_LLM_DONE "/evidence/$agent.out"
    python3 - "/evidence/$agent-requests.jsonl" "$marker" "$agent" <<'PY'
import json
import sys
path, marker, agent = sys.argv[1:]
bodies = [json.loads(line)["body"] or {} for line in open(path)]
turns = [b for b in bodies if b.get("tools")]
assert turns, f"{agent} sent no request offering tools"
first = json.dumps(turns[0], ensure_ascii=False)
assert marker in first, f"{agent}: the first model request lacks {marker!r}"
results = []
for body in turns:
    for m in body.get("messages", []):
        if isinstance(m.get("content"), list):
            results += [json.dumps(p.get("content")) for p in m["content"] if p.get("type") == "tool_result"]
    items = body.get("input") if isinstance(body.get("input"), list) else []
    results += [json.dumps(i.get("output")) for i in items
                if isinstance(i, dict) and i.get("type") == "function_call_output"]
last = results[-2:] if len(results) >= 2 else results
assert len(last) == 2 and all("src/m1.py:1:" in r for r in last), f"{agent} tool results: {last}"
if "<persisted-output>" in first and "deterministic repository facts" in first:
    print(f"NOTE {agent} received the Pixel prompt as a persisted-output preview, not inline")
PY
    test "$(actions search-content)" -gt "$searches_before"
    if [ "$agent" != pi ]; then
        # pi's guard blocks instead of rerouting; pi_decision reports it.
        compat=$(actions search-compat)
        if [ "$compat" -gt "$compat_before" ]; then
            echo "NOTE $agent guard routed the native grep through pixel"
        else
            echo "NOTE $agent guard left the native grep unchanged"
        fi
    fi
    echo "PASS $agent session received the Pixel prompt and ran pixel from its shell"
}
before_searches=$(actions search-content)
before_compat=$(actions search-compat)
serve claude
ANTHROPIC_BASE_URL="http://127.0.0.1:$port" CLAUDE_CODE_OAUTH_TOKEN=smoke-fake-oauth \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_AUTOUPDATER=1 \
    timeout 120 claude -p 'Where is helper_1 defined?' --model claude-smoke-fake \
    --dangerously-skip-permissions --output-format json > /evidence/claude.json 2> /evidence/claude.err
python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["result"])' /evidence/claude.json > /evidence/claude.out
verify claude 'deterministic repository facts' "$before_searches" "$before_compat"
before_searches=$(actions search-content)
before_compat=$(actions search-compat)
before_hooks=$(hook_runs codex)
serve codex
# The trust entry is what accepting Codex's project prompt writes.
# shellcheck disable=SC2120 # extra flags come from the grep_session call below
codex_exec() {
    SMOKE_FAKE_KEY=fake timeout 120 codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
        -c model_provider=smoke \
        -c "model_providers.smoke={name=\"smoke\",base_url=\"http://127.0.0.1:$port/v1\",env_key=\"SMOKE_FAKE_KEY\",wire_api=\"responses\"}" \
        -c "projects.\"$project\".trust_level=\"trusted\"" -m smoke-fake-model "$@" 'Where is helper_1 defined?'
}
codex_exec > /evidence/codex.out 2> /evidence/codex.err
verify codex 'pixel:managed:begin' "$before_searches" "$before_compat"
echo "NOTE codex ran $(( $(hook_runs codex) - before_hooks )) Pixel hook(s)"
# Codex runs a hook only once the user has reviewed it (`/hooks`); the bypass
# stands in for that review, to tell an unreviewed hook from a broken one.
before_hooks=$(hook_runs codex)
before_compat=$(actions search-compat)
grep_session codex-hooks-trusted codex_exec --dangerously-bypass-hook-trust
echo "NOTE with hook review bypassed, codex ran $(( $(hook_runs codex) - before_hooks )) Pixel hook(s)" \
    "and its guard routed $(( $(actions search-compat) - before_compat )) grep(s) through pixel"
before_searches=$(actions search-content)
before_compat=$(actions search-compat)
serve pi
cat > "$HOME/.pi/agent/models.json" <<JSON
{"providers": {"smoke": {"baseUrl": "http://127.0.0.1:$port", "api": "anthropic-messages",
  "apiKey": "fake", "models": [{"id": "smoke-fake-model"}]}}}
JSON
# --approve is the one-shot form of trusting the project, which loads its guard.
pi_print() {
    timeout 120 pi --print --approve --provider smoke --model smoke-fake-model 'Where is helper_1 defined?'
}
pi_print > /evidence/pi.out 2> /evidence/pi.err < /dev/null
verify pi 'pixel:managed:begin' "$before_searches" "$before_compat"
echo "NOTE pi guard under the default policy: $(pi_decision)"
pixel config policy enforce > /dev/null
grep_session pi-enforce pi_print
echo "NOTE pi guard under policy enforce: $(pi_decision)"
pixel config policy advisory > /dev/null
cp .pixel/actions.jsonl /evidence/agents-actions.jsonl
cp .pixel/pi-policy.jsonl /evidence/pi-policy.jsonl 2>/dev/null || true
