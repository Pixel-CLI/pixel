// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby `require` and `require_relative` resolution, bounded by the Ruby
//! projects the repository declares.
//!
//! A directory holding a `Gemfile` or a `*.gemspec` is a project root. A
//! file belongs to the nearest one above it, and its `require "a/b"`
//! searches only that project's load roots: the `require_paths` of its own
//! gemspec (`lib` by default), and those of the local path gems its
//! `Gemfile` declares (`gem "x", path: "engines/x"`). A require whose first
//! segment names an external gem of the project — declared in its
//! `Gemfile` or listed, transitive dependencies included, in its
//! `Gemfile.lock` — is that gem's file, never a local one, so `require
//! "rails/generators"` cannot land on a local `lib/rails/generators.rb`. A
//! miss stays unresolved: nothing escapes into a sibling project, and no
//! file is matched by suffix.
//!
//! The manifests are read as data, never evaluated: literal `gem`, `path`
//! and `gemspec` calls of the Gemfile, literal `name` and `require_paths`
//! assignments of a gemspec, the spec lines of a lockfile. Anything else
//! (a computed path, `eval_gemfile`, `$LOAD_PATH` edits) is not modelled.
//! <https://bundler.io/v2.5/man/gemfile.5.html>
//! <https://guides.rubygems.org/specification-reference/#require_paths=>

use std::collections::{HashMap, HashSet};
use std::path::Path;

use tree_sitter::Node;

/// One Ruby project: its root directory (`""` for the repository root),
/// where its `require`s search, and the require names that belong to gems.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Project {
    pub root: String,
    /// Directories searched by `require`, in order: the project's own
    /// require paths, then each local path gem's.
    pub load_roots: Vec<String>,
    /// Require-path prefixes (whole segments) that belong to an external
    /// gem: `rails`, `net/http` for `net-http`.
    pub external: HashSet<String>,
}

/// The Ruby projects of a tree, and its file list.
#[derive(Debug, Default)]
pub struct Projects {
    /// Deepest root first, so the first containing one is the nearest.
    projects: Vec<Project>,
    files: HashSet<String>,
}

/// What a project's manifests declare, before it becomes a [`Project`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    /// Gem names the Gemfile declares from a remote source, with the
    /// `require:` name when it is a literal string.
    pub gems: Vec<(String, Option<String>)>,
    /// Local path gems: directory relative to the project root.
    pub path_gems: Vec<String>,
    /// The gemspec's literal `name`, if any.
    pub name: Option<String>,
    /// The gemspec's literal `require_paths`, if any.
    pub require_paths: Option<Vec<String>>,
}

fn dir_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn join(dir: &str, rel: &str) -> String {
    super::normalize(dir, rel)
}

fn is_gemspec(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|f| f.ends_with(".gemspec") && f.len() > ".gemspec".len())
}

fn is_gemfile(path: &str) -> bool {
    path == "Gemfile" || path.ends_with("/Gemfile")
}

/// True iff a change to `path` can change how Ruby requires resolve.
pub fn is_manifest(path: &str) -> bool {
    is_gemfile(path)
        || is_gemspec(path)
        || path == "Gemfile.lock"
        || path.ends_with("/Gemfile.lock")
}

impl Projects {
    /// The projects of `files` with only their roots and default `lib` load
    /// roots: what is known without reading a manifest.
    pub fn from_paths(files: &[String]) -> Self {
        Self::build(files, |_| None)
    }

    /// The projects of `files`, reading each project's `Gemfile`,
    /// `*.gemspec` and `Gemfile.lock` under `root`.
    pub fn load(root: &Path, files: &[String]) -> Self {
        Self::build(files, |rel| std::fs::read(root.join(rel)).ok())
    }

