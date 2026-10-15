#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# pr-swarm — one rmux pane per open-PR worktree.
#
#   pr-swarm.sh reconcile [--wait N] [--no-wait] [--dry-run]
#       Open PRs whose worktree resolves get a pane; a MERGED PR closes its
#       pane and then removes the worktree, but only when every rail passes.
#   pr-swarm.sh status
#       Read-only table: PR, branch, worktree, pane, agent name, action.
#   pr-swarm.sh up <PR> [--worktree]
#       Explicit opt-in for one PR; --worktree also materialises
#       $HOME/Documents/pixel-pr-<N> on branch pr/<N>.
#   pr-swarm.sh down <PR> [--force] [--delete-branch] [--dry-run]
#       Close one pane and run the same teardown rails by hand.
#   pr-swarm.sh watch
#       Reconcile every $PIXEL_PR_SWARM_INTERVAL (300) seconds until killed.
#       Single instance per state directory, by $STATE_DIR/watch.pid.
#   pr-swarm.sh hook-session-start
#       SessionStart entry point: spawn that watch loop, detached, and return 0.
#
# State is the rmux pane title ("PR#<N> <branch>") and nothing else: a title
# survives a reconciler restart, a reboot of the daemon and a
# `git worktree` change, and cannot drift from the panes the way a sidecar
# could. Everything a run does is therefore a function of
# (open PRs, worktree list, pane titles).
#
# The session is the default rmux server's `pr-swarm`, single window `swarm`,
# so `rmux switch-client -t pr-swarm` reaches the swarm from the client the
# user already has. The trade is that any `rmux kill-server` takes it down;
# the next reconcile rebuilds it from the titles it re-reads.
#
# No launchd: a gui/$UID LaunchAgent gets `Operation not permitted` on every
# path under ~/Documents (macOS TCC, probed 2026-09-30), so the trigger is the
# SessionStart watcher, which inherits the session's grant.
#
# Env: PIXEL_PR_SWARM_SESSION, PIXEL_PR_SWARM_MAX_PANES,
#      PIXEL_PR_SWARM_TEARDOWN (off|safe|aggressive), PIXEL_PR_SWARM_INTERVAL,
#      PIXEL_PR_SWARM_REPO, PIXEL_PR_SWARM_CACHE, PIXEL_PR_SWARM_STATE_DIR,
#      PIXEL_RMUX_BIN, PIXEL_CLAUDE_BIN.
set -euo pipefail

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

REPO="${PIXEL_PR_SWARM_REPO:-$HOME/Documents/pixel}"
# The shared dependency cache: a directory of build output, never a checkout
# and never a worktree. Rails below refuse it by name.
CACHE="${PIXEL_PR_SWARM_CACHE:-$HOME/Documents/pixel-integration}"
STATE_DIR="${PIXEL_PR_SWARM_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/pixel/pr-swarm}"
SESSION="${PIXEL_PR_SWARM_SESSION:-pr-swarm}"
WINDOW="swarm"
MAX_PANES="${PIXEL_PR_SWARM_MAX_PANES:-6}"
TEARDOWN="${PIXEL_PR_SWARM_TEARDOWN:-safe}"
INTERVAL="${PIXEL_PR_SWARM_INTERVAL:-300}"
RMUX_BIN="${PIXEL_RMUX_BIN:-$HOME/.local/bin/rmux}"
CLAUDE_BIN="${PIXEL_CLAUDE_BIN:-$HOME/.local/bin/claude}"

LOG="$STATE_DIR/reconcile.log"
LOCK="$STATE_DIR/lock"
WATCH_PID="$STATE_DIR/watch.pid"
OVERRIDES="$STATE_DIR/resolve.tsv"
LAST_RUN="$STATE_DIR/last-run.jsonl"

# Only a title with this prefix belongs to us. Every pane we did not create
# has the bare hostname as its title, and `PR#4350` cannot read as `PR#435`
# because the digits are terminated by a space.
TITLE_PREFIX='PR#'

# Set by gather_worktrees; read by the resolvers.
WT_TABLE=""
# Set by gather_panes.
PANES_TSV=""

usage() {
    sed -n '2,/^set -euo pipefail$/p' "$SELF" | sed -n 's/^# \{0,1\}//p'
}

say() {   # one line to the append-only human log
    mkdir -p "$STATE_DIR"
    printf '[pr-swarm] %s %s\n' "$(date -u "+%Y-%m-%dT%H:%M:%SZ")" "$*" >> "$LOG"
}

# ── naming ────────────────────────────────────────────────────────────────────
# slugify is the tr pipeline from dev-stack-on-push.sh: it builds paths, so it
# keeps `.`. agent_name is a Claude session name, whose grammar is
# ^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$ — no dot — hence the extra tr.
slugify() { printf '%s' "$1" | tr '/' '-' | tr -c '[:alnum:]._-' '-'; }

