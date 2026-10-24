// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Import spec → file resolution for the name-resolved languages. Best effort
//! per language: `None` is an acceptable answer, and the import then stays
//! open. Lifted from Pixel's `pixel-graph` `imports.rs`.
//!
//! A resolution says whether it is *proved*: the spec names exactly that
//! path (a relative import, a path that exists as written). A suffix match,
//! however unique, is evidence and not proof, so what it resolves is `Likely`.

use rustc_hash::FxHashSet as HashSet;

/// The paths of the language's files in one generation.
pub(super) struct Files<'a> {
    set: HashSet<&'a str>,
    /// The same paths in order, so a scan never depends on listing order.
    sorted: Vec<&'a str>,
}

impl<'a> Files<'a> {
    pub(super) fn new(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut sorted: Vec<&str> = paths.into_iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        Files {
            set: sorted.iter().copied().collect(),
            sorted,
        }
    }

    pub(super) fn contains(&self, path: &str) -> bool {
        self.set.contains(path)
    }

    fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.sorted.iter().copied()
    }
}

/// The file an import names, and whether the spec proves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Resolved {
    pub path: String,
    pub proved: bool,
}

impl Resolved {
    fn proved(path: String) -> Option<Self> {
        Some(Resolved { path, proved: true })
    }

    fn guessed(path: String) -> Option<Self> {
        Some(Resolved {
            path,
            proved: false,
        })
    }
}

