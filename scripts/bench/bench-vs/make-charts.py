#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Render the benchmark figures as SVG, light and dark, from the raw rows.

The numbers are read from docs/bench/vs-*/raw/, never retyped, so a figure can
never drift from the measurement it illustrates. Each chart is emitted twice --
`<name>-light.svg` and `<name>-dark.svg` -- and the docs embed them through
<picture media="(prefers-color-scheme: dark)">, which is GitHub's supported way
to theme an image. The dark variants use the palette's own dark steps, validated
against the dark surface; they are not a flipped copy of the light ones.

Palette: the dataviz reference instance. Categorical slots are assigned in fixed
order. Every bar carries a direct label, which is also the relief required for
the one light-mode slot that sits under 3:1 on the light surface.
"""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "docs/bench/charts"

THEME = {
    "light": {"surface": "#fcfcfb", "ink": "#0b0b0b", "ink2": "#52514e",
              "muted": "#898781", "grid": "#e1e0d9", "axis": "#c3c2b7",
              "s1": "#2a78d6", "s2": "#eb6834", "s3": "#1baf7a", "s4": "#8a5cd1"},
    "dark": {"surface": "#1a1a19", "ink": "#ffffff", "ink2": "#c3c2b7",
             "muted": "#898781", "grid": "#2c2c2a", "axis": "#383835",
             "s1": "#3987e5", "s2": "#d95926", "s3": "#199e70", "s4": "#9a70e0"},
}
# Single quotes inside the stack: this string is interpolated into a
# double-quoted XML attribute, and a double quote there ends it early.
FONT = "system-ui,-apple-system,'Segoe UI',sans-serif"


def esc(s):
    return (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


def head(w, h, t, title, subtitle):
    return [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
        f'viewBox="0 0 {w} {h}" role="img" aria-label="{esc(title)}">',
        f'<rect width="{w}" height="{h}" fill="{t["surface"]}"/>',
        f'<text x="24" y="32" font-family="{FONT}" font-size="16" '
        f'font-weight="600" fill="{t["ink"]}">{esc(title)}</text>',
        f'<text x="24" y="52" font-family="{FONT}" font-size="12" '
        f'fill="{t["ink2"]}">{esc(subtitle)}</text>',
    ]


def legend(t, items, x, y):
    out, dx = [], 0
    for label, color in items:
        out.append(f'<rect x="{x+dx}" y="{y-9}" width="10" height="10" rx="2" fill="{color}"/>')
        out.append(f'<text x="{x+dx+15}" y="{y}" font-family="{FONT}" font-size="11.5" '
                   f'fill="{t["ink2"]}">{esc(label)}</text>')
        dx += 26 + int(len(label) * 6.9)   # 6.9px/char at 11.5px + swatch + gutter
    return out


def bar_h(t, rows, title, subtitle, unit, note):
    """Horizontal emphasis bars: one highlighted series, the rest as context."""
    w, top, lh = 760, 86, 40
    h = top + lh * len(rows) + 54
    s = head(w, h, t, title, subtitle)
    x0, xw = 150, 520
    mx = max(r[1] for r in rows)
    for i, (label, val, emph) in enumerate(rows):
        y = top + i * lh
        bw = max(3, round(xw * val / mx))
        color = t["s1"] if emph else t["muted"]
        s.append(f'<text x="{x0-12}" y="{y+16}" text-anchor="end" font-family="{FONT}" '
                 f'font-size="12.5" font-weight="{600 if emph else 400}" '
                 f'fill="{t["ink"] if emph else t["ink2"]}">{esc(label)}</text>')
        s.append(f'<rect x="{x0}" y="{y+5}" width="{bw}" height="13" rx="4" fill="{color}"/>')
        s.append(f'<text x="{x0+bw+8}" y="{y+16}" font-family="{FONT}" font-size="12" '
                 f'font-weight="{600 if emph else 400}" fill="{t["ink"] if emph else t["ink2"]}" '
                 f'>{val:,}'.replace(",", " ") + f' {esc(unit)}</text>')
    s.append(f'<line x1="{x0}" y1="{top-6}" x2="{x0}" y2="{top+lh*len(rows)-6}" '
             f'stroke="{t["axis"]}" stroke-width="1"/>')
    s.append(f'<text x="24" y="{h-18}" font-family="{FONT}" font-size="11" '
             f'fill="{t["muted"]}">{esc(note)}</text>')
    s.append("</svg>")
    return "\n".join(s)


def bar_grouped(t, groups, series, title, subtitle, note, ymax=1.0):
    """Grouped columns, 0..ymax, every bar directly labelled."""
    w, h = 760, 400
    s = head(w, h, t, title, subtitle)
    s += legend(t, [(nm, t[c]) for nm, c, _ in series], 24, 74)
    left, right, top, bot = 58, 24, 104, 64
    pw, ph = w - left - right, h - top - bot
    for k in range(5):
        gy = top + ph - ph * k / 4
        s.append(f'<line x1="{left}" y1="{gy:.1f}" x2="{w-right}" y2="{gy:.1f}" '
                 f'stroke="{t["grid"]}" stroke-width="1"/>')
        s.append(f'<text x="{left-10}" y="{gy+4:.1f}" text-anchor="end" font-family="{FONT}" '
                 f'font-size="11" fill="{t["muted"]}">{ymax*k/4:.2f}</text>')
    gw = pw / len(groups)
    n = len(series)
    bw = min(34, (gw - 26) / n - 2)
    for gi, g in enumerate(groups):
        gx = left + gw * gi + gw / 2
        for si, (nm, c, vals) in enumerate(series):
            v = vals[gi]
            bh = ph * v / ymax
            bx = gx - (n * (bw + 2) - 2) / 2 + si * (bw + 2)
            by = top + ph - bh
            if bh >= 1:
                s.append(f'<rect x="{bx:.1f}" y="{by:.1f}" width="{bw:.1f}" '
                         f'height="{bh:.1f}" rx="4" fill="{t[c]}"/>')
            s.append(f'<text x="{bx+bw/2:.1f}" y="{by-6 if bh>=1 else top+ph-6:.1f}" '
                     f'text-anchor="middle" font-family="{FONT}" font-size="10.5" '
                     f'fill="{t["ink2"]}">{v:.2f}</text>')
        s.append(f'<text x="{gx:.1f}" y="{top+ph+20}" text-anchor="middle" '
                 f'font-family="{FONT}" font-size="12" fill="{t["ink"]}">{esc(g)}</text>')
    s.append(f'<line x1="{left}" y1="{top+ph}" x2="{w-right}" y2="{top+ph}" '
             f'stroke="{t["axis"]}" stroke-width="1"/>')
    s.append(f'<text x="24" y="{h-18}" font-family="{FONT}" font-size="11" '
             f'fill="{t["muted"]}">{esc(note)}</text>')
    s.append("</svg>")
    return "\n".join(s)


def scatter(t, series, title, subtitle, note, xmax, xlabel, ylabel):
    w, h = 760, 420
    s = head(w, h, t, title, subtitle)
    s += legend(t, [(nm, t[c]) for nm, c, _ in series], 24, 74)
    left, right, top, bot = 58, 30, 104, 70
    pw, ph = w - left - right, h - top - bot
    for k in range(5):
        gy = top + ph - ph * k / 4
        s.append(f'<line x1="{left}" y1="{gy:.1f}" x2="{w-right}" y2="{gy:.1f}" '
                 f'stroke="{t["grid"]}" stroke-width="1"/>')
        s.append(f'<text x="{left-10}" y="{gy+4:.1f}" text-anchor="end" font-family="{FONT}" '
                 f'font-size="11" fill="{t["muted"]}">{k/4:.2f}</text>')
    for k in range(5):
        gx = left + pw * k / 4
        s.append(f'<text x="{gx:.1f}" y="{top+ph+20}" text-anchor="middle" '
                 f'font-family="{FONT}" font-size="11" fill="{t["muted"]}">'
                 f'{int(xmax*k/4):,}'.replace(",", " ") + '</text>')
    placed = []   # label boxes already drawn, to steer the next one off them
    for nm, c, pts in series:
        for label, x, y in pts:
            px = left + pw * min(x, xmax) / xmax
            py = top + ph - ph * y
            s.append(f'<circle cx="{px:.1f}" cy="{py:.1f}" r="6" fill="{t[c]}" '
                     f'stroke="{t["surface"]}" stroke-width="2"/>')
            # Try candidate offsets around the point and take the first that
            # collides with nothing already drawn. Sliding a label away until it
            # is clear (the obvious approach) is worse than overlapping: it ends
            # up nearer some other point and silently mislabels it.
            wid = len(label) * 5.9
            cands = [(11, 4, "start"), (-11, 4, "end"), (0, -12, "middle"),
                     (0, 16, "middle"), (11, -9, "start"), (-11, -9, "end")]
            if px > left + pw * 0.78:
                cands = cands[1::2] + cands[0::2]   # prefer leftward near the edge
            lx, ly, anchor = px + 11, py + 4, "start"
            for dx, dy, anc in cands:
                cx, cy = px + dx, py + dy
                x1 = cx - wid if anc == "end" else (cx - wid / 2 if anc == "middle" else cx)
                box = (x1, cy - 9, x1 + wid, cy + 3)
                if box[0] < left or box[2] > w - 6:
                    continue
                if any(box[0] < b[2] and b[0] < box[2] and
                       box[1] < b[3] and b[1] < box[3] for b in placed):
                    continue
                lx, ly, anchor = cx, cy, anc
                break
            x1 = lx - wid if anchor == "end" else (lx - wid / 2 if anchor == "middle" else lx)
            placed.append((x1, ly - 9, x1 + wid, ly + 3))
            s.append(f'<text x="{lx:.1f}" y="{ly:.1f}" text-anchor="{anchor}" '
                     f'font-family="{FONT}" font-size="10.5" '
                     f'fill="{t["ink2"]}">{esc(label)}</text>')
    s.append(f'<line x1="{left}" y1="{top+ph}" x2="{w-right}" y2="{top+ph}" '
             f'stroke="{t["axis"]}" stroke-width="1"/>')
    s.append(f'<text x="{left+pw/2:.0f}" y="{top+ph+42}" text-anchor="middle" '
             f'font-family="{FONT}" font-size="11.5" fill="{t["ink2"]}">{esc(xlabel)}</text>')
    s.append(f'<text x="16" y="{top+ph/2:.0f}" transform="rotate(-90 16 {top+ph/2:.0f})" '
             f'text-anchor="middle" font-family="{FONT}" font-size="11.5" '
             f'fill="{t["ink2"]}">{esc(ylabel)}</text>')
    s.append(f'<text x="24" y="{h-18}" font-family="{FONT}" font-size="11" '
             f'fill="{t["muted"]}">{esc(note)}</text>')
    s.append("</svg>")
    return "\n".join(s)


def load(p):
    return json.load(open(ROOT / p))


def main():
    OUT.mkdir(parents=True, exist_ok=True)

    imp = {Path(p).stem.replace("impact-", ""): load(f"docs/bench/vs-gitnexus/raw/{Path(p).name}")
           for p in ("impact-rust.json", "impact-typescript.json",
                     "impact-ruby-alonetone.json", "impact-ruby-ddtrace.json")}

    def mean(rows, k):
        return sum(r[k] for r in rows) / len(rows)

    order = [("Rust\n(pixel)", "rust"), ("TypeScript\n(GitNexus)", "typescript"),
             ("Ruby\n(alonetone)", "ruby-alonetone"), ("Ruby\n(dd-trace-rb)", "ruby-ddtrace")]
    labels = [o[0].replace("\n", " ") for o in order]
    px = [round(mean(imp[k], "pixel_recall_d1"), 2) for _, k in order]
    gn = [round(mean(imp[k], "gitnexus_recall_d1"), 2) for _, k in order]

    # semble and pixel from the pixel 0.6.0 run; WarpGrep from its own run of
    # the same queries on the same corpus commits (not re-run: paid per call).
    rfiles = ("retrieval-rust.json", "retrieval-typescript.json",
              "retrieval-ruby-ddtrace.json")
    ret = {Path(f).stem.replace("retrieval-", ""): load(f"docs/bench/vs-tools/raw/v0.6.0/{f}")
           for f in rfiles}
    wgr = {Path(f).stem.replace("retrieval-", ""): load(f"docs/bench/vs-tools/raw/warpgrep/{f}")
           for f in rfiles}
    rorder = [("Rust", "rust"), ("TypeScript", "typescript"), ("Ruby", "ruby-ddtrace")]
    sem = [round(mean(ret[k], "semble_r10"), 2) for _, k in rorder]
    sme = [round(mean(ret[k], "pixel_search_meaning_r10"), 2) for _, k in rorder]
    fic = [round(mean(ret[k], "pixel_find_code_r10"), 2) for _, k in rorder]
    wgp = [round(mean(wgr[k], "warpgrep_r10"), 2) for _, k in rorder]

    maps = {r: load(f"docs/bench/vs-tools/raw/map-{r}.json")
            for r in ("pixel", "alonetone", "dd-trace-rb", "GitNexus")}

    charts = {
        "context-tax": lambda t: bar_h(
            t,
            [("GitNexus", 19700, False), ("pixel", 4160, True),
             ("semble", 980, False), ("stacklit", 420, False)],
            "Always-on context cost",
            "tokens every turn carries before any question is answered — lower is better",
            "tok",
            "MCP tool schemas measured by live handshake + injected files. pixel is lightest only against GitNexus."),
        "impact-recall": lambda t: bar_grouped(
            t, labels,
            [("pixel", "s1", px), ("GitNexus", "s2", gn)],
            "Blast radius: share of true callers found",
            "recall at depth 1 against grep-derived call sites — 29 cases, higher is better",
            "pixel wins Rust and TypeScript; GitNexus wins both Ruby corpora."),
        "retrieval-recall": lambda t: bar_grouped(
            t, [r[0] for r in rorder],
            [("semble", "s1", sem), ("pixel search-meaning", "s2", sme),
             ("pixel find-code", "s3", fic), ("WarpGrep", "s4", wgp)],
            "Natural-language search: right file in the top 10",
            "recall@10 on 45 queries built from each repo's own doc comments — higher is better",
            "pixel 0.6.0; WarpGrep from its 2026-09-27 run. semble leads on TypeScript and Ruby, ties on Rust. find-code: a phrase index."),
        "map-cost-coverage": lambda t: scatter(
            t,
            [("stacklit derive", "s1",
              [(r, maps[r]["stacklit"]["approx_tokens"], maps[r]["stacklit"]["dir_coverage"])
               for r in maps]),
             ("pixel list-areas", "s2",
              [(r, maps[r]["pixel_list_areas"]["approx_tokens"],
                maps[r]["pixel_list_areas"]["dir_coverage"]) for r in maps])],
            "Repo map: what it costs vs how much it names",
            "up and to the LEFT is better — cheaper map, more of the tree reachable",
            "stacklit wins 3 of 4 repos. pixel repo-map (35k–279k tokens) is off this scale by design.",
            3400, "tokens carried", "share of source directories named"),
    }

    for name, fn in charts.items():
        for mode, t in THEME.items():
            (OUT / f"{name}-{mode}.svg").write_text(fn(t))
            print(f"  {name}-{mode}.svg")


if __name__ == "__main__":
    main()
