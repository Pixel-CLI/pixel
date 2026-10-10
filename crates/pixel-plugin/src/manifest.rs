// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel-plugin.toml`: what a plugin directory declares about itself.

use std::path::{Component, Path};

use toml_edit::{DocumentMut, Item};

use crate::Error;

/// The manifest's file name, at the root of a plugin directory.
pub const MANIFEST_FILE: &str = "pixel-plugin.toml";

/// The plugin API this host speaks; a manifest must declare exactly it.
pub const API_VERSION: i64 = 1;

/// Longest plugin name; it becomes a directory name and a `pixel-<name>` binary.
const MAX_NAME_LEN: usize = 64;

/// What a plugin can be asked to do. API 1 knows one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// `pixel <name> [args…]` runs the plugin.
    Command,
}

impl Capability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "command" => Some(Self::Command),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub api: i64,
    pub description: String,
    /// Relative path of the executable, inside the plugin directory.
    pub run: String,
    pub capabilities: Vec<Capability>,
    /// A plugin that reaches the network needs `pixel plugin enable`.
    pub network: bool,
}

impl Manifest {
    pub fn has_command(&self) -> bool {
        self.capabilities.contains(&Capability::Command)
    }
}

/// A name usable as a directory and as `pixel-<name>` on PATH.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= MAX_NAME_LEN
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Parse and validate manifest text. The error is the reason, without a path.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let doc: DocumentMut = text.parse().map_err(|e| format!("not valid TOML: {e}"))?;
    let string = |key: &str| -> Result<String, String> {
        doc.get(key)
            .and_then(Item::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("`{key}` is required and must be a string"))
    };
    let name = string("name")?;
    if !valid_name(&name) {
        return Err(Error::InvalidName(name).to_string());
    }
    let version = string("version")?;
    if version.trim().is_empty() {
        return Err("`version` must not be empty".to_owned());
    }
    let api = doc
        .get("api")
        .and_then(Item::as_integer)
        .ok_or_else(|| "`api` is required and must be an integer".to_owned())?;
    if api != API_VERSION {
        return Err(format!(
            "`api` = {api}, but this pixel speaks plugin API {API_VERSION}"
        ));
    }
    let description = string("description")?;
    let run = string("run")?;
    run_is_relative(&run)?;
    let capabilities = parse_capabilities(&doc)?;
    let network = match doc.get("network") {
        None => false,
        Some(item) => item
            .as_bool()
            .ok_or_else(|| "`network` must be true or false".to_owned())?,
    };
    Ok(Manifest {
        name,
        version,
        api,
        description,
        run,
        capabilities,
        network,
    })
}

fn parse_capabilities(doc: &DocumentMut) -> Result<Vec<Capability>, String> {
    let list = doc
        .get("capabilities")
        .and_then(Item::as_array)
        .ok_or_else(|| "`capabilities` is required and must be an array of strings".to_owned())?;
    let mut out = Vec::new();
    for entry in list {
        let text = entry
            .as_str()
            .ok_or_else(|| "`capabilities` must hold strings only".to_owned())?;
        let capability = Capability::parse(text).ok_or_else(|| {
            format!("unknown capability \"{text}\": plugin API 1 knows only \"command\"")
        })?;
        if !out.contains(&capability) {
            out.push(capability);
        }
    }
    Ok(out)
}

/// `run` is a path inside the plugin directory: never absolute, never `..`.
/// The resolved path is checked again at run time (symlinks); this is the
/// early, readable refusal.
fn run_is_relative(run: &str) -> Result<(), String> {
    if run.is_empty() {
        return Err("`run` must not be empty".to_owned());
    }
    let path = Path::new(run);
    let escapes = path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if escapes {
        return Err(format!(
            "`run` = \"{run}\" must be a relative path inside the plugin directory"
        ));
    }
    Ok(())
}

