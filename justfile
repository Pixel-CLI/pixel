# Disk reclamation for a pixel checkout. `just` lists the recipes.
#
# The work is scripts/dev/clean.sh, which runs the same way without just
# (`scripts/dev/clean.sh --help` documents every scope and what it costs to
# rebuild): a recipe here only names a scope. Every scope covers all the
# worktrees `git worktree list` reports, not only the one just runs from --
# four worktrees carry four workspace builds, and that is where the disk went.
# The other side of that: a build or a test run going on in another worktree
# loses its output mid-flight.
#
# `just disk` is the read-only one: run it first, it prints exactly what the
# other recipes would remove.

# List the recipes.
_default:
    @just --list

# Report every reclaimable byte -- build output, indexes, caches, scratch. Removes nothing.
disk:
    @scripts/dev/clean.sh all --dry-run

# Remove build output: target/ of every worktree, plus the ignored scratch in the tree.
clean:
    @scripts/dev/clean.sh build

# Remove the .pixel index of every worktree (its daemon is stopped first). Rebuild: pixel build-index --history .
clean-index:
    @scripts/dev/clean.sh index

# Remove the base-shard cache shared by every worktree (~/.cache/pixel/shards).
clean-cache:
    @scripts/dev/clean.sh cache

# Remove the /tmp scratch left by scripts/bench/pixel-bench.sh.
clean-bench:
    @scripts/dev/clean.sh bench

# Remove all of the above. The recall corpus and ~/.local/state/pixel are never touched.
clean-all:
    @scripts/dev/clean.sh all
