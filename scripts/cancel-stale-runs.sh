#!/usr/bin/env bash
# Empty the workflow-run queue of work that no longer matters. A merged (or
# closed) pull request leaves its validation runs pointing at a head that
# does not exist on any branch; the cancel-stale workflow calls this on
# `pull_request: closed`, and a human can call it to purge a backlog (a2
# drains one job at a time, so a stale queue delays every live PR).
#
# By default this only prints what it would cancel. Pass --apply to cancel.
set -euo pipefail

owner_repo=${PIXEL_REPO:-Pixel-CLI/pixel}
branch=
all=0
older_than=0
apply=0

usage() {
    cat >&2 <<'EOF'
usage: cancel-stale-runs.sh [--repo owner/repo] [--branch NAME | --all]
                            [--older-than MINUTES] [--apply]

  --branch NAME     cancel queued and in-progress runs of that head branch
  --all, --apply    cancel every queued and in-progress run (--older-than
                    limits it), and actually cancel instead of dry-running
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --repo) owner_repo=$2; shift 2 ;;
        --branch) branch=$2; shift 2 ;;
        --all) all=1; shift ;;
        --older-than) older_than=$2; shift 2 ;;
        --apply) apply=1; shift ;;
        *) usage; exit 2 ;;
    esac
done

if [ -z "$branch" ] && [ "$all" -ne 1 ]; then
    echo "cancel-stale: pick --branch NAME or --all" >&2
    usage
    exit 2
fi

older() {
    python3 - "$1" "$2" <<'PY'
import datetime, sys
created, minutes = sys.argv[1], int(sys.argv[2])
start = datetime.datetime.fromisoformat(created.replace("Z", "+00:00"))
age = (datetime.datetime.now(datetime.timezone.utc) - start).total_seconds()
sys.exit(0 if age > minutes * 60 else 1)
PY
}

ids=()
for status in queued in_progress; do
    page=1
    while :; do
        rows=$(gh api "repos/$owner_repo/actions/runs?status=$status&per_page=100&page=$page" \
            --jq '.workflow_runs[] | [.id, .head_branch, .event, .created_at] | @tsv' 2>/dev/null || true)
        [ -z "$rows" ] && break
        while IFS=$'\t' read -r id head event created; do
            [ -z "$id" ] && continue
            [ -n "$branch" ] && [ "$head" != "$branch" ] && continue
            # --older-than keeps fresh runs and purges only the backlog
            # (older() exits 0 when the run is past the cutoff).
            if [ "$older_than" -gt 0 ] && ! older "$created" "$older_than"; then
                continue
            fi
            ids+=("$id:$head:$event")
        done <<< "$rows"
        count=$(printf '%s\n' "$rows" | wc -l)
        [ "$count" -lt 100 ] && break
        page=$((page + 1))
    done
done

if [ "${#ids[@]}" -eq 0 ]; then
    echo "cancel-stale: nothing to cancel" >&2
    exit 0
fi

for entry in "${ids[@]}"; do
    id=${entry%%:*}
    rest=${entry#*:}
    head=${rest%%:*}
    event=${rest#*:}
    if [ "$apply" -eq 1 ]; then
        gh api -X POST "repos/$owner_repo/actions/runs/$id/cancel" >/dev/null \
            && echo "cancelled $id ($event, $head)"
    else
        echo "would cancel $id ($event, $head)"
    fi
done

if [ "$apply" -ne 1 ]; then
    echo "dry run: pass --apply to cancel" >&2
fi