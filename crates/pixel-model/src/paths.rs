// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Facts about a repository path that every crate agrees on.

/// True when `path` looks like a test file (`specs/graph.md`): a `test`,
/// `tests`, `spec`, `specs` or `__tests__` directory, a `*_test.*`,
/// `*_spec.*`, `test_*.*`, `*Test.*` or `*Tests.*` file name, or a
/// `*.test.*` / `*.spec.*` one. Case-insensitive except for the CamelCase
/// suffixes, which need their capital (`Latest.java` is not a test).
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let components: Vec<&str> = lower.split('/').collect();
    let directories = &components[..components.len() - 1];
    if directories
        .iter()
        .any(|c| matches!(*c, "test" | "tests" | "spec" | "specs" | "__tests__"))
    {
        return true;
    }
    let file = components.last().copied().unwrap_or_default();
    let parts: Vec<&str> = file.split('.').collect();
    let stem = parts.first().copied().unwrap_or_default();
    if stem.ends_with("_test") || stem.ends_with("_spec") || stem.starts_with("test_") {
        return true;
    }
    if parts.len() >= 3 && matches!(parts[parts.len() - 2], "spec" | "test") {
        return true;
    }
    let original = path.rsplit('/').next().unwrap_or(path);
    let original_stem = original.split('.').next().unwrap_or(original);
    original_stem.len() > 4 && (original_stem.ends_with("Test") || original_stem.ends_with("Tests"))
}

/// Path-safety predicate shared by every consumer that surfaces an indexed
/// file's contents (the daemon's reads, the brief):
/// a tracked, non-ignored `.env` / `id_rsa` / `*.pem` is a
/// place the user almost certainly does not want echoed back. Eligibility
/// is metadata-only — content stays for the execution side — and a positive
/// answer here never claims to detect every secret.
///
/// Suffix/name based by design.
pub fn credential_path(path: &std::path::Path) -> bool {
    if path.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("secrets"))
    }) {
        return true;
    }
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    name.starts_with(".env")
        || name.ends_with(".env")
        || name.starts_with("credentials.")
        // `~/.aws/credentials`, `~/.netrc`, git's credential store.
        || matches!(
            name.as_str(),
            "credentials" | ".netrc" | "_netrc" | ".git-credentials"
        )
        // Extensionless too: `secret_token` or `client_secret` holds the
        // value itself as often as `app_secret.yaml` does.
        || name.contains("secret")
        || name == "serviceaccountkey.json"
        || name.ends_with("-credentials.json")
        || [
            ".pem",
            ".key",
            ".p12",
            ".pfx",
            ".jks",
            ".keystore",
            ".truststore",
            "_rsa",
            "_dsa",
            "_ecdsa",
            "_ed25519",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_test_directory_or_a_test_file_name_is_a_test() {
        for path in [
            "src/test/foo.rs",
            "crates/x/tests/cli.rs",
            "spec/models/user_spec.rb",
            "app/__tests__/a.js",
            "src/foo_test.go",
            "pkg/test_util.py",
            "web/a.spec.ts",
            "web/a.test.tsx",
            "src/FooTest.java",
            "src/FooTests.cs",
            "Tests/Foo.swift",
        ] {
            assert!(is_test_path(path), "{path}");
        }
    }

    #[test]
    fn a_name_that_only_looks_like_one_is_not() {
        for path in [
            "src/latest.rs",
            "src/Latest.java",
            "src/attest.rs",
            "src/contest/a.rs",
            "test.rs",
            "src/spec.rs",
            "docs/testing.md",
            "Test.java",
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }

    #[test]
    fn the_file_name_of_a_path_in_a_test_directory_need_not_say_so() {
        assert!(is_test_path("tests/fixtures/data.json"));
        assert!(!is_test_path("test"), "a file named `test` is no directory");
    }

    /// `credential_path` is the single predicate every reader of file contents
    /// shares. Every name
    /// listed here must have a sibling test that would FAIL if the
    /// predicate silently grew a typo or lost a suffix — a positive answer
    /// here decides whether an `.env` byte reaches the caller's stdout.
    #[test]
    fn credential_path_matches_every_shape_in_the_predicate() {
        let positives = [
            ".env",
            ".env.local",
            "config/.env",
            "production.env",
            "credentials.json",
            "credentials.prod.json",
            "config/secrets/db.json",
            "nested/secrets/x.json",
            "serviceAccountKey.json",
            "service-account-credentials.json",
            "tls.pem",
            "tls.key",
            "keystore.p12",
            "trust.pfx",
            "trust.jks",
            "server.keystore",
            "client.truststore",
            "id_rsa",
            "id_dsa",
            "id_ecdsa",
            "id_ed25519",
            "deploy_key_rsa",
            "config/app_secret.yaml",
            // Extensionless secret-named files carry the value itself.
            "secret_token",
            "deploy/client_secret",
            ".aws/credentials",
            "credentials",
            ".netrc",
            "home/_netrc",
            ".git-credentials",
            "SECRET",
        ];
        for path in positives {
            assert!(
                credential_path(Path::new(path)),
                "expected `credential_path({path:?}) == true`"
            );
        }
        let negatives = [
            "src/main.rs",
            "src/credentials_helper.rs",
            "src/credentials_store/mod.rs",
            "netrc.md",
            ".git-credentials-helper.sh",
            "Cargo.toml",
            "config.yaml",
            "deploy/notes.md",
            "keymap.json",
            "src/server.pem.bak",
        ];
        for path in negatives {
            assert!(
                !credential_path(Path::new(path)),
                "expected `credential_path({path:?}) == false`"
            );
        }
    }
}