    fn build(files: &[String], read: impl Fn(&str) -> Option<Vec<u8>>) -> Self {
        let mut roots: HashMap<String, Manifest> = HashMap::new();
        for file in files {
            if is_gemfile(file) || is_gemspec(file) {
                roots.entry(dir_of(file).to_string()).or_default();
            }
        }
        let mut gemspecs: Vec<&String> = files.iter().filter(|f| is_gemspec(f)).collect();
        gemspecs.sort();
        for (dir, manifest) in &mut roots {
            let gemfile = join(dir, "Gemfile");
            if let Some(content) = read(&gemfile) {
                read_gemfile(&content, manifest);
            }
            for spec in gemspecs.iter().filter(|f| dir_of(f) == dir.as_str()) {
                if let Some(content) = read(spec) {
                    read_gemspec(&content, manifest);
                }
            }
        }
        let mut locks: HashMap<String, Vec<String>> = HashMap::new();
        for dir in roots.keys() {
            if let Some(content) = read(&join(dir, "Gemfile.lock")) {
                locks.insert(dir.clone(), lock_gems(&String::from_utf8_lossy(&content)));
            }
        }
        let own_roots = |dir: &str, manifest: Option<&Manifest>| -> Vec<String> {
            let paths = manifest
                .and_then(|m| m.require_paths.clone())
                .unwrap_or_else(|| vec!["lib".to_string()]);
            paths.iter().map(|p| join(dir, p)).collect()
        };
        let mut projects = Vec::new();
        for (dir, manifest) in &roots {
            let mut load_roots = own_roots(dir, Some(manifest));
            let mut local: HashSet<String> = manifest.name.iter().cloned().collect();
            for gem_dir in &manifest.path_gems {
                let gem_root = join(dir, gem_dir);
                let gem = roots.get(&gem_root);
                load_roots.extend(own_roots(&gem_root, gem));
                if let Some(name) = gem.and_then(|g| g.name.clone()) {
                    local.insert(name);
                }
                if let Some(last) = gem_root.rsplit('/').next() {
                    local.insert(last.to_string());
                }
            }
            let mut external = HashSet::new();
            let declared = manifest
                .gems
                .iter()
                .map(|(gem, require)| (gem.clone(), require.clone()));
            let locked = locks
                .get(dir)
                .into_iter()
                .flatten()
                .map(|gem| (gem.clone(), None));
            for (gem, require) in declared.chain(locked) {
                if local.contains(&gem) {
                    continue;
                }
                external.extend(require_names(&gem));
                if let Some(first) = require.as_deref().and_then(|r| r.split('/').next()) {
                    external.insert(first.to_string());
                }
            }
            for name in &local {
                for alias in require_names(name) {
                    external.remove(&alias);
                }
            }
            load_roots.dedup();
            projects.push(Project {
                root: dir.clone(),
                load_roots,
                external,
            });
        }
        projects.sort_by(|a, b| {
            let depth = |p: &Project| {
                if p.root.is_empty() {
                    0
                } else {
                    p.root.matches('/').count() + 1
                }
            };
            depth(b).cmp(&depth(a)).then_with(|| a.root.cmp(&b.root))
        });
        Self {
            projects,
            files: files.iter().cloned().collect(),
        }
    }

    /// The project `path` belongs to: the nearest root above it.
    pub fn project_of(&self, path: &str) -> Option<&Project> {
        self.projects
            .iter()
            .find(|p| p.root.is_empty() || path.starts_with(&format!("{}/", p.root)))
    }

    /// The repository file a Ruby load of `path` (as `ruby_require_path`
    /// stored it) from `importer` names, or `None`.
    pub fn resolve(&self, path: &str, importer: &str) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        if path.starts_with("./") || path.starts_with("../") {
            let target = with_rb(&join(dir_of(importer), path));
            return self.files.contains(&target).then_some(target);
        }
        if path.starts_with('/') {
            return None;
        }
        let project = self.project_of(importer)?;
        let mut prefix_ends = path.match_indices('/').map(|(i, _)| i).chain([path.len()]);
        if prefix_ends.any(|end| project.external.contains(&path[..end])) {
            return None;
        }
        let mut hits = project
            .load_roots
            .iter()
            .map(|root| with_rb(&join(root, path)))
            .filter(|candidate| self.files.contains(candidate));
        let hit = hits.next()?;
        // Two load roots holding the file: Ruby takes the first on
        // `$LOAD_PATH`, an order the manifests do not fix.
        hits.next().is_none().then_some(hit)
    }
}

/// `path` with the `.rb` Ruby adds to a feature name without an extension.
fn with_rb(path: &str) -> String {
    if path.ends_with(".rb") {
        path.to_string()
    } else {
        format!("{path}.rb")
    }
}

/// The require-path prefixes of gem `gem`: its name, and the two spellings
/// RubyGems' naming guide maps a dash to (`net-http` → `net/http`, `my-gem`
/// → `my_gem`). A gem whose files use another name
/// (`activesupport` → `active_support`) is only known by an explicit
/// `require:`; otherwise its requires resolve like any other.
/// <https://guides.rubygems.org/name-your-gem/>
fn require_names(gem: &str) -> Vec<String> {
    let mut names = vec![gem.to_string()];
    if gem.contains('-') {
        names.push(gem.replace('-', "/"));
        names.push(gem.replace('-', "_"));
    }
    names
}

