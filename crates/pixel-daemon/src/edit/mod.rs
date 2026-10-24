// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The edit hooks' decisions (`specs/hooks.md`): the card before an edit, the
//! stale callers after it, and the block at Stop. Each reads the graph
//! generation, changes only the [`Session`], and answers with the text to
//! inject (or the reason for silence) and the record the journal keeps.

pub mod session;
mod text;

use std::path::Path;

use pixel_graph::edit::{self, EditError, SymbolEdit};
use pixel_graph::extract;
use pixel_graph::lang;
use pixel_graph::shape::{ShapeChange, diff_shapes};
use pixel_graph::store::Store;
use pixel_model::{Symbol, SymbolKind};
use serde_json::{Value, json};

use session::{Base, Candidate, MAX_PENDING, MAX_SITES, Pending, PendingSite, Session};

/// What a hook is told to inject, or why it stays silent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Text for the model, and a count the live line shows.
    Inject { text: String, count: usize },
    /// Nothing to say, and the reason the journal keeps.
    Silent(&'static str),
}

impl Answer {
    /// The wire result of the op: `text` (or `null`), `silent`, `count`.
    #[must_use]
    pub fn to_value(&self, phase: &str) -> Value {
        match self {
            Answer::Inject { text, count } => {
                json!({"phase": phase, "text": text, "count": count, "bytes": text.len()})
            }
            Answer::Silent(reason) => {
                json!({"phase": phase, "text": null, "silent": reason, "count": 0})
            }
        }
    }
}

/// The shape of `symbol` as the session records it.
fn base_of(symbol: &Symbol) -> Base {
    Base {
        uid: symbol.uid.clone(),
        name: symbol.name.clone(),
        kind: symbol.kind.as_str().to_owned(),
        shape: symbol.shape.clone(),
    }
}

/// A `Symbol` carrying only what `diff_shapes` reads, rebuilt from a baseline.
fn symbol_of(file: &str, base: &Base) -> Option<Symbol> {
    Some(Symbol {
        uid: base.uid.clone(),
        file: file.to_owned(),
        kind: SymbolKind::parse(&base.kind)?,
        name: base.name.clone(),
        signature: String::new(),
        shape: base.shape.clone(),
        line: 0,
        end_line: 0,
    })
}

/// The name a uid (`path#name#kind`) carries.
fn name_of_uid(uid: &str) -> &str {
    uid.split('#').nth(1).unwrap_or(uid)
}

/// Records the file's baseline once per session, from the generation.
fn record_baseline(session: &mut Session, store: &Store, path: &str) -> Result<(), EditError> {
    if session.baselines.contains_key(path) || !session.has_room_for(path) {
        return Ok(());
    }
    let rows: Vec<Base> = store
        .symbols_in(path)?
        .iter()
        .filter(|symbol| !symbol.shape.is_empty())
        .map(base_of)
        .collect();
    if !rows.is_empty() {
        session.baselines.insert(path.to_owned(), rows);
    }
    Ok(())
}

/// Before an edit: the session's baseline for the file and, when the tool
/// carries the new text, the card for the symbols whose callers it strands.
///
/// # Errors
///
/// A graph or file read that failed; the hook stays silent on it.
pub fn before(
    session: &mut Session,
    store: &Store,
    root: &Path,
    path: &str,
    proposed: Option<&str>,
) -> Result<Answer, EditError> {
    record_baseline(session, store, path)?;
    let Some(proposed) = proposed else {
        return Ok(Answer::Silent("no_proposed_text"));
    };
    let report = edit::check(store, root, path, Some(proposed.as_bytes()))?;
    if report.reshaped == 0 {
        return Ok(Answer::Silent("no_shape_change"));
    }
    if report.is_empty() {
        return Ok(Answer::Silent("no_external_callers"));
    }
    let fresh: Vec<&SymbolEdit> = report
        .edits
        .iter()
        .filter(|edit| {
            session.carded.insert(session::card_key(
                &edit.symbol.uid,
                &change_text(&edit.change),
            ))
        })
        .collect();
    if fresh.is_empty() {
        return Ok(Answer::Silent("already_sent"));
    }
    for edit in &fresh {
        let sites = edit
            .proved
            .iter()
            .map(|site| Candidate {
                file: site.file.clone(),
                line: site.line,
                holder: site.holder.clone(),
            })
            .collect();
        session.remember_candidates(&edit.symbol.uid, sites);
    }
    let text = text::card(&fresh);
    Ok(Answer::Inject {
        count: fresh.iter().map(|edit| edit.proved.len()).sum(),
        text,
    })
}

/// The text a change is keyed by, so two different changes of one symbol card
/// twice.
fn change_text(change: &ShapeChange) -> String {
    match change {
        ShapeChange::Reshaped { new, .. } => format!("reshaped:{new}"),
        ShapeChange::Removed { .. } => "removed".to_owned(),
        ShapeChange::Renamed { to, .. } => format!("renamed:{to}"),
    }
}

/// The content hash of `file` under `root`, `None` when it cannot be read.
fn hash_of(root: &Path, file: &str) -> Option<String> {
    let bytes = edit::read(root, file).ok()??;
    Some(session::content_hash(&bytes))
}

