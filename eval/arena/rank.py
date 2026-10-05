#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Compare explicitly selected, paired arena runs."""
import argparse
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from score import codex_calls, events_of, load_result, score_answer, segments  # noqa: E402

RAW_SNAPSHOT_CONTEXT_POLICY = (
    "The raw arm removes Pixel-named instruction paths, then removes every "
    "case-insensitive line containing 'pixel' from AGENTS.md, .agents/rules/*.md, "
    "and .agents/skills/*/SKILL.md. A mixed-purpose line can therefore be removed "
    "along with Pixel guidance; compare captured manifests before claiming parity."
)


def median(values):
    known = [value for value in values if value is not None]
    return statistics.median(known) if known else None


def native_command_counts(transcript):
    counts = {}
    for call in codex_calls(events_of(transcript)):
        if call["kind"] != "bash":
            continue
        for argv in segments(call.get("command") or ""):
            if not argv:
                continue
            program = Path(argv[0]).name
            if program in ("pixel", "pixel-dev"):
                continue
            counts[program] = counts.get(program, 0) + 1
    return counts


def read_context_manifest(results, arm, rep):
    path = results / f"context-{arm}-{rep}.json"
    if not path.is_file():
        return None
    return json.loads(path.read_text())


def read_record(results, arm, task, rep, rubric):
    stem = f"{arm}-{task}-{rep}"
    transcript = results / f"{stem}.jsonl"
    failed_marker = results / f"{stem}.failed"
    seconds_file = results / f"{stem}.seconds"
    seconds = None
    if seconds_file.is_file():
        try:
            seconds = int(seconds_file.read_text().strip())
        except ValueError:
            seconds = None

    record = {
        "arm": arm,
        "task": task,
        "rep": rep,
        "status": "missing",
        "score": None,
        "max_score": sum(item["points"] for item in rubric.get("must", [])),
        "score_pct": None,
        "input_tokens": None,
        "generation_tokens": None,
        "cache_read_tokens": None,
        "cache_creation_tokens": None,
        "tokens": None,
        "cost_usd": None,
        "tool_calls": None,
        "pixel_calls": None,
        "native_search_calls": None,
        "native_command_count": None,
        "native_command_counts": {},
        "seconds": seconds,
        "reason": "no transcript or failure marker",
    }
    if failed_marker.exists():
        record.update(status="failed", reason="failure marker present")
        return record
    if not transcript.is_file():
        return record

    answer, metrics = load_result(transcript, "codex")
    if not metrics or not metrics.get("answered"):
        record.update(status="failed", reason="transcript did not complete")
        return record

    score, _, _ = score_answer(answer, rubric)
    maximum = record["max_score"]
    input_tokens = metrics.get("input_tokens")
    generation_tokens = metrics.get("gen_tokens")
    native_counts = native_command_counts(transcript)
    record.update(
        status="complete",
        score=score,
        score_pct=(100.0 * score / maximum) if maximum else 0.0,
        input_tokens=input_tokens,
        generation_tokens=generation_tokens,
        cache_read_tokens=metrics.get("cache_read_tokens"),
        cache_creation_tokens=metrics.get("cache_creation_tokens"),
        tokens=(input_tokens + generation_tokens)
        if input_tokens is not None and generation_tokens is not None
        else None,
        cost_usd=metrics.get("cost_usd"),
        tool_calls=metrics.get("tool_calls"),
        pixel_calls=metrics.get("pixel_calls"),
        native_search_calls=metrics.get("native_search_calls"),
        native_command_count=sum(native_counts.values()),
        native_command_counts=native_counts,
        reason=None,
    )
    return record


