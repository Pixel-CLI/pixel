# Private advisory release

Read this before promising a patched version or importing an advisory fix.
Keep the release record and unannounced details in the private advisory or a
private scratchpad. A public draft PR is public; delaying its merge does not
keep its patch or threat model under embargo.

1. **Choose the candidate and version first.** Apply the skill's version
   convention against the last shipped tag, including removed commands and
   flags without aliases. Decide a maintenance branch before promising a
   patch. Leave `patched_versions` unset until the choice is settled; record
   the reason and update it if the candidate changes.
2. **Validate the private patch before import.** Record base/head SHAs,
   regression tests (every consuming path, including read-only opens), full
   gates and a mutation verdict for that diff on authorized private
   infrastructure. GitHub Actions do not run in temporary advisory forks;
   waiting for their public required checks cannot produce this evidence.
   Do not push the patch publicly to obtain checks. The public
   `mutants.yml` dispatch accepts an ancestor of the dispatch branch; it is
   a post-import verification, not private pre-merge validation. If private
   infrastructure is unavailable, record the missing verdict and obtain a
   maintainer decision before import; never describe it as passed.
3. **Plan the merge with the maintainer.** Identify author, eligible reviewer
   and merge operator. Inspect the actual advisory merge UI and applicable
   rules before waiting on `Expected` checks. GitHub documents branch
   protection exceptions for advisory merges, but v0.7.0 still encountered
   a ruleset block: do not assume protections always apply or always bypass.
   A ruleset exception needs its own explicit maintainer authorization.
   Save the complete configuration privately, identify the exact rule,
   choose the narrowest available exception and limit it to the import.
   Verify restoration immediately, including enforcement, checks, reviews
   and bypass actors; if restoration fails, stop and alert the maintainer.
   A release request does not authorize disabling repository-wide protection.
4. **Import and verify.** Compare the imported patch to the validated
   private content, record the actual public SHA, and inspect its CI and
   mutation verdict. A changed patch needs validation again. Resolve a
   survivor before preparing the release, covering all affected paths.
   Put the repository's GHSA URL in the `security` fragment; `prepare.sh`
   accepts it without inventing a public PR number, even while it is a draft.
5. **Release, verify, then disclose.** Follow the normal candidate guard,
   merge, tag and publication checks. Only after the fixed assets and
   installation paths are verified publish the advisory with the version
   actually shipped. Verify its `published_at`, patched versions and link.
   Publish or update the threat model and other previously embargoed details
   afterwards. Public documentation that already exists can be maintained,
   but new vulnerability details remain private until disclosure.

Close the record with the GHSA, private and imported patch SHAs, validation
commands/logs/verdicts, actual patched version, publication timestamp and
any exception/restoration evidence. List each follow-up PR, owner and state;
"open" is not "done". Keep one current next action so another session can
resume without following obsolete instructions.

Reference: GitHub's [temporary private fork documentation](https://docs.github.com/en/code-security/tutorials/fix-reported-vulnerabilities/collaborate-in-a-fork).
