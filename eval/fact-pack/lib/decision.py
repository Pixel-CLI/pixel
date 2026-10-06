# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The decision rule of the fact-pack experiment.

Pre-register verification for each task. A candidate arm (fact-auto or
fact-ondemand) must be non-inferior to both baselines (no-pixel and
current-pixel) on verified success: the one-sided 95% confidence bound of
the difference in verified-success proportions must be greater than -5
percentage points. Subject to that bound, the candidate must show at least a
10% paired task-family improvement in verified completion time, with its
interval reported.

The success comparison is paired by task family: the difference in
proportions is (b - c) / n over the discordant pairs, with the paired standard
error. The time comparison is the per-pair improvement (baseline - candidate)
/ baseline, reported as the median with a t-interval.
"""

import math
import statistics

# The non-inferiority margin: the one-sided 95% CI lower bound of the
# verified-success difference (candidate - baseline) must exceed this.
NON_INFERIORITY_MARGIN = -0.05

# The paired-improvement threshold on verified completion time.
PAIRED_IMPROVEMENT_THRESHOLD = 0.10

# One-sided 95% standard-normal quantile.
_Z_95 = 1.645

# Two-sided 95% standard-normal quantile (for the time-improvement interval).
_Z_975 = 1.96


def non_inferiority(candidate_successes, baseline_successes):
    """One-sided 95% CI for the paired difference in verified-success proportions.

    `candidate_successes` and `baseline_successes` are equal-length sequences
    of 0/1, paired by task family. Returns
    (difference, lower_bound, is_non_inferior): the candidate is non-inferior
    to the baseline when the lower bound exceeds -5pp.
    """
    n = len(candidate_successes)
    if n == 0 or n != len(baseline_successes):
        return (0.0, 0.0, False)
    b = sum(1 for c, b in zip(candidate_successes, baseline_successes) if c == 1 and b == 0)
    c = sum(1 for c, b in zip(candidate_successes, baseline_successes) if c == 0 and b == 1)
    diff = (b - c) / n
    # Paired standard error of the difference in proportions.
    variance = (b + c - (b - c) ** 2 / n) / (n * n)
    se = math.sqrt(variance) if variance > 0 else 0.0
    lower = diff - _Z_95 * se
    return (diff, lower, lower > NON_INFERIORITY_MARGIN)


def paired_time_improvement(candidate_times, baseline_times):
    """Paired improvement in verified completion time, with its interval.

    `candidate_times` and `baseline_times` are equal-length sequences of
    positive seconds, paired by task family. The per-pair improvement is
    (baseline - candidate) / baseline. Returns a dict with the median
    improvement, the mean improvement, a t-interval on the mean, and whether
    the median clears the 10% threshold.
    """
    n = len(candidate_times)
    if n == 0 or n != len(baseline_times):
        return {"median": 0.0, "mean": 0.0, "ci_low": 0.0, "ci_high": 0.0,
                "meets_threshold": False, "n": 0}
    improvements = [(b - c) / b for c, b in zip(candidate_times, baseline_times) if b > 0]
    if not improvements:
        return {"median": 0.0, "mean": 0.0, "ci_low": 0.0, "ci_high": 0.0,
                "meets_threshold": False, "n": 0}
    median = statistics.median(improvements)
    mean = statistics.mean(improvements)
    if len(improvements) > 1:
        sd = statistics.stdev(improvements)
        se = sd / math.sqrt(len(improvements))
        # t critical for the two-sided 95% interval, small-n table.
        t = _t_critical(len(improvements) - 1)
        ci_low = mean - t * se
        ci_high = mean + t * se
    else:
        ci_low = ci_high = mean
    return {
        "median": median,
        "mean": mean,
        "ci_low": ci_low,
        "ci_high": ci_high,
        "meets_threshold": median >= PAIRED_IMPROVEMENT_THRESHOLD,
        "n": len(improvements),
    }


def _t_critical(df: int) -> float:
    """Two-sided 95% t critical values for small degrees of freedom."""
    table = {1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447,
             7: 2.365, 8: 2.306, 9: 2.262, 10: 2.228, 11: 2.201, 12: 2.179,
             13: 2.160, 14: 2.145, 15: 2.131, 16: 2.120, 17: 2.110, 18: 2.101,
             19: 2.093, 20: 2.086, 21: 2.080, 22: 2.074, 23: 2.069, 24: 2.064,
             25: 2.060, 26: 2.056, 27: 2.052, 28: 2.048, 29: 2.045, 30: 2.042}
    if df in table:
        return table[df]
    if df <= 60:
        return 2.000
    if df <= 120:
        return 1.980
    return _Z_975


def cost_per_verified_completion(costs, successes) -> float | None:
    """Total cost per verified completion, or None with no completion."""
    completions = sum(1 for s in successes if s == 1)
    if completions == 0:
        return None
    return sum(costs) / completions


def unavailable_rate(packets) -> float:
    """Fraction of fact requests that contributed no packet."""
    if not packets:
        return 0.0
    unavailable = sum(1 for p in packets if not p.available)
    return unavailable / len(packets)


def misleading_candidates(facts: dict, ground_truth: set) -> list[str]:
    """Packet candidates that are not in the task's ground-truth file set."""
    targets = facts.get("targets", []) if isinstance(facts, dict) else []
    misleading = []
    for target in targets:
        path = target.get("path") if isinstance(target, dict) else None
        if path and path not in ground_truth:
            misleading.append(path)
    return misleading


