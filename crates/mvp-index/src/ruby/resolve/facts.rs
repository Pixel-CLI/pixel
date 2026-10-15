// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What the Ruby files say beyond their symbols (`ruby/mod.rs`, "Facts"),
//! read once per generation: each fact names something a lookup cannot
//! prove, and the predicates here are where resolution abstains for it.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use mvp_model::{Symbol, SymbolKind};

use super::stubs::Tests;
use super::{Side, split_method};
use crate::ruby::dsl::ADDED;
use crate::ruby::unread::{INTERPOLATED, Unread, fits};
use crate::ruby::{Fact, singleton_of};

/// A constant path as written, with its nesting, innermost first.
type Written = (String, Vec<String>);

/// The facts of a generation's Ruby files.
#[derive(Default)]
pub(super) struct Facts {
    /// Owner → the names a lookup through it on a side cannot know
    /// (`None`: any name): `remove_method`, `undef_method`, `undef`, a
    /// computed `define_method` (F4, D16).
    unknown: HashMap<String, Vec<(Side, Option<String>)>>,
    /// Owner → the methods on a side that may be private or protected
    /// (`None`: any name), which no call on a constant receiver reaches
    /// ([`Fact::Hidden`], R9-03).
    private: HashMap<String, HashSet<(Side, Option<String>)>>,
    /// The visibility changes the rules do not read, on a constant's class
    /// or module (path, nesting, the name or `None` for any), and the names
    /// they may change on any class or module ([`Fact::Visibility`],
    /// R11-03).
    visibility: Vec<(String, Vec<String>, Option<String>)>,
    private_anywhere: HashSet<Option<String>>,
    /// `(file, name)`: a method or function defined more than once in the
    /// file, once at least under a condition (D06).
    unsettled: HashSet<(String, String)>,
    /// The compact class paths whose reading depends on other files, with
    /// their file (D09).
    compact: Vec<Fact>,
    /// The names of definitions another file may move under another
    /// scope, each with the other names it may have; what is nested in one
    /// moves with it (D09). A query reads the name's own prefixes, never
    /// every entry (R8-03).
    doubtful: HashSet<String>,
    /// Every name an unread region may define as a method (D12).
    unread_methods: HashSet<String>,
    /// Every last segment of a constant an unread region, or a `||=`, may
    /// define (R5-12).
    unread_constants: HashSet<String>,
    /// Every last segment of a class or module an unread region may open.
    unread_owners: HashSet<String>,
    /// Every last segment of a constant `const_set` or `X::Y =` may make
    /// an alias of a class or module ([`Fact::Alias`], R8-09).
    aliases: HashSet<String>,
    /// The constants a constructor assigns a class or module to
    /// ([`Fact::Constructed`]), each with the bodies a `class` or `module`
    /// keyword opens it in (file, first and last line).
    constructed: HashMap<String, Vec<(String, u32, u32)>>,
    /// Every method name an unread region may define where the scan
    /// cannot tell on what, and whether one may define any name there
    /// (B9).
    loose: (bool, HashSet<String>),
    /// Every `draw` of a routes file: the file, the name it reads, the
    /// module of its scope when the rules carry it (D04).
    draws: Vec<(String, String, Option<String>)>,
    /// `(file, hook method)` → the lines of its statements the rules do not
    /// read (D14).
    hooks: HashMap<(String, String), Vec<u32>>,
    /// `(file, hook method)` → the lines where it calls `super`.
    supers: HashMap<(String, String), Vec<u32>>,
    /// Each file's unread regions: whether they may define any name, and
    /// the names they may define.
    regions: Vec<(bool, HashSet<String>)>,
    /// Last segment of an owner an unread region may reopen (one it opens,
    /// or any owner of its file, which error recovery may have cut short)
    /// → those regions: each owner points at a region's names, which no
    /// owner copies, so a region costs its owners plus its names (R5-02).
    hidden: HashMap<String, Vec<usize>>,
    /// The constants code evaluated from outside their body changes, as
    /// written: path, nesting, side, name (`None`: any)
    /// ([`Fact::Evaluated`]).
    evaluated: Vec<(String, Vec<String>, Side, Option<String>)>,
    /// The modules included where the rules do not replay it, as written:
    /// path, nesting ([`Fact::Mixed`]).
    mixed: Vec<(String, Vec<String>)>,
    /// The mixins from outside a body of a module the rules cannot name,
    /// as written: path, nesting, side, whether a `prepend`
    /// ([`Fact::Opaque`]); once placed, by class or module.
    opaque: Vec<(String, Vec<String>, Side, bool)>,
    opaque_on: HashMap<String, Vec<(Side, bool)>>,
    /// Whether one of them names no class or module of the graph: a gem's,
    /// whose objects the graph holds behind a step it cannot name.
    opaque_elsewhere: bool,
    /// The methods defined on a constant's singleton class, and the bodies
    /// of `class << X`, with the constant as written and its nesting
    /// ([`Fact::Definee`]).
    definees: Vec<(String, String, Rc<[String]>)>,
    /// Concern → what its `included do` (`true`: `prepended do`) block
    /// defines on the objects it runs on, by side (`None`: any name)
    /// ([`Fact::Block`]).
    blocks: HashMap<String, Vec<(bool, Side, Option<String>)>>,
    /// Every name such a block defines, and whether one may define any.
    block_names: Names,
    /// What class macros define on the objects they run on: the macro's
    /// owner → its side and the side defined → the names (`None`: any)
    /// ([`Fact::Macro`]). A lookup reads the owners of the caller's
    /// singleton chain, never every macro: a generation may hold one per
    /// class (R7-02).
    macros: HashMap<String, HashMap<(Side, Side), Names>>,
    /// Every name a class macro defines on each side, and whether one may
    /// define any.
    macro_names: [Names; 2],
    /// Class or module → whether code evaluated from outside its body may
    /// give it any name, and the names it may define, remove or reorder
    /// (R9), once [`Facts::place`]d.
    changed: HashMap<(String, Side), Names>,
    /// The modules included into an object whose lookup the rules do not
    /// replay, once placed.
    mixed_into: HashSet<String>,
    /// Every method name defined on an object the rules cannot name
    /// exactly, and whether one may be any name ([`Fact::Tainted`]).
    tainted: Names,
    /// The same for the definitions written in test files
    /// ([`super::test_file`]), which taint lookups only when
    /// [`Facts::with_tests`] says so: the callers in test files (#106),
    /// with the files whose stubs they meet (ADR-0039).
    test_tainted: Names,
    tests: bool,
    /// Each test file's definitions on objects the rules cannot name.
    test_tainted_by: HashMap<String, Names>,
    /// Owner → the names its instance methods define on the singleton
    /// class of the object they run on (`define_singleton_method(n)` in a
    /// `def`, [`Fact::Fronted`]): an object whose chain
    /// holds the owner's instance step may hold one in front of its whole
    /// chain, and no other object can (#106).
    fronts: HashMap<String, Names>,
    /// Every name one of them defines, and whether one may define any.
    front_names: Names,
    /// The modules mixed into an object the rules cannot name, as written:
    /// path, nesting ([`Fact::Spread`]); once placed, the modules.
    spread: Vec<(String, Vec<String>)>,
    spread_into: HashSet<String>,
    /// The same written in test files, with the file: mixed in only for
    /// the callers in test files ([`Facts::with_tests`], #106).
    test_spread: Vec<(String, String, Vec<String>)>,
    /// The modules example groups include, with the file
    /// ([`Fact::GroupSpread`]), until [`Facts::group_mixins`] says
    /// whether RSpec's model names the groups; then, when it does, the
    /// modules alone, mixed in for the callers in test files.
    group_spread: Vec<(String, String, Vec<String>)>,
    group_mixed: Vec<(String, Vec<String>)>,
    /// The associations declared outside a class body
    /// ([`Fact::LooseAssociation`], ADR-0038).
    loose_associations: HashSet<String>,
}

