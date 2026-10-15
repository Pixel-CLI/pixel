// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby's `include` and `prepend` as `include_modules_at` runs them, on
//! the lists of the owner being built ([`Building`]).

use super::{At, Building, Decl, Node, Object, Part, Side, Step};

impl<'a> Building<'a, '_> {
    pub(super) fn part(&mut self, side: Side) -> (&mut Part<'a>, &[Step<'a>]) {
        match side {
            Side::Instance => (&mut self.instance, &self.tail_instance),
            Side::Singleton => (&mut self.singleton, &self.tail_singleton),
        }
    }

    /// Ruby's `include` of `ancestry` into this list (`include_modules_at`
    /// with `search_super`).
    pub(super) fn include(&mut self, side: Side, ancestry: &[Step<'a>], decl: Decl<'a>) {
        let owner = self.owner;
        let index = self.index;
        let played = self.maybe;
        let inserted = self.at(decl);
        let (part, tail) = self.part(side);
        let mut at = part.origin + 1;
        for step in ancestry {
            let mut region = step.region;
            // A step that may not be in the module's own list may not be
            // here either (R5-07), nor one its declaration may put there
            // after this one runs (R12-02).
            let mut maybe = step.maybe || index.late(step, decl);
            if step.node != Node::Unknown {
                if let Some(j) = part.steps.iter().position(|s| s.node == step.node) {
                    // Past a step that may not be there, or one the module
                    // may not bring, where the rest goes depends on whether
                    // it is (B4, R5-07).
                    if (part.steps[j].maybe && j > at) || (maybe && j >= at) {
                        part.steps.insert(at, Step::new(Node::Unknown));
                        at += 1;
                    } else if j >= at {
                        at = j + 1;
                    }
                    continue;
                }
                if let Some(there) = tail.iter().find(|s| s.node == step.node) {
                    // Whether the superclass's declaration has run by now:
                    // not yet when it is below this one in the same file,
                    // unknown when another file's load order decides.
                    let ran = match there.at {
                        _ if there.maybe => None,
                        Some(by) => index.before((by.file, by.line), by.by, decl.at),
                        None => Some(true),
                    };
                    match ran {
                        Some(false) => {}
                        // Declared where load order is unknown: here or
                        // there, the same label for both.
                        _ if there.region.is_some() => region = there.region,
                        Some(true) => continue,
                        None => maybe = true,
                    }
                }
                // A step of this list or of the superclass's chain the rules
                // cannot name may hold the module already, and Ruby skips
                // it then (R5-06).
                if let Node::Known(module, _) = step.node
                    && part
                        .steps
                        .iter()
                        .chain(tail)
                        .any(|s| index.may_hold(*s, module))
                {
                    maybe = true;
                }
            }
            if decl.uncertain {
                region = Some((owner, side, false));
            }
            part.steps.insert(
                at,
                Step {
                    node: step.node,
                    region,
                    at: Some(inserted),
                    stands: step.stands,
                    maybe: maybe || played,
                },
            );
            at += 1;
        }
    }

    /// Where `decl`, a declaration of the owner being built, is written.
    pub(super) fn at(&self, decl: Decl<'a>) -> At<'a> {
        At {
            file: decl.at.0,
            line: decl.at.1,
            by: self.owner,
        }
    }

    /// Ruby's `prepend` of `ancestry` before the owner on this side.
    pub(super) fn prepend(&mut self, side: Side, ancestry: &[Step<'a>], decl: Decl<'a>) {
        let owner = self.owner;
        let index = self.index;
        let played = self.maybe;
        let (part, _) = self.part(side);
        let mut at = 0;
        for step in ancestry {
            // One the module may not bring, now or yet (R5-07, R12-02).
            let maybe = step.maybe || index.late(step, decl);
            if step.node != Node::Unknown
                && let Some(j) = part.steps[..part.origin]
                    .iter()
                    .position(|s| s.node == step.node)
            {
                // Past a step that may not be there, or one the module may
                // not bring, where it goes depends on whether it is (B4,
                // R5-07).
                if (part.steps[j].maybe || maybe) && j >= at {
                    part.steps.insert(at, Step::new(Node::Unknown));
                    at += 1;
                    part.origin += 1;
                } else if j >= at {
                    at = j + 1;
                }
                continue;
            }
            // A module prepended before that the rules cannot name may hold
            // it already (R5-06).
            let held = matches!(step.node, Node::Known(module, _)
                if part.steps[..part.origin].iter().any(|s| index.may_hold(*s, module)));
            let region = if decl.uncertain {
                Some((owner, side, true))
            } else {
                step.region
            };
            let held = held || played || maybe;
            part.steps.insert(
                at,
                Step {
                    node: step.node,
                    region,
                    at: Some(At {
                        file: decl.at.0,
                        line: decl.at.1,
                        by: owner,
                    }),
                    stands: step.stands,
                    maybe: held,
                },
            );
            at += 1;
            part.origin += 1;
        }
    }

    /// Whether `module` is among this side's ancestors when `decl` runs:
    /// `base < self`, for a concern; `None` when a declaration in a file
    /// loaded in an order the graph cannot tell decides (B4).
    pub(super) fn has(&mut self, side: Side, module: &'a str, decl: Decl<'a>) -> Option<bool> {
        let node = Node::Known(module, Side::Instance);
        let index = self.index;
        let (part, tail) = self.part(side);
        let mut own = part.steps.iter().filter(|s| s.node == node);
        let mut has = match own.next() {
            Some(step) if !step.maybe => return Some(true),
            Some(_) => None,
            None => Some(false),
        };
        for step in tail.iter().filter(|s| s.node == node) {
            let ran = match step.at {
                _ if step.maybe => None,
                Some(by) => index.before((by.file, by.line), by.by, decl.at),
                None => Some(true),
            };
            match ran {
                Some(true) => return Some(true),
                Some(false) => {}
                None => has = None,
            }
        }
        has
    }

    /// What mixing `module` into `object` does is beyond the rules (a
    /// declaration played inside itself, or past [`MAX_ANCESTOR_DEPTH`]):
    /// every lookup through `object` is unknown, and it may hold the module
    /// anywhere (R5-01).
    pub(super) fn cut(&mut self, object: Object, module: &'a str, decl: Decl<'a>) {
        self.unknown_front(object, decl);
        if object == Object::Owner {
            self.replaced.push((Side::Instance, module));
        }
        self.replaced.push((Side::Singleton, module));
    }

    /// Something the rules do not read may have prepended to `object`:
    /// every lookup through it is unknown.
    pub(super) fn unknown_front(&mut self, object: Object, decl: Decl<'a>) {
        let unknown = [Step::unread()];
        if object == Object::Owner {
            self.prepend(Side::Instance, &unknown, decl);
        }
        self.prepend(Side::Singleton, &unknown, decl);
    }
}