agent_name() {   # $1 = PR number, $2 = head branch
    local s
    s="$(slugify "$2" | tr '.' '_')"
    printf 'pr-%s-%s' "$1" "$s" | cut -c1-64
}

shq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

pane_title_for() { printf '%s%s %s' "$TITLE_PREFIX" "$1" "$2"; }

# The command a pane runs. The title is set from inside the pane as well as
# after creation, so a crash between the two cannot orphan an untitled pane
# that the next reconcile would duplicate. The trailing login shell keeps the
# pane alive when claude exits: with the default remain-on-exit off, a bare
# `claude` would close its own pane, and a crash-looping session would churn
# panes every tick with the error nowhere.
pane_command() {   # $1 = PR number, $2 = head branch
    local cmd
    cmd="$(shq "$RMUX_BIN") select-pane -T $(shq "$(pane_title_for "$1" "$2")") 2>/dev/null; "
    cmd+="$(shq "$CLAUDE_BIN") -n $(shq "$(agent_name "$1" "$2")"); "
    cmd+='printf "\n[pr-swarm] claude exited %s\n" "$?"; '
    cmd+='exec "${SHELL:-/bin/zsh}" -l'
    printf '%s' "$cmd"
}

# ── concurrency ───────────────────────────────────────────────────────────────
# mkdir is atomic on APFS and ships everywhere; flock is not on macOS.
# Staleness is pid liveness, not age: an age threshold either steals the lock
# from a live slow run or leaves a dead one held.
acquire_lock() {   # $1 = seconds to wait, 0 = try once
    local deadline pid
    mkdir -p "$STATE_DIR"
    deadline=$(( $(date +%s) + $1 ))
    while ! mkdir "$LOCK" 2>/dev/null; do
        pid="$(cat "$LOCK/pid" 2>/dev/null || true)"
        if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
            rm -rf "$LOCK"
            continue
        fi
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 1
    done
    printf '%s' "$$" > "$LOCK/pid"
    printf '%s' "$(date +%s)" > "$LOCK/started"
    trap 'rm -rf "$LOCK"' EXIT INT TERM
    return 0
}

# ── gh ────────────────────────────────────────────────────────────────────────
# A failed or empty `gh pr list` must never read as "every PR is closed":
# teardown is driven by a per-PR `gh pr view`, never by absence from this list.
# rc 3 is "gh answered, but not with the JSON we asked for".
gh_open_prs() {   # stdout: number \t headRefName \t headRefOid
    local raw=""
    raw="$(gh pr list --state open --limit 100 \
        --json number,headRefName,headRefOid,isCrossRepository 2>/dev/null)" || return 1
    printf '%s' "$raw" | python3 -c '
import sys, json
try:
    rows = json.load(sys.stdin)
except Exception:
    sys.exit(3)
if not isinstance(rows, list):
    sys.exit(3)
for r in rows:
    if r.get("isCrossRepository"):
        continue          # a fork PR is never adopted
    print(r["number"], r["headRefName"], r["headRefOid"], sep="\t")
'
}

gh_pr_state() {   # $1 = PR number -> MERGED | OPEN | CLOSED | ""
    local raw=""
    raw="$(gh pr view "$1" --json state 2>/dev/null)" || return 1
    printf '%s' "$raw" | python3 -c '
import sys, json
try:
    print(json.load(sys.stdin).get("state", ""))
except Exception:
    sys.exit(1)
'
}

gh_pr_fields() {   # $1 = PR number, $2 = comma fields -> JSON object
    gh pr view "$1" --json "$2" 2>/dev/null
}

# ── rmux ──────────────────────────────────────────────────────────────────────
list_panes() {   # TSV: num \t pane_id \t session \t window \t index \t cwd \t title
    local raw=""
    raw="$("$RMUX_BIN" find-panes --title-prefix "$TITLE_PREFIX" --json 2>/dev/null)" || return 1
    printf '%s' "$raw" | python3 -c '
import sys, json, re
try:
    doc = json.load(sys.stdin)
except Exception:
    sys.exit(1)
if not isinstance(doc, dict):
    sys.exit(1)
for p in doc.get("panes", []):
    title = p.get("title", "")
    m = re.match(r"^PR#([0-9]+) ", title)
    if not m:
        continue          # a title without our prefix is not ours
    print(m.group(1), p.get("pane_id", ""), p.get("session_name", ""),
          p.get("window_index", ""), p.get("pane_index", ""),
          p.get("cwd", ""), title, sep="\t")
'
}

