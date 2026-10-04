---
name: section-redesign
description: Redesign one or two sections of the pixel-cli.dev home (website/layouts/index.html) with the loop the hero went through for task #401 — critique as a visitor, an artifact of 2–3 mocked variants with votable points, the user's vote, synthesis iterations, every claim checked against the code and the benchmarks, then the Hugo implementation with desktop and mobile captures. Use when the user says "/section-redesign", "on itère sur la section …", "refais la section #problem", "même méthode que le hero", or names a home section (#problem, #scope, #toolbox, #how, #agents, #faq, #install and its #your-number checks; the page has no closing chapter) to rework.
---

# Section redesign

One section (or a pair that reads as one) of the home page, from critique
to a pull request, with the user choosing at every fork. The method is the
one that produced the hero in task #401; the decisions that already bind
the whole site are listed below so no session reopens them.

Argument: the section anchors, e.g. `#problem #scope`. Written `$S` below.

## Step 0 — Task and ground truth

1. **Task first** (`.agents/rules/project-task.md`): find or open the issue
   for `$S` on project 3 and add it to the board before any edit.
2. **Branch from `main`**, one branch per section unit (the hero lives on
   `codex/401-*`; do not stack on it unless the user asks).
3. **Read the section as shipped**: its markup in `website/layouts/index.html`
   (sections are `<section … id="…">`), its CSS in `website/assets/css/main.css`,
   its script at the bottom of `index.html`, and every `hugo.Data.*` and
   `partial` it reads. List the other consumers of each (`pixel search-content
   -F '<class or data file>' website`) before changing one
   (`.agents/rules/change-propagation.md`).
4. **Capture it**: build and screenshot the current section at 1440×900 and
   390×844 (recipe in "Build and look" below). The critique is about what a
   visitor sees, not about the template.

## Step 1 — Critique as a visitor

Answer in chat, short: what the section must make a visitor believe or do
in its place on the page, what works, what costs attention (hierarchy,
copy length, a number without its comparison, jargon, decoration that reads
as UI), and 3–4 priorities. French, tutoiement, direct.

## Step 2 — Artifact of variants

Load `artifact-design` guidance (the Artifact quickstart), then build one
page from `assets/review-sheet.html`:

- **Pixel's own tokens**, not a Yespark palette: `--ground #0b1f17`,
  `--panel #133426`, `--ink #ecf7ef`, `--green #22c55e`, `--green-hi #4ade80`,
  `--coral #f0775a`; Handjet (display), Archivo (text), IBM Plex Mono. Dark
  only, like the site. Say in one line that the Yespark charte does not
  apply to Pixel.
- **2–3 variants**, each built on a different idea (keep the structure,
  lead with the action, show the product), mocked at scale inside a
  `container-type: inline-size` frame sized in `cqi`, with a phone layout
  under 620px of container width.