/// The gem names a `Gemfile.lock` lists as specs (four-space indented lines
/// under `specs:`) in its `GEM` and `GIT` sections, transitive ones
/// included. `PATH` specs are local and left out.
pub(crate) fn lock_gems(lock: &str) -> Vec<String> {
    let mut gems = Vec::new();
    let mut section = "";
    for line in lock.lines() {
        if !line.starts_with(' ') {
            section = line.trim();
            continue;
        }
        if !matches!(section, "GEM" | "GIT") {
            continue;
        }
        let Some(spec) = line.strip_prefix("    ") else {
            continue;
        };
        if spec.starts_with(' ') {
            continue;
        }
        if let Some(name) = spec.split(' ').next().filter(|n| !n.is_empty()) {
            gems.push(name.to_string());
        }
    }
    gems
}

/// Read the literal declarations of a Gemfile into `manifest`.
pub(crate) fn read_gemfile(content: &[u8], manifest: &mut Manifest) {
    let Some(tree) = crate::extract::parse_file("Gemfile", content) else {
        return;
    };
    walk_gemfile(content, tree.root_node(), None, manifest);
}

fn walk_gemfile(src: &[u8], node: Node, path_block: Option<&str>, manifest: &mut Manifest) {
    let mut block_dir: Option<String> = None;
    if node.kind() == "call" && node.child_by_field_name("receiver").is_none() {
        let method = text(src, node.child_by_field_name("method"));
        let args = node.child_by_field_name("arguments");
        let first = args
            .and_then(|a| named(a).into_iter().next())
            .and_then(|n| literal(src, n));
        match method.as_deref() {
            Some("gem") => {
                if let Some(name) = first {
                    let path_value = option_node(src, args, "path");
                    let path = path_value.and_then(|v| literal(src, v));
                    let remote = ["git", "github", "gitlab", "bitbucket"]
                        .iter()
                        .any(|key| option_node(src, args, key).is_some());
                    match (path, path_block) {
                        (Some(path), _) => manifest.path_gems.push(path),
                        // A computed `path:` names a local gem the graph
                        // cannot place: neither local nor external.
                        (None, _) if path_value.is_some() => {}
                        // In a `path "dir" do` block a gem lives in
                        // `dir/<name>`, or in `dir` itself.
                        (None, Some(dir)) if !remote => {
                            manifest.path_gems.push(format!("{dir}/{name}"));
                        }
                        _ => {
                            let require = option(src, args, "require");
                            manifest.gems.push((name, require));
                        }
                    }
                }
            }
            Some("gemspec") => {
                if let Some(path) = option(src, args, "path") {
                    manifest.path_gems.push(path);
                }
            }
            Some("path") if node.child_by_field_name("block").is_some() => block_dir = first,
            _ => {}
        }
    }
    let dir = block_dir.as_deref().or(path_block);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_gemfile(src, child, dir, manifest);
    }
}

/// Read a gemspec's literal `spec.name = "x"` and `spec.require_paths =
/// ["lib"]` (any receiver name) into `manifest`.
pub(crate) fn read_gemspec(content: &[u8], manifest: &mut Manifest) {
    let Some(tree) = crate::extract::parse_file("x.gemspec", content) else {
        return;
    };
    walk_gemspec(content, tree.root_node(), manifest);
}

fn walk_gemspec(src: &[u8], node: Node, manifest: &mut Manifest) {
    if node.kind() == "assignment"
        && let Some(left) = node
            .child_by_field_name("left")
            .filter(|l| l.kind() == "call")
        && let Some(right) = node.child_by_field_name("right")
    {
        match text(src, left.child_by_field_name("method")).as_deref() {
            Some("name") => {
                if let Some(name) = literal(src, right) {
                    manifest.name.get_or_insert(name);
                }
            }
            Some("require_paths") => {
                let paths = match right.kind() {
                    "array" => named(right)
                        .into_iter()
                        .map(|n| literal(src, n))
                        .collect::<Option<Vec<_>>>(),
                    _ => literal(src, right).map(|p| vec![p]),
                };
                if let Some(paths) = paths.filter(|p| !p.is_empty()) {
                    manifest.require_paths.get_or_insert(paths);
                }
            }
            _ => {}
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_gemspec(src, child, manifest);
    }
}

fn named(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

fn text(src: &[u8], node: Option<Node>) -> Option<String> {
    node.map(|n| String::from_utf8_lossy(&src[n.byte_range()]).into_owned())
}

/// A literal string or symbol without interpolation.
fn literal(src: &[u8], node: Node) -> Option<String> {
    match node.kind() {
        "string" => {
            let mut out = String::new();
            let mut cursor = node.walk();
            for part in node.children(&mut cursor) {
                match part.kind() {
                    "string_content" => out.push_str(&text(src, Some(part))?),
                    "interpolation" => return None,
                    _ => {}
                }
            }
            Some(out)
        }
        "simple_symbol" => text(src, Some(node)).map(|t| t[1..].to_string()),
        _ => None,
    }
}

/// The value node of a `key: value` (or `:key => value`) argument.
fn option_node<'t>(src: &[u8], args: Option<Node<'t>>, key: &str) -> Option<Node<'t>> {
    named(args?).into_iter().find_map(|arg| {
        if arg.kind() != "pair" {
            return None;
        }
        let k = text(src, arg.child_by_field_name("key"))?;
        (k.trim_start_matches(':').trim_end_matches(':') == key)
            .then(|| arg.child_by_field_name("value"))
            .flatten()
    })
}