/// After an edit: re-extract the file from disk, compare it with the
/// session's baseline, and add the callers of every changed shape that have
/// not changed since to the pending set.
///
/// # Errors
///
/// A graph or file read that failed; the hook stays silent on it.
pub fn after(
    session: &mut Session,
    store: &Store,
    root: &Path,
    path: &str,
) -> Result<Answer, EditError> {
    refresh(session, root);
    let Some(rows) = session.baselines.get(path) else {
        return Ok(Answer::Silent("no_baseline"));
    };
    let Some(language) = lang::wanted(path) else {
        return Ok(Answer::Silent("no_baseline"));
    };
    let Some(bytes) = edit::read(root, path)? else {
        return Ok(Answer::Silent("no_baseline"));
    };
    let baseline: Vec<Symbol> = rows.iter().filter_map(|row| symbol_of(path, row)).collect();
    let fresh = extract::rows(path, language, &bytes);
    let changes = diff_shapes(&baseline, &fresh.symbols);

    // A symbol that came back to its baseline shape leaves the set.
    let changed: Vec<&str> = changes.iter().map(ShapeChange::uid).collect();
    session
        .pending
        .retain(|p| p.file != path || changed.contains(&p.uid.as_str()));
    if changes.is_empty() {
        return Ok(Answer::Silent("no_shape_change"));
    }

    let mut added: Vec<Pending> = Vec::new();
    for change in &changes {
        let uid = change.uid();
        let Some(old) = baseline.iter().find(|s| s.uid == uid) else {
            continue;
        };
        let sites = sites_of(session, store, root, old);
        if sites.is_empty() {
            continue;
        }
        let new = match change {
            ShapeChange::Reshaped { new, .. } => Some(new.clone()),
            ShapeChange::Removed { .. } | ShapeChange::Renamed { .. } => None,
        };
        let entry = Pending {
            uid: uid.to_owned(),
            name: old.name.clone(),
            file: path.to_owned(),
            old: old.shape.clone(),
            new,
            sites,
        };
        let grew = match session.pending.iter().find(|p| p.uid == entry.uid) {
            None => true,
            Some(held) => entry.sites.iter().any(|s| {
                !held
                    .sites
                    .iter()
                    .any(|h| (h.file.as_str(), h.line) == (s.file.as_str(), s.line))
            }),
        };
        session.pending.retain(|p| p.uid != entry.uid);
        if grew {
            added.push(entry.clone());
        }
        session.pending.push(entry);
    }
    session.pending.truncate(MAX_PENDING);
    if added.is_empty() {
        return Ok(Answer::Silent("nothing_new"));
    }
    Ok(Answer::Inject {
        count: added.iter().map(|p| p.sites.len()).sum(),
        text: text::stale(&added),
    })
}

/// The proved caller sites outside `old` that a change strands, each with
/// the hash its file has now: from the generation while it still holds the
/// symbol, else from what a card named.
fn sites_of(session: &Session, store: &Store, root: &Path, old: &Symbol) -> Vec<PendingSite> {
    let mut found: Vec<(String, u32, String)> = Vec::new();
    if let Ok(Some(current)) = store.symbol(&old.uid)
        && let Ok((proved, _)) = edit::outside_callers(store, &current)
    {
        found.extend(proved.into_iter().map(|s| (s.file, s.line, s.holder)));
    }
    if found.is_empty() {
        found.extend(
            session
                .candidates
                .get(&old.uid)
                .into_iter()
                .flatten()
                .map(|c| (c.file.clone(), c.line, c.holder.clone())),
        );
    }
    found.sort();
    found.dedup();
    found
        .into_iter()
        .filter(|(file, _, _)| file != &old.file)
        .filter(|(file, _, _)| !session.fixed.contains(&Session::fixed_key(&old.uid, file)))
        .take(MAX_SITES)
        .filter_map(|(file, line, holder)| {
            let hash = hash_of(root, &file)?;
            Some(PendingSite {
                file,
                line,
                holder,
                hash,
            })
        })
        .collect()
}

/// Drops from the pending set the sites whose file changed since they were
/// recorded (the agent edited the caller), and the symbols left with none.
pub fn refresh(session: &mut Session, root: &Path) {
    for pending in &mut session.pending {
        let mut fixed = Vec::new();
        pending.sites.retain(|site| {
            let held = hash_of(root, &site.file).is_some_and(|now| now == site.hash);
            if !held {
                fixed.push(Session::fixed_key(&pending.uid, &site.file));
            }
            held
        });
        session.fixed.extend(fixed);
    }
    session.pending.retain(|p| !p.sites.is_empty());
}

/// At Stop: block once per distinct pending set.
pub fn stop(session: &mut Session, root: &Path) -> Answer {
    refresh(session, root);
    if session.pending.is_empty() {
        return Answer::Silent("empty");
    }
    let hash = session.pending_hash();
    if !session.blocked.insert(hash) {
        return Answer::Silent("already_blocked");
    }
    Answer::Inject {
        count: session.pending.iter().map(|p| p.sites.len()).sum(),
        text: text::block(&session.pending),
    }
}

#[cfg(test)]
mod tests;
