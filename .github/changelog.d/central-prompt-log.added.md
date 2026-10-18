Feature: Central prompt log

Every prompt and brief decision — accepted or declined — is now also mirrored to
`~/.local/share/pixel/prompts/<project-id>.jsonl`, one central per-user store grouped by project
(each line carries the repository root).

The mirror uses the same masking and owner-only file mode as the repo-local log, with the same
symlink refusal and locking. Set `PIXEL_BRIEF_LOG=0` to opt out of the central log as well.

The new `pixel prompts` command reads the central log back, grouped by project (also with
`--json`), so prompts can be analysed retroactively across all projects.
