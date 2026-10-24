// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! RSpec's example groups as classes (ADR-0041).
//!
//! A call written where `self` is an example of a group (`extract::rspec`)
//! follows the groups from its own outwards: each group's `def`s, `let`s
//! and `subject`s, the last written of a name winning, then its parent's,
//! up to the top group, past which come what RSpec and the configuration
//! mix in, which the rules do not hold. A group with metadata of its own
//! may have modules the configuration filters on that metadata mixed in
//! between itself and its parent; a group whose body holds a statement
//! the rules do not read may define any name: lookup proves nothing past
//! either. A `def` found is the call's target; a `let` or `subject` found
//! is no symbol, but the value of its block's last expression types the
//! receivers it makes, read from the caller's groups, since its block runs
//! on the caller's example (`values`).
//!
//! `include_context "n"` and `include_examples "n"` run the shared group
//! the repository declares once at a file's top level under that name, in
//! the including group's body: its `def`s and `let`s are that group's, at
//! the include's line. Declared nowhere (a gem's), more than once, or also
//! in a group (which hides it there), or including another itself, the
//! including group is unknown from its own step. A shared group declared
//! with metadata may be included in any group with matching metadata, so
//! past the top group nothing is proven.
//!
//! The model is read only for a program whose spec code calls
//! `RSpec.configure` or `RSpec.describe` and holds nothing that may change
//! RSpec's DSL ([`Fact::RspecOff`]); otherwise a group's `def` taints its
//! name for test callers as before.

use rustc_hash::FxHashMap as HashMap;

use super::ancestors::Lookup;
use super::{Decision, Index, Side};
use crate::ruby::routes::camelize_path;
use crate::ruby::{Context, Fact, GROUP, SHARED_GROUP};

/// The example groups of the program's spec files.
#[derive(Default)]
pub(super) struct Rspec {
    /// The program's spec code uses RSpec, and nothing turns the model off.
    on: bool,
    /// Spec file → its groups.
    files: HashMap<String, Spec>,
    /// Each `def` written in a group, by file: what taints test callers'
    /// lookups when the model is off.
    pub(super) defs: Vec<(String, String)>,
    /// A shared group declared with metadata, which may be included in
    /// any group with matching metadata.
    meta_shared: bool,
    /// The configuration includes `FactoryBot::Syntax::Methods`.
    factory_syntax: bool,
    /// factory_bot's factories by name; `None` for one declared twice.
    factories: HashMap<String, Option<Factory>>,
}

/// A factory as declared ([`Fact::Factory`]).
struct Factory {
    class: String,
    parent: String,
    scope: Vec<String>,
    initialized: bool,
}

/// A shared group as declared, at a file's top level or in a group.
struct Shared {
    file: String,
    line: u32,
}

/// The groups of one spec file.
struct Spec {
    /// By the line of their declaration; `None` for a line two groups
    /// share.
    groups: HashMap<u32, Option<Group>>,
    /// Group → the groups nested in it; `None` past a line two groups
    /// share, which may hide one.
    children: Option<HashMap<u32, Vec<u32>>>,
}

struct Group {
    parent: u32,
    described: String,
    scope: Vec<String>,
    metadata: bool,
    opaque: bool,
    nests: bool,
    members: Vec<Member>,
    /// The modules its body includes, as written, in order.
    mixins: Vec<String>,
}

/// A `def`, `let` or `subject` of a group.
#[derive(Clone)]
pub(super) struct Member {
    pub(super) name: String,
    /// Where it is defined in its group: the include's line for one a
    /// shared group brings.
    pub(super) line: u32,
    pub(super) def: bool,
    /// The value path of a `let`'s or `subject`'s last expression.
    pub(super) value: Option<String>,
    /// For one a shared group brings: the file and line written, and the
    /// nesting there.
    pub(super) shared: Option<(String, u32, Vec<String>)>,
}

