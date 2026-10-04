#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Held-out check for ab-rename-mutants-per-shard. Runs in the agent's
# worktree after the agent exits ($HELDOUT = this directory's copy).
# Passes when no whole-word MUTANTS_PER_SHARD is left in any tracked file,
# the code, the workflow comment and the rule all say MUTANTS_PER_JOB, and
# the gate's arithmetic is unchanged.
set -u
fail=0
if git grep -n -w MUTANTS_PER_SHARD -- . ':!eval'; then
  echo "FAIL: MUTANTS_PER_SHARD is still spelled above"
  fail=1
fi
uses=$(grep -c -w MUTANTS_PER_JOB scripts/mutants-gate.py || true)
if [ "${uses:-0}" -lt 3 ]; then
  echo "FAIL: scripts/mutants-gate.py names MUTANTS_PER_JOB $uses time(s), expected the definition, the comment and the use"
  fail=1
fi
for file in .agents/rules/test-campaigns.md .github/workflows/mutants.yml; do
  grep -q -w MUTANTS_PER_JOB "$file" || { echo "FAIL: $file does not name MUTANTS_PER_JOB"; fail=1; }
done
python3 - <<'PY' || fail=1
import importlib.util
spec = importlib.util.spec_from_file_location("gate", "scripts/mutants-gate.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
assert gate.MUTANTS_PER_JOB == 10, gate.MUTANTS_PER_JOB
assert not hasattr(gate, "MUTANTS_PER_SHARD"), "the old name is still bound"
assert gate.MAX_SHARDS == 15, gate.MAX_SHARDS
assert [gate.shard_count(n) for n in (0, 1, 10, 11, 25, 150, 10_000)] == [0, 1, 1, 2, 3, 15, 15]
print("arithmetic unchanged")
PY
[ "$fail" = 0 ] || exit 1
echo PASS
