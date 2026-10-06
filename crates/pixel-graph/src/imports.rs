// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Import-spec → file resolution. Best-effort per language family;
//! `None` is an acceptable answer (the import simply stays unresolved).

use crate::extract::{is_binstub_candidate, lang_of};

pub mod ruby;

/// Resolve `spec` (as written in `importer_rel`) to a repo-relative file
/// path from `all_files`, or `None` when no confident match exists. A Ruby
/// importer sees its projects' default load roots only; the build reads
/// their manifests through [`resolve_import_in`].
pub fn resolve_import(spec: &str, importer_rel: &str, all_files: &[String]) -> Option<String> {
    resolve_import_in(
        spec,
        importer_rel,
        all_files,
        &ruby::Projects::from_paths(all_files),
    )
}

/// [`resolve_import`] with the Ruby projects of the tree, which `Ruby`
/// requires resolve against (`ruby::Projects::load`). `projects` must
/// describe `all_files`.
pub fn resolve_import_in(
    spec: &str,
    importer_rel: &str,
    all_files: &[String],
    projects: &ruby::Projects,
) -> Option<String> {
    // A binstub has imports only once its shebang made it Ruby.
    if is_binstub_candidate(importer_rel) {
        return projects.resolve(spec, importer_rel);
    }
    match lang_of(importer_rel)? {
        "ruby" => projects.resolve(spec, importer_rel),
        "ts" | "tsx" | "js" => resolve_js(spec, importer_rel, all_files),
        "rust" => resolve_rust(spec, importer_rel, all_files),
        "python" => resolve_python(spec, importer_rel, all_files),
        "go" => resolve_go(spec, all_files),
        "java" => resolve_java(spec, all_files),
        _ => None,
    }
}

fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Join + normalize `.`/`..` segments into a clean repo-relative path.
fn normalize(base_dir: &str, rel: &str) -> String {
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

fn contains(all_files: &[String], candidate: &str) -> bool {
    all_files.iter().any(|f| f == candidate)
}

fn first_suffix_match(all_files: &[String], suffix: &str) -> Option<String> {
    // A suffix is fallback evidence, not a scope: multiple matches must remain
    // unresolved rather than depend on directory traversal order.
    let tail = format!("/{suffix}");
    let mut matches = all_files
        .iter()
        .filter(|f| f.as_str() == suffix || f.ends_with(&tail));
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first.clone())
}

/// Directory-suffix match used by the package imports (Go, Java wildcard):
/// the smallest `ext` path inside the only directory whose path suffix is
/// `suffix` (or that is `suffix` itself). Several files in one directory are
/// one package and count as one candidate; a suffix shared by several
/// directories names no single package, so it stays unresolved rather than
/// follow directory traversal order. The smallest path, not the first
/// listed, because the full build lists files in walk order and the
/// incremental update in store order, and both must store the same row.
fn first_suffix_dir_match(all_files: &[String], suffix: &str, ext: &str) -> Option<String> {
    let tail = format!("/{suffix}");
    let mut dirs: Vec<&str> = Vec::new();
    for f in all_files {
        if !f.ends_with(ext) {
            continue;
        }
        let dir = dir_of(f);
        if dir == suffix || dir.ends_with(&tail) {
            dirs.push(dir);
        }
    }
    let first_dir = dirs.first()?;
    if dirs.iter().any(|d| d != first_dir) {
        return None;
    }
    all_files
        .iter()
        .filter(|f| f.ends_with(ext) && dir_of(f) == *first_dir)
        .min()
        .cloned()
}

// --- TS / JS --------------------------------------------------------------

const JS_EXTS: [&str; 6] = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];

fn resolve_js(spec: &str, importer_rel: &str, all_files: &[String]) -> Option<String> {
    if !spec.starts_with('.') {
        return None; // bare specifier: package import, out of scope
    }
    let base = normalize(dir_of(importer_rel), spec);
    if lang_of(&base).is_some() && contains(all_files, &base) {
        return Some(base);
    }
    for ext in JS_EXTS {
        let cand = format!("{base}{ext}");
        if contains(all_files, &cand) {
            return Some(cand);
        }
    }
    for ext in JS_EXTS {
        let cand = format!("{base}/index{ext}");
        if contains(all_files, &cand) {
            return Some(cand);
        }
    }
    None
}

// --- Rust -----------------------------------------------------------------