/// What lookup from an example finds for a name among its groups.
pub(super) enum Found<'r> {
    /// The nearest definition, with the nesting its group is written in.
    Member(&'r Member, &'r [String]),
    /// No group defines it, and nothing between them may.
    Nowhere,
    /// A step the rules cannot see past comes first.
    Unknown,
}

impl Rspec {
    /// Reads the groups of the files `visible` accepts.
    pub(super) fn read(files: &[(String, String)], visible: &dyn Fn(&str) -> bool) -> Rspec {
        let mut rspec = Rspec::default();
        let mut off = false;
        let mut shared: HashMap<String, Vec<Shared>> = HashMap::default();
        let mut shared_groups: HashMap<(String, u32), Group> = HashMap::default();
        let mut includes: Vec<(String, u32, String, u32)> = Vec::new();
        for (path, facts) in files.iter().filter(|(path, _)| visible(path)) {
            if !["group", "rspec", "shared", "factory", "FactoryBot"]
                .iter()
                .any(|tag| facts.contains(tag))
            {
                continue;
            }
            let mut groups: HashMap<u32, Option<Group>> = HashMap::default();
            let mut members: Vec<(u32, Member)> = Vec::new();
            let mut values: HashMap<(u32, String, u32), String> = HashMap::default();
            let mut mixins: Vec<(u32, String)> = Vec::new();
            for fact in Fact::decode_all(facts) {
                match fact {
                    Fact::Rspec => rspec.on = true,
                    Fact::RspecOff => off = true,
                    Fact::GroupDef { name } => rspec.defs.push((path.clone(), name)),
                    Fact::Shared {
                        name,
                        line,
                        metadata,
                        ..
                    } => {
                        rspec.meta_shared |= metadata;
                        shared.entry(name).or_default().push(Shared {
                            file: path.clone(),
                            line,
                        });
                    }
                    Fact::Include { group, name, line } => {
                        includes.push((path.clone(), group, name, line));
                    }
                    Fact::GroupInclude { group, module } => {
                        mixins.push((group, module));
                    }
                    Fact::Spread { path, .. } => {
                        rspec.factory_syntax |=
                            path.trim_start_matches("::") == "FactoryBot::Syntax::Methods";
                    }
                    Fact::Factory {
                        name,
                        class,
                        parent,
                        scope,
                        initialized,
                    } => {
                        let factory = Factory {
                            class,
                            parent,
                            scope,
                            initialized,
                        };
                        rspec
                            .factories
                            .entry(name)
                            .and_modify(|twice| *twice = None)
                            .or_insert(Some(factory));
                    }
                    Fact::Group {
                        line,
                        parent,
                        described,
                        scope,
                        metadata,
                        opaque,
                        nests,
                    } => {
                        let group = Group {
                            parent,
                            described,
                            scope,
                            metadata,
                            opaque,
                            nests,
                            members: Vec::new(),
                            mixins: Vec::new(),
                        };
                        if parent == SHARED_GROUP {
                            shared_groups.insert((path.clone(), line), group);
                            continue;
                        }
                        groups
                            .entry(line)
                            .and_modify(|shared| *shared = None)
                            .or_insert(Some(group));
                    }
                    Fact::Member {
                        group,
                        name,
                        line,
                        def,
                    } => members.push((
                        group,
                        Member {
                            name,
                            line,
                            def,
                            value: None,
                            shared: None,
                        },
                    )),
                    Fact::LetValue {
                        group,
                        name,
                        line,
                        value,
                    } => {
                        values.insert((group, name, line), value);
                    }
                    _ => {}
                }
            }
            for (group, mut member) in members {
                member.value = values.remove(&(group, member.name.clone(), member.line));
                if let Some(Some(group)) = groups.get_mut(&group) {
                    group.members.push(member);
                } else if let Some(group) = shared_groups.get_mut(&(path.clone(), group)) {
                    group.members.push(member);
                }
            }
            for (group, module) in mixins {
                if let Some(Some(group)) = groups.get_mut(&group) {
                    group.mixins.push(module);
                } else if let Some(group) = shared_groups.get_mut(&(path.clone(), group)) {
                    // What a shared group includes lands in its includer,
                    // which the rules do not follow.
                    group.opaque = true;
                }
            }
            if !groups.is_empty() {
                let mut children: Option<HashMap<u32, Vec<u32>>> = Some(HashMap::default());
                for (&line, group) in &groups {
                    match (group, children.as_mut()) {
                        (Some(group), Some(children)) => {
                            children.entry(group.parent).or_default().push(line);
                        }
                        _ => children = None,
                    }
                }
                rspec.files.insert(path.clone(), Spec { groups, children });
            }
        }
        // A shared group that includes another brings what the rules do
        // not follow.
        for (file, line, _, _) in &includes {
            if let Some(group) = shared_groups.get_mut(&(file.clone(), *line)) {
                group.opaque = true;
            }
        }
        // What each `include_context` brings, at its line.
        for (file, line, name, at) in includes {
            let brought = rspec.shared(&shared, &shared_groups, &name);
            let Some(Some(group)) = rspec
                .files
                .get_mut(&file)
                .and_then(|spec| spec.groups.get_mut(&line))
            else {
                continue;
            };
            let Some((file, _, from)) = brought else {
                group.opaque = true;
                continue;
            };
            group.opaque |= from.opaque;
            group.nests |= from.nests;
            // The last of a name in the shared block, brought at `at`.
            let mut last: HashMap<&str, &Member> = HashMap::default();
            for member in &from.members {
                last.entry(member.name.as_str())
                    .and_modify(|kept| {
                        if member.line > kept.line {
                            *kept = member;
                        }
                    })
                    .or_insert(member);
            }
            for member in last.into_values() {
                group.members.push(Member {
                    line: at,
                    shared: Some((file.clone(), member.line, from.scope.clone())),
                    ..member.clone()
                });
            }
        }
        rspec.on &= !off;
        if !rspec.on {
            rspec.files.clear();
        }
        rspec
    }

