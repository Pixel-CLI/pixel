# How to judge a sampled Ruby call edge

Each row of the sample TSV is one call edge a code-graph tool claims:
the call written at `caller_path:line` (inside the definition `caller`)
dispatches to the definition `callee`, written at `callee_path:callee_line`.
`written` is the receiver and method as the tool recorded them.

Read the source at BOTH ends (open the files; read enough context: the
enclosing class/module, includes, superclass, how the receiver is bound,
`let`/fixtures/associations/factories, etc.). Then give one verdict:

- **correct**: at run time Ruby (with Rails' own deterministic conventions:
  Zeitwerk autoload paths, association names -> classes, fixtures accessor
  -> model class, `delegate`, callbacks, concerns' `included do` /
  `class_methods`, ActiveJob/ActionMailer class-method dispatch, etc.)
  dispatches this call to exactly that definition, as far as the code shows.
- **wrong**: something else is called: a framework/gem/stdlib method
  (e.g. ActiveRecord `reload`, Capybara `visit`, RSpec `let`, Minitest
  `assert`), another class's method of the same name, a JavaScript/TS
  function, a local variable, etc.
- **undecidable**: the code does not settle it (receiver type unknowable
  from the source). Undecidable counts against the tool.

Rules:
- An edge to the only definition of a name in the repository counts as
  correct only when the receiver is shown to BE that type (or an object
  whose ancestry includes the callee's owner). Name uniqueness alone is not
  proof.
- A call with no receiver is a call on `self`: check that `self` at that
  point (instance of the enclosing class, the class itself in a class
  method, the RSpec example group, the Minitest test case, ...) really has
  the callee in its method lookup chain, and that nothing earlier in the
  chain (the class itself, a prepended/included module, a superclass,
  including gems/Rails) defines the same name first. A local variable of
  the same name shadows the method -> wrong.
- If the callee is a method defined by a gem the repo merely *monkey
  patches or reopens* in the repo, and the call really reaches the repo's
  definition, that is correct.
- Line numbers may be off by up to 3 lines for calls split over lines.

Output: write a TSV with columns `id<TAB>verdict<TAB>reason` (reason: one
short line, e.g. "fixture users(:kevin) is a User; User#ban defined",
"Capybara visit, not the repo's", "receiver is a Hash"), one row per input
row, to the output path you were given. Then reply with the counts
(correct / wrong / undecidable). Do not modify the repositories.

## Additions for this measurement (2026-10-15, the hybrid in v1)

- **Delta files** (`samples/*-delta-call.tsv`) have a `side` column:
  `v1-only` rows are edges v1's own graph has and the hybrid dropped
  (pixel-mvp proved another target at that site); `hybrid-only` rows are
  edges the hybrid added (pixel-mvp's proof). Judge each row's edge as
  written, whatever its side: is the call at `caller_path:line` dispatched
  to `callee`?
- **Routes** (`*-routes-added.tsv`): the row is a route declared at
  `config/routes*.rb:line` (`site_text`) and the claimed handler `callee`
  (`Controller#action`). Correct when Rails, reading that declaration with
  its enclosing `namespace`/`scope`/`resources`/`controller` blocks,
  dispatches one of the routes it declares to exactly that controller
  class and action method (the action defined in that class or inherited
  by it). Wrong when the route's controller or action is another, or when
  the declaration declares no such action (`only:`/`except:`).
- **References** (`*-references-added.tsv`): the row is a constant or
  method named without a call: a superclass (`class A < B`), an
  `include`/`extend`/`prepend`, a callback or a `send` target. Correct
  when Ruby's constant lookup (or the method lookup, for a symbol) from
  that line names exactly that definition.
- A row with a `reused_verdict` was judged before (the `from` column names
  the verdict); copy it unless the source shows it wrong.