def rank_results(results, scenarios_dir, arms, tasks, reps, baseline_arm="raw",
                 run_metadata=None, assert_context_parity=False):
    results = Path(results)
    scenarios_dir = Path(scenarios_dir)
    if not results.is_dir():
        raise ValueError(f"results directory does not exist: {results}")
    if not arms or len(set(arms)) != len(arms):
        raise ValueError("select one or more distinct arms")
    if baseline_arm not in arms:
        raise ValueError(f"baseline arm {baseline_arm!r} must be selected")
    if not tasks or len(set(tasks)) != len(tasks):
        raise ValueError("select one or more distinct tasks")
    if reps < 1:
        raise ValueError("reps must be at least 1")

    run_metadata = preserve_run_metadata(
        results, arms, tasks, reps, baseline_arm, run_metadata
    )

    rubrics = {}
    for task in tasks:
        path = scenarios_dir / f"{task}.json"
        if not path.is_file():
            raise ValueError(f"selected scenario does not exist: {path}")
        rubrics[task] = json.loads(path.read_text())

    rows = []
    pairs = []
    context_manifests = {}
    context_checks = []
    rep_ids = [str(number) for number in range(1, reps + 1)]
    if assert_context_parity and not {"raw", "pixel"}.issubset(arms):
        raise ValueError("context parity assertion requires both raw and pixel arms")
    for rep in rep_ids:
        context_manifests[rep] = {
            arm: read_context_manifest(results, arm, rep) for arm in arms
        }
        if assert_context_parity:
            raw_context = context_manifests[rep]["raw"]
            pixel_context = context_manifests[rep]["pixel"]
            if raw_context is None or pixel_context is None:
                raise ValueError(f"context parity missing manifest for rep {rep}")
            if raw_context != pixel_context:
                raise ValueError(f"static context differs between raw and pixel in rep {rep}")
    task_summaries = {}
    for task in tasks:
        task_rows = []
        complete_rep_ids = []
        for rep in rep_ids:
            rep_rows = [read_record(results, arm, task, rep, rubrics[task]) for arm in arms]
            rows.extend(rep_rows)
            task_rows.extend(rep_rows)
            statuses = {row["arm"]: row["status"] for row in rep_rows}
            complete = all(status == "complete" for status in statuses.values())
            pairs.append({
                "task": task,
                "rep": rep,
                "status": "complete" if complete else "incomplete",
                "arms": statuses,
            })
            if complete:
                complete_rep_ids.append(rep)

            if assert_context_parity:
                pixel_row = next(row for row in rep_rows if row["arm"] == "pixel")
                check = {
                    "task": task,
                    "rep": rep,
                    "static_context_equal": True,
                    "pixel_calls": pixel_row["pixel_calls"],
                    "pixel_calls_zero": pixel_row["pixel_calls"] == 0,
                }
                context_checks.append(check)
                if not complete:
                    raise ValueError(f"context parity requires complete pair for {task} rep {rep}")
                if pixel_row["pixel_calls"] != 0:
                    raise ValueError(
                        f"context parity requires zero Pixel calls for {task} rep {rep}"
                    )

        rows_by_rep_arm = {
            (row["rep"], row["arm"]): row for row in task_rows
        }
        token_rep_ids = [
            rep for rep in complete_rep_ids
            if all(rows_by_rep_arm[(rep, arm)]["tokens"] is not None for arm in arms)
        ]
        seconds_rep_ids = [
            rep for rep in complete_rep_ids
            if all(rows_by_rep_arm[(rep, arm)]["seconds"] is not None for arm in arms)
        ]

        per_arm = {}
        for arm in arms:
            quality_rows = [
                row for row in task_rows
                if row["arm"] == arm and row["rep"] in complete_rep_ids
            ]
            token_rows = [
                rows_by_rep_arm[(rep, arm)] for rep in token_rep_ids
            ]
            seconds_rows = [
                rows_by_rep_arm[(rep, arm)] for rep in seconds_rep_ids
            ]
            arm_rows = [row for row in task_rows if row["arm"] == arm]
            per_arm[arm] = {
                "median_score": median([row["score"] for row in quality_rows]),
                "max_score": sum(item["points"] for item in rubrics[task].get("must", [])),
                "median_score_pct": median([row["score_pct"] for row in quality_rows]),
                "median_tokens": median([row["tokens"] for row in token_rows]),
                "median_seconds": median([row["seconds"] for row in seconds_rows]),
                "complete_pairs": len(complete_rep_ids),
                "quality_pairs": len(complete_rep_ids),
                "token_pairs": len(token_rep_ids),
                "seconds_pairs": len(seconds_rep_ids),
                "failed": sum(row["status"] == "failed" for row in arm_rows),
                "missing": sum(row["status"] == "missing" for row in arm_rows),
                "unknown_token_usage": sum(
                    row["status"] == "complete" and row["tokens"] is None
                    for row in arm_rows
                ),
            }

        baseline_rows = {
            rep: rows_by_rep_arm[(rep, baseline_arm)] for rep in token_rep_ids
        }
        token_savings = {}
        for arm in arms:
            deltas = []
            for rep in token_rep_ids:
                baseline = baseline_rows[rep]
                candidate = rows_by_rep_arm[(rep, arm)]
                if baseline["tokens"]:
                    deltas.append(100.0 * (baseline["tokens"] - candidate["tokens"])
                                  / baseline["tokens"])
            token_savings[arm] = median(deltas)

        task_summaries[task] = {
            "complete_pairs": len(complete_rep_ids),
            "expected_pairs": reps,
            "metric_pair_counts": {
                "quality": len(complete_rep_ids),
                "tokens": len(token_rep_ids),
                "seconds": len(seconds_rep_ids),
            },
            "arms": per_arm,
            "median_token_saving_pct_vs_baseline": token_savings,
        }

    summaries = {}
    for arm in arms:
        task_values = [
            task_summaries[task]["arms"][arm]["median_score_pct"]
            for task in tasks
            if task_summaries[task]["arms"][arm]["median_score_pct"] is not None
        ]
        token_values = [
            task_summaries[task]["arms"][arm]["median_tokens"]
            for task in tasks
            if task_summaries[task]["arms"][arm]["median_tokens"] is not None
        ]
        seconds_values = [
            task_summaries[task]["arms"][arm]["median_seconds"]
            for task in tasks
            if task_summaries[task]["arms"][arm]["median_seconds"] is not None
        ]
        token_task_count = sum(
            task_summaries[task]["metric_pair_counts"]["tokens"] > 0
            for task in tasks
        )
        summaries[arm] = {
            "macro_avg_score_pct": statistics.mean(task_values) if task_values else None,
            "mean_task_median_tokens": statistics.mean(token_values) if token_values else None,
            "mean_task_median_seconds": statistics.mean(seconds_values) if seconds_values else None,
            "tasks_with_completed_pairs": len(task_values),
            "tasks_with_token_pairs": token_task_count,
        }

    token_tiebreak_available = all(
        summaries[arm]["tasks_with_token_pairs"] == len(tasks)
        and summaries[arm]["mean_task_median_tokens"] is not None
        for arm in arms
    )
    ranked = sorted(
        arms,
        key=lambda arm: (
            -(summaries[arm]["macro_avg_score_pct"] or 0.0),
            summaries[arm]["mean_task_median_tokens"] if token_tiebreak_available else 0,
        ),
    )
    return {
        "run": {
            "results_dir": str(results.resolve()),
            "scenarios_dir": str(scenarios_dir.resolve()),
            **(run_metadata or {}),
        },
        "arms": arms,
        "tasks": tasks,
        "reps": reps,
        "baseline_arm": baseline_arm,
        "rows": rows,
        "pairs": pairs,
        "context_audit": {
            "required": assert_context_parity,
            "raw_snapshot_policy": RAW_SNAPSHOT_CONTEXT_POLICY,
            "pixel_snapshot_policy": "repo snapshot followed by pixel install before context capture",
            "manifests": context_manifests,
            "checks": context_checks,
        },
        "task_summaries": task_summaries,
        "summary": summaries,
        "ranked": ranked,
        "rank_basis": "quality_then_paired_tokens" if token_tiebreak_available
        else "quality_only_incomplete_paired_tokens",
    }


