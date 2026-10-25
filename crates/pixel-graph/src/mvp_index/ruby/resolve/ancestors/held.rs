// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The objects whose lookup chain holds a step: a call on `self` in a
//! method must find the same definition from each of them (F1, R4), and
//! one whose chain was cut may hold any step past the cut (R5-01).

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use super::super::{Index, Side};
use super::{Chain, Lookup, Node, Stands};

/// The steps before a step the rules cannot name, but that may be one of
/// the graph's classes or modules (B5), in one object's chain: lookup from
/// that object meets them before whatever the step holds.
pub(in crate::mvp_index::ruby::resolve) struct Behind<'a> {
    pub(super) chain: Rc<Chain<'a>>,
    /// Where the step is in the chain.
    pub(super) at: usize,
    /// The object's side.
    pub(super) on: Side,
    /// The step may be a class (a superclass's), not only a module.
    pub(super) classes: bool,
    /// What the steps before it the rules cannot name may hold: any class
    /// or module of the graph, or only one mixed in where the rules do
    /// not replay it ([`Index::may_hold`]).
    pub(super) hidden: Hidden,
}

/// What the unknown steps before an opaque one may hold (R6-08, R6-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::mvp_index::ruby::resolve) enum Hidden {
    Nothing,
    Spread,
    Anything,
}

/// Every [`Behind`], indexed by the names its steps define, so that a call
/// on `self` looks at those alone and not at every object (R6-03).
#[derive(Default)]
pub(in crate::mvp_index::ruby::resolve) struct Opaque<'a> {
    pub(super) steps: Vec<Behind<'a>>,
    /// Method name → the entries one of whose steps defines it, among
    /// those whose steps no fact touches ([`Index::quiet`]).
    pub(super) defining: HashMap<&'a str, Vec<usize>>,
    /// The entries a fact touches a step of, or with an unknown step that
    /// may hold one of the graph's: looked at for every name.
    pub(super) unsure: Vec<usize>,
    /// Every name a class or module of the graph defines, on either side,
    /// and every one a module mixed in where the rules do not replay it
    /// defines: what such an unknown step may hold.
    pub(super) defined: HashSet<&'a str>,
    pub(super) spread_defined: HashSet<&'a str>,
}

/// (owner, side) → the objects whose lookup chain holds that step: a
/// class's instances (`Instance`), a class or module itself (`Singleton`).
type Containers<'a> = HashMap<(&'a str, Side), Vec<(&'a str, Side)>>;

/// Objects whose chains are the same past their own lists, each step of
/// which is one the graph names: lookup from each of a name no step of
/// its own list defines, nor leaves unsure ([`Index::unsure`]), finds what
/// lookup along the rest finds (R10-01), and of a name one defines, what
/// the first such step defines, unless it may not be there or its place
/// is unordered; an object a step of whose own list is unsure of the name
/// is looked up alone. An own list holds the object's own step and the
/// modules its own declarations mix in, so models that each include their
/// own concerns still share their superclass's chain (R11-01), and so do
/// those a concern's `included do` block or a class macro defines methods
/// on, each unsure of those names alone (R12-01).
struct Share<'a> {
    /// All of them.
    members: Vec<Member<'a>>,
    /// The steps of their own lists, each once.
    steps: Vec<(&'a str, Side)>,
    /// Method name → the steps that define it.
    defining: HashMap<&'a str, Vec<u32>>,
    /// The steps a fact touches ([`Facts::touches`]), and those a concern's
    /// block or a class macro may define a name on: either may be unsure
    /// of some name, the latter only of one a block or a macro defines.
    ///
    /// [`Facts::touches`]: super::super::facts::Facts::touches
    touched: Vec<u32>,
    blocked: Vec<u32>,
    /// Each step → those of them whose own lists hold it.
    holding: Vec<Vec<u32>>,
    /// A step past their own lists another file may move (D09), or a walk
    /// cut short: no lookup through them is proven.
    unproven: bool,
}

/// What may leave lookup through a step of an own list unsure of a name
/// but a tainted one ([`Index::unsure`]).
#[derive(Clone, Copy)]
enum Quiet {
    /// Nothing ([`Index::quiet`]).
    Yes,
    /// A fact that touches it, for any name.
    Touched,
    /// Only a concern's block run on it or a class macro, for a name one
    /// of them may define.
    Blocked,
}

/// One object of a [`Share`].
struct Member<'a> {
    object: (&'a str, Side),
    chain: Rc<Chain<'a>>,
    /// Where the rest starts in its chain.
    rest: usize,
    /// Its own list: each step, and whether it is there for sure and in
    /// its place (neither `maybe` nor in a region).
    list: Vec<(u32, bool)>,
}

/// Every [`Share`] of two objects or more, with the one each object is
/// in and its place there.
#[derive(Default)]
pub(in crate::mvp_index::ruby::resolve) struct Shares<'a> {
    shares: Vec<Share<'a>>,
    of: HashMap<(&'a str, Side), (usize, u32)>,
}

/// The objects holding one step, when a share holds it: shares, with the
/// objects of each that hold it when not all of them do (the step is in
/// their own lists), and the others, looked up one by one (R10-01).
pub(in crate::mvp_index::ruby::resolve) struct Held<'a> {
    pub(super) shares: Vec<(usize, Option<HashSet<u32>>)>,
    pub(super) single: Vec<(&'a str, Side)>,
}

/// (owner, side) → [`Held`] for that step, `None` when no share holds it.
pub(super) type Holders<'a> = HashMap<(&'a str, Side), Option<Rc<Held<'a>>>>;