/// Read `<dir>/pixel-plugin.toml`. `Ok(None)` when the file is absent (the
/// directory is not a plugin); a manifest that exists but is invalid, or
/// names another plugin than its directory, is an error.
pub fn load(dir: &Path, expected_name: Option<&str>) -> Result<Option<Manifest>, Error> {
    let path = dir.join(MANIFEST_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("read {}", path.display()), e)),
    };
    let manifest = parse(&text).map_err(|message| Error::Manifest {
        path: path.clone(),
        message,
    })?;
    if let Some(expected) = expected_name
        && manifest.name != expected
    {
        return Err(Error::Manifest {
            path,
            message: format!(
                "`name` = \"{}\" does not match its directory \"{expected}\"",
                manifest.name
            ),
        });
    }
    Ok(Some(manifest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
name = "flow"
version = "1.2.3"
api = 1
description = "Replay flows"
run = "bin/pixel-flow"
capabilities = ["command"]
network = true
"#;

    #[test]
    fn a_complete_manifest_parses_to_every_field() {
        let m = parse(GOOD).unwrap();
        assert_eq!(
            m,
            Manifest {
                name: "flow".into(),
                version: "1.2.3".into(),
                api: 1,
                description: "Replay flows".into(),
                run: "bin/pixel-flow".into(),
                capabilities: vec![Capability::Command],
                network: true,
            }
        );
        assert!(m.has_command());
    }

    #[test]
    fn network_defaults_to_false_and_capabilities_may_be_empty() {
        let text = GOOD
            .replace("network = true", "")
            .replace("[\"command\"]", "[]");
        let m = parse(&text).unwrap();
        assert!(!m.network);
        assert!(m.capabilities.is_empty());
        assert!(!m.has_command());
    }

    #[test]
    fn duplicate_capabilities_collapse() {
        let text = GOOD.replace("[\"command\"]", "[\"command\", \"command\"]");
        assert_eq!(
            parse(&text).unwrap().capabilities,
            vec![Capability::Command]
        );
    }

    #[test]
    fn each_required_field_is_named_when_missing() {
        for key in [
            "name",
            "version",
            "api",
            "description",
            "run",
            "capabilities",
        ] {
            let text: String = GOOD
                .lines()
                .filter(|l| !l.starts_with(&format!("{key} =")))
                .collect::<Vec<_>>()
                .join("\n");
            let err = parse(&text).unwrap_err();
            assert!(err.contains(&format!("`{key}`")), "{key}: {err}");
        }
    }

    #[test]
    fn only_api_one_is_accepted() {
        for api in ["0", "2", "\"1\""] {
            let err = parse(&GOOD.replace("api = 1", &format!("api = {api}"))).unwrap_err();
            assert!(err.contains("`api`"), "{api}: {err}");
        }
        let err = parse(&GOOD.replace("api = 1", "api = 2")).unwrap_err();
        assert!(err.contains("`api` = 2") && err.contains("API 1"), "{err}");
    }

    #[test]
    fn an_unknown_capability_is_refused_by_name() {
        let err =
            parse(&GOOD.replace("[\"command\"]", "[\"command\", \"chain-step\"]")).unwrap_err();
        assert!(err.contains("\"chain-step\""), "{err}");
        let err = parse(&GOOD.replace("[\"command\"]", "[1]")).unwrap_err();
        assert!(err.contains("strings only"), "{err}");
    }

    #[test]
    fn run_must_stay_relative_and_inside() {
        for run in ["/bin/sh", "../x", "a/../../x", "./ok/../../x"] {
            let err = parse(&GOOD.replace("bin/pixel-flow", run)).unwrap_err();
            assert!(err.contains("relative path"), "{run}: {err}");
        }
        assert!(parse(&GOOD.replace("bin/pixel-flow", "./run.sh")).is_ok());
        let err = parse(&GOOD.replace("bin/pixel-flow", "")).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn a_blank_version_and_a_non_boolean_network_are_refused() {
        let err = parse(&GOOD.replace("1.2.3", "  ")).unwrap_err();
        assert!(err.contains("`version`"), "{err}");
        let err = parse(&GOOD.replace("network = true", "network = \"yes\"")).unwrap_err();
        assert!(err.contains("`network`"), "{err}");
    }

    #[test]
    fn invalid_toml_is_reported_as_such() {
        assert!(parse("name = ").unwrap_err().contains("not valid TOML"));
    }

    #[test]
    fn names_are_directory_safe() {
        for good in ["flow", "a", "0", "plan-rollback", "a_b", &"a".repeat(64)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "Flow",
            "-x",
            "_x",
            "a b",
            "a/b",
            "..",
            "a.b",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        let err = parse(&GOOD.replace("\"flow\"", "\"../x\"")).unwrap_err();
        assert!(err.contains("invalid plugin name"), "{err}");
    }

    #[test]
    fn load_distinguishes_absent_invalid_and_misnamed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), None).unwrap(), None);

        std::fs::write(dir.path().join(MANIFEST_FILE), GOOD).unwrap();
        assert_eq!(
            load(dir.path(), Some("flow")).unwrap().unwrap().name,
            "flow"
        );
        let err = load(dir.path(), Some("other")).unwrap_err().to_string();
        assert!(
            err.contains("does not match its directory \"other\""),
            "{err}"
        );

        std::fs::write(dir.path().join(MANIFEST_FILE), "api = 9").unwrap();
        let err = load(dir.path(), None).unwrap_err().to_string();
        assert!(err.contains(MANIFEST_FILE), "{err}");
    }
}
