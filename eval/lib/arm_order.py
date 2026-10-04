"""Counterbalanced arm order for one (repetition, host/scenario) cell.

Port of `armOrder` in eval/controlled.ts ("sha256-permutation-three-
repetition-rotation-v1"), generalised from its three fixed arms to any arm
list: a seeded Fisher-Yates shuffle keyed on (seed, case, rep // n), rotated
by rep % n, so over every n consecutive repetitions each arm runs once in
each position. With three arms the permutation is index-for-index the one
controlled.ts computes (scripts/test-agent-ab-harness.py pins that).

Usage: arm_order.py <seed> <rep0> <case> <arm>...   (rep0 counts from 0)
Prints the arms in run order, one per line.
"""
import hashlib
import json
import sys

ALGORITHM = "sha256-permutation-n-repetition-rotation-v1"


def arm_order(seed: str, rep: int, case: str, arms: list[str]) -> list[str]:
    n = len(arms)
    order = list(arms)
    # JSON.stringify spelling: no spaces, non-ASCII kept as UTF-8.
    key = json.dumps([seed, case, rep // n], separators=(",", ":"), ensure_ascii=False)
    digest = hashlib.sha256(key.encode()).digest()
    for i in range(n - 1, 0, -1):
        j = digest[i] % (i + 1)
        order[i], order[j] = order[j], order[i]
    offset = rep % n
    return order[offset:] + order[:offset]


def main() -> None:
    if len(sys.argv) < 5:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    seed, rep, case, arms = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4:]
    print("\n".join(arm_order(seed, rep, case, arms)))


if __name__ == "__main__":
    main()
