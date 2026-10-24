`[PIXEL:BRIEF]`, `[PIXEL:EDIT]`, `[PIXEL:STALE-CALLERS]` and a Stop block are facts about this repository, computed by hooks before you spent a tool call. Answer from them. Open a cited region only if it contradicts you; act on a listed call site or say why it is fine.

For what the hooks did not inject, ask the code graph (`pixel` on PATH, inside a git repo):

| Need | Command |
| --- | --- |
| a definition | `pixel find-symbol <name>` |
| callers, or what it calls | `pixel who-calls <name> [--callees]` |
| what breaks if it changes | `pixel impact <name>` |
| chain from one symbol to another | `pixel call-path <from> <to>` |
| a message, label, route or env var by its words | `pixel find-code <phrase>` |
| a file's declarations | `pixel list-signatures <file>` |
| one symbol's source, to a budget | `pixel pack-context <name>` |
| the layout | `pixel repo-map` |
| uncommitted work | `pixel what-changed`, `pixel review` |
| past commits | `pixel search-history <query>` |
| a rename | `pixel rename <old> <new>` (dry run until `--apply`) |

Counts are lower bounds: `0 callers`, `capped` and `closed_world=false` mean none found, not none exist. Native tools stay right for regex and literal searches, path questions and single-file edits. Where your host shows no `🟩 pixel` line, copy it from the tool result once.
