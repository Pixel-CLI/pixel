# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Read task-family fields for run.py, and validate a family corpus.

  scenario.py get <family.json> <key> [default]   print one field ("" if absent)
  scenario.py list <families-dir>                 ids, sorted
  scenario.py check <families-dir> [repo]         validate every family; exit 1 on a defect

A task family is one JSON file named <id>.json:

  id                      required, matches the file name
  tasks                   required, non-empty; each task is one prompt run in
                          every arm, and is the pairing unit across arms
  tasks[].id              required, unique across the corpus
  tasks[].prompt          required, non-empty
  tasks[].mode            "answer" (default) or "edit"
  tasks[].must / never    regex rubric (answer tasks need `must`)
  tasks[].verifier        edit tasks: {"script": "check.sh", "timeout_s": N}
  tasks[].commit          40-hex SHA the scratch worktree is pinned to
  tasks[].ground_truth    optional list of file paths the task's answer must
                          name; the harness reports packet candidates outside
                          it as misleading
"""

import json
import re
import subprocess
import sys
from pathlib import Path


def load(path: Path) -> dict:
    return json.loads(path.read_text())


def get(path: str, key: str, default: str = "") -> str:
    value = load(Path(path))
    for part in key.split("."):
        value = value.get(part) if isinstance(value, dict) else None
    if value is None:
        return default
    return value if isinstance(value, str) else json.dumps(value)


def family_ids(directory: str) -> list[str]:
    return sorted(p.stem for p in Path(directory).glob("*.json"))


def check(directory: str, repo: str | None = None) -> list[str]:
    """Every defect of the corpus, as one line each (empty when sound)."""
    defects = []
    paths = sorted(Path(directory).glob("*.json"))
    ids = [p.stem for p in paths]
    seen_task_ids = set()
    for path in paths:
        family = load(path)
        fid = path.stem
        where = f"{path.name}:"
        if family.get("id") != fid:
            defects.append(f"{where} id {family.get('id')!r} differs from the file name")
        tasks = family.get("tasks")
        if not isinstance(tasks, list) or not tasks:
            defects.append(f"{where} tasks must be a non-empty list")
            continue
        for task in tasks:
            tid = task.get("id", "")
            if not tid:
                defects.append(f"{where} a task has no id")
            elif tid in seen_task_ids:
                defects.append(f"{where} task id {tid!r} is not unique across the corpus")
            else:
                seen_task_ids.add(tid)
            if not str(task.get("prompt", "")).strip():
                defects.append(f"{where} task {tid!r} has an empty prompt")
            mode = task.get("mode", "answer")
            if mode not in ("answer", "edit"):
                defects.append(f"{where} task {tid!r} mode {mode!r}")
            if mode == "answer" and not task.get("must"):
                defects.append(f"{where} answer task {tid!r} needs a `must` rubric")
            for kind in ("must", "never"):
                for item in task.get(kind, []):
                    try:
                        re.compile(item["pattern"])
                    except (re.error, KeyError) as error:
                        defects.append(f"{where} task {tid!r} bad {kind} pattern {item!r}: {error}")
            if mode == "edit":
                verifier = task.get("verifier") or {}
                if not verifier.get("script") or not isinstance(verifier.get("timeout_s"), int) \
                        or verifier["timeout_s"] <= 0:
                    defects.append(f"{where} edit task {tid!r} needs verifier.script and a positive timeout_s")
            commit = task.get("commit", "")
            if commit and repo:
                if not re.fullmatch(r"[0-9a-f]{40}", commit):
                    defects.append(f"{where} task {tid!r} commit must be a full 40-hex SHA, got {commit!r}")
                else:
                    exists = subprocess.run(["git", "-C", repo, "cat-file", "-e", f"{commit}^{{commit}}"],
                                            capture_output=True)
                    if exists.returncode != 0:
                        defects.append(f"{where} task {tid!r} commit {commit} is not in {repo}")
    return defects


def main() -> None:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    command = sys.argv[1]
    if command == "get":
        print(get(sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else ""))
    elif command == "list":
        print("\n".join(family_ids(sys.argv[2])))
    elif command == "check":
        defects = check(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
        for line in defects:
            print(line)
        sys.exit(1 if defects else 0)
    else:
        print(__doc__, file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