/// Every object's chain, read once: the objects holding each step, and
/// those whose chain has a step the rules cannot name that may be one of
/// the graph's (B5).
#[derive(Default)]
pub(super) struct Contained<'a> {
    map: Containers<'a>,
    pub(super) opaque: Vec<(&'a str, Side)>,
    /// The objects whose chain met the depth cap or the nesting bound: past
    /// the cut, they may hold any step (R5-01).
    truncated: Vec<(&'a str, Side)>,
}

impl<'a> Contained<'a> {
    /// The objects whose lookup chain holds `owner`'s step on `side`, but
    /// that step itself: each class's instances and the class itself, each
    /// module itself and the objects it is mixed into (F1, R4).
    fn holding(&self, owner: &'a str, side: Side) -> &[(&'a str, Side)] {
        self.map.get(&(owner, side)).map_or(&[], Vec::as_slice)
    }
}

impl<'a> Index<'a> {
    /// Whether lookup from every object holding `owner`'s step on `side`
    /// reaches `Object` ([`Index::reaches_object`]), read once.
    pub(in crate::mvp_index::ruby::resolve) fn reach(&self, owner: &'a str, side: Side) -> bool {
        if let Some(&reach) = self.walks.reach.borrow().get(&(owner, side)) {
            return reach;
        }
        let reach = self
            .contained()
            .holding(owner, side)
            .iter()
            .all(|&(object, on)| self.reaches_object(object, on));
        self.walks.reach.borrow_mut().insert((owner, side), reach);
        reach
    }

