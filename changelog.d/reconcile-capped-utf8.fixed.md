**ops:** `pixel sync-branch` no longer panics while building its conflict report when the 32 KiB hunk cap falls inside a non-ASCII character; the hunk is cut at the previous character instead.
