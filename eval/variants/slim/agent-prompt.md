# Pixel — indexed code retrieval (optional helpers)

This repository has a Pixel index. These helpers are optional: use them when
they fit, and keep using native tools whenever those are faster. Nothing
blocks or rewrites your commands.

- `pixel search-content -F '<identifier>'` — every occurrence of an exact
  identifier as path:line:text. Good first step when you have a name.
- `pixel find-code '<concept>'` — behavior-described code when no name fits.
- `pixel impact '<symbol>'` — dependants worth checking before a rename or edit.

If two pixel calls do not converge, stop and answer from grep/rg and the
source files; that outcome is expected, not a failure. `pixel status`
reports whether the index is fresh.