pane_row() { printf '%s\n' "$PANES_TSV" | awk -F'\t' -v n="$1" '$1 == n { print; exit }'; }
pane_id_of() { pane_row "$1" | cut -f2; }
pane_title_of() { pane_row "$1" | cut -f7; }
pane_cwd_of() { pane_row "$1" | cut -f6; }
our_pane_count() { printf '%s\n' "$PANES_TSV" | awk 'NF { n++ } END { print n + 0 }'; }

# ── git worktrees ─────────────────────────────────────────────────────────────
# path \t HEAD \t branch, one line per worktree. `substr` rather than $2 so a
# path with a space survives.
gather_worktrees() {
    WT_TABLE="$(git -C "$REPO" worktree list --porcelain 2>/dev/null \
        | awk '/^worktree /{p=substr($0,10)} /^HEAD /{h=$2} /^branch /{b=$2} /^detached/{b="detached"} \
               /^$/{if(p!="")print p"\t"h"\t"b; p=""} END{if(p!="")print p"\t"h"\t"b}' || true)"
}

wt_lookup() { printf '%s\n' "$WT_TABLE" | awk -F'\t' -v p="$1" '$1 == p { print; exit }'; }

# The worktree a path sits in; the longest match wins, since worktrees nest.
wt_owner() {
    printf '%s\n' "$WT_TABLE" | awk -F'\t' -v p="$1" '
        { r = $1
          if (p == r || index(p, r "/") == 1) { if (length(r) > length(best)) best = r } }
        END { if (best != "") print best; else print p }'
}

# Layered resolution, first hit wins. Nothing here keys on identity but the PR
# number: a branch rename changes the readable tail of a title and nothing else.
# Prints the worktree path on a hit; sets RESOLVED_BY.
resolve_worktree() {   # $1 = PR number, $2 = head branch, $3 = head OID
    local slug path
    RESOLVED_BY=""
    slug="$(slugify "$2")"

    path="$(printf '%s\n' "$WT_TABLE" | awk -F'\t' -v b="refs/heads/$2" '$3 == b { print $1; exit }')"
    if [ -n "$path" ]; then RESOLVED_BY="branch $2"; printf '%s' "$path"; return 0; fi

    if [ -n "$3" ]; then
        path="$(printf '%s\n' "$WT_TABLE" | awk -F'\t' -v h="$3" '$2 == h { print $1; exit }')"
        if [ -n "$path" ]; then RESOLVED_BY="head ${3:0:12}"; printf '%s' "$path"; return 0; fi
    fi

    path="$(printf '%s\n' "$WT_TABLE" | awk -F'\t' \
        -v b="refs/heads/integration/$slug" -v n="pixel-$slug" '
        { base = $1; sub(".*/", "", base)
          if ($3 == b || base == n) { print $1; exit } }')"
    if [ -n "$path" ]; then RESOLVED_BY="union $slug"; printf '%s' "$path"; return 0; fi

    if [ -f "$OVERRIDES" ]; then
        path="$(awk -F'\t' -v n="$1" '$1 == n { print $2; exit }' "$OVERRIDES" || true)"
        if [ -n "$path" ] && [ -n "$(wt_lookup "$path")" ]; then
            RESOLVED_BY="override"; printf '%s' "$path"; return 0
        fi
    fi

    return 1
}

