// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby lexical constants and the method owners their receivers identify.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use super::Decision;
use crate::store::{GraphStore, StoreError};

#[derive(Debug)]
struct Scope {
    owner: String,
    start: u32,
    end: u32,
}

#[derive(Debug, Default)]
pub(super) struct Index {
    constants: HashSet<String>,
    classes: HashSet<String>,
    methods: HashMap<String, Vec<i64>>,
    scopes: HashMap<i64, Vec<Scope>>,
}

impl Index {
    pub(super) fn build(store: &GraphStore) -> Result<Self, StoreError> {
        let mut index = Self::default();
        let mut query = store.conn().prepare(
            "SELECT s.id, s.file_id, s.qualified, s.kind, s.start_line, s.end_line
             FROM symbols s JOIN files f ON f.id=s.file_id WHERE f.lang='ruby'",
        )?;
        let rows = query.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, u32>(5)?,
            ))
        })?;
        for row in rows {
            let (id, file, qualified, kind, start, end) = row?;
            match kind.as_str() {
                "class" | "module" => {
                    // Each lookup owns its key; the scope retains the qualified name.
                    index.constants.insert(qualified.clone());
                    if kind == "class" {
                        index.classes.insert(qualified.clone());
                    }
                    index.scopes.entry(file).or_default().push(Scope {
                        owner: qualified,
                        start,
                        end,
                    });
                }
                "method" => {
                    index.methods.entry(qualified).or_default().push(id);
                }
                _ => {}
            }
        }
        for scopes in index.scopes.values_mut() {
            scopes.sort_by_key(|scope| (Reverse(scope.start), scope.end));
        }
        Ok(index)
    }

    /// Ruby uses actual lexical nesting, not prefixes of a qualified class name.
    /// `class A::B` sees A::B but not A; `module A; class B` sees both.
    /// A nearer first segment shadows outer paths even if its suffix is absent.
    /// <https://docs.ruby-lang.org/en/master/syntax/modules_and_classes_rdoc.html#label-Constants>
    fn constant(&self, file: i64, path: &str, line: Option<u32>) -> Option<String> {
        if let Some(rooted) = path.strip_prefix("::") {
            return self.constants.contains(rooted).then(|| rooted.to_string());
        }
        let line = line?;
        let first = path.split("::").next()?;
        if let Some(scopes) = self.scopes.get(&file) {
            let visible: Vec<_> = scopes
                .iter()
                .filter(|s| s.start <= line && line <= s.end)
                .collect();
            // A superclass expression runs outside the new class, as does a
            // statement after `end` on the same line. Line-only spans cannot
            // place these sites, or prove the nesting of identical spans.
            if visible.iter().any(|s| s.start == line || s.end == line)
                || visible
                    .windows(2)
                    .any(|pair| (pair[0].start, pair[0].end) == (pair[1].start, pair[1].end))
            {
                return None;
            }
            for scope in visible {
                if self
                    .constants
                    .contains(&format!("{}::{first}", scope.owner))
                {
                    let qualified = format!("{}::{path}", scope.owner);
                    return self.constants.contains(&qualified).then_some(qualified);
                }
            }
        }
        self.constants.contains(path).then(|| path.to_string())
    }

    /// None means absent; Unresolved means competing definitions forbid fallback.
    fn method(&self, owner: &str, separator: char, name: &str) -> Option<Decision> {
        let candidates = self.methods.get(&format!("{owner}{separator}{name}"))?;
        Some(match candidates.as_slice() {
            [id] => Decision::Exact(*id),
            _ => Decision::Unresolved,
        })
    }

    pub(super) fn decide(
        &self,
        file: i64,
        receiver: &str,
        name: &str,
        line: Option<u32>,
    ) -> Option<Decision> {
        let receiver = receiver.trim();
        if !receiver.starts_with("::") && !receiver.chars().next().is_some_and(char::is_uppercase) {
            return None;
        }
        Some(
            self.constant_call(file, receiver, name, line)
                .unwrap_or(Decision::Unresolved),
        )
    }

    fn constant_call(
        &self,
        file: i64,
        receiver: &str,
        name: &str,
        line: Option<u32>,
    ) -> Option<Decision> {
        // Extraction strips arguments only from AST-confirmed constant.new/set calls.
        let (path, via) = receiver.rsplit_once('.').unwrap_or((receiver, ""));
        if !constant_path(path) {
            return None;
        }
        let owner = self.constant(file, path, line)?;
        match via {
            "new" => {
                if !self.classes.contains(&owner) || self.method(&owner, '.', "new").is_some() {
                    return None;
                }
                self.method(&owner, '#', name)
            }
            "set" => {
                if !self.classes.contains(&owner)
                    || !matches!(name, "perform_later" | "perform_now")
                    || self.method(&owner, '.', "set").is_some()
                    || self.method(&owner, '.', name).is_some()
                {
                    return None;
                }
                self.method(&owner, '#', "perform").map(probable)
            }
            "" => {
                if let Some(explicit) = self.method(&owner, '.', name) {
                    return Some(explicit);
                }
                if !self.classes.contains(&owner) {
                    return None;
                }
                if name == "new" {
                    return self.method(&owner, '#', "initialize");
                }
                if matches!(
                    name,
                    "perform_later" | "perform_now" | "perform_async" | "perform_in" | "perform_at"
                ) {
                    return self.method(&owner, '#', "perform").map(probable);
                }
                if owner.ends_with("Mailer") {
                    return self.method(&owner, '#', name).map(probable);
                }
                None
            }
            _ => None,
        }
    }
}

