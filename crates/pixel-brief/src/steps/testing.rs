// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! A repository the steps' tests read: tracked paths with their text, behind
//! the `Evidence` calls a step makes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use regex::Regex;

use crate::chain::{CallerHit, Evidence, Found, RichFound, RichHit, SymbolHit};
use crate::region_pack::Block;

use super::overview::Tree;

type Reader = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A repository as the steps read it: tracked paths, sorted, and their text.
pub(super) struct Repo {
    pub(super) files: Vec<String>,
    /// Canned git answers: the first needle the joined arguments contain wins.
    pub(super) git: Vec<(String, String)>,
    text: Arc<BTreeMap<String, String>>,
    read: Reader,
}

impl Repo {
    pub(super) fn new(files: &[(&str, &str)]) -> Self {
        let text: BTreeMap<String, String> = files
            .iter()
            .map(|(path, text)| ((*path).to_string(), (*text).to_string()))
            .collect();
        let text = Arc::new(text);
        let held = Arc::clone(&text);
        Self {
            files: text.keys().cloned().collect(),
            git: Vec::new(),
            text,
            read: Box::new(move |path| held.get(path).cloned()),
        }
    }

    /// The git answers: a command whose arguments contain the needle prints
    /// the text.
    pub(super) fn with_git(mut self, answers: &[(&str, &str)]) -> Self {
        self.git = answers
            .iter()
            .map(|(needle, text)| ((*needle).to_string(), (*text).to_string()))
            .collect();
        self
    }

    pub(super) fn tree(&self) -> Tree<'_> {
        Tree {
            files: &self.files,
            read: &*self.read,
        }
    }
}

impl Evidence for Repo {
    fn files_with(&self, _anchor: &str, _deadline: Instant) -> Result<Found, String> {
        Err("unused".to_string())
    }
    fn concept(&self, _phrase: &str, _deadline: Instant) -> Result<Found, String> {
        Err("unused".to_string())
    }
    fn symbols(&self, _name: &str, _deadline: Instant) -> Result<Vec<SymbolHit>, String> {
        Err("unused".to_string())
    }
    fn callers(&self, _target: &str, _deadline: Instant) -> Result<Vec<CallerHit>, String> {
        Err("unused".to_string())
    }
    fn tracked_files(&self, _deadline: Instant) -> Result<Vec<String>, String> {
        Ok(self.files.clone())
    }
    fn read_file(&self, path: &str, max: u64, _deadline: Instant) -> Result<String, String> {
        let text = self.text.get(path).ok_or_else(|| "absent".to_string())?;
        if text.len() as u64 > max {
            return Err("too long".to_string());
        }
        Ok(text.clone())
    }
    fn git_read(&self, args: &[&str], _max: usize, _deadline: Instant) -> Result<String, String> {
        let joined = args.join(" ");
        self.git
            .iter()
            .find(|(needle, _)| joined.contains(needle.as_str()))
            .map(|(_, text)| text.clone())
            .ok_or_else(|| format!("no answer for git {joined}"))
    }
    fn grep(&self, pattern: &str, limit: usize, _deadline: Instant) -> Result<RichFound, String> {
        let pattern = Regex::new(pattern).map_err(|error| error.to_string())?;
        let hits: Vec<RichHit> = self
            .text
            .iter()
            .flat_map(|(path, text)| {
                text.lines()
                    .enumerate()
                    .filter(|(_, line)| pattern.is_match(line))
                    .map(|(index, line)| RichHit {
                        path: path.clone(),
                        line: index as u64 + 1,
                        text: Some(line.to_string()),
                    })
            })
            .collect();
        let capped = hits.len() > limit;
        Ok(RichFound {
            hits: hits.into_iter().take(limit).collect(),
            capped,
        })
    }
}

pub(super) fn texts(blocks: &[Block]) -> Vec<String> {
    blocks.iter().map(Block::text).collect()
}
