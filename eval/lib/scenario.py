# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Read scenario fields for run.sh, and validate a scenario corpus.

  scenario.py get <scenario.json> <key> [default]   print one field ("" if absent)
  scenario.py list <scenarios-dir> <suite>          ids whose "suite" matches, sorted
  scenario.py check <scenarios-dir> <heldout-dir> [repo]
                                                    validate every scenario; exit 1 on a defect

A scenario is one JSON file named <id>.json:

  id, prompt                 required
  must / never               regex rubric (score.py); an answer task needs `must`
  suite                      optional corpus name (run.sh SUITE=<name>)
  task_class                 required in a suite: one of TASK_CLASSES
  mode                       "answer" (default) or "edit"
  commit                     40-hex SHA the scratch worktree is pinned to
                             (default: the harness HEAD)
  max_turns                  optional per-scenario turn budget (claude)
  warm                       optional shell command run in the worktree before
                             the agent starts (builds the cache, not timed)
  verifier                   edit tasks: {"script": "check.sh", "timeout_s": N};
                             the script lives in heldout/<id>/ and never enters
                             the agent's worktree before the agent exits
"""
import json
import re
import subprocess
import sys
from pathlib import Path

TASK_CLASSES = (
    "exact-identifier",
    "concept",
    "callers-impact",
    "rename",
    "bugfix",
    "feature",
    "git-ops",
    "non-code",
    "explanation",
)


def load(path: Path) -> dict:
    return json.loads(path.read_text())


def get(path: str, key: str, default: str = "") -> str:
    value = load(Path(path))
    for part in key.split("."):
        value = value.get(part) if isinstance(value, dict) else None
    if value is None:
        return default
    return value if isinstance(value, str) else json.dumps(value)


def suite_ids(directory: str, suite: str) -> list[str]:
    return sorted(p.stem for p in Path(directory).glob("*.json") if load(p).get("suite") == suite)


def check(directory: str, heldout: str, repo: str | None) -> list[str]:
    """Every defect of the corpus, as one line each (empty when sound)."""
    defects = []
    paths = sorted(Path(directory).glob("*.json"))
    ids = [p.stem for p in paths]
    for path in paths:
        s = load(path)
        sid = path.stem
        where = f"{path.name}:"
        if s.get("id") != sid:
            defects.append(f"{where} id {s.get('id')!r} differs from the file name")
        if not str(s.get("prompt", "")).strip():
            defects.append(f"{where} empty prompt")
        # score.py matches transcripts by `<id>-<arm>.<cli>`: an id that is
        # another id plus "-..." would make that match ambiguous.
        for other in ids:
            if other != sid and other.startswith(sid + "-"):
                defects.append(f"{where} id is a prefix of {other}")
        for kind in ("must", "never"):
            for item in s.get(kind, []):
                try:
                    re.compile(item["pattern"])
                except (re.error, KeyError) as error:
                    defects.append(f"{where} bad {kind} pattern {item!r}: {error}")
        if "suite" not in s:
            continue
        mode = s.get("mode", "answer")
        if s.get("task_class") not in TASK_CLASSES:
            defects.append(f"{where} task_class {s.get('task_class')!r} not in {TASK_CLASSES}")
        if mode not in ("answer", "edit"):
            defects.append(f"{where} mode {mode!r}")
        commit = s.get("commit", "")
        if not re.fullmatch(r"[0-9a-f]{40}", commit):
            defects.append(f"{where} commit must be a full 40-hex SHA, got {commit!r}")
        elif repo:
            exists = subprocess.run(["git", "-C", repo, "cat-file", "-e", f"{commit}^{{commit}}"],
                                    capture_output=True)
            if exists.returncode != 0:
                defects.append(f"{where} commit {commit} is not in {repo}")
            else:
                # The answer must not travel inside the tree the agent sees.
                for leak in (f"eval/scenarios/{sid}.json", f"eval/heldout/{sid}"):
                    seen = subprocess.run(["git", "-C", repo, "cat-file", "-e", f"{commit}:{leak}"],
                                          capture_output=True)
                    if seen.returncode == 0:
                        defects.append(f"{where} pinned commit already contains {leak}")
        if mode == "answer" and not s.get("must"):
            defects.append(f"{where} an answer task needs a `must` rubric")
        if mode == "edit":
            verifier = s.get("verifier") or {}
            script = Path(heldout) / sid / str(verifier.get("script", ""))
            if not verifier.get("script") or not script.is_file():
                defects.append(f"{where} edit task without heldout/{sid}/<script>")
            if not isinstance(verifier.get("timeout_s"), int) or verifier["timeout_s"] <= 0:
                defects.append(f"{where} verifier.timeout_s must be a positive integer")
    return defects


def main() -> None:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    command = sys.argv[1]
    if command == "get":
        print(get(sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else ""))
    elif command == "list":
        print("\n".join(suite_ids(sys.argv[2], sys.argv[3])))
    elif command == "check":
        defects = check(sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else None)
        for line in defects:
            print(line)
        sys.exit(1 if defects else 0)
    else:
        print(__doc__, file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
