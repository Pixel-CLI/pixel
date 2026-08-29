//! Warning contract, mirroring usable-git's `warningSchema`
//! (`{code, message}`, both non-empty in usable-git's zod schema; this crate
//! does not re-enforce non-emptiness — that is a validation concern for
//! whichever op constructs the warning).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
}