    /// Whether lookup of `name` from every object of this project holding
    /// `owner`'s step on `side` finds `own` (F1), the objects of a share
    /// read together ([`Share`]; R10-01, R11-01).
    pub(in crate::mvp_index::ruby::resolve) fn agree(
        &self,
        owner: &'a str,
        side: Side,
        name: &str,
        own: Lookup,
    ) -> bool {
        let one = |&(object, on): &(&'a str, Side)| self.lookup(object, on, name) == own;
        let Some(held) = self.held(owner, side) else {
            return self.contained().holding(owner, side).iter().all(one);
        };
        let shares = self.shares();
        held.single.iter().all(one)
            && held
                .shares
                .iter()
                .all(|(i, some)| self.share_agrees(&shares.shares[*i], some.as_ref(), name, own))
    }

    /// Whether lookup of `name` from each object of `share` holding a step
    /// (all of them, or `some`) finds `own`: an object a step of whose own
    /// list defines it finds what the first one defines, read once per
    /// step; the others find what lookup along the rest finds, looked up
    /// once.
    fn share_agrees(
        &self,
        share: &Share<'a>,
        some: Option<&HashSet<u32>>,
        name: &str,
        own: Lookup,
    ) -> bool {
        let defining = share.defining.get(name).map_or(&[][..], Vec::as_slice);
        // A tainted name leaves each step of an own list unsure.
        let sure = !share.unproven && !self.facts.tainted(name);
        // The steps of own lists unsure of the name: an object holding one
        // is looked up alone.
        let blocks = self.facts.block_named(name)
            || self.facts.macro_named(Side::Instance, Some(name))
            || self.facts.macro_named(Side::Singleton, Some(name));
        let unsure: Vec<u32> = if sure {
            let blocked = if blocks { &share.blocked[..] } else { &[] };
            share
                .touched
                .iter()
                .chain(blocked)
                .copied()
                .filter(|&k| {
                    let (o, s) = share.steps[k as usize];
                    self.unsure(o, s, name)
                })
                .collect()
        } else {
            Vec::new()
        };
        let any = !unsure.is_empty();
        let mut alone = vec![false; if any { share.members.len() } else { 0 }];
        for &k in &unsure {
            for &m in &share.holding[k as usize] {
                alone[m as usize] = true;
            }
        }
        let place: HashMap<u32, usize> = defining
            .iter()
            .enumerate()
            .map(|(k, &step)| (step, k))
            .collect();
        // Whether what each defining step defines is `own`, read once.
        let mut hits: Vec<Option<bool>> = vec![None; defining.len()];
        // An object holding two such steps is read once.
        let several = defining.len() + unsure.len() > 1;
        let mut seen = vec![false; if several { share.members.len() } else { 0 }];
        let mut looked = 0;
        let holders = defining
            .iter()
            .chain(&unsure)
            .flat_map(|&step| &share.holding[step as usize])
            .filter(|m| some.is_none_or(|set| set.contains(m)));
        for &m in holders {
            if several {
                if seen[m as usize] {
                    continue;
                }
                seen[m as usize] = true;
            }
            looked += 1;
            let member = &share.members[m as usize];
            let first = member
                .list
                .iter()
                .find_map(|&(step, clean)| Some((*place.get(&step)?, clean)));
            let alone = alone.get(m as usize).copied().unwrap_or(false);
            let agrees = match first {
                Some((k, true)) if sure && !alone => *hits[k].get_or_insert_with(|| {
                    let (o, s) = share.steps[defining[k] as usize];
                    self.hit(o, s, name) == own
                }),
                _ => self.lookup(member.object.0, member.object.1, name) == own,
            };
            if !agrees {
                return false;
            }
        }
        looked == some.map_or(share.members.len(), HashSet::len) || {
            let found = if sure {
                let first = &share.members[0];
                self.lookup_in(&first.chain.steps[first.rest..], name)
            } else {
                Lookup::Abstain
            };
            found == own
        }
    }

    /// [`Held`] for `owner`'s step on `side`, built once; `None` when no
    /// share holds it.
    pub(in crate::mvp_index::ruby::resolve) fn held(
        &self,
        owner: &'a str,
        side: Side,
    ) -> Option<Rc<Held<'a>>> {
        if let Some(held) = self.walks.held.borrow().get(&(owner, side)) {
            return held.clone();
        }
        let shares = self.shares();
        let home = shares.of.get(&(owner, side)).map(|&(i, _)| i);
        // Whether each share's steps past its own lists hold this step,
        // and the objects of the others that do.
        let mut whole: HashMap<usize, bool> = HashMap::default();
        let mut some: HashMap<usize, HashSet<u32>> = HashMap::default();
        let mut single = Vec::new();
        let contained = self.contained();
        for &(object, on) in contained.holding(owner, side) {
            let Some(&(i, m)) = shares
                .of
                .get(&(object, on))
                .filter(|&&(i, _)| Some(i) != home)
            else {
                single.push((object, on));
                continue;
            };
            let holds = *whole.entry(i).or_insert_with(|| {
                let first = &shares.shares[i].members[0];
                first.chain.steps[first.rest..]
                    .iter()
                    .any(|step| step.node == Node::Known(owner, side))
            });
            if !holds {
                some.entry(i).or_default().insert(m);
            }
        }
        let mut held = Held {
            shares: whole
                .into_iter()
                .filter(|&(_, holds)| holds)
                .map(|(i, _)| (i, None))
                .collect(),
            single,
        };
        for (i, members) in some {
            if members.len() > 1 {
                held.shares.push((i, Some(members)));
            } else {
                let share = &shares.shares[i];
                held.single
                    .extend(members.iter().map(|&m| share.members[m as usize].object));
            }
        }
        held.shares.sort_unstable_by_key(|&(i, _)| i);
        let held = (!held.shares.is_empty()).then(|| Rc::new(held));
        self.walks
            .held
            .borrow_mut()
            .insert((owner, side), held.clone());
        held
    }

