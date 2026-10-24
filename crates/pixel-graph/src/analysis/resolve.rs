// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Naming a symbol the way a command line does: by uid, or by a qualified or
//! last-segment name that may mean several.

use pixel_model::Symbol;

use super::impact::resolve_target;
use crate::store::{Store, StoreError};

/// What a token names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    One(Symbol),
    /// Nothing: no such uid, no symbol of that name.
    None,
    /// Several; the caller asks for a uid.
    Many(Vec<Symbol>),
}

/// The symbols `token` names. A token with a `#` is a uid; anything else a
/// name, matched case-insensitively against qualified names and last
/// segments, excluding files and packages.
pub fn resolve(store: &Store, token: &str) -> Result<Resolved, StoreError> {
    let mut found = if token.contains('#') {
        store.symbol(token)?.into_iter().collect()
    } else {
        resolve_target(store, token)?
    };
    Ok(match found.len() {
        0 => Resolved::None,
        1 => Resolved::One(found.remove(0)),
        _ => Resolved::Many(found),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::fixture::project;

    #[test]
    fn a_name_a_uid_and_a_miss_resolve_to_one_one_and_none() {
        let (_repo, store) = project();
        let Resolved::One(by_name) = resolve(&store, "run").unwrap() else {
            panic!("one symbol");
        };
        let Resolved::One(by_uid) = resolve(&store, &by_name.uid).unwrap() else {
            panic!("one symbol");
        };
        assert_eq!(by_name, by_uid);
        assert_eq!(resolve(&store, "nobody").unwrap(), Resolved::None);
        assert_eq!(
            resolve(&store, "src/a.rs#nobody#function").unwrap(),
            Resolved::None
        );
    }

    #[test]
    fn a_name_defined_twice_is_many() {
        let repo = crate::testutil::repo_with(&[
            ("a.ts", "export function twice() {}\n"),
            ("b.ts", "export function twice() {}\n"),
        ]);
        crate::refresh(repo.path(), None, None).unwrap();
        let store = Store::open(repo.path()).unwrap();
        let Resolved::Many(found) = resolve(&store, "twice").unwrap() else {
            panic!("several symbols");
        };
        assert_eq!(found.len(), 2);
    }
}
