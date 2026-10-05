# Optional installed-path diagnosis

Local rebuild, install, index and doctor are optional. Use them when debugging
installed behavior or when the user asks to deploy locally. They are not
prerequisites to writing tests, publishing a PR or merging a CI-green change.

When diagnosing a hook or deployed prompt, prefer a real payload through the
installed hook path to a hand-built call that bypasses the failing seam. Follow
AGENTS.md's safe side-build procedure and preserve the managed home install.

Only claim the installed behavior is verified when evidence comes from that
binary and path. A passing unit test or CI run verifies its own candidate;
it does not imply the user's installed binary was updated. If authentication
prevents a real agent session, report that limit and the hook-level evidence.