    /// [`Shares`], built once from the objects [`Self::contain`] walks: an
    /// object is in one when each step of its own list is one the graph
    /// names that may be read for it, and a superclass's chain follows it.
    fn shares(&self) -> Rc<Shares<'a>> {
        let built = self.walks.shares.get_or_init(|| {
            let mut classes: Vec<&'a str> = self.classes.keys().copied().collect();
            classes.sort_unstable();
            let mut objects: Vec<&'a str> = classes
                .into_iter()
                .chain(self.modules.iter().copied())
                .collect();
            objects.sort_unstable();
            objects.dedup();
            // Whether a step of an own list may be read for a share: one no
            // other file may move, whose singleton chain was not cut, and
            // not being walked; and then whether lookup through it proves
            // what it finds for every name but a tainted one, or which
            // facts may leave it unsure of some ([`Share`]).
            let mut plain_steps: HashMap<(&'a str, Side), Option<Quiet>> = HashMap::default();
            let mut plain = |owner: &'a str, side: Side| {
                *plain_steps.entry((owner, side)).or_insert_with(|| {
                    let walks = &self.walks;
                    let walked = walks.building.borrow().contains(&owner)
                        || walks.walking.borrow().contains(&(owner, Side::Singleton));
                    if self.facts.doubtful(owner)
                        || walked
                        || self.chain(owner, Side::Singleton).is_cut()
                    {
                        None
                    } else if self.quiet(owner, side) {
                        Some(Quiet::Yes)
                    } else if self.touched(owner, side) {
                        Some(Quiet::Touched)
                    } else {
                        Some(Quiet::Blocked)
                    }
                })
            };
            // Each plain object's chain, with where its rest starts and the
            // hash of the rest.
            let mut keyed = Vec::new();
            for (object, on) in objects
                .into_iter()
                .flat_map(|object| [(object, Side::Instance), (object, Side::Singleton)])
            {
                let chain = self.chain(object, on);
                let Some(parts) = self.parts(object) else {
                    continue;
                };
                let part = match on {
                    Side::Instance => &parts.instance,
                    Side::Singleton => &parts.singleton,
                };
                let own = part.steps.len();
                // Past a module, or a class of `Object`, nothing is left to
                // share; a step of the object's name but its own (a class
                // and a module of one name) would read as its own.
                if chain.steps.len() <= own
                    || chain.steps[..own] != part.steps[..]
                    || chain.steps().filter(|&(o, _)| o == object).count() > 1
                    || !part.steps.iter().all(|step| match step.node {
                        Node::Known(owner, side) => plain(owner, side).is_some(),
                        Node::Unknown => false,
                    })
                {
                    continue;
                }
                let mut hasher = DefaultHasher::new();
                chain.steps[own..].hash(&mut hasher);
                keyed.push(((object, on), chain, own, hasher.finish()));
            }
            // How many objects have steps of each hash: one alone is
            // looked up alone, and no share is kept for it.
            let mut counts: HashMap<u64, usize> = HashMap::default();
            for (_, _, _, hash) in &keyed {
                *counts.entry(*hash).or_default() += 1;
            }
            let mut shares = Shares::default();
            // The hash of the steps past an object's own list → the shares
            // with such steps.
            let mut tails: HashMap<u64, Vec<usize>> = HashMap::default();
            // Each share's steps → their place there.
            let mut places: HashMap<usize, HashMap<(&'a str, Side), u32>> = HashMap::default();
            for ((object, on), chain, rest, hash) in keyed {
                if counts[&hash] < 2 {
                    continue;
                }
                let tail = &chain.steps[rest..];
                let same = tails.entry(hash).or_default();
                let found = same.iter().copied().find(|&i| {
                    let first = &shares.shares[i].members[0];
                    first.chain.steps[first.rest..] == *tail
                });
                let i = found.unwrap_or_else(|| {
                    same.push(shares.shares.len());
                    shares.shares.push(Share {
                        members: Vec::new(),
                        steps: Vec::new(),
                        defining: HashMap::default(),
                        touched: Vec::new(),
                        blocked: Vec::new(),
                        holding: Vec::new(),
                        unproven: chain.is_cut()
                            || tail.iter().any(|step| {
                                matches!(step.node, Node::Known(o, _) if self.facts.doubtful(o))
                            }),
                    });
                    shares.shares.len() - 1
                });
                let share = &mut shares.shares[i];
                let place = places.entry(i).or_default();
                let m = share.members.len() as u32;
                let mut list = Vec::new();
                for step in &chain.steps[..rest] {
                    let Node::Known(o, s) = step.node else {
                        continue;
                    };
                    let at = *place.entry((o, s)).or_insert_with(|| {
                        let at = share.steps.len() as u32;
                        share.steps.push((o, s));
                        share.holding.push(Vec::new());
                        let methods = self.methods[s as usize].get(o);
                        for &name in methods.into_iter().flat_map(|m| m.keys()) {
                            share.defining.entry(name).or_default().push(at);
                        }
                        match plain_steps.get(&(o, s)).copied().flatten() {
                            Some(Quiet::Touched) => share.touched.push(at),
                            Some(Quiet::Blocked) => share.blocked.push(at),
                            _ => {}
                        }
                        at
                    });
                    let holding = &mut share.holding[at as usize];
                    if holding.last() != Some(&m) {
                        holding.push(m);
                    }
                    list.push((at, !step.maybe && step.region.is_none()));
                }
                share.members.push(Member {
                    object: (object, on),
                    chain,
                    rest,
                    list,
                });
            }
            // Two objects of a hash may still differ in their steps.
            shares.shares.retain(|share| share.members.len() > 1);
            for (i, share) in shares.shares.iter().enumerate() {
                for (m, member) in share.members.iter().enumerate() {
                    shares.of.insert(member.object, (i, m as u32));
                }
            }
            Rc::new(shares)
        });
        Rc::clone(built)
    }

