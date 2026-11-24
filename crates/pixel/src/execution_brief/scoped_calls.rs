// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Incoming call sites of a declaration the prompt identifies by name, owner
//! and file. Such a request can be answered by a single published-graph read.

use std::sync::LazyLock;
use std::time::Instant;

use pixel_graph::GraphStore;
use pixel_graph::store::{self, SymbolKind};
use regex::Regex;

use super::chain::{BRIEF_BYTES, BRIEF_TAG, SKILL_LINE, has_source_extension};

const MORE: &str = "\nAdditional indexed sites omitted.";

static SCOPED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?ix)\b(?:calls\s+to|call\s+sites?\s+(?:of|for)|callers\s+of|who\s+calls|qui\s+appelle|appels?\s+(?:de|à))\s+",
        r"(?:(?:(?:the|la|le)\s+)?(?:function|method|fonction|méthode)\s+)?",
        r"`?(?<target>[a-z_]\w*)`?\s+(?:(?:declared|defined)\s+(?:in|inside)|(?:déclaré|défini)e?\s+dans)\s+",
        r"`?(?<owner>[a-z_]\w*)\b`?"
    )).expect("literal scoped call-site request")
});

pub(crate) struct Query {
    target: String,
    owner: String,
    file: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Answer {
    pub(super) text: String,
    pub(super) paths: Vec<String>,
    pub(super) verified: bool,
}

impl Query {
    pub(super) fn parse(prompt: &str) -> Option<Self> {
        let mut scopes = SCOPED.captures_iter(prompt);
        let caps = scopes.next()?;
        let mut files = prompt
            .split(|c: char| !c.is_alphanumeric() && !matches!(c, '_' | '-' | '.' | '/'))
            .map(|word| word.trim_end_matches('.'))
            .map(|word| word.strip_prefix("./").unwrap_or(word))
            .filter(|word| has_source_extension(word));
        let file = files.next()?;
        // Do not choose between two files or two requests. Leave room for
        // the header and footer even when the prompt contains long tokens.
        if scopes.next().is_some()
            || files.any(|other| other != file)
            || file.len() + caps["target"].len() + caps["owner"].len() > BRIEF_BYTES / 2
        {
            return None;
        }
        Some(Self {
            target: caps["target"].to_owned(),
            owner: caps["owner"].to_owned(),
            file: file.to_owned(),
        })
    }

    pub(super) fn read(&self, store: &GraphStore, deadline: Instant) -> store::Result<Answer> {
        let Some(file) = store.file_by_path(&self.file)? else {
            return Ok(self.unverified());
        };
        let symbols = store.symbols_in_file(file.id)?;
        let functions = symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method));
        let mut targets = functions.clone().filter(|s| s.name == self.target);
        let Some(target) = targets.next() else {
            return Ok(self.unverified());
        };
        // Same-file homonyms can share a lexical call binding in the graph.
        // A site attributed to either is insufficient evidence in that case.
        let owner = functions
            .filter(|s| {
                s.id != target.id
                    && s.start_line <= target.start_line
                    && target.end_line <= s.end_line
            })
            .min_by_key(|s| s.end_line - s.start_line);
        // With line-only extents two declarations touching the same boundary
        // may be siblings. Only an unambiguous enclosing range proves scope.
        if targets.next().is_some()
            || !owner.is_some_and(|s| {
                s.name == self.owner
                    && s.start_line < target.start_line
                    && target.end_line < s.end_line
            })
        {
            return Ok(self.unverified());
        }
        let mut stmt = store.conn().prepare(
            "SELECT DISTINCT f.path, e.site_line FROM edges e
             JOIN symbols caller ON caller.id = e.src_id JOIN files f ON f.id = caller.file_id
             WHERE e.dst_id = ?1 AND e.kind = 'calls' AND e.tier = 'exact' AND e.site_line > 0
             ORDER BY f.path, e.site_line",
        )?;
        let mut rows = stmt.query([target.id])?;
        let mut answer = Answer {
            text: format!(
                "{BRIEF_TAG}\nDefinition: {}:{} ({} in {})\nExact indexed call sites:",
                self.file, target.start_line, self.target, self.owner
            ),
            verified: true,
            paths: vec![self.file.clone()],
        };
        let mut has_sites = false;
        while let Some(row) = rows.next()? {
            has_sites = true;
            let path: String = row.get(0)?;
            let line: u32 = row.get(1)?;
            let location = format!("\n{path}:{line}");
            if answer.text.len() + location.len() + MORE.len() + SKILL_LINE.len() + 1 > BRIEF_BYTES
                || Instant::now() >= deadline
            {
                answer.text.push_str(MORE);
                break;
            }
            answer.text.push_str(&location);
            if !answer.paths.contains(&path) {
                answer.paths.push(path);
            }
        }
        if !has_sites {
            answer.text.push_str(" none recorded.");
        }
        Ok(answer)
    }

    fn unverified(&self) -> Answer {
        Answer {
            text: format!(
                "{BRIEF_TAG}\nThe index could not verify the call sites of {} in {} ({}).",
                self.target, self.owner, self.file
            ),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_calls_should_leave_outgoing_walks_and_multi_file_questions_to_the_normal_routes() {
        for prompt in [
            "What does `alive` defined in runTask (a.mjs) call?",
            "Compare calls to alive defined in runTask in a.mjs and b.mjs.",
            "List calls to alive defined in runTask and calls to save defined in stopTask (a.mjs).",
        ] {
            assert!(Query::parse(prompt).is_none(), "{prompt}");
        }
    }
}