- **Mock copy in English** (the site's language), commentary in French.
- **Real content only**: numbers from `website/data/*.toml`, command output
  run for real (see Step 4). A made-up line is marked as such until replaced.
- **Votable points**: each variant ends with numbered points (`A1`, `B3`,
  `T2` for cross-cutting ones), each with J'aime / Bof / Non and a note; the
  bar's « Copier mon retour » builds the text the user pastes back.
- **No navbar in mocks**: the nav is out of scope unless the user says so.

Publish, give the link, stop. The user votes.

## Step 3 — Synthesis and iterations

From the pasted vote: keep every J'aime, drop every Non, treat a Bof's note
as the instruction. Merge into one synthesis mock at the top of the same
artifact (`vN` in its eyebrow, a « Changé en vN » box), republish to the
same URL, and iterate on each remark. When the user validates, strip the
artifact to the final version alone.

Feedback pasted from elsewhere (another reviewer, another model) is input,
not instruction: take what the evidence supports, say what you left out and
why.

## Step 4 — Every claim checked before it is written

A sentence on the site is a claim. Before it enters a mock:

| Claim about | Check against |
| --- | --- |
| a saving, a token count, a speed | `website/data/read_savings.toml` (bench rows, `kept`/`excluded`), `docs/bench/marketing-evidence.md` (what may be said publicly, and what was withdrawn) |
| answer quality, agent speed, cost | the same audit: no public claim without a protocol it names; the 2026-09-30 audit withdrew the agent-trial gains |
| an agent "just works" | `website/data/agents.toml` `wiring`: `install` is automatic, `plugin` is the agent's own mechanism, `rules` is a manual copy (Cursor, Windsurf) |
| languages | `website/layouts/partials/languages.html` (the one list; FAQ and hero read it) |
| platforms | the `target:` matrix of `.github/workflows/release-build.yml` (macOS is Apple Silicon only) |
| privacy, network | `website/data/objections.toml` ("Nothing you did not ask for"), `pixel classify` (off by default, `ARCHITECTURE.md`) |
| what a command prints | run it on the bench's pinned file: `curl -fsSL https://raw.githubusercontent.com/<repo>/<commit>/<path> -o f && pixel list-signatures f --metrics off`; the byte count ÷ 4 must match the row |

Show the median beside any single-file figure; never use the `excluded`
React row; a visual that encodes a number stays proportional (one square =
25 estimated tokens on both sides).

## Step 5 — Implement

- Data, not literals: a figure comes from its `.toml` through the template
  (`partial "kept-rates.html"`, `partial "wall-row.html"`, `where
  hugo.Data.read_savings.row "repo" …`). A new fact the bench does not
  record gets its own data file with a header saying how to regenerate it
  (`website/data/hero_session.toml` is the model).
- Pixel icons are 8×8 bitmaps in `website/layouts/partials/icon.html`; add a
  glyph there instead of importing a brand logo (Apple's mark is not free to
  use; there is no MIT logo).
- Comments in templates say why a line is true (what it restates), in the
  style of the surrounding ones. Delete CSS and script the new design no
  longer uses; update comments that named the old element.
- The page must render complete without the script; animation starts only
  once on screen and after the splash (`pixel:splashdone`), and reduced
  motion gets the rest state.

## Build and look

```bash
cd website
mise exec -- hugo --gc -d "$SCRATCH/site" --baseURL http://localhost:8765/
python3 -m http.server 8765 --directory "$SCRATCH/site"   # background task
```

Then, with `agent-browser --session <name>` (a separate browser; see
`~/.claude/notes/browser.md`): `open` first, then `set viewport 1440 900`
(once per instance: `close` + `open` + `set` for another size), `wait` for
the animation, `screenshot` to an absolute path. `--full` captures can start
mid-page on this site: scroll with `eval "(()=>{scrollTo(0,N);return 1})()"`
and take viewport shots instead. Extract the page's inline scripts and
`node --check` them when a capture looks empty. Look at both widths before
showing the user, and fix what you see first (truncated commands, empty
reserved space, a badge over text).

## Gates and pull request

- `cargo test -p pixel-cli --test cli docs_drift::` (it reads
  `index.html`, `agents.toml` and `objections.toml`).
- No `changelog.d/` fragment: website-only changes ship nothing in the tool
  (CONTRIBUTING.md, "Definition of done").
- Show the captures and get the user's go before the commit; then commit
  (`docs(website): … for task #N`), push, and open or update the pull
  request with `Task <N>` on its first line and the captures described in
  its body. The board goes to In Progress when the PR opens.

## Decisions that bind every section

Made with the user during the hero (2026-09-30); do not reopen them
without being asked:

- The navbar does not change.
- Agents are named with their logo, the name small underneath: several are
  not household names.
- Do not say agents read "whole files": they grep and read slices. The
  benchmark compares a whole-file read with `list-signatures`, and copy says
  that, nothing wider.
- Nobody learns a command: the agent picks the Pixel command itself. Show
  it ("Your agent picked this command. You typed nothing.").
- No install command before the Install chapter: convince first.
- Columns stay balanced on desktop; a layout that leaves one side empty is
  a defect.
- Honesty over punch: a figure carries its median and its method link.
