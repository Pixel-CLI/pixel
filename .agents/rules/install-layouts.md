---
paths:
  - "crates/pixel-install/**"
---

# Install Layouts

Loaded when `pixel-install` is in play. `install`, `uninstall` and `doctor`
run on machines whose layout the author did not choose; test the layouts
below beside the ordinary one.

- **The repository can be the home directory.** `pixel setup --repo "$HOME"`
  makes the repo's `.claude/settings.json` the global one and puts the
  repo-local and global backups in one place. A repo-scoped install write
  that resolves to a global file is refused (compare with `same_file` before
  that write, not after). The one write allowed on the shared file first is
  the migration cleanup (`remove_pre_tool_use_guard`), which touches only
  `PreToolUse` and takes out only Pixel's guard, putting back the RTK group
  a delegate adopted. A cleanup that deletes a shared backup first checks
  that no install on the other side still uses it. Test the `repo == home`
  case beside the ordinary one.
- **A foreign config is not a broken install.** Another tool's hooks or
  project settings without a Pixel install are reported as absent, never as
  red: `doctor` judges what Pixel wrote.
- **Quote every path in a command the user is told to paste.** A hint like
  `rm <backup>` or `pixel setup --repo <root>` goes through
  `routing::quoted_executable`, and its test uses a path with a space and an
  apostrophe. `root.display()` in a suggested command names the wrong file
  the first time a path has a space in it.
- **Global and repo-local state stay apart.** A repo-local adoption (a
  delegate guard, an RTK backup) is recorded under the repository, never in
  a `~/.claude/` file that every other repository reads.
- **Claude hooks also run under Devin.** Devin CLI loads the hooks of
  `.claude/settings*.json` by default (`read_config_from.claude`), so every
  `--provider claude` entry `pixel setup` writes also fires inside Devin,
  with `DEVIN_PROJECT_DIR` set; an exit 2 blocks the Devin prompt too. A
  Claude-only side effect checks that variable (verified 2026-09: the
  handoff removed in #397 blocked `Add the "story" feature` in both hosts).
