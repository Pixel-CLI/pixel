#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Billed tokens and dollars of an agent session, read from its transcripts.

    python3 -I eval/lib/usage.py <transcript.jsonl>... [--json]
    python3 -I eval/lib/usage.py --self-test

A model or prompt size with no listed price makes the total `usd: null` and is
named under `unpriced`: a dollar figure is never guessed.
"""

import glob
import json
import sys
from collections import Counter

PRICES_AS_OF = "2026-11-08"
PRICE_SOURCES = (
    "https://platform.claude.com/docs/en/about-claude/pricing.md",
    "https://developers.openai.com/api/docs/pricing",
)
# $ per million tokens: input, short cache write, long cache write, cache read, output.
# Short/long is 5m/1h for Claude and 30m/12h for OpenAI.
PRICES = {
    "claude-fable-5-1": (10, 12.5, 20, 0.25, 50),
    "claude-fable-5": (10, 12.5, 20, 1, 50),
    "claude-opus-5-5": (4, 5, 8, 0.2, 20),
    "claude-opus-5": (5, 6.25, 10, 0.5, 25),
    "claude-sonnet-5-5": (2, 2.5, 4, 0.1, 10),
    "claude-sonnet-5": (2, 2.5, 4, 0.2, 10),
    "claude-haiku-5-5": (0.1, 0.125, 0.2, 0.01, 0.5),
    "gpt-6-astra": (10, 12.5, 25, 1, 50),
    "gpt-6.1-sol": (2, 2.5, 5, 0.1, 10),
    "gpt-6-luna": (0.1, 0.125, 0.25, 0.01, 0.5),
}
# Above this many prompt tokens another price applies; None: not listed here.
LONG_PROMPT = {
    "claude-haiku-5-5": (100_000, (0.5, 0.625, 1, 0.05, 2.5)),
    "gpt-6-astra": (272_000, None),
    "gpt-6.1-sol": (272_000, None),
    "gpt-6-luna": (272_000, None),
}
FAST_MULTIPLIER = 2
KINDS = ("input", "write_short", "write_long", "read", "output")


def claude_rows(paths):
    """One row per API response: a response split over several lines keeps its last usage."""
    last = {}
    for path in paths:
        with open(path, errors="ignore") as lines:
            for number, line in enumerate(lines):
                if '"usage"' not in line:
                    continue
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                message = entry.get("message") or {}
                if entry.get("type") == "assistant" and message.get("usage"):
                    last[message.get("id") or (path, number)] = message
    rows = []
    for message in last.values():
        usage = message["usage"]
        written = usage.get("cache_creation_input_tokens") or 0
        long_write = (usage.get("cache_creation") or {}).get("ephemeral_1h_input_tokens") or 0
        rows.append(
            {
                "model": message.get("model"),
                "input": usage.get("input_tokens") or 0,
                "write_short": written - long_write,
                "write_long": long_write,
                "read": usage.get("cache_read_input_tokens") or 0,
                "output": usage.get("output_tokens") or 0,
                "fast": usage.get("speed") == "fast",
            }
        )
    return rows


def codex_rows(path):
    """One row per `token_count` that moved the cumulative total.

    `input_tokens` includes the cached and written tokens, unlike Claude's.
    """
    rows, model, seen = [], None, 0
    with open(path, errors="ignore") as lines:
        for line in lines:
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            payload = entry.get("payload") or {}
            if entry.get("type") == "turn_context":
                model = payload.get("model")
            info = payload.get("info") if payload.get("type") == "token_count" else None
            total = ((info or {}).get("total_token_usage") or {}).get("total_tokens", 0)
            if not info or total <= seen:
                continue
            seen = total
            usage = info.get("last_token_usage") or {}
            read = usage.get("cached_input_tokens") or 0
            written = usage.get("cache_write_input_tokens") or 0
            rows.append(
                {
                    "model": model,
                    "input": (usage.get("input_tokens") or 0) - read - written,
                    "write_short": written,
                    "write_long": 0,
                    "read": read,
                    "output": usage.get("output_tokens") or 0,
                    "fast": False,
                }
            )
    return rows


def price_of(row):
    """The row's five prices, or None when no price is listed for it."""
    prices = PRICES.get(row["model"])
    limit, long_prices = LONG_PROMPT.get(row["model"], (None, None))
    prompt = row["input"] + row["write_short"] + row["write_long"] + row["read"]
    if prices and limit is not None and prompt > limit:
        return long_prices
    return prices


def billed(rows):
    """Token totals by kind, dollars, and what could not be priced."""
    tokens, usd, unpriced = Counter(), 0.0, set()
    for row in rows:
        if not any(row[kind] for kind in KINDS):
            continue  # synthetic messages carry an empty usage
        tokens.update({kind: row[kind] for kind in KINDS})
        prices = price_of(row)
        if prices is None:
            unpriced.add(str(row["model"]))
            continue
        cost = sum(row[kind] * price for kind, price in zip(KINDS, prices)) / 1_000_000
        usd += cost * (FAST_MULTIPLIER if row["fast"] else 1)
    return {
        "tokens": dict(tokens),
        "requests": sum(1 for row in rows if any(row[kind] for kind in KINDS)),
        "usd": None if unpriced else round(usd, 6),
        "unpriced": sorted(unpriced),
        "prices_as_of": PRICES_AS_OF,
    }