# ── the rails ─────────────────────────────────────────────────────────────────
# The pane close is cheap and reversible; the worktree removal is neither. So
# the pane always closes on MERGED and the worktree goes only when every rail
# passes; a refusal is a reason string, never a non-zero exit.
# Returns 0 with the reason on stdout when the removal must be refused,
# 1 when it may proceed. `git worktree remove --force` is never passed.
removal_refusal() {   # $1 = worktree path
    local wt="$1" rec count unpushed

    [ -n "$wt" ] || { printf 'no worktree resolved for this pane'; return 0; }

    # 1/2. Never the main checkout, and never inside it: a nested worktree is
    # the rustflags-doubling trap dev-stack-guard.sh refuses to create.
    [ "$wt" != "$REPO" ] || { printf 'that is the main checkout (%s)' "$REPO"; return 0; }
    case "$wt" in
        "$REPO"/*) printf 'inside the main checkout (%s)' "$REPO"; return 0 ;;
    esac

    # 3. Never the shared dependency cache: build output, not a checkout.
    [ "$wt" != "$CACHE" ] || { printf 'that is the shared dependency cache'; return 0; }
    case "$wt" in
        "$CACHE"/*) printf 'inside the shared dependency cache'; return 0 ;;
    esac

    # 4. Never a mutants scratch tree: removing one mid-campaign corrupts a run
    # that is still in flight.
    case "$wt" in
        /private/var/folders/*/T/pixel-mutants-preflight.*) printf 'a mutants preflight scratch tree'; return 0 ;;
        /var/folders/*/T/pixel-mutants-preflight.*) printf 'a mutants preflight scratch tree'; return 0 ;;
    esac
    if [ -n "${TMPDIR:-}" ]; then
        case "$wt" in
            "${TMPDIR%/}"*/pixel-mutants-preflight.*) printf 'a mutants preflight scratch tree'; return 0 ;;
        esac
    fi

    # 5. Still a registered worktree, re-read now: guards a stale path.
    rec="$(git -C "$REPO" worktree list --porcelain 2>/dev/null | awk -v p="$wt" '
        /^worktree /{ if (substr($0,10) == p) found = 1 }
        END { print found ? "yes" : "" }' || true)"
    [ "$rec" = "yes" ] || { printf 'no longer a registered worktree'; return 0; }

    # 6. Clean tree. Not `git diff --quiet`: untracked files are exactly what
    # an agent leaves behind.
    count="$( { git -C "$wt" status --porcelain 2>/dev/null || true; } | wc -l | tr -d ' ')"
    [ "$count" = "0" ] || { printf '%s modified or untracked files' "$count"; return 0; }

    # 7. No unpushed work. `HEAD --not --remotes` holds for a detached scratch
    # tree too, where an upstream comparison has nothing to point at.
    unpushed="$(git -C "$wt" rev-list --count HEAD --not --remotes 2>/dev/null || printf '1')"
    [ "$unpushed" = "0" ] || { printf '%s commits not on any remote' "$unpushed"; return 0; }

    # 8. The owner is not the worktree an open PR still needs. A pane's PR
    # can flip back to OPEN in the window between `gh pr list` and the
    # `gh pr view` for state; the worktree is for the PR, not the pane.
    if [ -n "${DESIRED:-}" ]; then
        if printf '%s\n' "$DESIRED" | awk -F'\t' -v p="$wt" '$3 == p { found = 1 } END { exit !found }'; then
            printf 'worktree is in use by an open PR'
            return 0
        fi
    fi

    return 1
}

# ── the read-only half ────────────────────────────────────────────────────────
# Sets OPEN_TSV, WT_TABLE, PANES_TSV and DESIRED. Returns 0, or the rc of the
# call that failed: 1 gh list, 3 gh JSON, 4 rmux.
DESIRED=""
UNRESOLVED=""

gather() {
    gather_worktrees

    local rc=0
    OPEN_TSV="$(gh_open_prs)" || rc=$?
    [ "$rc" -eq 0 ] || return "$rc"

    DESIRED=""
    UNRESOLVED=""
    local num branch oid wt
    while IFS=$'\t' read -r num branch oid; do
        [ -n "$num" ] || continue
        if wt="$(resolve_worktree "$num" "$branch" "$oid")"; then
            DESIRED+="$(printf '%s\t%s\t%s' "$num" "$branch" "$wt")"$'\n'
        else
            UNRESOLVED+="$(printf '%s\t%s' "$num" "$branch")"$'\n'
        fi
    done <<< "$OPEN_TSV"

    rc=0
    PANES_TSV="$(list_panes)" || rc=4
    return "$rc"
}

open_pr_numbers() { printf '%s\n' "${OPEN_TSV:-}" | cut -f1; }
open_pr_count() { printf '%s\n' "${OPEN_TSV:-}" | awk 'NF { n++ } END { print n + 0 }'; }

