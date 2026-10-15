---
name: rust-guidelines
description: Microsoft's Pragmatic Rust Guidelines (M-* rules) applied to new or changed Rust code in this workspace. Use whenever writing, refactoring or reviewing code under crates/ — API shape, error types, panics vs errors, docs, performance, test design. Adds the borrowing, type-state, TODO and test-naming rules from Apollo GraphQL's rust-best-practices that Microsoft does not cover. Complements .agents/rules (mutation gate, lint idioms), does not replace them.
---

# Pragmatic Rust Guidelines for this workspace

The full text of Microsoft's Pragmatic Rust Guidelines (MIT, 90 rules with
ids `M-*`) lives next to this file in `guidelines.txt` (136 KB). Do not read
the whole file into context; grep the id you need:

```bash
grep -n -A 40 "(M-PANIC-ON-BUG)" .agents/skills/rust-guidelines/guidelines.txt
```

Precedence when two rules disagree: `Cargo.toml` lint table > `.agents/rules/`
> this checklist. The rules in `.agents/rules/` (`rust-style.md`,
`mutation-gate.md`, `test-hygiene.md`) already cover the clippy idioms and
the mutation gate; this skill adds the design-level rules.

The checklist also carries the points of Apollo GraphQL's
`rust-best-practices` skill (MIT, ideas only, no text vendored) that
Microsoft does not cover: borrowing and ownership, the type-state pattern,
`TODO(#NN)`, test naming and snapshot tests. Where the two sources
disagree, Microsoft wins. The one known conflict: Apollo says "never
`unwrap()`/`expect()` outside tests"; M-PANIC-ON-BUG applies here instead.
An `expect("<invariant that was violated>")` on a detected programming bug
is correct; an `unwrap()` on external input (I/O, user input, git state,
another process's output) is not, that is an error to return.

## Checklist for every Rust edit

Run through it before `cargo clippy`; each line names the rule to grep.

**Errors and panics**
- A detected programming bug (broken invariant, impossible state) is a
  `panic!`/`assert!` with a message that says what was violated, not an
  `Err` (M-PANIC-ON-BUG, M-PANIC-MESSAGE). Expected failures (I/O, user
  input, git state) are errors.
- Library crates expose one error struct per module or crate,
  `thiserror`-based, with a kind/variant readable by callers; no
  `Box<dyn Error>` or `String` errors in public signatures
  (M-ERRORS-CANONICAL-STRUCTS, M-DONT-LEAK-TYPES).
- Conversions between error types go through `From` impls and `?`, not
  repeated `map_err` (M-FROM-ERROR).
- `unsafe` needs a `// SAFETY:` reason and a safe alternative was tried
  first (M-UNSAFE, M-UNSOUND).

**Borrowing and ownership**
- Parameters take `&str` and `&[T]`, not `String` and `Vec<T>`, unless the
  callee stores the value; `&T` over `.clone()` unless ownership must
  transfer.
- `Copy` types of 24 bytes or less (ids, offsets, small enums, `(u32, u32)`)
  are passed by value, not by reference.
- `Cow<'_, str>` / `Cow<'_, [T]>` when a function sometimes returns its
  input unchanged and sometimes an owned rewrite (path normalisation,
  escaping).
- No `.clone()` inside a loop without a comment on the same line saying why
  a borrow does not work; `clippy::redundant_clone` catches the rest.
- Iterator chains over index loops; no intermediate `.collect()` between
  two adaptors (`needless_collect`), collect once at the end into a
  pre-sized container (M-INITIAL-CAPACITY).

**API shape (public and crate-internal)**
- Prefer concrete types over generics, generics over `dyn Trait`
  (M-DI-HIERARCHY). Do not nest wrappers in signatures
  (`Arc<Mutex<Option<..>>>` as a parameter type) (M-AVOID-WRAPPERS,
  M-SIMPLE-ABSTRACTIONS).
- Accept `impl AsRef<Path>`/`AsRef<str>` and `impl Read`/`Write` at
  boundaries (M-IMPL-ASREF, M-IMPL-IO); keep `&Path`/`&str` inside.
- Essential behaviour is an inherent method, traits only add to it
  (M-ESSENTIAL-FN-INHERENT). Regular functions over associated ones when
  no `Self` is involved (M-REGULAR-FN).
- Types with more than three constructor parameters get a builder whose
  `.build()` returns `Result` and does the validation (M-INIT-BUILDER,
  M-BUILD-RESULT).
- Newtypes guard an invariant; if they cannot, use the plain type
  (M-STRONG-TYPES, M-STRONG-TYPES-GUARD).
- Type-state for a state machine whose invalid transition is a bug, not
  an error: the state is a type parameter, and a method exists only on the
  state that may call it. Use it where an enum state plus a `match` that
  panics on the wrong arm exists today (daemon connection and protocol
  steps, the install/uninstall restore flow); keep the enum when a wrong
  transition is a runtime outcome the caller must handle (M-STRONG-TYPES).

  ```rust
  struct Connection<State> { socket: UnixStream, _state: PhantomData<State> }
  struct Disconnected;
  struct Connected;

  impl Connection<Disconnected> {
      fn handshake(self) -> Result<Connection<Connected>, DaemonError> { /* … */ }
  }
  impl Connection<Connected> {
      fn send(&mut self, frame: &[u8]) -> io::Result<()> { /* only a connected socket can send */ }
  }
  ```
- Public types derive `Debug`; user-facing ones implement `Display`
  (M-PUBLIC-DEBUG, M-PUBLIC-DISPLAY). Types crossing threads are `Send`
  (M-TYPES-SEND). Service-like structs (daemon client, stores) are cheap
  `Clone` handles (M-SERVICES-CLONE).
- One visible path per item: no glob re-exports, no preludes, no re-export
  of foreign crates' items (M-SINGLE-ITEM-PATH, M-NO-GLOB-REEXPORTS,
  M-NO-PRELUDE, M-FOREIGN-REEXPORTS).
