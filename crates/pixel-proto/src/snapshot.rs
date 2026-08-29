//! Snapshot token contract.
//!
//! Reproduces usable-git's 12-hex-char snapshot token scheme
//! (`reference/usable-git/packages/usable-git/src/mutations/snapshot-store.ts`):
//! a sha256 digest of `{root, head, sorted fingerprints}`, truncated to 12
//! hex characters, matching `^[a-f0-9]{12}$`. This crate only carries the
//! *type* (a validated newtype) — computing/storing tokens is pixel-ops'
//! job, not a contract concern.

use std::fmt;

use serde::{Deserialize, Serialize, Serializer};

/// A validated 12-lowercase-hex-character snapshot token.
///
/// Serializes as a bare JSON string (newtype transparency). Deserializing
/// validates the pattern so a malformed token can never silently enter the
/// system as a `SnapshotToken`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapshotToken(String);

impl SnapshotToken {
    /// Parse and validate a token string. Rejects wrong length, uppercase,
    /// or non-hex characters.
    pub fn parse(value: &str) -> Result<Self, String> {
        let is_valid = value.len() == 12 && value.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if !is_valid {
            return Err(format!(
                "invalid snapshot token {value:?}: must match ^[a-f0-9]{{12}}$"
            ));
        }
        Ok(SnapshotToken(value.to_string()))
    }
}

impl fmt::Display for SnapshotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SnapshotToken {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for SnapshotToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SnapshotToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        SnapshotToken::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The envelope's `snapshot` field (v1): `{token, head, branch, dirty}` where
/// `dirty` is a simple boolean. Retained for backward compatibility — the
/// Envelope v2 `snapshot` field now carries [`SnapshotInfo`] instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Snapshot {
    #[serde(default)]
    pub token: Option<SnapshotToken>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub dirty: bool,
}

/// Envelope v2 `snapshot` field: `{token, head, branch, dirty}`.
///
/// Unlike the v1 [`Snapshot`], `token` is a plain `Option<String>` (the
/// daemon may populate it with a raw hex digest before a `SnapshotToken` is
/// validated), and `dirty` is `Vec<String>` — the list of repo-relative paths
/// with uncommitted changes — so callers can show *which* files are dirty,
/// not merely *whether* any are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SnapshotInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirty: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_valid_tokens() {
        assert!(SnapshotToken::parse("abcdef012345").is_ok());
        assert!(SnapshotToken::parse("000000000000").is_ok());
        assert!(SnapshotToken::parse("f00dfacecafe").is_ok());
    }

    #[test]
    fn parse_rejects_wrong_length() {
        assert!(SnapshotToken::parse("abcdef01234").is_err()); // 11 chars
        assert!(SnapshotToken::parse("abcdef0123456").is_err()); // 13 chars
        assert!(SnapshotToken::parse("").is_err());
    }

    #[test]
    fn parse_rejects_uppercase() {
        assert!(SnapshotToken::parse("ABCDEF012345").is_err());
        assert!(SnapshotToken::parse("abcDEF012345").is_err());
    }

    #[test]
    fn parse_rejects_non_hex() {
        assert!(SnapshotToken::parse("abcdefg12345").is_err()); // 'g' not hex
        assert!(SnapshotToken::parse("abcdef01234z").is_err());
        assert!(SnapshotToken::parse("abcdef-12345").is_err());
    }

    #[test]
    fn serializes_as_bare_string() {
        let token = SnapshotToken::parse("abcdef012345").unwrap();
        assert_eq!(serde_json::to_string(&token).unwrap(), "\"abcdef012345\"");
    }

    #[test]
    fn deserialize_rejects_malformed_token() {
        let result: Result<SnapshotToken, _> = serde_json::from_str("\"not-a-token\"");
        assert!(result.is_err());
    }

    #[test]
    fn snapshot_info_omits_none_fields_and_empty_dirty() {
        let info = SnapshotInfo {
            token: None,
            head: Some("abc123".into()),
            branch: None,
            dirty: vec![],
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"head": "abc123"})
        );
    }

    #[test]
    fn snapshot_info_serializes_dirty_file_list() {
        let info = SnapshotInfo {
            token: Some("abcdef012345".into()),
            head: Some("deadbeef".into()),
            branch: Some("main".into()),
            dirty: vec!["src/a.rs".into(), "README.md".into()],
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "token": "abcdef012345",
                "head": "deadbeef",
                "branch": "main",
                "dirty": ["src/a.rs", "README.md"],
            })
        );
    }
}