def evaluate_candidate(candidate_id, trajectories_by_arm, packets_by_arm,
                       ground_truth_by_pair=None):
    """The full decision-rule verdict for one candidate arm.

    `trajectories_by_arm` maps arm id to a list of trajectory dicts (each with
    `verified`, `elapsed_s`, `api_usage`, `packet_bytes`). `packets_by_arm`
    maps arm id to a list of FactPacket. `ground_truth_by_pair` maps a task id
    (the packet's `pair_id`, unique across the corpus) to a set of file paths
    the task's answer must name; packet candidates outside it are reported as
    misleading. Returns a dict with the non-inferiority verdicts against each
    baseline, the paired time improvement against each baseline, and the
    reported cost components.
    """
    ground_truth_by_pair = ground_truth_by_pair or {}
    verdict = {"candidate": candidate_id, "non_inferiority": {}, "time": {},
               "cost": {}}
    cand = trajectories_by_arm.get(candidate_id, [])
    cand_packets = packets_by_arm.get(candidate_id, [])
    misleading = []
    for pkt in cand_packets:
        if pkt.available and pkt.facts:
            key = getattr(pkt, "pair_id", None)
            truth = ground_truth_by_pair.get(key, set())
            if truth:
                misleading.extend(misleading_candidates(pkt.facts, truth))
    for baseline in ("no-pixel", "current-pixel"):
        base = trajectories_by_arm.get(baseline, [])
        # Pair by (task_family, pair_id).
        base_by_pair = {(t["task_family"], t["pair_id"]): t for t in base}
        paired_cand, paired_base = [], []
        for t in cand:
            key = (t["task_family"], t["pair_id"])
            if key in base_by_pair:
                paired_cand.append(t)
                paired_base.append(base_by_pair[key])
        cand_succ = [1 if t["verified"] == "success" else 0 for t in paired_cand]
        base_succ = [1 if t["verified"] == "success" else 0 for t in paired_base]
        diff, lower, ok = non_inferiority(cand_succ, base_succ)
        verdict["non_inferiority"][baseline] = {
            "difference": diff, "lower_bound": lower, "non_inferior": ok,
            "n_pairs": len(paired_cand),
        }
        cand_times = []
        base_times = []
        for ct, bt in zip(paired_cand, paired_base):
            if ct["verified"] == "success" and bt["verified"] == "success":
                cand_times.append(ct["elapsed_s"])
                base_times.append(bt["elapsed_s"])
        verdict["time"][baseline] = paired_time_improvement(cand_times, base_times)
    # Cost components.
    cand_costs = [t.get("api_usage", {}).get("cost_usd", 0.0) for t in cand]
    cand_succ = [1 if t["verified"] == "success" else 0 for t in cand]
    verdict["cost"] = {
        "cost_per_verified_completion": cost_per_verified_completion(cand_costs, cand_succ),
        "total_cost": sum(cand_costs),
        "failures": sum(1 for t in cand if t["verified"] == "failure"),
        "timeouts": sum(1 for t in cand if t["verified"] == "timeout"),
        "unavailable_rate": unavailable_rate(cand_packets),
        "injected_packet_tokens": sum(t.get("packet_bytes", 0) for t in cand) // 4,
        "misleading_candidates": misleading,
    }
    return verdict