# ── reconcile ─────────────────────────────────────────────────────────────────
cmd_reconcile() {
    local wait_s=30 dry=0 created=0 killed=0 refused=0 retitled=0 removed=0
    while [ $# -gt 0 ]; do
        case "$1" in
            --wait) wait_s="${2:-30}"; shift 2 ;;
            --no-wait) wait_s=0; shift ;;
            --dry-run) dry=1; shift ;;
            *) echo "pr-swarm reconcile: unknown option: $1" >&2; exit 2 ;;
        esac
    done
    mkdir -p "$STATE_DIR"

    # Step 0 — lock. Never exit non-zero from a hook path.
    if ! acquire_lock "$wait_s"; then
        if [ "$wait_s" -eq 0 ]; then
            exit 0                                   # the hook path: silent
        fi
        say "another reconcile holds the lock after ${wait_s}s; skipping"
        exit 0
    fi

    # Step 1 — the desired set. A failure here leaves every pane alone.
    local rc=0
    gather || rc=$?
    case "$rc" in
        0) ;;
        3) say "gh pr list returned unparseable JSON; leaving panes alone"; exit 0 ;;
        4) say "rmux server unavailable; nothing to reconcile"; exit 0 ;;
        *) say "gh pr list failed; leaving panes alone"; exit 0 ;;
    esac

    local num branch wt pane_id title cur
    while IFS=$'\t' read -r num branch; do
        [ -n "$num" ] || continue
        say "PR#$num unresolved: no worktree on $branch"
    done <<< "$UNRESOLVED"

    # Step 3 — the actual set, keyed by PR number, and the two diffs.
    local pending=""
    while IFS=$'\t' read -r num branch wt; do
        [ -n "$num" ] || continue
        pane_id="$(pane_id_of "$num")"
        title="$(pane_title_for "$num" "$branch")"
        if [ -z "$pane_id" ]; then
            pending+="$(printf '%s\t%s\t%s' "$num" "$branch" "$wt")"$'\n'
            continue
        fi
        cur="$(pane_title_of "$num")"
        if [ "$cur" != "$title" ]; then
            # A rename, not a different PR: retitle in place, never restart.
            if [ "$dry" -eq 1 ]; then
                say "PR#$num would retitle '$cur' -> '$title'"
            else
                "$RMUX_BIN" select-pane -T "$title" -t "$pane_id" >/dev/null 2>&1 || true
                retitled=$((retitled + 1))
            fi
        fi
    done <<< "$DESIRED"

    # Step 4 — create. Past MAX_PANES a tiled cell stops being usable.
    local count pane_count
    pane_count="$(our_pane_count)"
    while IFS=$'\t' read -r num branch wt; do
        [ -n "$num" ] || continue
        if [ "$pane_count" -ge "$MAX_PANES" ]; then
            say "PR#$num skipped: $(open_pr_count) open PRs, MAX_PANES=$MAX_PANES"
            continue
        fi
        if [ "$dry" -eq 1 ]; then
            say "PR#$num would open a pane in $wt"
            pane_count=$((pane_count + 1))
            continue
        fi
        if ! pane_id="$(create_pane "$num" "$branch" "$wt")"; then
            say "PR#$num: could not open a pane in $wt"
            continue
        fi
        # Belt and braces over the title the pane's own command sets.
        "$RMUX_BIN" select-pane -T "$(pane_title_for "$num" "$branch")" -t "$pane_id" >/dev/null 2>&1 || true
        created=$((created + 1))
        pane_count=$((pane_count + 1))
    done <<< "$pending"

    # Steps 5-7 — teardown, driven by each pane's own PR state.
    local state reason owner
    while IFS=$'\t' read -r num pane_id wt_ignored win idx cwd title; do
        [ -n "$num" ] || continue
        if printf '%s\n' "$(open_pr_numbers)" | grep -qx "$num"; then
            continue
        fi

        if ! state="$(gh_pr_state "$num")"; then
            say "PR#$num state unknown (gh pr view failed); pane kept"
            continue
        fi
        case "$state" in
            MERGED) ;;
            CLOSED)
                # Deliberately not torn down: a closed-unmerged PR returns the
                # board item to Todo, so the work may resume and the worktree
                # is live work.
                say "PR#$num closed unmerged — pane kept (board returns to Todo)"
                continue ;;
            OPEN) say "PR#$num open but unresolved — pane kept"; continue ;;
            *) say "PR#$num state '${state:-unknown}' — pane kept"; continue ;;
        esac

        if [ "$dry" -eq 1 ]; then
            say "PR#$num merged: would close $pane_id in $cwd"
            continue
        fi

        "$RMUX_BIN" kill-pane -t "$pane_id" >/dev/null 2>&1 || true
        killed=$((killed + 1))

        owner="$(wt_owner "${cwd:-}")"
        if reason="$(removal_refusal "$owner")"; then
            say "PR#$num merged but worktree kept: $reason — $owner"
            refused=$((refused + 1))
            continue
        fi
        if [ "$TEARDOWN" = "off" ]; then
            say "PR#$num merged: worktree left in place (PIXEL_PR_SWARM_TEARDOWN=off) — git -C $REPO worktree remove $owner"
            refused=$((refused + 1))
            continue
        fi
        if git -C "$REPO" worktree remove "$owner" 2>/dev/null; then
            git -C "$REPO" worktree prune >/dev/null 2>&1 || true
            removed=$((removed + 1))
            if [ "$TEARDOWN" = "aggressive" ]; then
                delete_branch_for "$owner" || true
            fi
        else
            say "PR#$num merged but git worktree remove failed: $owner"
            refused=$((refused + 1))
        fi
    done <<< "$PANES_TSV"

    # Step 8 — layout, only when something moved, and the run's record.
    if [ "$created" -gt 0 ] && [ "$dry" -eq 0 ]; then
        "$RMUX_BIN" select-layout -t "$SESSION:$WINDOW" tiled >/dev/null 2>&1 || true
    fi

    local panes_now
    panes_now="$(our_pane_count)"
    if [ "$dry" -eq 0 ]; then
        printf '{"at":"%s","open":%s,"resolved":%s,"panes":%s,"created":%s,"killed":%s,"refused":%s}\n' \
            "$(date -u "+%Y-%m-%dT%H:%M:%SZ")" "$(open_pr_count)" \
            "$(printf '%s\n' "${DESIRED:-}" | awk 'NF { n++ } END { print n + 0 }')" \
            "$panes_now" "$created" "$killed" "$refused" >> "$LAST_RUN"
    fi

    printf 'pr-swarm: open=%s resolved=%s panes=%s created=%s retitled=%s killed=%s removed=%s refused=%s\n' \
        "$(open_pr_count)" \
        "$(printf '%s\n' "${DESIRED:-}" | awk 'NF { n++ } END { print n + 0 }')" \
        "$panes_now" "$created" "$retitled" "$killed" "$removed" "$refused"
}