/// The literal value of a `key:` argument.
fn option(src: &[u8], args: Option<Node>, key: &str) -> Option<String> {
    option_node(src, args, key).and_then(|v| literal(src, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemfile_literals_should_name_remote_gems_path_gems_and_require_names() {
        let gemfile = br#"
source "https://rubygems.org"
gemspec path: "gems/core"
gem "rails", "~> 7.1"
gem "my-client", require: "my_client/api"
gem "quiet", require: false
gem "billing", path: "engines/billing"
gem "forked", github: "acme/forked"
gem "computed", path: File.join("x", "y")
group :test do
  gem "rspec-rails"
end
path "components" do
  gem "search"
end
"#;
        let mut manifest = Manifest::default();
        read_gemfile(gemfile, &mut manifest);
        assert_eq!(
            manifest.gems,
            [
                ("rails".to_string(), None),
                ("my-client".to_string(), Some("my_client/api".to_string())),
                ("quiet".to_string(), None),
                ("forked".to_string(), None),
                ("rspec-rails".to_string(), None),
            ]
        );
        assert_eq!(
            manifest.path_gems,
            ["gems/core", "engines/billing", "components/search"]
        );
    }

    #[test]
    fn gemspec_literals_should_give_the_name_and_require_paths() {
        let mut manifest = Manifest::default();
        read_gemspec(
            br#"Gem::Specification.new do |spec|
  spec.name = "billing"
  spec.require_paths = ["lib", "ext/lib"]
  spec.version = Billing::VERSION
end
"#,
            &mut manifest,
        );
        assert_eq!(manifest.name.as_deref(), Some("billing"));
        assert_eq!(
            manifest.require_paths,
            Some(vec!["lib".to_string(), "ext/lib".to_string()])
        );
        let mut dynamic = Manifest::default();
        read_gemspec(
            b"Gem::Specification.new do |s|\n  s.name = NAME\n  s.require_paths = Dir['lib']\nend\n",
            &mut dynamic,
        );
        assert_eq!(dynamic, Manifest::default());
    }

    #[test]
    fn lockfiles_should_list_remote_specs_with_transitive_ones_but_not_path_specs() {
        let lock = [
            "PATH",
            "  remote: engines/billing",
            "  specs:",
            "    billing (0.1.0)",
            "      rails (>= 7)",
            "",
            "GEM",
            "  remote: https://rubygems.org/",
            "  specs:",
            "    rails (7.1.0)",
            "      railties (= 7.1.0)",
            "    railties (7.1.0)",
            "      thor (~> 1.0)",
            "    thor (1.3.0)",
            "",
            "PLATFORMS",
            "  ruby",
            "",
            "DEPENDENCIES",
            "  billing!",
            "  rails",
        ]
        .join("\n");
        assert_eq!(lock_gems(&lock), ["rails", "railties", "thor"]);
    }

    #[test]
    fn require_names_should_follow_the_dash_conventions() {
        assert_eq!(require_names("rails"), ["rails"]);
        assert_eq!(
            require_names("net-http"),
            ["net-http", "net/http", "net_http"]
        );
    }

    #[test]
    fn manifests_should_be_recognised_by_file_name() {
        for path in [
            "Gemfile",
            "app/Gemfile",
            "Gemfile.lock",
            "x/Gemfile.lock",
            "a.gemspec",
        ] {
            assert!(is_manifest(path), "{path}");
        }
        for path in ["Gemfile.rb", "MyGemfile", "lib/gemspec.rb", ".gemspec"] {
            assert!(!is_manifest(path), "{path}");
        }
    }
}
