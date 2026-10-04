#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

set -eu
# The bootstrap names the binary's directory and whether pixel owns it.
# shellcheck source=/dev/null
. /etc/pixel-smoke.env
export PATH="$PIXEL_BIN_DIR:$PATH"
export PIXEL_METRICS=0
test "$(command -v pixel)" = "$PIXEL_BIN_DIR/pixel"
report_ok() {
    python3 - "$1" <<'PY'
import json
import sys
with open(sys.argv[1]) as report:
    data = json.load(report)
assert data["ok"] is True, data
PY
}
# Every red check must say how to repair it.
reds_have_fixes() {
    python3 - "$1" <<'PY'
import sys
lines = open(sys.argv[1]).read().splitlines()
reds = [i for i, line in enumerate(lines) if line.lstrip().startswith("[red]")]
assert reds, "expected red checks in a repository that was never prepared"
for i in reds:
    assert i + 1 < len(lines) and lines[i + 1].lstrip().startswith("fix: "), lines[i]
PY
}
managed_state() {
    find "$HOME/.claude" "$HOME/.codex" "$HOME/.pi" "$HOME/.local/share/pixel" "$HOME/.bashrc" \
        -type f ! -name '*.pixel-bak.*' -exec sha256sum {} + 2>/dev/null | sort
}
# Personal settings a user had before Pixel: install keeps them, uninstall restores them.
mkdir -p "$HOME/.claude" "$HOME/.codex" "$HOME/.pi/agent"
cat > "$HOME/.claude/settings.json" <<'JSON'
{
  "model": "opus",
  "permissions": {"allow": ["Bash(ls:*)"]},
  "hooks": {
    "PreToolUse": [{"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "/usr/local/bin/my-edit-guard"}]}],
    "SessionStart": [{"hooks": [{"type": "command", "command": "echo my-session-hook"}]}]
  }
}
JSON
printf '# my global claude rules\n' > "$HOME/.claude/CLAUDE.md"
printf 'model = "gpt-5"\n\n[profiles.work]\nmodel = "o3"\n' > "$HOME/.codex/config.toml"
printf '{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo my-codex-hook"}]}]}}\n' \
    > "$HOME/.codex/hooks.json"
printf '# my codex rules\n' > "$HOME/.codex/AGENTS.md"
printf '{"defaultProvider": "anthropic"}\n' > "$HOME/.pi/agent/settings.json"
printf '# my pi rules\n' > "$HOME/.pi/agent/AGENTS.md"
printf '# my pi system append\n' > "$HOME/.pi/agent/APPEND_SYSTEM.md"
mkdir /evidence/personal-before
(cd "$HOME" && cp -R --parents .claude .codex .pi /evidence/personal-before/)
(cd /evidence/personal-before && find . -type f | sort) > /evidence/personal-files.txt
personal_kept() {
    python3 - /evidence/personal-before "$1" <<'PY'
import json
import pathlib
import sys
before, phase = pathlib.Path(sys.argv[1]), sys.argv[2]
home = pathlib.Path.home()
def load(root, rel):
    return json.loads((root / rel).read_text())
for rel in (".claude/settings.json", ".codex/hooks.json"):
    old, new = load(before, rel), load(home, rel)
    if phase == "uninstalled":
        assert new == old, (rel, new)
        continue
    for key, value in old.items():
        if key != "hooks":
            assert new.get(key) == value, (rel, key, new.get(key))
    for event, groups in old["hooks"].items():
        for group in groups:
            assert group in new["hooks"].get(event, []), (rel, event, group)
for rel in (".claude/CLAUDE.md", ".codex/AGENTS.md", ".pi/agent/AGENTS.md", ".pi/agent/settings.json"):
    assert (home / rel).read_bytes() == (before / rel).read_bytes(), rel
for rel in (".codex/config.toml", ".pi/agent/APPEND_SYSTEM.md"):
    old, new = (before / rel).read_text(), (home / rel).read_text()
    if phase == "uninstalled":
        assert new == old, (rel, new)
    else:
        assert all(line in new.splitlines() for line in old.splitlines()), (rel, new)
PY
}
mkdir "$HOME/project"
cd "$HOME/project"
git init -q
git config user.email smoke@example.com
git config user.name smoke
mkdir src
for i in 1 2 3; do
    printf 'def helper_%s(value):\n    """Add %s."""\n    return value + %s\n\n\ndef caller_%s():\n    return helper_%s(1)\n' \
        "$i" "$i" "$i" "$i" "$i" > "src/m$i.py"
done
printf 'from src.m1 import helper_1\n\nprint(helper_1(2))\n' > main.py
printf '# user instruction\n' > AGENTS.md
cp AGENTS.md /evidence/original-AGENTS.md
git add -A
git commit -qm init
cp -R "$HOME/project" "$HOME/doctor-project"
pixel install --shell bash --json > /evidence/install-1.json
report_ok /evidence/install-1.json
test -s "$HOME/.local/share/pixel/agent-prompt.md"
cp "$HOME/.local/share/pixel/agent-prompt.md" /evidence/first-prompt.md
managed_state > /evidence/managed-1.txt
personal_kept installed
echo 'PASS global install keeps personal settings, hooks and instructions'
pixel install --shell bash --json > /evidence/install-2.json
report_ok /evidence/install-2.json
managed_state > /evidence/managed-2.txt
cmp /evidence/managed-1.txt /evidence/managed-2.txt
echo 'PASS global reinstall leaves every managed file unchanged'
# Read the persisted value without the runner's metrics override.
env -u PIXEL_METRICS pixel config metrics off --global
env -u PIXEL_METRICS pixel config metrics > /evidence/metrics.txt
grep -q '^metrics: off' /evidence/metrics.txt
test -s "$HOME/.pixel/config.yaml"
echo 'PASS global config persistence'
test ! -e .pixel/graph.db
start=$(date +%s%N)
pixel audit > /evidence/audit-1.out 2> /evidence/audit-1.err
echo "audit-1-ms: $(( ($(date +%s%N) - start) / 1000000 ))" > /evidence/audit-timing.txt
grep -qx 'pixel: audit: no code graph yet, building it (first run only)' /evidence/audit-1.err
grep -q '^indexed: python 4/4 ' /evidence/audit-1.out
grep -q '^total, 3 of 4 indexed source files' /evidence/audit-1.out
pixel audit > /evidence/audit-2.out 2> /evidence/audit-2.err
if grep -q 'building it' /evidence/audit-2.err; then
    echo 'FAIL second audit rebuilt the graph' >&2
    exit 1
fi
echo 'PASS first audit announces the graph build and reports coverage; second reuses it'
cd "$HOME/doctor-project"
if pixel doctor . --fail-on yellow > /evidence/doctor-before.txt 2>&1; then
    echo 'FAIL doctor passed in a repository that was never prepared' >&2
    exit 1
fi
reds_have_fixes /evidence/doctor-before.txt
pixel doctor . --fix --fail-on red > /evidence/doctor-fix.txt 2>&1
pixel doctor . --fail-on red > /evidence/doctor-after.txt 2>&1
# After --fix only the Codex hook review may stay yellow: no command can
# review a hook for the user, so it must say which step does.
python3 - /evidence/doctor-after.txt <<'PY'
import sys
lines = open(sys.argv[1]).read().splitlines()
yellow = [line.strip() for line in lines if line.lstrip().startswith("[yellow]")]
allowed = ("[yellow] install.codex-hook-review:", "[yellow] repo.codex-hook-review:")
for line in yellow:
    assert line.startswith(allowed), line
    assert "`/hooks`" in line, line
PY
if grep -q 'codex-hook-review' /evidence/doctor-after.txt; then
    echo 'NOTE doctor reports Pixel hooks Codex has not reviewed, naming /hooks'
fi
echo 'PASS doctor names a fix for each red check, and --fix repairs them'
cd "$HOME/project"
pixel install --repo . --json > /evidence/repo-install.json
report_ok /evidence/repo-install.json
grep -q '^# user instruction$' AGENTS.md
pixel install --repo . --json > /evidence/repo-install-2.json
report_ok /evidence/repo-install-2.json
echo 'PASS project install preserves user instructions, reinstall succeeds'
# A personal Bash hook holds the Claude guard back: install says what the
# user can do, and doctor stays yellow with no fix line, since no command
# can choose between their hook and the guard.
cp -R "$HOME/doctor-project" "$HOME/guard-project"
cp "$HOME/.claude/settings.json" /evidence/settings-before-bash-hook.json
python3 - "$HOME/.claude/settings.json" <<'PY'
import json
import sys
path = sys.argv[1]
data = json.load(open(path))
data["hooks"]["PreToolUse"].append(
    {"matcher": "Bash", "hooks": [{"type": "command", "command": "/usr/local/bin/my-guard"}]})
json.dump(data, open(path, "w"), indent=2)
PY
(cd "$HOME/guard-project" && pixel install --repo . --json > /evidence/guard-install.json)
report_ok /evidence/guard-install.json
(cd "$HOME/guard-project" && pixel doctor . --only repo.claude-hooks --fail-on red > /evidence/guard-doctor.txt 2>&1)
python3 - /evidence/guard-install.json /evidence/guard-doctor.txt <<'PY'
import json
import sys
steps = {s["id"]: s for s in json.load(open(sys.argv[1]))["steps"]}
step = steps["hooks.claude"]
assert step["status"] == "yellow", step
summary = step["summary"]
assert "`/usr/local/bin/my-guard`" in summary and "also rewrites shell calls" in summary, summary
if "narrow that hook's `matcher`" not in summary:
    print("NOTE this pixel does not say how to get the guard past a personal Bash hook (AG-05)")
    sys.exit(0)
lines = open(sys.argv[2]).read().splitlines()
yellow = [i for i, line in enumerate(lines) if line.lstrip().startswith("[yellow] repo.claude-hooks:")]
assert len(yellow) == 1, lines
assert "narrow that hook's `matcher`" in lines[yellow[0]], lines
following = lines[yellow[0] + 1] if yellow[0] + 1 < len(lines) else ""
assert not following.lstrip().startswith("fix: "), lines
print("PASS a personal Bash hook holding the guard back is named with what to do, by install and doctor")
PY
cp /evidence/settings-before-bash-hook.json "$HOME/.claude/settings.json"
pixel config classify off
if pixel classify test --label yes --label no > /evidence/classify.out 2>/evidence/classify.err; then
    echo 'FAIL disabled classify succeeded' >&2
    exit 1
fi
test ! -s /evidence/classify.out
grep -q 'classify is disabled' /evidence/classify.err
echo 'PASS disabled classify refuses without a model'
pixel uninstall --repo . --json > /evidence/repo-uninstall.json
report_ok /evidence/repo-uninstall.json
cmp /evidence/original-AGENTS.md AGENTS.md
echo 'PASS project uninstall restores user instructions exactly'
# Uninstall deletes a binary pixel owns; a copy runs the second check.
cp "$PIXEL_BIN_DIR/pixel" /tmp/pixel-runner
pixel uninstall --shell bash --json > /evidence/uninstall-1.json
report_ok /evidence/uninstall-1.json
test ! -e "$HOME/.local/share/pixel/agent-prompt.md"
personal_kept uninstalled
if [ "$PIXEL_OWNS_BINARY" = 1 ]; then
    test ! -e "$PIXEL_BIN_DIR/pixel"
    /tmp/pixel-runner uninstall --shell bash --binary-path "$PIXEL_BIN_DIR/pixel" --json > /evidence/uninstall-2.json
else
    # A package manager's binary stays for that manager to remove.
    test -x "$PIXEL_BIN_DIR/pixel"
    pixel uninstall --shell bash --json > /evidence/uninstall-2.json
    test -x "$PIXEL_BIN_DIR/pixel"
fi
report_ok /evidence/uninstall-2.json
personal_kept uninstalled
echo 'PASS global uninstall restores personal settings and hooks; repeat succeeds'
# Files left behind are reported, not failed: the contract above is what must hold.
# Shell profile backups sit at the root of the home.
(cd "$HOME" && { find .claude .codex .pi .local/share/pixel .pixel -type f 2>/dev/null;
    find . -maxdepth 1 -type f -name '*.pixel-bak.*' | sed 's|^\./||'; } | sed 's|^|./|' | sort) \
    | comm -13 /evidence/personal-files.txt - > /evidence/residue.txt
echo "NOTE $(wc -l < /evidence/residue.txt) file(s) left after uninstall, listed in residue.txt"
# The backups among them must be exactly those the last uninstall reported,
# with a removal command a shell reads back as the same paths; the rest is
# the pixel config the checks set with `pixel config --global`.
python3 - /evidence/uninstall-2.json /evidence/residue.txt <<'PY'
import json
import os
import shlex
import sys
report, residue = sys.argv[1], sys.argv[2]
home = os.path.expanduser("~")
steps = [s for s in json.load(open(report))["steps"] if s["id"] == "backups"]
left = open(residue).read().splitlines()
if not steps:
    print("NOTE this pixel does not report the backups uninstall leaves (IN-03)")
    sys.exit(0)
detail = steps[0].get("detail")
words = shlex.split(detail) if detail else ["rm", "--"]
assert words[:2] == ["rm", "--"], detail
reported = sorted("./" + os.path.relpath(p, home) for p in words[2:])
backups = sorted(line for line in left if ".pixel-bak." in line)
assert reported == backups, (reported, backups)
others = sorted(line for line in left if ".pixel-bak." not in line)
assert others == ["./.pixel/config.yaml"], others
print(f"PASS uninstall reports the {len(reported)} backup(s) left, and how to remove them")
PY