create_pane() {   # $1 = PR number, $2 = branch, $3 = worktree; prints the pane id
    local cmd
    cmd="$(pane_command "$1" "$2")"
    if "$RMUX_BIN" has-session -t "$SESSION" >/dev/null 2>&1; then
        "$RMUX_BIN" split-window -t "$SESSION:$WINDOW" -P -F '#{pane_id}' -c "$3" "$cmd" 2>/dev/null
    else
        # -x/-y matter: a detached session at the default geometry splits into
        # unusably narrow panes before anyone attaches.
        "$RMUX_BIN" new-session -d -P -F '#{pane_id}' -s "$SESSION" -n "$WINDOW" \
            -x 220 -y 50 -c "$3" "$cmd" 2>/dev/null
    fi
}

delete_branch_for() {   # $1 = worktree path; best effort, reported when refused
    local rec branch
    rec="$(wt_lookup "$1")"
    branch="$(printf '%s' "$rec" | cut -f3)"
    case "$branch" in refs/heads/*) branch="${branch#refs/heads/}" ;; *) return 0 ;; esac
    git -C "$REPO" branch -D "$branch" >/dev/null 2>&1 || true
}

# ── status ────────────────────────────────────────────────────────────────────
cmd_status() {
    local rc=0
    gather || rc=$?
    case "$rc" in
        0) ;;
        3) echo "pr-swarm: gh pr list returned unparseable JSON" >&2; return 0 ;;
        4) echo "pr-swarm: rmux server unavailable (no panes to report)" >&2 ;;
        *) echo "pr-swarm: gh pr list failed" >&2; return 0 ;;
    esac

    printf '%-7s %-28s %-44s %-6s %-26s %s\n' PR BRANCH WORKTREE PANE AGENT ACTION
    local num branch wt pane_id title
    while IFS=$'\t' read -r num branch wt; do
        [ -n "$num" ] || continue
        pane_id="$(pane_id_of "$num")"
        title="$(pane_title_for "$num" "$branch")"
        printf '%-7s %-28s %-44s %-6s %-26s %s\n' \
            "#$num" "$branch" "$wt" "${pane_id:-—}" "$(agent_name "$num" "$branch")" \
            "$( [ -z "$pane_id" ] && echo 'open a pane' || \
               { [ "$(pane_title_of "$num")" = "$title" ] && echo 'none (current)' || echo 'retitle'; } )"
    done <<< "$DESIRED"

    while IFS=$'\t' read -r num branch; do
        [ -n "$num" ] || continue
        printf '%-7s %-28s %-44s %-6s %-26s %s\n' \
            "#$num" "$branch" '—' '—' "$(agent_name "$num" "$branch")" 'skipped: no worktree'
    done <<< "$UNRESOLVED"

    # Panes whose PR is no longer open are teardown candidates; show the state
    # a reconcile would act on, without acting.
    local state action
    while IFS=$'\t' read -r num pane_id wt win idx cwd title; do
        [ -n "$num" ] || continue
        if printf '%s\n' "$(open_pr_numbers)" | grep -qx "$num"; then
            continue
        fi
        state="$(gh_pr_state "$num" 2>/dev/null || echo unknown)"
        case "$state" in
            MERGED) action="close, remove worktree (rails permitting)" ;;
            CLOSED) action="closed unmerged - pane kept" ;;
            OPEN|'') action="stale pane: PR is $state" ;;
            *)      action="[$state] pane kept" ;;
        esac
        printf '%-7s %-28s %-44s %-6s %-26s %s\n' \
            "#$num" '—' "${cwd:-—}" "$pane_id" '—' "$action"
    done <<< "$PANES_TSV"
}

# ── up / down ─────────────────────────────────────────────────────────────────
cmd_up() {
    local num="" make_wt=0
    while [ $# -gt 0 ]; do
        case "$1" in
            --worktree) make_wt=1; shift ;;
            -*) echo "pr-swarm up: unknown option: $1" >&2; exit 2 ;;
            *) num="$1"; shift ;;
        esac
    done
    [ -n "$num" ] || { echo "pr-swarm up: a PR number is required" >&2; exit 2; }
    mkdir -p "$STATE_DIR"

    local branch oid wt
    branch="$(gh_pr_fields "$num" headRefName,headRefOid 2>/dev/null \
        | python3 -c 'import sys,json; print(json.load(sys.stdin)["headRefName"])' 2>/dev/null || true)"
    [ -n "$branch" ] || { echo "pr-swarm up: could not read PR #$num from gh" >&2; exit 2; }

    acquire_lock 10 || { say "another reconcile holds the lock; $num untouched"; exit 0; }
    gather_worktrees
    PANES_TSV="$(list_panes)" || PANES_TSV=""

    if [ "$make_wt" -eq 1 ]; then
        wt="$HOME/Documents/pixel-pr-$num"
        case "$wt" in
            "$REPO"/*|"$CACHE"|"$CACHE"/*)
                echo "pr-swarm up: refusing a worktree inside $REPO or $CACHE" >&2; exit 2 ;;
        esac
        if [ "$PWD" = "$CACHE" ]; then
            echo "pr-swarm up: refusing to create a worktree from inside $CACHE" >&2; exit 2
        fi
        if [ -z "$(wt_lookup "$wt")" ]; then
            if git -C "$REPO" show-ref --verify --quiet "refs/heads/pr/$num"; then
                git -C "$REPO" worktree add "$wt" "pr/$num" >&2
            else
                git -C "$REPO" worktree add -b "pr/$num" "$wt" "origin/$branch" >&2
            fi || {
                echo "pr-swarm up: git worktree add failed for $wt" >&2
                exit 2
            }
            gather_worktrees
        fi
    else
        wt="$(resolve_worktree "$num" "$branch" "")" \
            || { echo "pr-swarm up: no worktree for #$num ($branch); pass --worktree" >&2; exit 2; }
    fi

    if [ -n "$(pane_id_of "$num")" ]; then
        printf 'pr-swarm: #%s already has pane %s\n' "$num" "$(pane_id_of "$num")"
        exit 0
    fi
    local pane_id
    pane_id="$(create_pane "$num" "$branch" "$wt")" \
        || { echo "pr-swarm up: could not open a pane in $wt" >&2; exit 2; }
    "$RMUX_BIN" select-pane -T "$(pane_title_for "$num" "$branch")" -t "$pane_id" >/dev/null 2>&1 || true
    "$RMUX_BIN" select-layout -t "$SESSION:$WINDOW" tiled >/dev/null 2>&1 || true
    say "PR#$num pane $pane_id opened in $wt"
    printf 'pr-swarm: #%s -> pane %s in %s\n' "$num" "$pane_id" "$wt"
}

cmd_down() {
    local num="" force=0 del_branch=0 dry=0
    while [ $# -gt 0 ]; do
        case "$1" in
            --force) force=1; shift ;;
            --delete-branch) del_branch=1; shift ;;
            --dry-run) dry=1; shift ;;
            -*) echo "pr-swarm down: unknown option: $1" >&2; exit 2 ;;
            *) num="$1"; shift ;;
        esac
    done
    [ -n "$num" ] || { echo "pr-swarm down: a PR number is required" >&2; exit 2; }
    mkdir -p "$STATE_DIR"
    acquire_lock 10 || { say "another reconcile holds the lock; $num untouched"; exit 0; }

    local rc=0
    gather || rc=$?
    case "$rc" in 1|3) echo "pr-swarm down: gh is unavailable; refusing" >&2; exit 0 ;; 4) ;; esac

    local pane_id cwd state owner reason
    pane_id="$(pane_id_of "$num")"
    [ -n "$pane_id" ] || { echo "pr-swarm down: no pane for #$num" >&2; exit 0; }
    cwd="$(pane_cwd_of "$num")"

    if [ "$force" -eq 0 ]; then
        state="$(gh_pr_state "$num" 2>/dev/null || echo unknown)"
        if [ "$state" != "MERGED" ]; then
            echo "pr-swarm down: #$num state is $state; teardown needs state=MERGED or --force" >&2
            exit 0
        fi
    fi

    if [ "$dry" -eq 1 ]; then
        printf 'pr-swarm down: would close %s and remove %s\n' "$pane_id" "$(wt_owner "$cwd")"
        exit 0
    fi

    "$RMUX_BIN" kill-pane -t "$pane_id" >/dev/null 2>&1 || true
    owner="$(wt_owner "$cwd")"
    if reason="$(removal_refusal "$owner")"; then
        say "PR#$num pane closed but worktree kept: $reason — $owner"
        printf 'pr-swarm: pane %s closed; worktree kept: %s\n' "$pane_id" "$reason"
        exit 0
    fi
    if git -C "$REPO" worktree remove "$owner" 2>/dev/null; then
        git -C "$REPO" worktree prune >/dev/null 2>&1 || true
        [ "$del_branch" -eq 1 ] && delete_branch_for "$owner" || true
        say "PR#$num pane $pane_id closed, worktree $owner removed"
        printf 'pr-swarm: pane %s closed, worktree removed\n' "$pane_id"
    else
        say "PR#$num pane closed but git worktree remove failed: $owner"
        printf 'pr-swarm: pane %s closed; git worktree remove failed for %s\n' "$pane_id" "$owner"
    fi
}

# ── watch / hook ──────────────────────────────────────────────────────────────
# One watcher per state directory, not one per session start. A session that
# restarts, a second Claude window on the same repo, and a hook that fires
# twice all reach here, and each must be a no-op rather than another loop:
# stacked loops do not reconcile wrong, they just multiply the gh calls and
# the pane reads for the rest of the day.
#
# The claim is a pidfile rather than "exit when the parent session is gone",
# because the parent a watcher is given is the hook process, which exits in
# milliseconds -- the loop is reparented to init by design, so liveness has to
# be recorded, not inherited. `set -C` (noclobber) makes creation itself the
# atomic test: two simultaneous starts race to exactly one winner, and a
# pidfile whose owner is gone (a kill -9, a reboot) is reclaimed.
watch_running() {
    local pid
    pid="$(cat "$WATCH_PID" 2>/dev/null || true)"
    [ -n "$pid" ] || return 1
    kill -0 "$pid" 2>/dev/null
}

claim_watch() {   # 0 = this process now owns $WATCH_PID; 1 = a live one has it
    if ( set -o noclobber; printf '%s\n' "$$" > "$WATCH_PID" ) 2>/dev/null; then
        return 0
    fi
    watch_running && return 1
    rm -f "$WATCH_PID"
    ( set -o noclobber; printf '%s\n' "$$" > "$WATCH_PID" ) 2>/dev/null
}

release_watch() {   # only its own: a successor may already own the file
    if [ "$(cat "$WATCH_PID" 2>/dev/null || true)" = "$$" ]; then
        rm -f "$WATCH_PID"
    fi
    return 0
}

cmd_watch() {
    mkdir -p "$STATE_DIR"
    if ! claim_watch; then
        say "watch already running (pid $(cat "$WATCH_PID" 2>/dev/null)); not starting a second"
        return 0
    fi
    # EXIT alone would let a `kill` run the handler and carry straight on with
    # the loop; the signal handlers exit, which is what fires it. A trap during
    # the foreground `sleep` is not deferred (bash 3.2, measured: 37 ms), so
    # `kill` stops the loop at once rather than after the rest of the interval.
    trap release_watch EXIT
    trap 'exit 0' INT TERM
    say "watch started (interval ${INTERVAL}s, pid $$)"
    while :; do
        # The lock is taken per tick, inside reconcile, never for the loop:
        # a long interval must not keep a session start's own reconcile out.
        "$SELF" reconcile --wait 30 >/dev/null 2>&1 || true
        sleep "$INTERVAL"
    done
}

cmd_hook_session_start() {
    # Never rejects a session start: the loop is detached and every failure
    # path exits 0. The pre-check is only to avoid forking a doomed process on
    # the common repeat; claim_watch above is what actually decides.
    mkdir -p "$STATE_DIR" 2>/dev/null || true
    if watch_running; then
        exit 0
    fi
    ( nohup "$SELF" watch >>"$LOG" 2>&1 & ) 2>/dev/null || true
    exit 0
}

# ── dispatch ──────────────────────────────────────────────────────────────────
case "${1:-}" in
    reconcile) shift; cmd_reconcile "$@" ;;
    status) shift; cmd_status "$@" ;;
    up) shift; cmd_up "$@" ;;
    down) shift; cmd_down "$@" ;;
    watch) shift; cmd_watch "$@" ;;
    hook-session-start) shift; cmd_hook_session_start "$@" ;;
    ""|-h|--help|help) usage ;;
    *) echo "pr-swarm: unknown subcommand: $1" >&2; usage >&2; exit 2 ;;
esac
