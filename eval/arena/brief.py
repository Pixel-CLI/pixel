#!/usr/bin/env python3
# Bounded Pixel evidence chain: prompt -> anchors -> conditional ops -> brief.
# Each later step only runs if the prior evidence narrowed the task. Hard
# budget: 4 pixel ops. Never forces pixel, never blocks native tools.
import json
import re
import subprocess
import sys

BUDGET_OPS = 4
ops: list[dict] = []
brief: dict = {"anchors": [], "files": [], "symbols": [], "callers": [],
               "exclusions": [], "unresolved": [], "ops": ops}


def px(*args: str) -> str:
    if len(ops) >= BUDGET_OPS:
        return ""
    try:
        r = subprocess.run(["pixel", *args], capture_output=True, text=True,
                           timeout=30)
        ops.append({"op": args[0], "args": list(args[1:]),
                    "bytes": len(r.stdout)})
        return r.stdout
    except Exception as e:  # pixel missing/stale -> brief degrades, not dies
        brief["unresolved"].append(f"pixel {args[0]} failed: {e}")
        return ""


def main(prompt: str) -> None:
    # --- anchors: quoted/backticked ids, paths, CamelCase/snake_case words ---
    quoted = [g for t in re.findall(r"[`'\"]([\w./()\[\]-]+)[`'\"]", prompt)
              for g in [t] if re.search(r"[a-zA-Z]", t)]
    paths = [w for w in re.findall(r"[\w.-]+/[\w./-]+", prompt)]
    cased = [w for w in re.findall(r"\b[a-z]+[A-Z][\w]*\b|\b\w+_\w+\b", prompt)
             if len(w) > 4 and w not in quoted]
    anchors = list(dict.fromkeys(quoted + paths + cased))[:4]
    # search takes identifiers; a path anchor is context, not a query term
    anchors.sort(key=lambda a: "/" in a)
    brief["anchors"] = anchors

    change_intent = re.search(
        r"rename|remove|delete|deprecat|impact|callers?|depends?|refactor|"
        r"affect|break|blast", prompt, re.I)

    # --- step 1: locate candidates (identifier -> literal; else concept) ---
    files: list[str] = []
    if anchors:
        out = px("search-content", "-F", anchors[0], "-l")
        files = [l.strip() for l in out.splitlines()
                 if l.strip() and not l.startswith(("🟩", "│", "├", "└", " "))]
    if not files:
        concept = " ".join(re.findall(r"[a-zA-Z]{4,}", prompt)[:6])
        out = px("find-code", concept)
        files = [m.group(1) for m in
                 re.finditer(r"^(\S+\.(?:ts|tsx|rs|py|js)):", out, re.M)][:8]
        if not anchors:
            m = re.search(r"^\S+#(\w+)#", out, re.M) or \
                re.search(r"\bname[\"']?\s*[:=]\s*[\"']?(\w+)", out)
            if m:
                anchors.append(m.group(1))
    # junk results (generated data dumps) are exclusions, not candidates
    keep = [f for f in files if not re.search(r"\.json$|\.lock$|output/|dist/|node_modules", f)]
    brief["exclusions"] = [f for f in files if f not in keep]
    files = keep[:12]
    brief["files"] = files

    # --- step 2: change/caller intent -> graph evidence on resolved symbol ---
    symbol = anchors[0].split("/")[-1].rsplit(".", 1)[0] if anchors else ""
    if change_intent and symbol:
        out = px("impact", symbol, "--json")
        try:
            d = json.loads(out)
            seen: set[str] = set()
            for x in d.get("d1_will_break", []):
                key = f"{x['path']}#{x['name']}"
                if key not in seen:
                    seen.add(key)
                    brief["callers"].append(
                        {"file": x["path"], "via": x["name"],
                         "line": x.get("line")})
        except (ValueError, KeyError):
            brief["unresolved"].append("impact returned no graph evidence")

    brief["budget"] = {"ops_used": len(ops), "ops_max": BUDGET_OPS}
    brief["confidence"] = "high" if brief["callers"] else (
        "medium" if files else "low")
    brief["native_fallback"] = (
        "the cited evidence is current and indexed — answer from it directly; "
        "open a cited region only if it contradicts the claim you would make")


def render(b: dict) -> str:
    lines = ["=== PIXEL EVIDENCE BRIEF ==="]
    if b["anchors"]:
        lines.append("anchors: " + ", ".join(b["anchors"]))
    if b["files"]:
        lines.append("files referencing anchors:")
        lines += [f"  {f}" for f in b["files"]]
    if b["callers"]:
        lines.append("callers via import graph (impact):")
        lines += [f"  {c['file']} -> {c['via']}" for c in b["callers"]]
    site = next((a for a in b["anchors"] if "/" in a), "")
    if site:
        lines.append(f"definition site (also changes): {site}")
    if b["exclusions"]:
        lines.append("excluded (generated/non-source): "
                     + ", ".join(b["exclusions"]))
    if b["unresolved"]:
        lines.append("unresolved: " + "; ".join(b["unresolved"]))
    lines.append(f"confidence: {b['confidence']} | ops: "
                 f"{b['budget']['ops_used']}/{b['budget']['ops_max']}")
    lines.append(b["native_fallback"])
    lines.append("=== END BRIEF ===")
    return "\n".join(lines)


if __name__ == "__main__":
    main(sys.argv[1])
    print(render(brief))