/// Method names, some of them patterns of the names they may be
/// ([`fits`]), or any name. The patterns are kept apart, indexed by their
/// literal text: a name that is none of the exact ones is checked against
/// the patterns its own text may fit alone, never against every name or
/// every pattern, which a generation may hold by the thousand (R6-02,
/// R7-01).
#[derive(Default)]
struct Names {
    any: bool,
    exact: HashSet<String>,
    patterns: HashSet<String>,
    /// The text a pattern starts with → the text it ends with → the
    /// patterns, for those that start or end with some text.
    ends: HashMap<String, HashMap<String, Vec<String>>>,
    /// The lengths of those starts, and of those ends, each once: a name
    /// is cut at these lengths alone, never at each of its characters.
    head_lengths: BTreeSet<usize>,
    tail_lengths: BTreeSet<usize>,
    /// The longest text of each other pattern (`#{a}_by_#{b}`) → the
    /// patterns: a name it fits holds that text.
    inner: HashMap<String, Vec<String>>,
    /// The lengths of those texts, each once.
    inner_lengths: BTreeSet<usize>,
    /// What the patterns said of each name asked, which a lookup asks once
    /// per step of a chain.
    fitted: RefCell<HashMap<String, bool>>,
}

/// The most work, in bytes hashed or compared, one name may cost the
/// patterns; past it the name counts as fitting one, which only makes
/// resolution abstain (R8-02: hashing every text a name holds cost 14
/// times main for 80-character names).
const FIT_WORK: usize = 1 << 14;

impl Names {
    /// Adds `name`, `None` for any name.
    fn insert(&mut self, name: Option<&str>) {
        self.fitted.get_mut().clear();
        match name {
            Some(name) if name.contains(INTERPOLATED) => {
                if !self.patterns.insert(name.to_owned()) {
                    return;
                }
                let pieces: Vec<&str> = name.split(INTERPOLATED).collect();
                let (head, tail) = (pieces[0], pieces[pieces.len() - 1]);
                let longest = pieces.iter().copied().max_by_key(|piece| piece.len());
                match longest {
                    _ if !head.is_empty() || !tail.is_empty() => {
                        self.head_lengths.insert(head.len());
                        self.tail_lengths.insert(tail.len());
                        let tails = self.ends.entry(head.to_owned()).or_default();
                        tails
                            .entry(tail.to_owned())
                            .or_default()
                            .push(name.to_owned());
                    }
                    Some(piece) if !piece.is_empty() => {
                        self.inner_lengths.insert(piece.len());
                        let patterns = self.inner.entry(piece.to_owned()).or_default();
                        patterns.push(name.to_owned());
                    }
                    // Interpolations alone: any name.
                    _ => self.any = true,
                }
            }
            Some(name) => {
                self.exact.insert(name.to_owned());
            }
            None => self.any = true,
        }
    }

    /// Whether `name` is any name, one of the names, or fits a pattern.
    fn fit(&self, name: &str) -> bool {
        if self.any || self.exact.contains(name) {
            return true;
        }
        if self.patterns.is_empty() {
            return false;
        }
        if let Some(&fit) = self.fitted.borrow().get(name) {
            return fit;
        }
        let fit = self.fits_pattern(name);
        self.fitted.borrow_mut().insert(name.to_owned(), fit);
        fit
    }

