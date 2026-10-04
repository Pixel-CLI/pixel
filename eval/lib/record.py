"""Merge fields into a JSON sidecar (run.sh's per-run and campaign records).

  record.py <file.json> key=value ... key:=<json> ... key@=<file>

`key=value` stores a string, `key:=` a JSON literal (number, bool, null,
object), `key@=` the sha256 of a file or, for a directory, of its sorted
(path, bytes) listing ("" when it does not exist). Existing keys not named
are kept, so a sidecar can be filled in several steps.
"""
import hashlib
import json
import sys
from pathlib import Path


def digest(path: Path) -> str:
    if path.is_file():
        return hashlib.sha256(path.read_bytes()).hexdigest()
    if path.is_dir():
        h = hashlib.sha256()
        for item in sorted(p for p in path.rglob("*") if p.is_file()):
            h.update(str(item.relative_to(path)).encode() + b"\0" + item.read_bytes() + b"\0")
        return h.hexdigest()
    return ""


def main() -> None:
    target = Path(sys.argv[1])
    data = json.loads(target.read_text()) if target.exists() else {}
    for arg in sys.argv[2:]:
        if ":=" in arg and arg.index(":=") < arg.index("=") + 1:
            key, value = arg.split(":=", 1)
            data[key] = json.loads(value)
        elif "@=" in arg and arg.index("@=") < arg.index("=") + 1:
            key, value = arg.split("@=", 1)
            data[key] = digest(Path(value))
        else:
            key, value = arg.split("=", 1)
            data[key] = value
    target.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