fn probable(decision: Decision) -> Decision {
    match decision {
        Decision::Exact(id) => Decision::Probable(id),
        other => other,
    }
}

fn constant_path(path: &str) -> bool {
    path.strip_prefix("::")
        .unwrap_or(path)
        .split("::")
        .all(|part| {
            part.chars().next().is_some_and(char::is_uppercase)
                && part.chars().all(|c| c.is_alphanumeric() || c == '_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SymbolKind;

    #[test]
    fn constant_lookup_should_honor_scope_boundaries_and_shadow_the_first_segment() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("local.rb", "local", "ruby").unwrap();
        let remote = store.replace_file("remote.rb", "remote", "ruby").unwrap();
        store
            .insert_symbol(local, "outer", "A", "A", SymbolKind::Module, 1, 20, "")
            .unwrap();
        store
            .insert_symbol(local, "inner", "C", "A::C", SymbolKind::Class, 2, 10, "")
            .unwrap();
        for qualified in ["B", "A::B", "A::C::B", "B::Child", "A::B::Child"] {
            store
                .insert_symbol(
                    remote,
                    qualified,
                    "B",
                    qualified,
                    SymbolKind::Class,
                    1,
                    1,
                    "",
                )
                .unwrap();
        }
        let index = Index::build(&store).unwrap();
        for (line, target) in [
            (0, Some("B")),
            (1, None),
            (2, None),
            (3, Some("A::C::B")),
            (9, Some("A::C::B")),
            (10, None),
            (11, Some("A::B")),
            (19, Some("A::B")),
            (20, None),
            (21, Some("B")),
        ] {
            assert_eq!(
                index.constant(local, "B", Some(line)),
                target.map(str::to_string),
                "line {line}"
            );
        }
        assert_eq!(
            index.constant(local, "B::Child", Some(5)),
            None,
            "A::C::B exists, so its missing Child cannot fall back to A::B::Child"
        );
        assert_eq!(
            index.constant(local, "B::Child", Some(11)),
            Some("A::B::Child".into())
        );
        assert_eq!(index.constant(local, "::B", None), Some("B".into()));
        assert_eq!(index.constant(local, "::Missing", Some(5)), None);
        assert_eq!(index.constant(local, "B", None), None);
    }

    #[test]
    fn constant_receivers_should_reject_expressions_without_guessing_from_the_method_name() {
        for valid in ["Foo", "A::B", "::A::B", "Some_Job", "École"] {
            assert!(constant_path(valid), "{valid}");
        }
        for invalid in [
            "",
            "::",
            "a",
            "Foo::",
            "Foo::::Bar",
            "Foo::bar",
            "Foo!",
            "Foo.new()",
        ] {
            assert!(!constant_path(invalid), "{invalid}");
        }
        let index = Index::default();
        for receiver in ["Foo", "::Foo", "Foo.new(1)", "Foo.tap", "École"] {
            assert_eq!(
                index.decide(1, receiver, "run", Some(1)),
                Some(Decision::Unresolved)
            );
        }
        for receiver in ["self", "variable", "@thing", "", "123"] {
            assert_eq!(index.decide(1, receiver, "run", Some(1)), None);
        }
    }
}
