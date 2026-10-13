# Designing deep modules

The vocabulary and rules every candidate, card and grilling question uses.
Adapted from `codebase-design` in `mattpocock/skills` (MIT, see
[LICENSE](LICENSE)), with the dependency categories mapped onto pixel's own
seams.

## Glossary

Use these terms exactly. Do not substitute "component", "service", "unit",
"API", "signature", "boundary" or "layer": consistent words are what make
two reviews comparable.

- **Module**: anything with an interface and an implementation. Deliberately
  scale-agnostic: a function, a type with its `impl`, a Rust module, a
  crate, or a slice across crates (the CLI → socket → daemon path of one
  op).
- **Interface**: everything a caller must know to use the module correctly:
  the types, and also invariants, ordering constraints, error modes,
  required configuration, environment variables, files it expects on disk,
  performance characteristics. A `pub fn` signature is only part of it.
- **Implementation**: the code inside a module.
- **Depth**: leverage at the interface — how much behaviour a caller (or a
  test) can exercise per unit of interface it has to learn. **Deep**: much
  behaviour behind a small interface. **Shallow**: an interface nearly as
  complex as the implementation.
- **Seam** (Michael Feathers): a place where behaviour can be altered
  without editing in that place; the location where a module's interface
  lives. Where to put the seam is a decision of its own, separate from what
  goes behind it.
- **Adapter**: a concrete thing that satisfies an interface at a seam. It
  names a role (which slot it fills), not a substance.
- **Leverage**: what callers get from depth — one implementation pays back
  across N call sites and M tests.
- **Locality**: what maintainers get from depth — change, bugs, knowledge
  and verification concentrate in one place. Fix once, fixed everywhere.

## Principles

- **Depth is a property of the interface, not of the implementation.** A
  deep module may be built from small private parts; they are just not in
  its interface. It can have **internal seams** (private, used by its own
  tests) as well as the **external seam** at its interface. Do not widen the
  interface because a test wants an internal seam.
- **The deletion test.** Imagine deleting the module. If complexity
  vanishes, it was a pass-through. If complexity reappears across N
  callers, it was earning its keep.
- **The interface is the test surface.** Callers and tests cross the same
  seam. Wanting to test *past* the interface means the module is the wrong
  shape.
- **One adapter means a hypothetical seam; two adapters mean a real one.**
  Do not introduce a trait or a port unless something actually varies
  across it (typically production plus test).
- **Accept dependencies, return results.** A function that takes its git
  repository, clock or socket as an argument, and returns a value instead of
  mutating shared state, is testable through its interface.

## Rejected framings

- **Depth as a ratio of implementation lines to interface lines**
  (Ousterhout): it rewards padding the implementation. `pixel audit`'s
  sizes and signature counts locate candidates; they never grade them.
- **"Interface" as the `pub` items or a `trait`**: too narrow; the interface
  includes every fact a caller must know.
- **"Boundary"**: overloaded (bounded contexts, crate boundaries). Say seam
  or interface.

## Dependency categories

Classify what a candidate depends on. The category decides how the deepened
module is tested across its seam.

1. **In-process.** Pure computation, in-memory state, no I/O: ranking,
   parsing, scoring, formatting. Always deepenable: merge, then test through
   the new interface directly. No adapter.
2. **Local-substitutable.** A dependency with a local stand-in already used
   by the suite: a temporary git repository built as a fixture
   (`.agents/rules/test-hygiene.md`), a `tempdir` for the file system, a
   SQLite file in that tempdir. Deepenable whenever the stand-in exists; the
   seam stays internal, no port at the interface.
3. **Owned, across a process (ports and adapters).** pixel's own daemon,
   reached over its socket with the `pixel-proto` envelope. The seam
   already has two adapters: the CLI integration tests spawn the binary and
   talk over the socket, while the daemon tests call `Service::handle`
   directly (`crates/pixel-daemon/src/api.rs`). Logic belongs on one side of
   that seam, in one deep module; the transport is the adapter.
4. **True external (mock).** What pixel does not control: a remote LLM
   provider (`classify --remote-preset`), the web lookup (`web-search`),
   GitHub, the embedding model download, the agent tools whose configs
   `pixel setup` edits. The deepened module takes it as an injected port;
   tests provide a fake (the `docker-setup-smoke` skill's scripted fake model
   is one).

## Testing strategy: replace, don't layer

- Once tests exist at the deepened module's interface, the old unit tests
  on the shallow modules it absorbed are waste: delete them — after checking
  that each of their assertions is carried by an interface test, because
  the mutation gate judges every function the diff touches.
- New tests assert on observable outcomes through the interface (JSON
  fields, exit codes, files written), not on internal state.
- A test that must change when the implementation changes is testing past
  the interface.

## Design it twice

When the user wants to compare interfaces for the chosen candidate (after
Ousterhout: the first idea is rarely the best):

1. **Frame the problem space** for the user: the constraints any interface
   must meet, the dependencies and their category, and a rough code sketch
   that makes the constraints concrete (not a proposal). Show it, then go
   straight to step 2; the user reads while the sub-agents work.
2. **Spawn three or four sub-agents in parallel**, each with the same
   technical brief (file paths, coupling, dependency category, what sits
   behind the seam, this file's vocabulary, the ARCHITECTURE.md section)
   and a different constraint:
   - minimise the interface: one to three entry points, maximum leverage
     each;
   - maximise flexibility: many use cases, extension;
   - optimise for the most common caller: the default case trivial;
   - (when a category 3 or 4 dependency is involved) design around ports
     and adapters.

   Each returns: the interface (types, functions, invariants, ordering,
   error modes), a caller's usage example, what the implementation hides,
   the dependency strategy and adapters, and where leverage is high or
   thin.
3. **Present the designs one after another, then compare** them on depth,
   locality and seam placement. Give your recommendation, or a hybrid when
   parts combine well. Be opinionated: the user wants a strong read, not a
   menu.
