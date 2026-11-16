---
paths:
  - "crates/**/*.rs"
---

# Rust Style the Workspace Lints Enforce

Loaded when a Rust source file is in play. `cargo clippy --workspace
--all-targets -- -D warnings` is the CI gate and the lint table in the root
`Cargo.toml` is the policy; write new code in the shape the four pedantic lints
below expect instead of fixing it after. Run `cargo clippy -p <crate> --all-targets -- -D warnings` before
every commit; `cargo clippy --fix` handles three of the four but needs the
cleanup listed at the end.

## `uninlined_format_args`: name the argument inside the braces

Every `format!`, `println!`, `eprintln!`, `write!`, `panic!`, `assert!`
message inlines a plain identifier. Expressions stay outside, named or not.

```rust
// no
format!("{}: {}", path.display(), e)
eprintln!("tick {}: {:?}", n, report)
// yes
format!("{}: {e}", path.display())      // `path.display()` is an expression: stays out
eprintln!("tick {n}: {report:?}")
assert!(out.status.success(), "git {args:?}: {out:?}")
```

## `map_unwrap_or`: fold the map into the fallback

`Option`/`Result` chains ending in `.map(f).unwrap_or(x)` collapse into one
adapter; pick the one that says what the fallback is.

| You wrote | Write instead | When |
| --- | --- | --- |
| `.map(f).unwrap_or(x)` | `.map_or(x, f)` | the default is cheap |
| `.map(f).unwrap_or_else(g)` | `.map_or_else(g, f)` | the default is computed |
| `.map(pred).unwrap_or(false)` | `.is_some_and(pred)` / `.is_ok_and(pred)` | a boolean test |
| `.map(pred).unwrap_or(true)` | `.is_none_or(pred)` | a boolean test that passes on absence |
| `.map(f).unwrap_or_default()` | `.map_or_default(f)` | the default is `Default::default()` (Clippy 1.100+) |

```rust
// no
SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
entry.get("command").and_then(|c| c.as_str()).map(|c| !c.contains(marker)).unwrap_or(true)
// yes
SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
entry.get("command").and_then(|c| c.as_str()).is_none_or(|c| !c.contains(marker))
by_file.get(f).map_or(&[][..], Vec::as_slice)   // `&[]` alone does not coerce here
```

`clippy --fix` may leave `.map_or(true, |v| v == x)`; that is
`unnecessary_map_or`, write `.is_none_or(|v| v == x)`.

## `redundant_closure_for_method_calls`: pass the method, not a closure around it

When the closure only calls one method on its argument, name the method.
Use the short path: the prelude traits and `str`/`Vec`/`String`/`Path`
are in scope, so `std::string::ToString::to_string` (what `--fix` writes)
becomes `ToString::to_string`.

```rust
// no
args.iter().map(|s| s.to_string())
names.iter().map(|s| s.as_str())
.map(|v| v.len())
.map(|e| e.to_lowercase())
.filter(|s| !s.is_empty())          // negation: keep the closure
// yes
args.iter().map(ToString::to_string)
names.iter().map(String::as_str)
.map(Vec::len)
.map(str::to_lowercase)
.filter(|s| !s.is_empty())
```

Other shapes met in this tree: `AsRef::as_ref`, `Vec::is_empty`,
`Path::to_path_buf`, `DirEntry::file_name`, `IntentSource::as_str`.

## `items_after_statements`: declare items before the first statement

A `const`, `static`, `struct`, `fn` or `use` inside a body goes before the
first `let`/expression of that body, or out of the body altogether. Which
one, by kind of item:

- **A cap or limit used by one operation** (`CONTENT_PROBE_LIMIT`,
  `MAP_FILE_CAP`, `SNIPPET_CAP_CHARS`, `MAX_INLINE_BACKLOG`): module-level
  `const` next to the function, its explanatory comment moved with it as a
  doc comment. Two arms that declared the same value (`EDGE_LIMIT` twice)
  share one constant.
- **A local type or helper that only this function uses** (`struct Staged`,
  `struct Acc`, `type ScoredRow`, `fn bump`, `fn walk`): first lines of the
  function body, doc comment attached.
- **A `use`** (`use fs2::FileExt;`, `use sha2::{Digest, Sha256};`,
  `use pixel_context::estimate_tokens;`): the file's import block. In a
  test, the test module's `use` list.
- **A `static` counter** (`BACKUP_SEQ`): first line of the function.
- **A test-only `const`** (`const N: usize = 5300;`): first line of the test.

```rust
// no
fn op_map(&mut self) -> Result<Value, String> {
    let store = self.graph()?;
    // Hard cap so a pathological repo stays bounded ...
    const MAP_FILE_CAP: usize = 2000;
    ...
// yes
/// Hard cap so `map` on a pathological repo stays bounded; the flag
/// surfaces in the output so a truncated map is never passed off as complete.
const MAP_FILE_CAP: usize = 2000;

fn op_map(&mut self) -> Result<Value, String> {
    let store = self.graph()?;
    ...
```

## After `cargo clippy --fix`

It applies the first three lints mechanically and never touches
`items_after_statements`. Before committing its output:

1. `sed` the long paths: `std::string::ToString::to_string` →
   `ToString::to_string`, `std::string::String::as_str` → `String::as_str`,
   `std::vec::Vec::len` → `Vec::len`, `std::convert::AsRef::as_ref` →
   `AsRef::as_ref`.
2. Fix the `unnecessary_map_or` it can introduce (see above).
3. `cargo fmt --all`: the rewrites change line lengths.
4. Read the diff: `--fix` does not know that `&[]` needs `&[][..]` in a
   `map_or`, and it does not move items.

## The rest of the table

The other lints in `[workspace.lints.clippy]` are older and their idioms are
in the table's comments: `// SAFETY:` above every `unsafe` block,
`T::try_from(x).is_ok()` over manual range checks, `assert!(cond, ..)` over
`if !cond { panic!() }`, no wildcard arm that hides a single variant,
`sort_unstable` for primitives, `1_000_000` not `1000000`, no `dbg!`/`todo!`.
Enabling another pedantic lint means bringing every crate to zero in a stack
of one PR per crate and adding it to the table in the last one.
