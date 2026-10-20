// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! How far a stub written in a test file reaches (ADR-0039).
//!
//! #106 made what a test file defines on an object the rules cannot name a
//! stub, which taints the lookups of every caller in a test file. An
//! example file (`*_spec.rb`, `*_test.rb`) runs only in its own examples,
//! on objects it creates or receives from the application and from shared
//! test code: another example file's code never runs on them, unless it
//! requires the file, the file declares shared code (`shared_examples`,
//! `shared_context`, `RSpec.configure`), or it names one of the file's
//! classes or modules as a superclass or a mixin. A shared group declared
//! without a receiver in a group's body is no shared code: RSpec makes it
//! that group's and its nested groups' alone, all in the file. So a
//! caller in an
//! example file meets the stubs of shared test code and of its own file
//! only; a caller in shared test code, which may run on any example
//! file's objects, meets them all.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mvp_model::{EdgeKind, SymbolKind};

use super::{Decision, Index, Side, split_method, test_file};
use crate::lang::World;
use crate::ruby::{Context, Fact, MIXIN, VALUE, VALUE_FIXTURE, VALUE_LET};

/// Whose stubs an index's lookups meet.
#[derive(Clone)]
pub(super) enum Tests {
    /// None: the callers outside test files.
    Without,
    /// Every test file's.
    All,
    /// Every test file's but those of these example files.
    Except(Rc<HashSet<String>>),
}

impl Tests {
    pub(super) fn any(&self) -> bool {
        !matches!(self, Tests::Without)
    }

    /// Whether the stubs of the test file `file` count.
    pub(super) fn keeps(&self, file: &str) -> bool {
        match self {
            Tests::Without => false,
            Tests::All => true,
            Tests::Except(files) => !files.contains(file),
        }
    }
}

/// The calls that declare test code other files' examples run.
const SHARED: &[&str] = &[
    "shared_examples",
    "shared_examples_for",
    "shared_context",
    "configure",
];

/// The example files of the test program: test files named `*_spec.rb` or
/// `*_test.rb` that no other file requires, that declare no shared code
/// (but shared groups scoped to one of their groups), and none of whose
/// classes or modules another file names as a superclass or a mixin.
fn examples(world: &World<'_>, file_of: &HashMap<&str, &str>) -> HashSet<String> {
    let stem = |path: &str| {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.strip_suffix(".rb").unwrap_or(name).to_owned()
    };
    let mut examples: HashMap<String, String> = world
        .files
        .iter()
        .map(|(path, _)| path.as_str())
        .filter(|path| {
            test_file(path) && (path.ends_with("_spec.rb") || path.ends_with("_test.rb"))
        })
        .map(|path| (path.to_owned(), stem(path)))
        .collect();
    // The shared groups declared in a group's body, which that group
    // alone sees (ADR-0041), by line: each answers one declaration there.
    let mut scoped: HashMap<(&str, u32), usize> = HashMap::new();
    for (path, facts) in world.files {
        if path.ends_with("_spec.rb") && facts.contains("shared") {
            for fact in Fact::decode_all(facts) {
                if let Fact::Shared {
                    line, scoped: true, ..
                } = fact
                {
                    *scoped.entry((path.as_str(), line)).or_default() += 1;
                }
            }
        }
    }
    let mut required = HashSet::new();
    let mut shared = HashSet::new();
    let mut mixed: Vec<(String, &str)> = Vec::new();
    for edge in world.edges {
        let file = file_of.get(edge.src.as_str()).copied().unwrap_or_default();
        match edge.kind {
            EdgeKind::Import => {
                required.insert(stem(&Context::decode(&edge.context).name));
            }
            // A call's target as written ends with its name.
            EdgeKind::Call if SHARED.iter().any(|name| edge.target.ends_with(name)) => {
                let context = Context::decode(&edge.context);
                if SHARED.contains(&context.name.as_str())
                    && matches!(context.qualifier.trim_start_matches("::"), "" | "RSpec")
                    && file.ends_with("_spec.rb")
                {
                    match scoped.get_mut(&(file, edge.line)) {
                        Some(left) if *left > 0 => *left -= 1,
                        _ => {
                            shared.insert(file);
                        }
                    }
                }
            }
            EdgeKind::Reference => {
                let context = Context::decode(&edge.context);
                if context.qualifier.starts_with(MIXIN) {
                    let last = context.name.rsplit("::").next().unwrap_or_default();
                    mixed.push((last.to_owned(), file));
                }
            }
            _ => {}
        }
    }
    examples.retain(|path, stem| !required.contains(stem) && !shared.contains(path.as_str()));
    let mut named: HashMap<&str, Vec<&str>> = HashMap::new();
    for symbol in world.symbols {
        if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Module)
            && examples.contains_key(&symbol.file)
        {
            let last = symbol.name.rsplit("::").next().unwrap_or_default();
            named.entry(last).or_default().push(symbol.file.as_str());
        }
    }
    for (last, by) in &mixed {
        for file in named.get(last.as_str()).into_iter().flatten() {
            if file != by {
                examples.remove(*file);
            }
        }
    }
    examples.into_keys().collect()
}