    /// Whether an object whose chain was cut may hold `owner`'s step on
    /// `side` past the cut: a class's instance step only a class's
    /// instances, its own step only a class; a module's instance step any
    /// object, its own methods none but the module (R5-01).
    pub(in crate::mvp_index::ruby::resolve) fn held_past_a_cut(
        &self,
        owner: &'a str,
        side: Side,
    ) -> bool {
        let module = !self.classes.contains_key(owner);
        if module && side == Side::Singleton {
            return false;
        }
        self.contained().truncated.iter().any(|&(object, on)| {
            let may = match on {
                Side::Instance => side == Side::Instance,
                Side::Singleton => side == Side::Singleton || module,
            };
            object != owner
                && may
                && (module || self.classes.contains_key(object))
                && !self
                    .chain(object, on)
                    .steps()
                    .any(|step| step == (owner, side))
        })
    }

    /// [`Opaque`], built once from the objects [`Contained`] lists.
    pub(super) fn behind(&self) -> Rc<Opaque<'a>> {
        if let Some(behind) = self.walks.behind.borrow().clone() {
            return behind;
        }
        let outer = self.walks.capped.replace(false);
        let mut index = Opaque::default();
        let mut quiet: HashMap<(&'a str, Side), bool> = HashMap::default();
        for &(object, on) in self.contained().opaque.iter() {
            let chain = self.chain(object, on);
            for (at, step) in chain.steps.iter().enumerate() {
                let Stands::Opaque { classes } = step.stands else {
                    continue;
                };
                if step.node != Node::Unknown {
                    continue;
                }
                let entry = index.steps.len();
                let before = || {
                    chain.steps[..at].iter().filter_map(|s| match s.node {
                        Node::Known(owner, side) => Some((owner, side)),
                        Node::Unknown => None,
                    })
                };
                // An unknown step before it may hold a module of the graph,
                // in a place the rules do not know (R6-08, R6-15).
                let hidden = chain.steps[..at]
                    .iter()
                    .filter(|s| s.node == Node::Unknown)
                    .map(|s| match s.stands {
                        Stands::Concern => Hidden::Nothing,
                        Stands::Any => Hidden::Spread,
                        _ => Hidden::Anything,
                    })
                    .max_by_key(|hidden| *hidden as u8)
                    .unwrap_or(Hidden::Nothing);
                let touched = hidden != Hidden::Nothing
                    || before().any(|step| {
                        !*quiet
                            .entry(step)
                            .or_insert_with(|| self.quiet(step.0, step.1))
                    });
                if touched {
                    index.unsure.push(entry);
                } else {
                    for (owner, side) in before() {
                        let methods = self.methods[side as usize].get(owner);
                        for &name in methods.into_iter().flat_map(|m| m.keys()) {
                            let entries = index.defining.entry(name).or_default();
                            if entries.last() != Some(&entry) {
                                entries.push(entry);
                            }
                        }
                    }
                }
                index.steps.push(Behind {
                    chain: Rc::clone(&chain),
                    at,
                    on,
                    classes,
                    hidden,
                });
            }
        }
        index.defined = self
            .methods
            .iter()
            .flat_map(|owners| owners.values())
            .flat_map(|methods| methods.keys().copied())
            .collect();
        index.spread_defined = self
            .spread
            .iter()
            .filter_map(|module| self.methods[Side::Instance as usize].get(module))
            .flat_map(|methods| methods.keys().copied())
            .collect();
        let capped = self.walks.capped.get();
        self.walks.capped.set(outer || capped);
        let index = Rc::new(index);
        *self.walks.behind.borrow_mut() = Some(Rc::clone(&index));
        index
    }