    /// Whether the configuration includes factory_bot's syntax methods.
    pub(super) fn factory_syntax(&self) -> bool {
        self.factory_syntax
    }

    /// The class the factory `name` builds, as written, with the nesting
    /// it is written in: its own `class:`, else its parent's, else the
    /// camelized name of the factory at the root; `None` where one on the
    /// way may build anything or is not the repository's once.
    pub(super) fn factory_class(&self, name: &str) -> Option<(String, Vec<String>)> {
        let mut at = name;
        let mut class = None;
        for _ in 0..super::ancestors::MAX_ANCESTOR_DEPTH {
            let factory = self.factories.get(at)?.as_ref()?;
            // `initialize_with` is inherited, wherever it is written.
            if factory.initialized {
                return None;
            }
            if class.is_none() && !factory.class.is_empty() {
                class = Some((factory.class.clone(), factory.scope.clone()));
            }
            if factory.parent.is_empty() {
                return class.or_else(|| Some((format!("::{}", camelize_path(at)), Vec::new())));
            }
            at = &factory.parent;
        }
        None
    }

    /// The shared group `name` names, with its file and line: one declared
    /// once in the program, at a file's top level (one declared in a group
    /// records no group of its own).
    fn shared<'s>(
        &self,
        shared: &HashMap<String, Vec<Shared>>,
        groups: &'s HashMap<(String, u32), Group>,
        name: &str,
    ) -> Option<(String, u32, &'s Group)> {
        // A name that is no literal may be this one.
        if shared.contains_key("") {
            return None;
        }
        // Declared in a group too, which hides it there: twice.
        let [one] = shared.get(name)?.as_slice() else {
            return None;
        };
        let group = groups.get(&(one.file.clone(), one.line))?;
        Some((one.file.clone(), one.line, group))
    }

    /// Whether the model reads the program.
    pub(super) fn on(&self) -> bool {
        self.on
    }

    fn group(&self, file: &str, line: u32) -> Option<&Group> {
        self.files.get(file)?.groups.get(&line)?.as_ref()
    }

    /// The groups whose examples `self` may be where a call carries the
    /// group at `line` (`open`: that group's and its nested groups', in a
    /// hook, a `let` or a `def`); `None` when a nested group is one the
    /// rules do not read (`it_behaves_like`).
    pub(super) fn groups(&self, file: &str, line: u32, open: bool) -> Option<Vec<u32>> {
        let mut found = vec![line];
        if open {
            let spec = self.files.get(file)?;
            let children = spec.children.as_ref()?;
            let mut pending = std::collections::VecDeque::from([line]);
            while let Some(at) = pending.pop_front() {
                for &child in children.get(&at).into_iter().flatten() {
                    found.push(child);
                    pending.push_back(child);
                }
                // A group nested in itself: a cycle no file writes.
                if found.len() > spec.groups.len() {
                    return None;
                }
            }
        }
        for &line in &found {
            if self
                .group(file, line)
                .is_none_or(|group| open && group.nests)
            {
                return None;
            }
        }
        Some(found)
    }

    /// What lookup from an example of any of `groups` finds for `name`,
    /// the same from each, or [`Found::Unknown`]; `passes` says whether a
    /// module a group includes, as written in the nesting given, leaves
    /// the name to the steps after it.
    pub(super) fn find_in(
        &self,
        file: &str,
        groups: &[u32],
        name: &str,
        passes: &dyn Fn(&str, &[String]) -> bool,
    ) -> Found<'_> {
        let mut found = Found::Nowhere;
        for (i, &line) in groups.iter().enumerate() {
            let here = self.find(file, line, name, passes);
            let same = match (&found, &here) {
                _ if i == 0 => true,
                (Found::Nowhere, Found::Nowhere) => true,
                (Found::Member(a, _), Found::Member(b, _)) => std::ptr::eq(*a, *b),
                _ => false,
            };
            if !same || matches!(here, Found::Unknown) {
                return Found::Unknown;
            }
            found = here;
        }
        found
    }

    /// What lookup from an example of the group at `line` in `file` finds
    /// for `name` ([`Self::find_in`]).
    pub(super) fn find(
        &self,
        file: &str,
        mut line: u32,
        name: &str,
        passes: &dyn Fn(&str, &[String]) -> bool,
    ) -> Found<'_> {
        for _ in 0..super::ancestors::MAX_ANCESTOR_DEPTH {
            let Some(group) = self.group(file, line) else {
                return Found::Unknown;
            };
            if group.opaque {
                return Found::Unknown;
            }
            // Defined on the group class itself, the last one written wins.
            if let Some(member) = group
                .members
                .iter()
                .filter(|member| member.name == name)
                .max_by_key(|member| member.line)
            {
                let scope = member.shared.as_ref().map_or(&group.scope, |(_, _, s)| s);
                return Found::Member(member, scope);
            }
            // The modules its body includes come after its own methods,
            // the last included first.
            if !group
                .mixins
                .iter()
                .rev()
                .all(|module| passes(module, &group.scope))
            {
                return Found::Unknown;
            }
            if group.metadata {
                return Found::Unknown;
            }
            // Past the top group, a shared group declared with metadata may
            // have been included where a group's derived metadata matches.
            if group.parent == 0 {
                return if self.meta_shared {
                    Found::Unknown
                } else {
                    Found::Nowhere
                };
            }
            line = group.parent;
        }
        Found::Unknown
    }

    /// The constant the example's `described_class` is, as written, with
    /// the nesting it is written in: the innermost group's first argument
    /// that is a constant; `None` past one that is an expression.
    pub(super) fn described(&self, file: &str, mut line: u32) -> Option<(&str, &[String])> {
        for _ in 0..super::ancestors::MAX_ANCESTOR_DEPTH {
            let group = self.group(file, line)?;
            match group.described.as_str() {
                "" if group.parent != 0 => line = group.parent,
                "" | "?" => return None,
                written => return Some((written, &group.scope)),
            }
        }
        None
    }
}