/// Decides the sites of test callers at `positions`. Without any stub in a
/// test file, with `index`; else a caller in shared test code, or in an
/// example file that mixes a module into an object the rules cannot name,
/// with every stub (#106); a caller in another example file with the
/// stubs of shared test code, then none of its own file's names.
pub(super) fn decide<'a>(
    world: &World<'a>,
    file_of: &HashMap<&str, &str>,
    positions: Vec<usize>,
    index: Index<'a>,
    build: &dyn Fn(&Tests) -> Index<'a>,
    decided: &mut [Decision],
) {
    let stubbed = std::iter::once(&index)
        .chain(&index.loaders)
        .any(|index| index.facts.test_tainted());
    if !stubbed {
        for position in positions {
            decided[position] = index.decide(position, &world.edges[position]);
        }
        return;
    }
    let examples = examples(world, file_of);
    let stubbing: HashSet<String> = examples
        .iter()
        .filter(|file| index.facts.stubs_in(file))
        .cloned()
        .collect();
    let file = |position: usize| {
        file_of
            .get(world.edges[position].src.as_str())
            .copied()
            .unwrap_or_default()
    };
    // An example file's own mixins into objects the rules cannot name, as
    // the modules they may bring: `None` where one may be any module.
    let mut spread: HashMap<&str, Option<HashSet<&'a str>>> = HashMap::new();
    let (own, every): (Vec<usize>, Vec<usize>) = positions.into_iter().partition(|&position| {
        let file = file(position);
        examples.contains(file)
            && spread
                .entry(file)
                .or_insert_with(|| spread_by(&index, file))
                .is_some()
    });
    if !every.is_empty() {
        let all = build(&Tests::All);
        for position in every {
            decided[position] = all.decide(position, &world.edges[position]);
        }
    }
    if own.is_empty() {
        return;
    }
    let shared = build(&Tests::Except(Rc::new(stubbing)));
    for position in own {
        let edge = &world.edges[position];
        let file = file(position);
        let modules = spread.get(file).cloned().flatten().unwrap_or_default();
        decided[position] = shared.decide(position, edge).filter(|&target| {
            !stubbed_here(
                &shared,
                file,
                &modules,
                &Context::decode(&edge.context),
                target,
            )
        });
    }
}

