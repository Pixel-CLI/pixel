//! Ref-injection defense, ported from `pixel-cli`'s `rescue_cmd::validate_ref`
//! (the strictest of the three original checks) so every consumer of
//! `pixel-git` gets the same guarantee consistently, instead of each call
//! site re-implementing (or forgetting) its own check.

use crate::error::GitError;

/// Reject anything that is not a plain hex object id or simple ref name —
/// in particular anything starting with `-` (option injection).
///
/// Originally ported from `pixel-cli::rescue_cmd::validate_ref`, which
/// rejected `-` *anywhere* in the string (not just as the first character)
/// because the allowed-charset filter omitted `-` entirely. That was a
/// pre-existing defect: ordinary branch names like `fix-bug` or
/// `feature/my-branch` failed validation. Now that pixel-git is the single
/// validator all call sites use (M1 wiring), the charset includes `-` so
/// mid-string dashes are accepted; the leading-dash option-injection guard
/// is preserved.
pub fn validate_ref(r: &str) -> Result<(), GitError> {
    let ok = !r.is_empty()
        && !r.starts_with('-')
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '~' | '^' | '-'));
    if ok {
        Ok(())
    } else {
        Err(GitError::InvalidRef(r.to_string()))
    }
}

/// Named constant for the `--end-of-options` flag (git >= 2.36), which makes
/// git treat the next token strictly as a rev/pathspec, never an option —
/// defense in depth on top of `validate_ref`, instead of the magic string
/// repeated across the three original wrappers.
pub const fn end_of_options() -> &'static str {
    "--end-of-options"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_leading_dash() {
        assert!(validate_ref("--upload-pack=/bin/sh").is_err());
        assert!(validate_ref("-x").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_ref("").is_err());
    }

    #[test]
    fn accepts_normal_ref_name() {
        assert!(validate_ref("main").is_ok());
        assert!(validate_ref("feature/mybranch").is_ok());
        assert!(validate_ref("HEAD").is_ok());
        assert!(validate_ref("v1.2.3").is_ok());
    }

    /// Regression for the over-restrictiveness bug inherited from
    /// `pixel-cli::rescue_cmd::validate_ref`: the original rejected `-`
    /// anywhere in the string (not just leading), so ordinary branch names
    /// like `feature/my-branch` or `fix-bug` failed validation. Now that
    /// pixel-git is the single validator (M1 wiring), mid-string dashes are
    /// accepted; only a *leading* dash (option injection) is rejected.
    #[test]
    fn accepts_dash_inside_an_otherwise_ordinary_ref_name() {
        assert!(validate_ref("feature/my-branch").is_ok());
        assert!(validate_ref("fix-bug").is_ok());
        assert!(validate_ref("release-1.0-rc").is_ok());
        // Leading dash is still rejected (option injection).
        assert!(validate_ref("-x").is_err());
        assert!(validate_ref("--upload-pack=/bin/sh").is_err());
    }

    #[test]
    fn accepts_40_hex_oid() {
        assert!(validate_ref("abcdef0123456789abcdef0123456789abcdef01").is_ok());
    }

    #[test]
    fn end_of_options_is_the_expected_flag() {
        assert_eq!(end_of_options(), "--end-of-options");
    }
}