/// The directory of a repo-relative path, empty for a top-level file.
pub(super) fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Joins `rel` to `base_dir` and normalises its `.` and `..` segments into a
/// clean repo-relative path.
pub(super) fn normalize(base_dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The extensions a TS/JS import may leave out, in resolution order.
const JS_EXTS: [&str; 8] = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

/// A TS/JS relative spec (`./a`, `../b/c`) to the file it names: the path
/// itself, the path plus an extension, or the directory's `index`. A bare
/// specifier (a package, an alias) names no file of the repository.
pub(super) fn resolve_ts(spec: &str, importer: &str, files: &Files) -> Option<Resolved> {
    resolve_ts_path(spec, importer, files).and_then(Resolved::proved)
}

fn resolve_ts_path(spec: &str, importer: &str, files: &Files) -> Option<String> {
    if !spec.starts_with('.') {
        return None;
    }
    let base = normalize(dir_of(importer), spec);
    if files.contains(&base) {
        return Some(base);
    }
    for ext in JS_EXTS {
        let candidate = format!("{base}{ext}");
        if files.contains(&candidate) {
            return Some(candidate);
        }
    }
    // `import "./a.js"` in TypeScript names `a.ts`.
    if let Some(stem) = base
        .strip_suffix(".js")
        .or_else(|| base.strip_suffix(".jsx"))
    {
        for ext in [".ts", ".tsx"] {
            let candidate = format!("{stem}{ext}");
            if files.contains(&candidate) {
                return Some(candidate);
            }
        }
    }
    for ext in JS_EXTS {
        let candidate = format!("{base}/index{ext}");
        if files.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// The only file that is `suffix` or ends in `/suffix`: a suffix is fallback
/// evidence, not a scope, so several matches stay unresolved rather than
/// depend on listing order.
fn first_suffix_match(files: &Files, suffix: &str) -> Option<String> {
    let tail = format!("/{suffix}");
    let mut matches = files.iter().filter(|f| *f == suffix || f.ends_with(&tail));
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first.to_owned())
}

/// The smallest `ext` path inside the only directory whose path suffix is
/// `suffix` (or that is `suffix`). Several files of one directory are one
/// package and count as one candidate; a suffix shared by several
/// directories names no single package. The smallest path, not the first
/// listed, because the full build and the incremental update list files in
/// different orders and must store the same row.
fn first_suffix_dir_match(files: &Files, suffix: &str, ext: &str) -> Option<String> {
    let tail = format!("/{suffix}");
    let mut found: Option<&str> = None;
    for f in files.iter().filter(|f| f.ends_with(ext)) {
        let dir = dir_of(f);
        if dir == suffix || dir.ends_with(&tail) {
            match found {
                Some(first) if first != dir => return None,
                Some(_) => {}
                None => found = Some(dir),
            }
        }
    }
    let dir = found?;
    // `iter` is sorted, so the first file of the directory is the smallest.
    files
        .iter()
        .find(|f| f.ends_with(ext) && dir_of(f) == dir)
        .map(str::to_owned)
}

/// A Python import: a relative one names a path exactly; an absolute one is
/// proved only when the module path exists as written from the root.
pub(super) fn resolve_python(spec: &str, importer: &str, files: &Files) -> Option<Resolved> {
    let dots = spec.chars().take_while(|&c| c == '.').count();
    let segs: Vec<&str> = spec[dots..].split('.').filter(|s| !s.is_empty()).collect();
    if dots > 0 {
        // One dot is the importer's package, each extra dot one level up.
        let mut dir = dir_of(importer).to_owned();
        for _ in 1..dots {
            dir = dir_of(&dir).to_owned();
        }
        let base = match (segs.is_empty(), dir.is_empty()) {
            (true, _) => dir,
            (false, true) => segs.join("/"),
            (false, false) => format!("{dir}/{}", segs.join("/")),
        };
        return [format!("{base}.py"), format!("{base}/__init__.py")]
            .into_iter()
            .find(|candidate| files.contains(candidate))
            .and_then(Resolved::proved);
    }
    if segs.is_empty() {
        return None;
    }
    for k in (1..=segs.len()).rev() {
        let modpath = segs[..k].join("/");
        for candidate in [format!("{modpath}.py"), format!("{modpath}/__init__.py")] {
            if files.contains(&candidate) {
                return Resolved::proved(candidate);
            }
            if let Some(hit) = first_suffix_match(files, &candidate) {
                return Resolved::guessed(hit);
            }
        }
    }
    None
}

/// A Go import path: the directory whose path ends with the import path (or
/// its trailing segments) is the package. Always a guess: a module path is
/// not a directory of the repository.
pub(super) fn resolve_go(spec: &str, _importer: &str, files: &Files) -> Option<Resolved> {
    let segs: Vec<&str> = spec.split('/').filter(|s| !s.is_empty()).collect();
    for take in (1..=segs.len()).rev() {
        let suffix = segs[segs.len() - take..].join("/");
        if let Some(found) = first_suffix_dir_match(files, &suffix, ".go") {
            return Resolved::guessed(found);
        }
    }
    None
}

/// A Java import: the class file under the package path, or a package
/// directory for a wildcard. A guess, as source roots are not known.
pub(super) fn resolve_java(spec: &str, _importer: &str, files: &Files) -> Option<Resolved> {
    if let Some(package) = spec.strip_suffix(".*") {
        return first_suffix_dir_match(files, &package.replace('.', "/"), ".java")
            .and_then(Resolved::guessed);
    }
    let path = format!("{}.java", spec.replace('.', "/"));
    first_suffix_match(files, &path)
        .or_else(|| {
            let class_file = format!("{}.java", spec.rsplit('.').next()?);
            first_suffix_match(files, &class_file)
        })
        .and_then(Resolved::guessed)
}

/// The generic languages carry no import resolver: an import stays open.
pub(super) fn resolve_none(_spec: &str, _importer: &str, _files: &Files) -> Option<Resolved> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(spec: &str, importer: &str, files: &[&str]) -> Option<String> {
        resolve_ts(spec, importer, &Files::new(files.iter().copied())).map(|r| r.path)
    }

    #[test]
    fn a_relative_spec_takes_the_first_extension_that_exists() {
        assert_eq!(
            resolve("./a", "src/main.ts", &["src/a.js", "src/a.ts"]),
            Some("src/a.ts".to_owned())
        );
        assert_eq!(
            resolve("../lib/b", "src/app/main.ts", &["src/lib/b.tsx"]),
            Some("src/lib/b.tsx".to_owned())
        );
    }

    #[test]
    fn a_directory_resolves_to_its_index() {
        assert_eq!(
            resolve("./util", "src/main.ts", &["src/util/index.ts"]),
            Some("src/util/index.ts".to_owned())
        );
    }

    #[test]
    fn a_file_named_with_its_extension_resolves_to_itself() {
        assert_eq!(
            resolve("./a.mjs", "src/main.js", &["src/a.mjs"]),
            Some("src/a.mjs".to_owned())
        );
    }

    #[test]
    fn a_js_extension_in_a_typescript_import_names_the_ts_file() {
        assert_eq!(
            resolve("./a.js", "src/main.ts", &["src/a.ts"]),
            Some("src/a.ts".to_owned())
        );
    }

    #[test]
    fn a_bare_specifier_or_a_missing_file_stays_open() {
        assert_eq!(resolve("react", "src/main.ts", &["src/react.ts"]), None);
        assert_eq!(resolve("@/lib/a", "src/main.ts", &["src/lib/a.ts"]), None);
        assert_eq!(resolve("./gone", "src/main.ts", &["src/a.ts"]), None);
    }

    #[test]
    fn a_spec_that_climbs_above_the_root_is_clamped_not_wrapped() {
        assert_eq!(
            resolve("../../x", "a.ts", &["x.ts"]),
            Some("x.ts".to_owned())
        );
        assert_eq!(normalize("a/b", "../../../c"), "c");
    }

    fn resolve_with(
        resolver: fn(&str, &str, &Files) -> Option<Resolved>,
        spec: &str,
        importer: &str,
        files: &[&str],
    ) -> Option<(String, bool)> {
        resolver(spec, importer, &Files::new(files.iter().copied())).map(|r| (r.path, r.proved))
    }

    #[test]
    fn a_python_relative_import_is_proved_and_a_suffix_match_is_not() {
        let files = [
            "pkg/a.py",
            "pkg/sub/__init__.py",
            "vendor/pkg/a.py",
            "lib/b.py",
        ];
        assert_eq!(
            resolve_with(resolve_python, ".a", "pkg/main.py", &files),
            Some(("pkg/a.py".to_owned(), true))
        );
        assert_eq!(
            resolve_with(resolve_python, ".sub", "pkg/main.py", &files),
            Some(("pkg/sub/__init__.py".to_owned(), true))
        );
        assert_eq!(
            resolve_with(resolve_python, "..a", "pkg/sub/m.py", &files),
            Some(("pkg/a.py".to_owned(), true))
        );
        assert_eq!(
            resolve_with(resolve_python, "lib.b", "pkg/main.py", &files),
            Some(("lib/b.py".to_owned(), true))
        );
        assert_eq!(
            resolve_with(resolve_python, "b", "pkg/main.py", &files),
            Some(("lib/b.py".to_owned(), false))
        );
        assert_eq!(
            resolve_with(resolve_python, "a", "pkg/main.py", &files),
            None,
            "two files end in `a.py`: no single module"
        );
    }

    #[test]
    fn a_go_import_is_the_one_directory_that_ends_with_its_path() {
        let files = ["cmd/x/main.go", "internal/util/b.go", "internal/util/a.go"];
        assert_eq!(
            resolve_with(
                resolve_go,
                "example.com/m/internal/util",
                "cmd/x/main.go",
                &files
            ),
            Some(("internal/util/a.go".to_owned(), false))
        );
        assert_eq!(
            resolve_with(
                resolve_go,
                "util",
                "cmd/x/main.go",
                &["a/util/x.go", "b/util/y.go"]
            ),
            None
        );
    }

    #[test]
    fn a_java_import_names_a_class_file_or_a_package_directory() {
        let files = [
            "src/main/java/com/acme/Foo.java",
            "src/main/java/com/acme/util/Bar.java",
            "src/main/java/com/acme/util/Baz.java",
        ];
        assert_eq!(
            resolve_with(resolve_java, "com.acme.Foo", "x/Main.java", &files),
            Some(("src/main/java/com/acme/Foo.java".to_owned(), false))
        );
        assert_eq!(
            resolve_with(resolve_java, "com.acme.util.*", "x/Main.java", &files),
            Some(("src/main/java/com/acme/util/Bar.java".to_owned(), false))
        );
        assert_eq!(
            resolve_with(resolve_java, "java.util.List", "x/Main.java", &files),
            None
        );
    }
}
