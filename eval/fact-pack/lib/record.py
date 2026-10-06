# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Frozen-input and trajectory recording for the fact-pack experiment.

Two records are written per run:

  frozen input  the fact-result identity: normalized task text, repository
                commit plus dirty-content signature, index
                base/delta/overlay/tombstone state, graph generation and
                signature, algorithm version, limit, and the disabled
                activity-reranking and semantic-fallback flags. Elapsed time
                is never part of the fact bytes; it is telemetry, recorded
                beside them.

  trajectory    model and harness versions, arm, task-family and pair
                identifiers, verified success/failure/timeout, elapsed time,
                API/token usage, Pixel/tool calls, files and source regions
                inspected, test time, edits, and rework (edits or tests after
                an initially failing verification).

Both are JSONL: one JSON object per line, appended in run order.
"""

import json
import subprocess
import time
from pathlib import Path

# The frozen-input fields, in the order the protocol lists them. These identify
# a fact result; a change to any of them means a different fact result.
FROZEN_INPUT_FIELDS = (
    "task",
    "limit",
    "index_commit_oid",
    "index_base_files",
    "index_delta_files",
    "index_overlay_files",
    "index_tombstones",
    "graph_generation",
    "graph_signature",
    "algorithm_version",
    "activity_reranking",
    "semantic_fallback",
)


def frozen_input_from_packet(packet) -> dict | None:
    """The frozen inputs that identify a fact result, from a FactPacket.

    Returns None when the packet is unavailable (no fact result was served).
    """
    if packet.inputs is None:
        return None
    return {field: packet.inputs.get(field) for field in FROZEN_INPUT_FIELDS}


def repo_signature(root: str) -> dict:
    """Repository commit plus dirty-content signature, for a frozen input."""
    commit = subprocess.run(["git", "-C", root, "rev-parse", "HEAD"],
                            capture_output=True, text=True)
    dirty = subprocess.run(["git", "-C", root, "status", "--porcelain"],
                           capture_output=True, text=True)
    return {
        "commit": commit.stdout.strip() if commit.returncode == 0 else None,
        "dirty_sha256": _sha256_text(dirty.stdout),
    }


def _sha256_text(text: str) -> str:
    import hashlib
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def append_jsonl(path: Path, record: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(record, sort_keys=True) + "\n")


def record_frozen_input(path: Path, packet, arm: str, task_family: str,
                        pair_id: str, repo: dict) -> None:
    record = {
        "kind": "frozen_input",
        "arm": arm,
        "task_family": task_family,
        "pair_id": pair_id,
        "repo": repo,
        "fact": frozen_input_from_packet(packet),
        "packet_bytes": packet.packet_bytes,
        "kept_targets": packet.kept_targets,
        "total_targets": packet.total_targets,
        "recorded_unix": time.time(),
    }
    append_jsonl(path, record)


def build_trajectory(*, model: str, harness: str, arm: str, task_family: str,
                     pair_id: str, verified: str, elapsed_s: float,
                     api_usage: dict | None = None, pixel_calls: int = 0,
                     tool_calls: int = 0, files_inspected=None,
                     test_time_s: float | None = None, edits: int = 0,
                     rework: bool = False, packet_bytes: int = 0) -> dict:
    """One trajectory record. `verified` is "success" | "failure" | "timeout"."""
    return {
        "kind": "trajectory",
        "model": model,
        "harness": harness,
        "arm": arm,
        "task_family": task_family,
        "pair_id": pair_id,
        "verified": verified,
        "elapsed_s": elapsed_s,
        "api_usage": api_usage or {},
        "pixel_calls": pixel_calls,
        "tool_calls": tool_calls,
        "files_inspected": files_inspected or [],
        "test_time_s": test_time_s,
        "edits": edits,
        "rework": rework,
        "packet_bytes": packet_bytes,
    }


def record_trajectory(path: Path, trajectory: dict) -> None:
    append_jsonl(path, trajectory)


def read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    records = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            records.append(json.loads(line))
    return records