    /// Whether `name` fits a pattern: one whose start and end it has, or
    /// whose longest text it holds. The name is cut only at the lengths
    /// those texts have, and each pattern met is matched piece by piece in
    /// one scan ([`fits`]); past [`FIT_WORK`], it fits.
    fn fits_pattern(&self, name: &str) -> bool {
        let mut work = 0;
        let mut spent = |cost: usize| {
            work += cost + 1;
            work > FIT_WORK
        };
        let len = name.len();
        let at = |cut: usize| name.is_char_boundary(cut);
        for &head in self.head_lengths.iter().take_while(|&&head| head <= len) {
            if !at(head) {
                continue;
            }
            if spent(head) {
                return true;
            }
            let Some(tails) = self.ends.get(&name[..head]) else {
                continue;
            };
            for &tail in self
                .tail_lengths
                .iter()
                .take_while(|&&tail| head + tail <= len)
            {
                if !at(len - tail) {
                    continue;
                }
                if spent(tail) {
                    return true;
                }
                for pattern in tails.get(&name[len - tail..]).into_iter().flatten() {
                    if spent(len + pattern.len()) || fits(pattern, name) {
                        return true;
                    }
                }
            }
        }
        for &piece in self.inner_lengths.iter().take_while(|&&piece| piece <= len) {
            for start in (0..=len - piece).filter(|&start| at(start) && at(start + piece)) {
                if spent(piece) {
                    return true;
                }
                for pattern in self
                    .inner
                    .get(&name[start..start + piece])
                    .into_iter()
                    .flatten()
                {
                    if spent(len + pattern.len()) || fits(pattern, name) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// The names and patterns, whatever `any` says.
    fn iter(&self) -> impl Iterator<Item = &str> {
        self.exact.iter().chain(&self.patterns).map(String::as_str)
    }
}

/// Ruby's `Object`, where a top-level `def` lives.
pub(super) const OBJECT: &str = "Object";
/// Ruby's `Module` and `Class`, where the hooks Ruby calls for every
/// module and class live (`included`, `inherited`).
pub(super) const MODULE: &str = "Module";
pub(super) const CLASS: &str = "Class";
/// Ruby's `BasicObject`, whose own methods, with `Object`'s, a class's
/// singleton chain reaches before `Class` (`inherited`, R7-07).
pub(super) const BASIC_OBJECT: &str = "BasicObject";

/// The last segment of a constant path: `Admin::User` → `User`.
pub(super) fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

impl Facts {
    /// The facts of the files `visible` accepts.
    pub(super) fn read(
        files: &[(String, String)],
        symbols: &[Symbol],
        visible: &dyn Fn(&str) -> bool,
    ) -> Facts {
        let mut facts = Facts::default();
        let mut owners_of: HashMap<&str, Vec<&str>> = HashMap::new();
        // Each constant a constructor's class or module is assigned to,
        // with how many times.
        let mut constructed: HashMap<String, usize> = HashMap::new();
        // Each class or module → the bodies a keyword opens it in.
        let mut bodies: HashMap<&str, Vec<(String, u32, u32)>> = HashMap::new();
        for symbol in symbols.iter().filter(|s| visible(&s.file)) {
            if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Module) {
                owners_of
                    .entry(symbol.file.as_str())
                    .or_default()
                    .push(last_segment(&symbol.name));
                let opened = symbol.signature.trim_start();
                if ["class ", "module "].iter().any(|k| opened.starts_with(k)) {
                    bodies.entry(symbol.name.as_str()).or_default().push((
                        symbol.file.clone(),
                        symbol.line,
                        symbol.end_line,
                    ));
                }
            }
        }
        for (path, encoded) in files.iter().filter(|(path, _)| visible(path)) {
            let path_of = path.as_str();
            for fact in Fact::decode_all(encoded) {
                match fact {
                    Fact::Unknown { owner, side, name } => {
                        let side = if side == '.' {
                            Side::Singleton
                        } else {
                            Side::Instance
                        };
                        facts.unknown.entry(owner).or_default().push((side, name));
                    }
                    Fact::Hidden { owner, side, name } => {
                        let side = if side == '.' {
                            Side::Singleton
                        } else {
                            Side::Instance
                        };
                        facts.private.entry(owner).or_default().insert((side, name));
                    }
                    Fact::Conditional { name } => {
                        facts.unsettled.insert((path.clone(), name));
                    }
                    Fact::Compact { .. } => facts.compact.push(fact),
                    Fact::Draw { name, module } => facts.draws.push((path.clone(), name, module)),
                    Fact::Super { method, line } => {
                        facts
                            .supers
                            .entry((path.clone(), method))
                            .or_default()
                            .push(line);
                    }
                    Fact::Hook { method, line } => {
                        facts
                            .hooks
                            .entry((path.clone(), method))
                            .or_default()
                            .push(line);
                    }
                    Fact::MaybeConstant { segment } => {
                        facts.unread_constants.insert(segment);
                    }
                    Fact::Alias { segment } => {
                        facts.aliases.insert(segment);
                    }
                    Fact::Visibility { path, name, .. } if path.is_empty() => {
                        facts.private_anywhere.insert(name);
                    }
                    Fact::Visibility { path, scope, name } => {
                        facts.visibility.push((path, scope, name));
                    }
                    Fact::Constructed { name } => {
                        *constructed.entry(name).or_default() += 1;
                    }
                    Fact::LooseAssociation { name } => {
                        facts.loose_associations.insert(name);
                    }
                    Fact::Unread(unread) => {
                        let parsed = owners_of.get(path.as_str()).into_iter().flatten();
                        facts.unread(&unread, parsed.copied());
                    }
                    Fact::Evaluated {
                        path,
                        scope,
                        side,
                        name,
                    } => {
                        let side = if side == '.' {
                            Side::Singleton
                        } else {
                            Side::Instance
                        };
                        facts.evaluated.push((path, scope, side, name));
                    }
                    Fact::Mixed { path, scope } => facts.mixed.push((path, scope)),
                    Fact::Opaque {
                        path,
                        scope,
                        side,
                        prepend,
                    } => {
                        let side = if side == '.' {
                            Side::Singleton
                        } else {
                            Side::Instance
                        };
                        facts.opaque.push((path, scope, side, prepend));
                    }
                    // `o.extend(M)` in a test file is a stub as well.
                    Fact::Spread {
                        path: spread,
                        scope,
                    } if super::test_file(path_of) => {
                        facts.test_spread.push((path.clone(), spread, scope));
                    }
                    Fact::Spread { path, scope } => facts.spread.push((path, scope)),
                    // Only a spec file declares a group.
                    Fact::GroupSpread {
                        path: spread,
                        scope,
                    } => facts.group_spread.push((path.clone(), spread, scope)),
                    // A test file's definition on an object the rules
                    // cannot name is a stub, as `allow(x).to receive(:m)`
                    // or `x.stubs(:m)` are, which the rules do not read
                    // either: only a test runner loads the file, so only
                    // the callers in test files may meet it (#106).
                    Fact::Tainted { name } if super::test_file(path_of) => {
                        let by = facts.test_tainted_by.entry(path.clone()).or_default();
                        by.insert(name.as_deref());
                    }
                    Fact::Tainted { name } => facts.taint(name.as_deref()),
                    Fact::Fronted { owner, name } => {
                        facts.front_names.insert(name.as_deref());
                        facts
                            .fronts
                            .entry(owner)
                            .or_default()
                            .insert(name.as_deref());
                    }
                    Fact::Definee {
                        name,
                        written,
                        scope,
                    } => facts.definees.push((name, written, scope)),
                    Fact::Macro {
                        owner,
                        method,
                        side,
                        name,
                    } => {
                        let side_of = |c: char| {
                            if c == '.' {
                                Side::Singleton
                            } else {
                                Side::Instance
                            }
                        };
                        let (method, side) = (side_of(method), side_of(side));
                        facts.macro_names[side as usize].insert(name.as_deref());
                        let names = facts.macros.entry(owner).or_default();
                        names
                            .entry((method, side))
                            .or_default()
                            .insert(name.as_deref());
                    }
                    Fact::Block {
                        concern,
                        prepended,
                        side,
                        name,
                    } => {
                        facts.block_names.insert(name.as_deref());
                        let side = if side == '.' {
                            Side::Singleton
                        } else {
                            Side::Instance
                        };
                        facts
                            .blocks
                            .entry(concern)
                            .or_default()
                            .push((prepended, side, name));
                    }
                    _ => {}
                }
            }
        }
        // Assigned twice, the constant holds the second class or module,
        // which the graph merges with the first: no decision is proven
        // through it (R10-06).
        for (name, times) in constructed {
            if times > 1 {
                facts.doubt(&name);
            }
            let opened = bodies.remove(name.as_str()).unwrap_or_default();
            facts.constructed.insert(name, opened);
        }
        facts
    }

    /// Whether a constructor made `owner` (`X = Class.new`, `Struct.new`,
    /// `Data.define`, `Module.new`), whose methods main holds only where a
    /// `class` or `module` keyword opens it: the bodies, `None` when no
    /// constructor made it.
    pub(super) fn constructed(&self, owner: &str) -> Option<&[(String, u32, u32)]> {
        self.constructed.get(owner).map(Vec::as_slice)
    }

    /// Whether an association `name` is declared outside a class body,
    /// on a class the rules do not place (ADR-0038).
    pub(super) fn loose_association(&self, name: &str) -> bool {
        self.loose_associations.contains(name)
    }

    /// The associations declared outside a class body.
    pub(super) fn loose_associations(&self) -> impl Iterator<Item = &str> {
        self.loose_associations.iter().map(String::as_str)
    }

    fn unread(&mut self, unread: &Unread, parsed: impl Iterator<Item = impl Into<String>>) {
        self.unread_methods.extend(unread.methods.iter().cloned());
        self.unread_methods.extend(unread.loose.iter().cloned());
        self.loose.0 |= unread.loose_any;
        self.loose.1.extend(unread.loose.iter().cloned());
        self.unread_constants
            .extend(unread.constants.iter().cloned());
        self.unread_owners.extend(unread.owners.iter().cloned());
        let region = self.regions.len();
        self.regions
            .push((unread.any, unread.methods.iter().cloned().collect()));
        let owners = unread.owners.iter().cloned().chain(parsed.map(Into::into));
        for owner in owners {
            let regions = self.hidden.entry(owner).or_default();
            if regions.last() != Some(&region) {
                regions.push(region);
            }
        }
    }

    /// The constant paths of the facts that name a class or module from
    /// code, each with its nesting, for [`Facts::place`].
    pub(super) fn written(&self) -> Vec<(&str, &[String])> {
        self.evaluated
            .iter()
            .map(|(path, scope, _, _)| (path.as_str(), scope.as_slice()))
            .chain(
                self.mixed
                    .iter()
                    .chain(&self.spread)
                    .map(|(path, scope)| (path.as_str(), scope.as_slice())),
            )
            .chain(
                self.definees
                    .iter()
                    .map(|(_, path, scope)| (path.as_str(), &scope[..])),
            )
            .chain(
                self.opaque
                    .iter()
                    .map(|(path, scope, _, _)| (path.as_str(), scope.as_slice())),
            )
            .chain(
                self.visibility
                    .iter()
                    .map(|(path, scope, _)| (path.as_str(), scope.as_slice())),
            )
            .collect()
    }

    /// Places each fact that names a class or module from code on every
    /// one `named` says the constant may be.
    pub(super) fn place(&mut self, named: &dyn Fn(&str, &[String]) -> Vec<String>) {
        for (path, scope, side, name) in &self.evaluated {
            let mut owners = named(path, scope);
            // `Object`, which the graph does not hold unless reopened, holds
            // the top-level functions, and `Module` and `Class` (and the own
            // side of `Object` and `BasicObject`) the hooks Ruby calls for
            // every module and class: whatever a path ending in one names,
            // it may be Ruby's (R6-01, R6-16, R7-07).
            let core = last_segment(path);
            if [OBJECT, MODULE, CLASS, BASIC_OBJECT].contains(&core) {
                owners.push(core.to_owned());
            }
            // `method_added` defined on a constant from outside its body:
            // a class macro that may define any name (R8-06).
            let added =
                *side == Side::Singleton && name.as_deref().is_some_and(|n| ADDED.contains(&n));
            for owner in owners {
                if added {
                    for defined in [Side::Instance, Side::Singleton] {
                        self.macro_names[defined as usize].insert(None);
                        let macros = self.macros.entry(owner.clone()).or_default();
                        macros
                            .entry((Side::Singleton, defined))
                            .or_default()
                            .insert(None);
                    }
                }
                let changed = self.changed.entry((owner, *side)).or_default();
                changed.insert(name.as_deref());
            }
        }
        for (path, scope) in &self.mixed {
            self.mixed_into.extend(named(path, scope));
        }
        for (path, scope, side, prepend) in &self.opaque {
            let owners = named(path, scope);
            self.opaque_elsewhere |= owners.is_empty();
            for owner in owners {
                self.opaque_on
                    .entry(owner)
                    .or_default()
                    .push((*side, *prepend));
            }
        }
        for (path, scope) in &self.spread {
            self.spread_into.extend(named(path, scope));
        }
        for (path, scope, name) in &self.visibility {
            for owner in named(path, scope) {
                let private = self.private.entry(owner).or_default();
                private.insert((Side::Instance, name.clone()));
                private.insert((Side::Singleton, name.clone()));
            }
        }
        // A method recorded on the class this file reads a constant as is
        // proven there only when no other class or module of the generation
        // may be the one Ruby finds (B2); otherwise it is doubtful, and a
        // lookup through any of them abstains for its name.
        for (name, path, scope) in &self.definees {
            let owner = match singleton_of(name) {
                Some(owner) => owner,
                None => match split_method(name) {
                    Some((owner, _, _)) => owner,
                    None => continue,
                },
            };
            let named = named(path, scope);
            if named.is_empty() || named == [owner] {
                continue;
            }
            self.doubtful.insert(name.clone());
            if let Some((_, _, method)) = split_method(name) {
                for other in named {
                    let changed = self.changed.entry((other, Side::Singleton)).or_default();
                    changed.insert(Some(method));
                }
            }
        }
    }

    /// Decides which compact paths are doubtful, given every constant the
    /// generation defines: `Foo::Bar` read at the top level is doubtful when
    /// some other `X::Foo` exists (a nearer scope or an ancestor of the
    /// innermost one may be that `X`); `Outer::Foo::Bar` when a scope nearer
    /// than `Outer` has a `Foo`.
    pub(super) fn settle(&mut self, constants: &[&str]) {
        for fact in &self.compact {
            let Fact::Compact {
                name,
                written,
                top,
                nearer,
            } = fact
            else {
                continue;
            };
            let first = written.split("::").next().unwrap_or(written);
            let mut moved: Vec<String> = nearer
                .iter()
                .filter(|scope| constants.contains(&format!("{scope}::{first}").as_str()))
                .map(|scope| format!("{scope}::{written}"))
                .collect();
            if *top {
                let suffix = format!("::{first}");
                moved.extend(
                    constants
                        .iter()
                        .filter_map(|c| c.strip_suffix(suffix.as_str()))
                        .map(|scope| format!("{scope}::{written}")),
                );
            }
            // Read through an ancestor (B10), the recorded name is one of
            // the alternatives.
            moved.retain(|other| other != name);
            if !moved.is_empty() {
                self.doubtful.insert(name.clone());
                self.doubtful.extend(moved);
            }
        }
    }

    /// What the concern's `included do` (`prepended`: `prepended do`)
    /// block defines on the objects it runs on (B6).
    pub(super) fn block_defs(&self, concern: &str, prepended: bool) -> Vec<(Side, Option<String>)> {
        self.blocks
            .get(concern)
            .into_iter()
            .flatten()
            .filter(|(p, _, _)| *p == prepended)
            .map(|(_, side, name)| (*side, name.clone()))
            .collect()
    }

    /// Whether some class macro may define `name` on `side` of the objects
    /// it runs on (B7); `None` for any name.
    pub(super) fn macro_named(&self, side: Side, name: Option<&str>) -> bool {
        let names = &self.macro_names[side as usize];
        match name {
            Some(name) => names.fit(name),
            None => names.any || names.iter().next().is_some(),
        }
    }

    /// Whether the method of `owner` on `method` is a class macro that may
    /// define `name` on `side` of the objects it runs on (B7); `None` for
    /// any name.
    pub(super) fn macro_defines(
        &self,
        owner: &str,
        method: Side,
        side: Side,
        name: Option<&str>,
    ) -> bool {
        self.macros
            .get(owner)
            .and_then(|macros| macros.get(&(method, side)))
            .is_some_and(|names| name.is_none_or(|name| names.fit(name)))
    }

    /// Whether a concern's block may define `name` on its includers.
    pub(super) fn block_named(&self, name: &str) -> bool {
        self.block_names.fit(name)
    }

    /// Whether a concern's block may define any name on its includers.
    pub(super) fn any_block(&self) -> bool {
        self.block_names.any || self.block_names.iter().next().is_some()
    }

    /// Whether a fact may leave a lookup through `owner` on `side` unsure
    /// of some name, whatever the name ([`Self::unknown`],
    /// [`Self::changed`], [`Self::hidden`]).
    pub(super) fn touches(&self, owner: &str, side: Side) -> bool {
        self.unknown
            .get(owner)
            .is_some_and(|marks| marks.iter().any(|(s, _)| *s == side))
            || self.changed.contains_key(&(owner.to_owned(), side))
            || self.hidden.contains_key(last_segment(owner))
    }

    /// Whether a lookup through `owner` on `side` cannot know whether it
    /// holds `name`.
    pub(super) fn unknown(&self, owner: &str, side: Side, name: &str) -> bool {
        self.unknown.get(owner).is_some_and(|marks| {
            marks
                .iter()
                .any(|(s, n)| *s == side && n.as_deref().is_none_or(|n| fits(n, name)))
        })
    }

    /// Whether the method `name` of `owner` on `side` may be private or
    /// protected ([`Fact::Hidden`], [`Fact::Visibility`]).
    pub(super) fn private(&self, owner: &str, side: Side, name: &str) -> bool {
        let named = Some(name.to_owned());
        self.private.get(owner).is_some_and(|marks| {
            marks.contains(&(side, None)) || marks.contains(&(side, named.clone()))
        }) || !self.private_anywhere.is_empty()
            && (self.private_anywhere.contains(&None) || self.private_anywhere.contains(&named))
    }

    /// The lines where the hook method `method` of `file` calls `super`.
    pub(super) fn super_lines(&self, file: &str, method: &str) -> Vec<u32> {
        self.supers
            .get(&(file.to_owned(), method.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    /// Every `draw`: the drawing file, the name, the module of its scope.
    pub(super) fn draws(&self) -> &[(String, String, Option<String>)] {
        &self.draws
    }

    /// The lines of the statements of the hook method `method` of `file`
    /// that the rules do not read.
    pub(super) fn hook_lines(&self, file: &str, method: &str) -> Vec<u32> {
        self.hooks
            .get(&(file.to_owned(), method.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether `file` defines `name` more than once, once at least under a
    /// condition.
    pub(super) fn unsettled(&self, file: &str, name: &str) -> bool {
        !self.unsettled.is_empty() && self.unsettled.contains(&(file.to_owned(), name.to_owned()))
    }

    /// Whether `name` (a class, module, constant or method) is, or is
    /// nested in, a definition another file may move.
    pub(super) fn doubtful(&self, name: &str) -> bool {
        if self.doubtful.is_empty() {
            return false;
        }
        // The name itself, and each prefix it holds before a `::`, `#` or
        // `.`: one set read per segment.
        let cuts = name
            .match_indices(['#', '.'])
            .chain(name.match_indices("::"))
            .map(|(at, _)| at);
        cuts.chain([name.len()])
            .any(|at| self.doubtful.contains(&name[..at]))
    }

    /// Whether an unread region may give `owner` a method `name` the graph
    /// lacks.
    pub(super) fn hidden(&self, owner: &str, name: &str) -> bool {
        self.hidden.get(last_segment(owner)).is_some_and(|regions| {
            regions.iter().any(|&region| {
                let (any, names) = &self.regions[region];
                *any || names.contains(name)
            })
        })
    }

    /// Whether code evaluated from outside its body may define, remove or
    /// reorder `name` on `side` of the class or module `owner` (R9).
    pub(super) fn changed(&self, owner: &str, side: Side, name: &str) -> bool {
        self.changed
            .get(&(owner.to_owned(), side))
            .is_some_and(|names| names.fit(name))
    }

    /// The constants of [`Fact::Mixed`] and of [`Fact::Spread`] as written
    /// (path, nesting), the latter last.
    pub(super) fn mixins_written(&self) -> (&[Written], &[Written]) {
        (&self.mixed, &self.spread)
    }

    /// The mixins from outside its body of a module the rules cannot name
    /// on the class or module `owner`: the side, whether a `prepend`.
    pub(super) fn opaque_on(&self, owner: &str) -> &[(Side, bool)] {
        self.opaque_on.get(owner).map_or(&[], Vec::as_slice)
    }

    /// Whether a mixin from outside its body of a module the rules cannot
    /// name acts on a class or module the graph does not hold, once
    /// placed.
    pub(super) fn opaque_elsewhere(&self) -> bool {
        self.opaque_elsewhere
    }

    /// The modules mixed into an object whose lookup the rules do not
    /// replay, once placed.
    pub(super) fn mixed(&self) -> impl Iterator<Item = &str> {
        self.mixed_into.iter().map(String::as_str)
    }

    /// The modules mixed into an object the rules cannot name, once
    /// placed.
    pub(super) fn spread(&self) -> impl Iterator<Item = &str> {
        self.spread_into.iter().map(String::as_str)
    }

    /// Records that `name` (a class or module) may be another than the
    /// one the graph names: neither its nesting nor its `self` is known.
    pub(super) fn doubt(&mut self, name: &str) {
        if !self.doubtful(name) {
            self.doubtful.insert(name.to_owned());
        }
    }

    /// Records that the test file `file` defines `name` on an object the
    /// rules cannot name, as a [`Fact::Tainted`] there does: an example
    /// group's `def` where RSpec's model is not read (ADR-0041).
    pub(super) fn taint_test(&mut self, file: &str, name: &str) {
        self.test_tainted_by
            .entry(file.to_owned())
            .or_default()
            .insert(Some(name));
    }

    /// Records that a method `name` (`None`: any) may be defined on an
    /// object the rules cannot name (R5-03, R5-04).
    pub(super) fn taint(&mut self, name: Option<&str>) {
        self.tainted.insert(name);
    }

    /// Whether a method `name` may be defined on an object the rules
    /// cannot name: no lookup of it proves anything.
    pub(super) fn tainted(&self, name: &str) -> bool {
        self.tainted.fit(name) || self.tests && self.test_tainted.fit(name)
    }

    /// Where the modules example groups include are mixed in: with
    /// RSpec's model (`modelled`), into the examples of the group, an
    /// object the model names, whose lookup reads the module as the
    /// group's step and which no other lookup meets; so the module's
    /// names are no object's the rules cannot name, but a call on `self`
    /// in its methods may run on that group, which no chain of the graph
    /// holds (R5-05). Without, into an object the rules cannot name, as
    /// any test file's `o.extend(M)` is (ADR-0039). Read before
    /// [`Facts::with_tests`].
    pub(super) fn group_mixins(&mut self, modelled: bool) {
        let group = std::mem::take(&mut self.group_spread);
        if modelled {
            self.group_mixed = group
                .into_iter()
                .map(|(_, path, scope)| (path, scope))
                .collect();
        } else {
            self.test_spread.extend(group);
        }
    }

    /// Whether the definitions test files make on objects the rules
    /// cannot name taint lookups: for the callers in test files.
    /// Read before anything is placed.
    pub(super) fn with_tests(&mut self, tests: &Tests) {
        self.tests = tests.any();
        if self.tests {
            self.mixed.extend(self.group_mixed.iter().cloned());
        }
        let mut names = Names::default();
        for (file, by) in &self.test_tainted_by {
            if tests.keeps(file) {
                if by.any {
                    names.insert(None);
                }
                for name in by.iter() {
                    names.insert(Some(name));
                }
            }
        }
        self.test_tainted = names;
        for (file, path, scope) in &self.test_spread {
            if tests.keeps(file) {
                self.spread.push((path.clone(), scope.clone()));
            }
        }
    }

    /// Whether a test file defines a method on an object the rules cannot
    /// name, or mixes a module into one or into an example group: the
    /// callers in test files then need an index of their own.
    pub(super) fn test_tainted(&self) -> bool {
        !self.test_tainted_by.is_empty()
            || !self.test_spread.is_empty()
            || !self.group_mixed.is_empty()
    }

    /// Whether the test file `file` defines a method on an object the
    /// rules cannot name, or mixes a module into one (ADR-0039).
    pub(super) fn stubs_in(&self, file: &str) -> bool {
        self.test_tainted_by.contains_key(file) || self.spreads_in(file)
    }

    /// Whether the test file `file` mixes a module into an object the
    /// rules cannot name.
    pub(super) fn spreads_in(&self, file: &str) -> bool {
        self.test_spread.iter().any(|(by, _, _)| by == file)
    }

    /// The modules the test file `file` mixes into objects the rules
    /// cannot name, as written: path, nesting.
    pub(super) fn spread_in<'s>(
        &'s self,
        file: &'s str,
    ) -> impl Iterator<Item = (&'s str, &'s [String])> + 's {
        self.test_spread
            .iter()
            .filter(move |(by, _, _)| by == file)
            .map(|(_, path, scope)| (path.as_str(), scope.as_slice()))
    }

    /// Whether the test file `file` may define a method `name` on an
    /// object the rules cannot name.
    pub(super) fn stubbed_in(&self, file: &str, name: &str) -> bool {
        self.test_tainted_by
            .get(file)
            .is_some_and(|names| names.fit(name))
    }

    /// Whether an instance method of some owner may define `name` on the
    /// singleton class of the object it runs on ([`Facts::fronts`]).
    pub(super) fn front_named(&self, name: &str) -> bool {
        !self.fronts.is_empty() && self.front_names.fit(name)
    }

    /// Whether an instance method of `owner` may define `name` on the
    /// singleton class of the object it runs on.
    pub(super) fn fronts(&self, owner: &str, name: &str) -> bool {
        self.fronts.get(owner).is_some_and(|names| names.fit(name))
    }

    /// Whether the instance methods of `owner` may define some name on the
    /// singleton class of the object they run on.
    pub(super) fn fronting(&self, owner: &str) -> bool {
        self.fronts.contains_key(owner)
    }

    /// Whether some owner's instance methods may define a name on the
    /// singleton class of the object they run on.
    pub(super) fn any_front(&self) -> bool {
        !self.fronts.is_empty()
    }

    /// The code evaluated on a constant's class from outside its body
    /// (path, nesting, the name it may define, `None` for any), and the
    /// methods defined on a constant's singleton class (path, nesting,
    /// the method's name), each as written.
    pub(super) fn defined_on_constants(&self) -> Vec<(&str, &[String], Option<&str>)> {
        let evaluated = self
            .evaluated
            .iter()
            .map(|(path, scope, _, name)| (path.as_str(), scope.as_slice(), name.as_deref()));
        let definees = self.definees.iter().filter_map(|(name, path, scope)| {
            let (_, _, method) = split_method(name)?;
            Some((path.as_str(), &scope[..], Some(method)))
        });
        evaluated.chain(definees).collect()
    }

    /// What a lookup through the module `owner` on its instance side may
    /// meet besides its definitions: the names code evaluated there, a
    /// `remove_method`, a computed `define_method` or a concern's block
    /// may touch, and whether one may touch any.
    pub(super) fn touched(&self, owner: &str) -> (bool, Vec<&str>) {
        let mut any = false;
        let mut names = Vec::new();
        for (side, name) in self.unknown.get(owner).into_iter().flatten() {
            if *side == Side::Instance {
                match name {
                    Some(name) => names.push(name.as_str()),
                    None => any = true,
                }
            }
        }
        if let Some(touched) = self.changed.get(&(owner.to_owned(), Side::Instance)) {
            any |= touched.any;
            names.extend(touched.iter());
        }
        for (_, side, name) in self.blocks.get(owner).into_iter().flatten() {
            if *side == Side::Instance {
                match name {
                    Some(name) => names.push(name.as_str()),
                    None => any = true,
                }
            }
        }
        (any, names)
    }

    /// Whether an unread region, or a `||=`, may define a constant whose
    /// last segment is `segment`.
    pub(super) fn unread_constant(&self, segment: &str) -> bool {
        self.unread_constants.contains(segment)
    }

    /// Whether an unread region may define a method `name` on a class or
    /// module the scan cannot name, which any lookup may then meet (B9).
    pub(super) fn loose(&self, name: &str) -> bool {
        self.loose.0 || self.loose.1.contains(name)
    }

    /// Whether an unread region may define a method or function `name`.
    pub(super) fn unread_method(&self, name: &str) -> bool {
        self.unread_methods.contains(name)
    }

    /// Whether `const_set` or a scoped assignment may make the constant
    /// whose last segment is `segment` an alias of a class or module.
    pub(super) fn aliased(&self, segment: &str) -> bool {
        self.aliases.contains(segment)
    }

    /// Whether an unread region may reopen the class or module `owner`.
    pub(super) fn unread_owner(&self, owner: &str) -> bool {
        self.unread_owners.contains(last_segment(owner))
    }
}

#[cfg(test)]
mod tests {
    use mvp_model::Lang;

    use super::{Facts, Names, Side};
    use crate::ruby::unread::INTERPOLATED;

    // R6-02: every miss scanned every tainted name for a pattern, once per
    // site and chain step: 16.9 s for 12 000 singleton defs.
    #[test]
    fn a_name_is_matched_against_the_patterns_alone() {
        let mut names = Names::default();
        for i in 0..1_000 {
            names.insert(Some(&format!("stub_{i}")));
        }
        names.insert(Some(&format!("{INTERPOLATED}_count")));
        assert!(names.fit("stub_7"));
        assert!(names.fit("posts_count"));
        assert!(!names.fit("stub"));
        assert_eq!(names.patterns.len(), 1);
        assert!(!names.any);
        names.insert(None);
        assert!(names.fit("anything"));
    }

    // R7-01: every miss still checked every pattern: 24.9 s for 6 000
    // classes beside 6 000 singleton defs named `"#{k}_dyn_#{i}"`. A name
    // is checked against the patterns its text may fit alone, and finds
    // what checking every one finds.
    #[test]
    fn a_name_is_matched_against_the_patterns_its_text_may_fit() {
        let i = INTERPOLATED;
        let mut names = Names::default();
        let mut patterns = Vec::new();
        for n in 0..2_000 {
            patterns.push(format!("{i}_dyn_{n}"));
        }
        patterns.extend([
            format!("find_by_{i}"),
            format!("_run_{i}_callbacks"),
            format!("{i}_by_{i}"),
            format!("{i}x{i}y{i}"),
            format!("ab{i}ba"),
            format!("é{i}é"),
        ]);
        for pattern in &patterns {
            names.insert(Some(pattern));
        }
        assert!(!names.any);
        assert_eq!(names.ends[""].len(), 2_000);
        for name in [
            "a_dyn_7",
            "_dyn_1999",
            "a_dyn_2000",
            "find_by_id",
            "find_by_",
            "find_b",
            "_run_save_callbacks",
            "_run__callbacks",
            "_run_callbacks",
            "order_by_name",
            "_by_",
            "by",
            "axy",
            "yx",
            "aba",
            "abba",
            "éé",
            "é",
            "plain",
        ] {
            let every = patterns.iter().any(|pattern| super::fits(pattern, name));
            assert_eq!(names.fit(name), every, "{name}");
        }
        names.insert(Some(&format!("{i}{i}")));
        assert!(names.fit("anything"));
    }

    // R8-02: a name was cut at every pair of its characters and each text
    // hashed, cubic in its length: 9.4 s against main's 0.66 s for 6 000
    // classes with 80-character call names beside one tainted
    // `"#{a}_changed_from_#{a}"`. A name is cut at the lengths of the
    // patterns' texts alone, and past a bounded work it fits, which only
    // abstains.
    #[test]
    fn a_name_costs_the_patterns_a_bounded_work() {
        let i = INTERPOLATED;
        let mut names = Names::default();
        names.insert(Some(&format!("{i}_changed_from_{i}")));
        names.insert(Some(&format!("find_by_{i}")));
        let short = "a".repeat(80);
        assert!(!names.fit(&short));
        assert!(names.fit(&format!("{short}_changed_from_{short}")));
        assert!(names.fit("find_by_id"));
        // Too long to scan within the bound, a name no pattern fits fits.
        let long = "b".repeat(100_000);
        assert!(names.fit(&long));
        assert!(!Names::default().fit(&long));
    }

    // R8-03: every query scanned every doubtful name, and `doubt` queried
    // before each insert: 5.97 s against main's 0.81 s for 12 000 classes
    // reopened through a constant alias. A query reads the set once per
    // segment of the name, and finds what the scan found.
    #[test]
    fn a_doubtful_name_is_read_by_its_prefixes() {
        let mut facts = Facts::default();
        for i in 0..20_000 {
            facts.doubt(&format!("Al{i}"));
        }
        facts.doubt("Ns::Inner");
        facts.doubt("#<Class:Admin>");
        facts.doubt("Ns::Inner::Deep");
        facts.doubt("Svc#run");
        // `Ns::Inner::Deep` is nested in a doubtful name already.
        assert_eq!(facts.doubtful.len(), 20_003);
        for (name, doubtful) in [
            ("Al7", true),
            ("Al7::X", true),
            ("Al7#save", true),
            ("Al7.find", true),
            ("Al70000", false),
            ("Al", false),
            ("Ns", false),
            ("Ns::Inner", true),
            ("Ns::Inner::Other#x", true),
            ("Ns::Innerer", false),
            ("#<Class:Admin>", true),
            ("#<Class:Admin>#x", true),
            ("Admin", false),
            ("#<Class:Admin2>", false),
            ("Svc#run", true),
            ("Svc#runner", false),
            ("Svc", false),
        ] {
            assert_eq!(facts.doubtful(name), doubtful, "{name}");
        }
    }

    /// What the facts hold on an unread region's account: each region's
    /// names, and each owner's regions.
    fn unread_entries(facts: &Facts) -> usize {
        let names: usize = facts.regions.iter().map(|(_, names)| names.len()).sum();
        names + facts.hidden.values().map(Vec::len).sum::<usize>()
    }

    // R5-02: every owner a region may reopen held a copy of the region's
    // names, owners times methods: 2.3 GB for a file of 6 000 classes.
    #[test]
    fn an_unread_region_costs_its_owners_plus_its_names() {
        let classes = 2_000;
        let mut text = "class Finder\n  def self.find(&)\n    lookup 1,&\n  end\nend\n".to_owned();
        for i in 0..classes {
            text.push_str(&format!(
                "class Model{i} < Base\n  def action_{i}; \"class Other{i}\"; end\nend\n"
            ));
        }
        let rows = crate::extract::rows("lib/models.rb", Lang::Ruby, text.as_bytes());
        let facts = Facts::read(
            &[("lib/models.rb".to_owned(), rows.facts.clone())],
            &rows.symbols,
            &|_| true,
        );
        assert!(facts.hidden("Model7", "action_1999"));
        assert!(facts.hidden("Other7", "action_0"));
        assert!(!facts.hidden("Unnamed", "action_0"));
        let entries = unread_entries(&facts);
        assert!(entries >= classes, "{entries}: the region was not read");
        assert!(
            entries <= 4 * classes,
            "{entries} entries for {classes} classes"
        );
    }

    // R7-02: every lookup read every class macro and checked each against
    // the caller's singleton chain: 2.24 s against main's 0.63 s for 6 000
    // classes, each with `def self.setup_N; include Mz; end`. A lookup
    // reads the macros of the owners on that chain alone.
    #[test]
    fn a_macro_is_read_from_its_owner() {
        let classes = 2_000;
        let mut text = "module Mz; def zz; end; end\n".to_owned();
        for i in 0..classes {
            text.push_str(&format!(
                "class MC{i}\n  def self.setup_{i}; include Mz; end\n  def self.named_{i}; attr_reader :a_{i}; end\nend\n"
            ));
        }
        let rows = crate::extract::rows("lib/m.rb", Lang::Ruby, text.as_bytes());
        let facts = Facts::read(
            &[("lib/m.rb".to_owned(), rows.facts.clone())],
            &rows.symbols,
            &|_| true,
        );
        let (instance, singleton) = (Side::Instance, Side::Singleton);
        assert_eq!(facts.macros.len(), classes);
        assert!(facts.macro_named(instance, Some("anything")));
        assert!(facts.macro_defines("MC7", singleton, instance, Some("zz")));
        assert!(facts.macro_defines("MC7", singleton, singleton, None));
        assert!(!facts.macro_defines("MC7", instance, instance, None));
        assert!(!facts.macro_defines("Mz", singleton, instance, None));
    }
}