/// The group a call's `extra` names, when it is written where `self` is an
/// example of one, and whether `self` may be an example of a group nested
/// in it.
pub(super) fn group_of(context: &Context) -> Option<(u32, bool)> {
    context.extra.iter().find_map(|extra| {
        let group = extra.strip_prefix(GROUP)?;
        match group.strip_suffix('+') {
            Some(open) => Some((open.parse().ok()?, true)),
            None => Some((group.parse().ok()?, false)),
        }
    })
}

impl<'a> Index<'a> {
    /// What lookup from an example of any of `groups` finds for `name`: a
    /// module a group includes leaves it to the steps after it when its
    /// chain is the graph's, holds no hook and defines no `name`.
    pub(super) fn groups_find(&self, file: &str, groups: &[u32], name: &str) -> Found<'_> {
        let passes = |written: &str, scope: &[String]| {
            self.constant_in_code(scope, written).is_some_and(|module| {
                self.plain_include(module)
                    && self.lookup(module, Side::Instance, name) == Lookup::NotFound
            })
        };
        self.rspec.find_in(file, groups, name, &passes)
    }

    /// A call without receiver, or on `self`, in an example of the group at
    /// `group`: the `def` the groups find.
    pub(super) fn example_call(&self, src: usize, group: (u32, bool), name: &str) -> Decision {
        let file = self.symbols[src].file.as_str();
        let groups = self.rspec.groups(file, group.0, group.1)?;
        let Found::Member(member, _) = self.groups_find(file, &groups, name) else {
            return None;
        };
        if !member.def {
            return None;
        }
        let at = match &member.shared {
            Some((file, line, _)) => (file.as_str(), *line),
            None => (file, member.line),
        };
        let defined = self.unowned.get(&at)?;
        let [def] = defined
            .iter()
            .copied()
            .filter(|&i| self.symbols[i].name == name)
            .collect::<Vec<_>>()[..]
        else {
            return None;
        };
        Some(def)
    }
}