fn resolve_rust(spec: &str, importer_rel: &str, all_files: &[String]) -> Option<String> {
    // Strip alias / braces: `crate::a::b as c`, `crate::a::{b, c}`.
    let spec = spec.split(" as ").next().unwrap_or(spec);
    let spec = spec
        .split('{')
        .next()
        .unwrap_or(spec)
        .trim_end_matches("::")
        .trim();
    let segs: Vec<&str> = spec
        .split("::")
        .filter(|s| !s.is_empty() && *s != "*")
        .collect();
    if segs.is_empty() {
        return None;
    }
    let importer_dir = dir_of(importer_rel);
    let (roots, segs): (Vec<String>, &[&str]) = match segs[0] {
        "crate" => (vec!["src".to_string(), String::new()], &segs[1..]),
        "self" => (vec![importer_dir.to_string()], &segs[1..]),
        "super" => {
            let mut dir = importer_dir.to_string();
            let mut rest = &segs[1..];
            loop {
                dir = dir_of(&dir).to_string();
                if rest.first() == Some(&"super") {
                    rest = &rest[1..];
                } else {
                    break;
                }
            }
            (vec![dir], rest)
        }
        "std" | "core" | "alloc" => return None,
        _ => (
            vec!["src".to_string(), String::new(), importer_dir.to_string()],
            &segs[..],
        ),
    };
    if segs.is_empty() {
        return None;
    }
    // Try longest module path first, dropping trailing item segments.
    for k in (1..=segs.len()).rev() {
        let modpath = segs[..k].join("/");
        for root in &roots {
            let base = if root.is_empty() {
                modpath.clone()
            } else {
                format!("{root}/{modpath}")
            };
            for cand in [format!("{base}.rs"), format!("{base}/mod.rs")] {
                if contains(all_files, &cand) {
                    return Some(cand);
                }
            }
        }
        // Fall back to a unique suffix match anywhere in the tree.
        if let Some(hit) = first_suffix_match(all_files, &format!("{modpath}.rs")) {
            return Some(hit);
        }
    }
    None
}

// --- Python ---------------------------------------------------------------

fn resolve_python(spec: &str, importer_rel: &str, all_files: &[String]) -> Option<String> {
    let dots = spec.chars().take_while(|&c| c == '.').count();
    let rest = &spec[dots..];
    let segs: Vec<&str> = rest.split('.').filter(|s| !s.is_empty()).collect();
    if dots > 0 {
        // Relative import: one dot = importer's package, each extra dot = up one.
        let mut dir = dir_of(importer_rel).to_string();
        for _ in 1..dots {
            dir = dir_of(&dir).to_string();
        }
        let base = if segs.is_empty() {
            dir.clone()
        } else if dir.is_empty() {
            segs.join("/")
        } else {
            format!("{}/{}", dir, segs.join("/"))
        };
        for cand in [format!("{base}.py"), format!("{base}/__init__.py")] {
            if contains(all_files, &cand) {
                return Some(cand);
            }
        }
        return None;
    }
    if segs.is_empty() {
        return None;
    }
    for k in (1..=segs.len()).rev() {
        let modpath = segs[..k].join("/");
        for cand in [format!("{modpath}.py"), format!("{modpath}/__init__.py")] {
            if contains(all_files, &cand) {
                return Some(cand);
            }
            if let Some(hit) = first_suffix_match(all_files, &cand) {
                return Some(hit);
            }
        }
    }
    None
}

// --- Go -------------------------------------------------------------------

fn resolve_go(spec: &str, all_files: &[String]) -> Option<String> {
    // Match a directory whose path suffix equals the import path (or its
    // trailing segments); its smallest .go path stands for the package. A suffix
    // shared by several directories names no single package (see
    // `first_suffix_dir_match`).
    let segs: Vec<&str> = spec.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    for take in (1..=segs.len()).rev() {
        let suffix = segs[segs.len() - take..].join("/");
        if let Some(f) = first_suffix_dir_match(all_files, &suffix, ".go") {
            return Some(f);
        }
    }
    None
}

// --- Java -----------------------------------------------------------------

fn resolve_java(spec: &str, all_files: &[String]) -> Option<String> {
    if let Some(pkg) = spec.strip_suffix(".*") {
        let dir_suffix = pkg.replace('.', "/");
        return first_suffix_dir_match(all_files, &dir_suffix, ".java");
    }
    let path = format!("{}.java", spec.replace('.', "/"));
    first_suffix_match(all_files, &path).or_else(|| {
        // Fall back: match on the class file name alone.
        let class_file = format!("{}.java", spec.rsplit('.').next()?);
        first_suffix_match(all_files, &class_file)
    })
}