/// The modules the example file `file` mixes into objects the rules cannot
/// name, with every module they bring along; `None` when one is written
/// through a constant whose value the rules do not read, which may be any.
pub(super) fn spread_by<'a>(index: &Index<'a>, file: &str) -> Option<HashSet<&'a str>> {
    let mut named = Vec::new();
    for (path, scope) in index.facts.spread_in(file) {
        if index
            .facts
            .unread_constant(super::facts::last_segment(path))
        {
            return None;
        }
        for name in index.may_name(path, scope) {
            if index.consts.contains(name.as_str()) {
                return None;
            }
            named.extend(index.known(&name));
        }
    }
    // A module a class or module of the graph mixes in may change chains
    // the decision reads, not only names: every stub then.
    let brought = index.brought(named);
    let last = |name: &str| name.rsplit("::").next().unwrap_or_default().to_owned();
    let lasts: HashSet<String> = brought.iter().map(|module| last(module)).collect();
    let held = index
        .mixins
        .iter()
        .any(|mixin| lasts.contains(&last(&mixin.name)));
    (!held).then_some(brought)
}

/// Whether what the example file `file` stubs may change the decision: it
/// defines on an object the rules cannot name a name the decision went
/// through (the call's, its target's, one its typed receiver's path calls,
/// ADR-0038), or mixes in a module defining one, or one the target's
/// owner, or its chain, holds.
fn stubbed_here(
    index: &Index<'_>,
    file: &str,
    modules: &HashSet<&str>,
    context: &Context,
    target: usize,
) -> bool {
    // A stub changes which method a name finds, never which class or
    // module a constant names.
    if !matches!(
        index.symbols[target].kind,
        SymbolKind::Method | SymbolKind::Function
    ) {
        return false;
    }
    let facts = &index.facts;
    let target = &index.symbols[target].name;
    let (owner, side, reached) = split_method(target).unwrap_or((target, Side::Instance, target));
    let mut names = vec![context.name.as_str(), reached];
    if let Some(path) = context.qualifier.trim().strip_prefix(VALUE) {
        let path = path
            .strip_prefix(VALUE_FIXTURE)
            .or_else(|| path.strip_prefix(VALUE_LET))
            .unwrap_or(path);
        names.extend(
            path.split('.')
                .map(|segment| segment.split('(').next().unwrap_or(segment)),
        );
    }
    let defined = |name: &str| {
        modules.iter().any(|module| {
            index.methods.iter().any(|owned| {
                owned
                    .get(module)
                    .is_some_and(|methods| methods.contains_key(name))
            })
        })
    };
    names
        .iter()
        .any(|name| facts.stubbed_in(file, name) || defined(name))
        || (!modules.is_empty()
            && (modules.contains(owner)
                || index
                    .chain(owner, side)
                    .owners()
                    .any(|step| modules.contains(step))))
}

#[cfg(test)]
mod audited_contract_tests {
    use super::super::fixture::Generation;
    use super::*;
    use mvp_model::SymbolKind::Class;

    #[test]
    fn stub_selection_should_keep_only_the_requested_examples() {
        assert!(!Tests::Without.any());
        assert!(Tests::All.any());
        let tests = Tests::Except(Rc::new(HashSet::from(["spec/one_spec.rb".into()])));
        assert!(tests.any());
        assert!(!tests.keeps("spec/one_spec.rb"));
        assert!(tests.keeps("spec/two_spec.rb"));
    }

    #[test]
    fn example_detection_should_exclude_non_test_paths_and_shared_or_required_example_files() {
        let mut generation = Generation::default();
        generation
            .file("spec/one_spec.rb")
            .file("test/two_test.rb")
            .file("lib/helper.rb")
            .file("spec/support.rb")
            .def("spec/shared_spec.rb", Class, "Shared", 1, 2);
        generation.facts(
            "spec/shared_spec.rb",
            &[Fact::Shared {
                name: "shared".into(),
                line: 1,
                scoped: false,
                metadata: false,
            }],
        );
        generation.call(
            "spec/shared_spec.rb",
            "spec/shared_spec.rb",
            1,
            "RSpec",
            "shared_examples",
            &[],
        );
        let world = generation.world();
        let file_of = world
            .symbols
            .iter()
            .map(|symbol| (symbol.uid.as_str(), symbol.file.as_str()))
            .collect();
        let examples = examples(&world, &file_of);
        assert_eq!(
            examples,
            HashSet::from(["spec/one_spec.rb".into(), "test/two_test.rb".into()])
        );
    }
}
