// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! zcode adapter — SQLite store at `~/.zcode/cli/db/db.sqlite`.
//!
//! The schema is the opencode shape (session / message / part with JSON
//! `data` columns and the same indexes), so this adapter delegates to the
//! shared helpers hosted in `sources::opencode`.

use std::path::PathBuf;

use crate::sources::opencode::{db_unit, oc_classify, oc_parse};
use crate::sources::{Change, IngestError, ParseOutput, SourceAdapter, SourceUnit};
use crate::store::IngestState;

pub struct Adapter {
    db_path: PathBuf,
}

impl Adapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            db_path: PathBuf::from(home).join(".zcode/cli/db/db.sqlite"),
        }
    }

    pub fn with_db(db_path: PathBuf) -> Self {
        Self { db_path }
    }
}

impl Default for Adapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceAdapter for Adapter {
    fn agent(&self) -> &'static str {
        "zcode"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        Ok(db_unit(&self.db_path).into_iter().collect())
    }

    fn classify(&self, unit: &SourceUnit, state: Option<&IngestState>) -> Change {
        oc_classify(unit, state)
    }

    fn parse(
        &self,
        unit: &SourceUnit,
        change: Change,
        state: Option<&IngestState>,
    ) -> Result<ParseOutput, IngestError> {
        oc_parse("zcode", unit, change, state)
    }
}
