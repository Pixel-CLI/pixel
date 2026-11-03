# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The four arms of the deterministic prompt fact-pack experiment.

Each arm is one retrieval setup over the same task families and the same
model. The arms differ in how — and whether — repository facts reach the
prompt:

  no-pixel        No Pixel prompt or hook.
  current-pixel   Current installed Pixel guidance/hook behavior.
  fact-auto       Every substantive prompt receives the at-most-1 KiB packet
                  when fresh facts are available.
  fact-ondemand   The harness requests the canonical full JSON fact result
                  only when it chooses to.

Pixel is a fact oracle in every arm: it answers `targets_facts` read-only and
never chooses a next action. Lifecycle routing — when to inject — is out of
scope; `fact-auto` injects on every substantive prompt and `fact-ondemand`
lets the harness choose, and the protocol records which.
"""

from dataclasses import dataclass

# The at-most-1 KiB budget of the automatic arm's injected packet.
PACKET_BUDGET_BYTES = 1024


@dataclass(frozen=True)
class Arm:
    id: str
    title: str
    # How repository facts reach the prompt in this arm.
    fact_delivery: str  # "none" | "installed-hooks" | "auto-packet" | "ondemand-full"
    # Maximum bytes of the deterministic packet the harness injects.
    # 0 = the harness injects no packet; None = the full JSON result.
    packet_budget: int | None


ARMS = {
    "no-pixel": Arm("no-pixel", "Unconstrained", "none", 0),
    "current-pixel": Arm("current-pixel", "Current Pixel", "installed-hooks", 0),
    "fact-auto": Arm("fact-auto", "Fact packet, automatic", "auto-packet", PACKET_BUDGET_BYTES),
    "fact-ondemand": Arm("fact-ondemand", "Fact packet, on demand", "ondemand-full", None),
}

# The protocol's arm order: the two baselines first, then the two candidates.
ARM_ORDER = ("no-pixel", "current-pixel", "fact-auto", "fact-ondemand")

# The arms the decision rule compares against both baselines.
CANDIDATE_ARMS = ("fact-auto", "fact-ondemand")

# The arms that need no fact packet from this harness.
BASELINE_ARMS = ("no-pixel", "current-pixel")


def arm(id: str) -> Arm:
    return ARMS[id]


def fact_arms() -> list[str]:
    return [aid for aid in ARM_ORDER if ARMS[aid].fact_delivery in ("auto-packet", "ondemand-full")]


def validate_arm_ids(ids) -> list[str]:
    """Every defect of an arm-id list, as one line each (empty when sound)."""
    defects = []
    for aid in ids:
        if aid not in ARMS:
            defects.append(f"unknown arm {aid!r}")
    if len(set(ids)) != len(ids):
        defects.append("duplicate arm id")
    return defects