#[cfg(test)]
mod audited_contract_tests {
    use super::*;

    fn group(parent: u32, nests: bool) -> Option<Group> {
        Some(Group {
            parent,
            described: String::new(),
            scope: Vec::new(),
            metadata: false,
            opaque: false,
            nests,
            members: Vec::new(),
            mixins: Vec::new(),
        })
    }

    fn model(children: &[(u32, Vec<u32>)]) -> Rspec {
        Rspec {
            files: HashMap::from_iter([(
                "spec/x_spec.rb".into(),
                Spec {
                    groups: HashMap::from_iter([
                        (1, group(0, false)),
                        (2, group(1, false)),
                        (3, group(2, false)),
                    ]),
                    children: Some(children.iter().cloned().collect()),
                },
            )]),
            ..Rspec::default()
        }
    }

    #[test]
    fn groups_should_expand_each_nested_group_once_in_breadth_first_order() {
        let rspec = model(&[(1, vec![2]), (2, vec![3])]);
        assert_eq!(rspec.groups("spec/x_spec.rb", 1, true), Some(vec![1, 2, 3]));
        assert_eq!(rspec.groups("spec/x_spec.rb", 1, false), Some(vec![1]));
        assert_eq!(rspec.groups("spec/missing_spec.rb", 1, true), None);
        assert_eq!(rspec.groups("spec/x_spec.rb", 99, false), None);
    }