    /// [`Contained`], built once.
    pub(super) fn contained(&self) -> Rc<Contained<'a>> {
        let built = self.walks.containers.borrow().clone();
        match built {
            Some(contained) => contained,
            None => {
                let contained = Rc::new(self.contain());
                *self.walks.containers.borrow_mut() = Some(Rc::clone(&contained));
                contained
            }
        }
    }

    pub(super) fn contain(&self) -> Contained<'a> {
        let mut classes: Vec<&'a str> = self.classes.keys().copied().collect();
        classes.sort_unstable();
        let mut modules: Vec<&'a str> = self.modules.clone();
        modules.sort_unstable();
        modules.dedup();
        // A module's instance side stands for every object that includes,
        // prepends or extends it, the graph's or not (R4). Each object once,
        // a name both a class and a module too: its steps are then pushed
        // in a row, and the last one pushed tells a step seen (R10-02).
        let objects = classes
            .iter()
            .flat_map(|&c| [(c, Side::Instance), (c, Side::Singleton)])
            .chain(
                modules
                    .iter()
                    .filter(|&&m| !self.classes.contains_key(m))
                    .flat_map(|&m| [(m, Side::Instance), (m, Side::Singleton)]),
            );
        let mut map: Containers<'a> = HashMap::default();
        let mut opaque = Vec::new();
        let mut truncated = Vec::new();
        let outer = self.walks.capped.replace(false);
        for (object, side) in objects {
            let before = self.walks.capped.replace(false);
            let chain = self.chain(object, side);
            if self.walks.capped.get() {
                truncated.push((object, side));
            }
            self.walks.capped.set(before || self.walks.capped.get());
            if chain.steps.iter().any(|step| {
                step.node == Node::Unknown && matches!(step.stands, Stands::Opaque { .. })
            }) {
                opaque.push((object, side));
            }
            let exposed: Vec<(&'a str, Side)> = self
                .unplaced(&chain, &modules)
                .into_iter()
                .flat_map(|module| {
                    self.chain(module, Side::Instance)
                        .steps()
                        .collect::<Vec<_>>()
                })
                .collect();
            for step in chain.steps().chain(exposed) {
                if step == (object, side) {
                    continue;
                }
                let held = map.entry(step).or_default();
                if held.last() != Some(&(object, side)) {
                    held.push((object, side));
                }
            }
        }
        let capped = self.walks.capped.get();
        self.walks.capped.set(outer || capped);
        Contained {
            map,
            opaque,
            truncated,
        }
    }

    /// The modules `chain` may hold where it has an unknown step (R7): one
    /// that code the rules do not play mixed in (a module whose
    /// `append_features` is unknown), with the modules under it, which
    /// that code can name (`base.extend(self::ClassMethods)`); a module's
    /// own list after `extend self`.
    pub(super) fn unplaced(&self, chain: &Chain<'a>, modules: &[&'a str]) -> Vec<&'a str> {
        let mut unplaced: Vec<&'a str> = Vec::new();
        for (owner, side) in chain.steps() {
            if let Some(parts) = self.parts(owner) {
                let replaced = parts.replaced.iter().filter(|(on, _)| *on == side);
                unplaced.extend(replaced.map(|&(_, module)| module));
            }
        }
        let under: Vec<&'a str> = unplaced
            .iter()
            .flat_map(|module| {
                let prefix = format!("{module}::");
                modules
                    .iter()
                    .copied()
                    .filter(move |m| m.starts_with(&prefix))
            })
            .collect();
        unplaced.extend(under);
        unplaced.extend(chain.steps.iter().filter_map(|step| match step.stands {
            Stands::Itself(module) if step.node == Node::Unknown => Some(module),
            _ => None,
        }));
        unplaced
    }
}