def preserve_run_metadata(results, arms, tasks, reps, baseline_arm, run_metadata):
    """Keep known provenance when reranking the same selected result set."""
    metadata = dict(run_metadata or {})
    previous_path = Path(results) / "ranking.json"
    if not previous_path.is_file():
        return metadata
    try:
        previous = json.loads(previous_path.read_text())
    except (OSError, json.JSONDecodeError):
        return metadata
    if (
        previous.get("arms") != list(arms)
        or previous.get("tasks") != list(tasks)
        or previous.get("reps") != reps
        or previous.get("baseline_arm") != baseline_arm
    ):
        return metadata

    previous_run = previous.get("run", {})
    for field in (
        "run_id",
        "model",
        "effort",
        "repo_snapshot",
        "pixel_image_id",
        "pixel_source_id",
        "codex_version",
    ):
        if metadata.get(field) is None and previous_run.get(field) is not None:
            metadata[field] = previous_run[field]
    return metadata


def show_report(report):
    print("task           arm      score median  tokens median  seconds median  pairs  failed  missing")
    for task in report["tasks"]:
        summary = report["task_summaries"][task]
        for arm in report["arms"]:
            row = summary["arms"][arm]
            score = (
                f"{row['median_score']:.1f}/{row['max_score']} ({row['median_score_pct']:.1f}%)"
                if row["median_score"] is not None else "—"
            )
            tokens = f"{row['median_tokens']:,.0f}" if row["median_tokens"] is not None else "—"
            seconds = f"{row['median_seconds']:g}" if row["median_seconds"] is not None else "—"
            print(f"{task:<14} {arm:<8} {score:>20} {tokens:>14} {seconds:>15}"
                  f" {row['complete_pairs']:>5}/{report['reps']:<1}"
                  f" {row['failed']:>7} {row['missing']:>8}")
        metric_pairs = summary["metric_pair_counts"]
        print(f"  paired metric samples: quality={metric_pairs['quality']}, "
              f"tokens={metric_pairs['tokens']}, seconds={metric_pairs['seconds']}")
        for pair in report["pairs"]:
            if pair["task"] == task and pair["status"] != "complete":
                details = ", ".join(f"{arm}={status}" for arm, status in pair["arms"].items())
                print(f"  excluded rep {pair['rep']}: {details}")

    context_audit = report["context_audit"]
    if context_audit["required"]:
        checks = context_audit["checks"]
        no_calls = sum(check["pixel_calls_zero"] for check in checks)
        print(f"static-context parity: {len(checks)}/{len(checks)}; "
              f"Pixel calls zero: {no_calls}/{len(checks)}")

    print("\noverall (macro-average of per-task median score percentages)")
    print(f"ranking basis: {report['rank_basis']}")
    for arm in report["ranked"]:
        summary = report["summary"][arm]
        quality = (f"{summary['macro_avg_score_pct']:.1f}%"
                   if summary["macro_avg_score_pct"] is not None else "—")
        tokens = (f"{summary['mean_task_median_tokens']:,.0f} mean task-median tokens"
                  if summary["mean_task_median_tokens"] is not None else "—")
        seconds = (f"{summary['mean_task_median_seconds']:.1f}s mean task-median time"
                   if summary["mean_task_median_seconds"] is not None else "—")
        print(f"  {arm:<8} quality {quality:>6}  {tokens}  {seconds}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", required=True)
    parser.add_argument("--scenarios-dir", required=True)
    parser.add_argument("--arms", nargs="+", required=True)
    parser.add_argument("--tasks", nargs="+", required=True,
                        help="scenario IDs to compare; only these tasks are scored")
    parser.add_argument("--reps", type=int, required=True,
                        help="expected numbered repetitions, starting at 1")
    parser.add_argument("--baseline-arm", default="raw")
    parser.add_argument("--run-id")
    parser.add_argument("--model")
    parser.add_argument("--effort")
    parser.add_argument("--repo-snapshot")
    parser.add_argument("--pixel-image-id")
    parser.add_argument("--pixel-source-id")
    parser.add_argument("--codex-version")
    parser.add_argument("--assert-context-parity", action="store_true",
                        help="require matching static Codex instructions and zero Pixel calls")
    args = parser.parse_args()
    try:
        report = rank_results(args.results, args.scenarios_dir, args.arms,
                              args.tasks, args.reps, args.baseline_arm,
                              {
                                  "run_id": args.run_id,
                                  "model": args.model,
                                  "effort": args.effort,
                                  "repo_snapshot": args.repo_snapshot,
                                  "pixel_image_id": args.pixel_image_id,
                                  "pixel_source_id": args.pixel_source_id,
                                  "codex_version": args.codex_version,
                                  "task_ids": args.tasks,
                                  "arms": args.arms,
                                  "reps": args.reps,
                              }, args.assert_context_parity)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    show_report(report)
    Path(args.results, "ranking.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