- Parameter order is consistent across a module (M-PARAMETER-CONSISTENCY).
  Short names, no weasel words such as `Manager`, `Helper`, `Util`, `Data`
  (M-SHORT-NAMES, M-WEASEL-WORDS).
- No `static mut`, no global registries; pass state in (M-AVOID-STATICS).

**Docs**
- The first doc sentence is one line, about 15 words, and states what the
  item does (M-FIRST-DOC-SENTENCE). Sections are `# Errors`, `# Panics`,
  `# Examples`, `# Safety` in that spelling (M-CANONICAL-DOCS).
- Every non-test module has a `//!` header saying what lives there and why
  (M-MODULE-DOCS). Magic numbers carry a comment with their origin
  (M-DOCUMENTED-MAGIC).
- Comments describe the code as it is, never the change history or the
  alternative that was rejected (M-NO-META-DESIGN-DOCUMENTATION).
- Every `TODO` names an issue, `// TODO(#NN): what is missing`; a `TODO`
  without one is not merged. The `todo` lint in `Cargo.toml` stays `warn`
  (a `todo!()` in the tree is visible in every build, never silent).

**Performance (indexers, graph, daemon paths)**
- Pre-size collections when the count is known; reuse buffers across loop
  iterations; `shrink_to_fit` or `into_boxed_slice` for long-lived
  immutable results (M-INITIAL-CAPACITY, M-MEM-REUSE, M-SHRINK-TO-FIT,
  M-BOX-DST).
- Logging on a hot path is gated by level before formatting
  (M-LOG-OVERHEAD); production code logs, it does not `println!`, except
  for a command's own stdout contract (M-LOG-NOT-PRINT).
- Long-running loops in async code yield (M-YIELD-POINTS); none exist yet,
  see "Out of scope".

**Tests**
- A test asserts an observable contract that a business-rule change would
  break, never `assert_eq!(f(x), f(x))` or a value copied from the
  implementation (M-TAUTOLOGICAL-TESTS). This is the same rule the
  mutation gate enforces.
- Integration tests live in `tests/`; shared helpers in a `test-util`
  feature or a dedicated crate, never in a `pub mod test_utils` of the
  library (M-INTEGRATION-TESTS, M-TEST-UTIL).
- I/O and clock access sit behind a small trait or injected function so
  tests do not hit the network or sleep (M-MOCKABLE-SYSCALLS).
- Test names read `subject_should_outcome_when_condition`
  (`publish_should_refuse_when_tree_is_clean`); the name states the
  contract the assertion checks, so a `MISSED` mutant line is readable
  without opening the test.
- Snapshot tests only for generated text (agent prompts, `--help` output,
  metrics lines) via `insta`; a changed snapshot is reviewed as a behaviour
  change with a reason in the PR, never accepted with `cargo insta accept`
  to make CI green. Structured values are asserted field by field.

**Project**
- New crates are flat siblings under `crates/`, listed in the workspace,
  inheriting `edition`, `rust-version` and lints from the root
  (M-CRATES-FLAT-FOLDER, M-CRATES-IN-WORKSPACE, M-CARGO-WORKSPACE).
- Lint overrides use `#[expect(lint, reason = "..")]`, not `#[allow]`
  (M-LINT-OVERRIDE-EXPECT). Features are additive (M-FEATURES-ADDITIVE):
  `fastembed` and `model2vec` may both be on.

## Out of scope here

FFI (M-FFI-*), proc-macro rules (M-PROC-*), `mimalloc` and `target-cpu`
(M-MIMALLOC-APPS, M-TARGET-CPU) are not applied: the workspace has no FFI
or proc-macro crate, and allocator and target flags are release decisions
taken in `Cargo.toml`/CI, not per edit.

Async patterns (wshobson's `rust-async-patterns`, Microsoft's M-ASYNC-* and
M-YIELD-POINTS in practice) are not applied either: the workspace has no
`async fn`, `tokio` is pulled with the `rt` feature only, by one crate, to
drive a dependency. Revisit when the daemon goes async (an `async fn` in
`pixel-daemon` or a `tokio::main`); that PR adds the async rules to this
checklist.

## Reviewing

When asked to review Rust code against these guidelines, list violations as
`file:line — M-… — one sentence`, most severe first, and stop. Do not
annotate compliant code with comments such as "guideline compliant".

Source: <https://microsoft.github.io/rust-guidelines/> (`agents/all.txt`),
MIT License, Copyright (c) Microsoft Corporation. Refresh with
`scripts/dev/refresh-guidelines.sh`: it re-downloads the file and prints the
diff of `## … (M-…)` headings, so an added, renamed or removed upstream
rule is noticed; `crates/pixel/tests/cli/docs_drift.rs` then fails the
build if this file names an id the new text no longer has.

Borrowing, type-state, `TODO(#NN)`, test naming and snapshot rules:
<https://github.com/apollographql/skills/tree/main/skills/rust-best-practices>
(MIT, Copyright (c) Apollo Graph, Inc.), ideas restated in this file, no
text copied.