    #[test]
    fn groups_should_reject_cycles_and_unknown_nested_groups() {
        let cyclic = model(&[(1, vec![2]), (2, vec![1])]);
        assert_eq!(cyclic.groups("spec/x_spec.rb", 1, true), None);
        let mut ambiguous = model(&[(1, vec![2])]);
        ambiguous
            .files
            .get_mut("spec/x_spec.rb")
            .unwrap()
            .groups
            .insert(2, group(1, true));
        assert_eq!(ambiguous.groups("spec/x_spec.rb", 1, true), None);
        assert_eq!(ambiguous.groups("spec/x_spec.rb", 2, false), Some(vec![2]));
        ambiguous
            .files
            .get_mut("spec/x_spec.rb")
            .unwrap()
            .groups
            .insert(2, None);
        assert_eq!(ambiguous.groups("spec/x_spec.rb", 2, false), None);
    }

    #[test]
    fn read_groups_should_preserve_member_lines_parentage_and_agreement_across_nested_examples() {
        let fact_group = |line, parent, described: &str| Fact::Group {
            line,
            parent,
            described: described.into(),
            scope: vec![],
            metadata: false,
            opaque: false,
            nests: false,
        };
        let facts = Fact::encode_all(&[
            Fact::Rspec,
            fact_group(1, 0, "Item"),
            fact_group(2, 1, ""),
            Fact::Member {
                group: 1,
                name: "helper".into(),
                line: 7,
                def: true,
            },
        ]);
        let model = Rspec::read(&[("spec/x_spec.rb".into(), facts)], &|_| true);
        assert_eq!(model.groups("spec/x_spec.rb", 1, true), Some(vec![1, 2]));
        assert_eq!(
            model.described("spec/x_spec.rb", 2).map(|(name, _)| name),
            Some("Item")
        );
        match model.find_in("spec/x_spec.rb", &[1, 2], "helper", &|_, _| true) {
            Found::Member(member, _) => assert_eq!(
                (member.name.as_str(), member.line, member.def),
                ("helper", 7, true)
            ),
            _ => panic!("the same inherited member must agree in both groups"),
        }
        assert!(matches!(
            model.find_in("spec/x_spec.rb", &[1, 2], "absent", &|_, _| true),
            Found::Nowhere
        ));
    }

    #[test]
    fn shared_group_reading_should_keep_the_latest_member_and_propagate_opaque_nested_groups() {
        let group = |line, parent, nests| Fact::Group {
            line,
            parent,
            described: String::new(),
            scope: vec![],
            metadata: false,
            opaque: false,
            nests,
        };
        let specs = Fact::encode_all(&[
            Fact::Rspec,
            group(1, 0, false),
            Fact::Include {
                group: 1,
                name: "shared".into(),
                line: 4,
            },
        ]);
        let shared = Fact::encode_all(&[
            Fact::Shared {
                name: "shared".into(),
                line: 1,
                scoped: false,
                metadata: false,
            },
            group(1, SHARED_GROUP, false),
            Fact::Member {
                group: 1,
                name: "helper".into(),
                line: 2,
                def: true,
            },
            Fact::Member {
                group: 1,
                name: "helper".into(),
                line: 7,
                def: false,
            },
        ]);
        let model = Rspec::read(
            &[
                ("spec/x_spec.rb".into(), specs.clone()),
                ("spec/shared.rb".into(), shared.clone()),
            ],
            &|_| true,
        );
        match model.find("spec/x_spec.rb", 1, "helper", &|_, _| true) {
            Found::Member(member, _) => assert_eq!(
                (member.line, member.def, member.shared.as_ref().map(|s| s.1)),
                (4, false, Some(7))
            ),
            _ => panic!("shared members must be brought at the include site"),
        }
        let mut nested = Fact::decode_all(&shared);
        for fact in &mut nested {
            if let Fact::Group { nests, .. } = fact {
                *nests = true;
            }
        }
        let shared = Fact::encode_all(&nested);
        let model = Rspec::read(
            &[
                ("spec/x_spec.rb".into(), specs),
                ("spec/shared.rb".into(), shared),
            ],
            &|_| true,
        );
        assert_eq!(model.groups("spec/x_spec.rb", 1, true), None);
    }
}