def codex_exec_usd(model, input_tokens, cached_tokens, output_tokens):
    """Dollars of one `codex exec`, whose stream gives one total for all its requests.

    shortcut: a request above 272K is priced at the short-context rate, since the
    total hides request sizes; read the rollout through `session` when that matters.
    """
    prices = PRICES.get(model)
    if prices is None or None in (input_tokens, cached_tokens, output_tokens):
        return None
    fresh = input_tokens - cached_tokens
    return round((fresh * prices[0] + cached_tokens * prices[3] + output_tokens * prices[4]) / 1_000_000, 6)


def is_codex(path):
    with open(path, errors="ignore") as lines:
        for line in lines:
            return '"session_meta"' in line
    return False


def with_subagents(path):
    """A Claude session bills its subagents too; they live beside its transcript."""
    return [path, *sorted(glob.glob(path.removesuffix(".jsonl") + "/subagents/*.jsonl"))]


def session(path):
    """The billed summary of one transcript of either agent."""
    return billed(codex_rows(path) if is_codex(path) else claude_rows(with_subagents(path)))


def self_test():
    import os
    import tempfile

    def line(message_id, model, usage):
        return json.dumps(
            {"type": "assistant", "message": {"id": message_id, "model": model, "usage": usage}}
        )

    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "s.jsonl")
        os.makedirs(os.path.join(tmp, "s", "subagents"))
        first = {"input_tokens": 1, "output_tokens": 1}
        full = {
            "input_tokens": 1_000_000,
            "output_tokens": 1_000_000,
            "cache_read_input_tokens": 1_000_000,
            "cache_creation_input_tokens": 3_000_000,
            "cache_creation": {"ephemeral_1h_input_tokens": 1_000_000},
        }
        with open(path, "w") as out:
            out.write(line("a", "claude-opus-5-5", first) + "\n")
            out.write(line("a", "claude-opus-5-5", full) + "\n")  # same response: last usage wins
            out.write(line("z", "<synthetic>", {"input_tokens": 0, "output_tokens": 0}) + "\n")
        with open(os.path.join(tmp, "s", "subagents", "x.jsonl"), "w") as out:
            out.write(line("b", "claude-sonnet-5-5", {"input_tokens": 1_000_000}) + "\n")
        result = session(path)
        assert result["usd"] == 4 + 2 * 5 + 8 + 0.2 + 20 + 2, result
        assert result["requests"] == 2 and result["unpriced"] == [], result

        codex = os.path.join(tmp, "c.jsonl")
        count = {
            "input_tokens": 100_000,
            "cached_input_tokens": 40_000,
            "cache_write_input_tokens": 10_000,
            "output_tokens": 100_000,
            "total_tokens": 200_000,
        }
        event = json.dumps(
            {
                "payload": {
                    "type": "token_count",
                    "info": {"last_token_usage": count, "total_token_usage": count},
                }
            }
        )
        with open(codex, "w") as out:
            out.write('{"type":"session_meta","payload":{}}\n')
            out.write('{"type":"turn_context","payload":{"model":"gpt-6.1-sol"}}\n')
            out.write(event + "\n" + event + "\n")  # a repeated event is one request
        result = session(codex)
        assert result["usd"] == round(0.05 * 2 + 0.01 * 2.5 + 0.04 * 0.1 + 0.1 * 10, 6), result
        assert result["requests"] == 1, result

    unknown = {"model": "other", "input": 5, "write_short": 0, "write_long": 0, "read": 0, "output": 0, "fast": False}
    assert billed([unknown])["usd"] is None and billed([unknown])["unpriced"] == ["other"]

    def haiku(tokens):
        return {**unknown, "model": "claude-haiku-5-5", "input": tokens}

    assert billed([haiku(100_000)])["usd"] == 0.01  # at the limit: the short-prompt price
    assert billed([haiku(100_001)])["usd"] == round(100_001 * 0.5 / 1_000_000, 6)
    assert billed([{**haiku(1_000_000), "model": "gpt-6-luna"}])["usd"] is None  # long-context price unlisted
    fast = {**unknown, "model": "claude-opus-5-5", "input": 1_000_000, "fast": True}
    assert billed([fast])["usd"] == 8
    assert codex_exec_usd("gpt-6.1-sol", 1_000_000, 400_000, 1_000_000) == round(1.2 + 0.04 + 10, 6)
    assert codex_exec_usd("default", 1, 0, 1) is None and codex_exec_usd("gpt-6.1-sol", None, 0, 1) is None
    print("self-test: ok")


def main(argv):
    if "--self-test" in argv:
        self_test()
        return 0
    paths = [arg for arg in argv if arg != "--json"]
    if not paths:
        print(__doc__, file=sys.stderr)
        return 2
    results = {path: session(path) for path in paths}
    if "--json" in argv:
        print(json.dumps(results, indent=2))
    else:
        for path, result in results.items():
            print(f"{result['usd']}  {result['requests']} requests  {result['unpriced'] or ''}  {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
