// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `require` resolution inside the Ruby project boundary, carried from v1's
//! `pixel-graph` (`src/imports/ruby.rs`).
//!
//! A directory holding a `Gemfile` or a `*.gemspec` is a project root. A
//! file belongs to the nearest one above it, and its `require "a/b"`
//! searches only that project's load roots: the `require_paths` of its own
//! gemspec (`lib` by default), and those of the local path gems its
//! `Gemfile` declares (`gem "x", path: "engines/x"`). A require whose first
//! segments name an external gem of the project, declared in its `Gemfile`
//! or listed (transitive dependencies included) in its `Gemfile.lock`, is
//! that gem's file, never a local one, so `require "rails/generators"`
//! cannot land on a local `lib/rails/generators.rb`. A miss stays
//! unresolved: nothing escapes into a sibling project, and no file is
//! matched by suffix.
//!
//! The manifests are read from the facts extraction stored (literal `gem`,
//! `path` and `gemspec` calls of a Gemfile, literal `name` and
//! `require_paths` of a gemspec, the spec lines of a lockfile), never
//! evaluated: a computed path, `eval_gemfile` or a `$LOAD_PATH` edit is not
//! modelled.
//! <https://bundler.io/v2.5/man/gemfile.5.html>,
//! <https://guides.rubygems.org/specification-reference/#require_paths=>

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::Fact;

/// One Ruby project: its root directory (`""` for the repository root),
/// where its `require`s search, and the require names that belong to gems.
#[derive(Debug)]
struct Project {
    root: String,
    /// The directories of the local path gems its `Gemfile` declares.
    path_gems: Vec<String>,
    /// Searched by `require`, in order: the project's own require paths,
    /// then each local path gem's.
    load_roots: Vec<String>,
    /// Require-path prefixes (whole segments) owned by an external gem:
    /// `rails`, `net/http` for `net-http`.
    external: HashSet<String>,
}

/// The Ruby projects of a generation, and its file list.
#[derive(Debug, Default)]
pub struct Projects<'a> {
    /// Deepest root first, so the first containing one is the nearest.
    projects: Vec<Project>,
    files: HashSet<&'a str>,
}

/// What a project's manifests declare, before it becomes a [`Project`].
#[derive(Debug, Default)]
struct Manifest {
    /// Gems from a remote source, with the literal `require:` name.
    gems: Vec<(String, Option<String>)>,
    /// Local path gems: directories relative to the project root.
    path_gems: Vec<String>,
    /// The gemspec's literal `name`.
    name: Option<String>,
    /// The gemspec's literal `require_paths`.
    require_paths: Option<Vec<String>>,
}

impl<'a> Projects<'a> {
    /// The projects of the generation's files (path and facts).
    pub fn build(files: &'a [(String, String)]) -> Projects<'a> {
        let facts: HashMap<&str, &str> = files
            .iter()
            .map(|(path, facts)| (path.as_str(), facts.as_str()))
            .collect();
        let read = |path: &str| -> Vec<Fact> {
            facts
                .get(path)
                .map(|facts| Fact::decode_all(facts))
                .unwrap_or_default()
        };
        let mut roots: HashMap<String, Manifest> = HashMap::default();
        for path in facts.keys() {
            if is_gemfile(path) || is_gemspec(path) {
                roots.entry(dir_of(path).to_owned()).or_default();
            }
        }
        let mut gemspecs: Vec<&str> = facts.keys().copied().filter(|p| is_gemspec(p)).collect();
        gemspecs.sort_unstable();
        for (dir, manifest) in &mut roots {
            for fact in read(&join(dir, "Gemfile")) {
                match fact {
                    Fact::Gem { name, require } => manifest.gems.push((name, require)),
                    Fact::PathGem { path } => manifest.path_gems.push(path),
                    _ => {}
                }
            }
            for spec in gemspecs.iter().filter(|spec| dir_of(spec) == dir.as_str()) {
                for fact in read(spec) {
                    match fact {
                        Fact::GemspecName { name } => {
                            manifest.name.get_or_insert(name);
                        }
                        Fact::RequirePaths { paths } if !paths.is_empty() => {
                            manifest.require_paths.get_or_insert(paths);
                        }
                        _ => {}
                    }
                }
            }
        }
        let locks: HashMap<&str, Vec<String>> = roots
            .keys()
            .map(|dir| {
                let locked = read(&join(dir, "Gemfile.lock"))
                    .into_iter()
                    .filter_map(|fact| match fact {
                        Fact::LockGem { name } => Some(name),
                        _ => None,
                    })
                    .collect();
                (dir.as_str(), locked)
            })
            .collect();
        let mut projects: Vec<Project> = roots
            .iter()
            .map(|(dir, manifest)| project(dir, manifest, &roots, &locks[dir.as_str()]))
            .collect();
        projects.sort_by(|a, b| {
            depth(&b.root)
                .cmp(&depth(&a.root))
                .then_with(|| a.root.cmp(&b.root))
        });
        Projects {
            projects,
            files: facts.into_keys().collect(),
        }
    }

    /// The root of the project `path` belongs to; `None` outside every
    /// project.
    pub fn root_of(&self, path: &str) -> Option<&str> {
        self.project_of(path).map(|p| p.root.as_str())
    }

    /// The projects whose definitions a site of the project rooted at
    /// `root` may resolve to (D13): its own, and the local path gems its
    /// `Gemfile` declares that are projects themselves, as its `require`s
    /// search them. Outside every project, the files outside every project.
    pub fn visible_from<'s>(&'s self, root: Option<&'s str>) -> Vec<Option<&'s str>> {
        let mut roots = vec![root];
        if let Some(project) = root.and_then(|r| self.projects.iter().find(|p| p.root == r)) {
            for gem in &project.path_gems {
                if let Some(other) = self.projects.iter().find(|p| &p.root == gem)
                    && !roots.contains(&Some(other.root.as_str()))
                {
                    roots.push(Some(other.root.as_str()));
                }
            }
        }
        roots
    }

    /// The other projects whose sites see the project rooted at `root`:
    /// those whose `Gemfile` declares it as a local path gem (R6).
    pub fn loaders_of<'s>(&'s self, root: Option<&'s str>) -> Vec<Option<&'s str>> {
        let Some(root) = root else {
            return Vec::new();
        };
        self.projects
            .iter()
            .filter(|p| p.root != root)
            .map(|p| Some(p.root.as_str()))
            .filter(|other| self.visible_from(*other).contains(&Some(root)))
            .collect()
    }

    /// The project `path` belongs to: the nearest root above it.
    fn project_of(&self, path: &str) -> Option<&Project> {
        self.projects.iter().find(|p| {
            p.root.is_empty()
                || path
                    .strip_prefix(p.root.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    /// The file a load of `path` from `importer` names, `path` as
    /// extraction stores it: `./x` or `../x` for `require_relative`, the
    /// bare feature name for a load-path `require`, `""` for a load written
    /// as a path relative to the process's working directory.
    pub fn resolve(&self, path: &str, importer: &str) -> Option<&'a str> {
        if path.is_empty() {
            return None;
        }
        if path.starts_with("./") || path.starts_with("../") {
            let target = with_rb(&join(dir_of(importer), path));
            return self.files.get(target.as_str()).copied();
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
            .filter_map(|root| self.files.get(with_rb(&join(root, path)).as_str()).copied());
        let hit = hits.next()?;
        // Two load roots holding the file: Ruby takes the first on
        // `$LOAD_PATH`, an order the manifests do not fix.
        hits.next().is_none().then_some(hit)
    }
}

/// The project rooted at `dir`: its load roots and external gems.
fn project(
    dir: &str,
    manifest: &Manifest,
    roots: &HashMap<String, Manifest>,
    locked: &[String],
) -> Project {
    let mut load_roots = own_roots(dir, Some(manifest));
    let mut local: HashSet<String> = manifest.name.iter().cloned().collect();
    let mut path_gems = Vec::new();
    for gem_dir in &manifest.path_gems {
        let gem_root = join(dir, gem_dir);
        path_gems.push(gem_root.clone());
        let gem = roots.get(&gem_root);
        load_roots.extend(own_roots(&gem_root, gem));
        if let Some(name) = gem.and_then(|g| g.name.clone()) {
            local.insert(name);
        }
        if let Some(last) = gem_root.rsplit('/').next() {
            local.insert(last.to_owned());
        }
    }
    let mut external = HashSet::default();
    let declared = manifest
        .gems
        .iter()
        .map(|(gem, require)| (gem, require.as_deref()));
    let locked = locked.iter().map(|gem| (gem, None));
    for (gem, require) in declared.chain(locked) {
        if local.contains(gem) {
            continue;
        }
        external.extend(require_names(gem));
        if let Some(first) = require.and_then(|r| r.split('/').next()) {
            external.insert(first.to_owned());
        }
    }
    for name in &local {
        for alias in require_names(name) {
            external.remove(&alias);
        }
    }
    // A root listed twice (`gemspec path: "."`, two path gems on one
    // directory) is one root: keep its first place.
    let mut listed = HashSet::default();
    load_roots.retain(|root| listed.insert(root.clone()));
    Project {
        root: dir.to_owned(),
        path_gems,
        load_roots,
        external,
    }
}

/// A project's own load roots: its gemspec's `require_paths`, or `lib`.
fn own_roots(dir: &str, manifest: Option<&Manifest>) -> Vec<String> {
    match manifest.and_then(|m| m.require_paths.as_ref()) {
        Some(paths) => paths.iter().map(|p| join(dir, p)).collect(),
        None => vec![join(dir, "lib")],
    }
}

fn depth(root: &str) -> usize {
    if root.is_empty() {
        0
    } else {
        root.matches('/').count() + 1
    }
}

/// The require-path prefixes of gem `gem`: its name, and the two spellings
/// RubyGems' naming guide maps a dash to (`net-http` → `net/http`, `my-gem`
/// → `my_gem`). A gem whose files use another name (`activesupport` →
/// `active_support`) is known only by an explicit `require:`.
/// <https://guides.rubygems.org/name-your-gem/>
fn require_names(gem: &str) -> Vec<String> {
    let mut names = vec![gem.to_owned()];
    if gem.contains('-') {
        names.push(gem.replace('-', "/"));
        names.push(gem.replace('-', "_"));
    }
    names
}

/// `path` with the `.rb` Ruby adds to a feature name without an extension.
fn with_rb(path: &str) -> String {
    if path.ends_with(".rb") {
        path.to_owned()
    } else {
        format!("{path}.rb")
    }
}

fn dir_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn is_gemspec(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|f| f.ends_with(".gemspec") && f.len() > ".gemspec".len())
}

fn is_gemfile(path: &str) -> bool {
    path == "Gemfile" || path.ends_with("/Gemfile")
}

/// `rel` joined to `dir`, `.` and `..` segments resolved; `..` past the
/// top stays at the top.
fn join(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            segment => parts.push(segment),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use crate::ruby::Fact;
    use crate::ruby::resolve::fixture::Generation;

    fn gem(name: &str, require: Option<&str>) -> Fact {
        Fact::Gem {
            name: name.to_owned(),
            require: require.map(str::to_owned),
        }
    }

    fn path_gem(path: &str) -> Fact {
        Fact::PathGem {
            path: path.to_owned(),
        }
    }

    fn lock_gem(name: &str) -> Fact {
        Fact::LockGem {
            name: name.to_owned(),
        }
    }

    fn require_paths(paths: &[&str]) -> Fact {
        Fact::RequirePaths {
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn file_uid(path: &str) -> Option<String> {
        Some(format!("{path}#{path}#file"))
    }

    #[test]
    fn c10_ruby_a_require_resolves_on_its_projects_load_root_only() {
        // Gemfile (no gem); lib/reports/pdf.rb; a sibling project
        // other_app/ with its Gemfile and other_app/lib/missing/thing.rb.
        // app/order.rb: `require "reports/pdf"`, `require
        // "missing/thing"`, `require "pdf"` (never a suffix match).
        let mut g = Generation::default();
        g.facts("Gemfile", &[])
            .file("lib/reports/pdf.rb")
            .facts("other_app/Gemfile", &[])
            .file("other_app/lib/missing/thing.rb")
            .file("app/order.rb");
        let pdf = g.require("app/order.rb", 1, "reports/pdf");
        let sibling = g.require("app/order.rb", 2, "missing/thing");
        let suffix = g.require("app/order.rb", 3, "pdf");
        let dst = g.resolve();
        assert_eq!(dst[pdf], file_uid("lib/reports/pdf.rb"));
        assert_eq!(dst[sibling], None);
        assert_eq!(dst[suffix], None);
    }

    #[test]
    fn c10_ruby_a_require_of_an_external_gem_never_lands_on_a_local_file() {
        // Gemfile: `gem "rails"`, `gem "activesupport", require:
        // "active_support/all"`, `gem "net-http"`; Gemfile.lock lists thor
        // (a transitive gem). lib/ holds a decoy for each, and
        // lib/rails_ext.rb, whose first segment is no gem's.
        let manifest = [
            gem("rails", None),
            gem("activesupport", Some("active_support/all")),
            gem("net-http", None),
        ];
        let mut g = Generation::default();
        g.facts("Gemfile", &manifest)
            .facts("Gemfile.lock", &[lock_gem("thor")]);
        let features = [
            "rails/generators",
            "thor",
            "active_support/core_ext",
            "net/http",
            "net_http",
        ];
        for feature in features.into_iter().chain(["rails_ext"]) {
            g.file(&format!("lib/{feature}.rb"));
        }
        let gems: Vec<usize> = features
            .iter()
            .map(|feature| g.require("app/order.rb", 1, feature))
            .collect();
        let local = g.require("app/order.rb", 2, "rails_ext");
        let dst = g.resolve();
        for (feature, import) in features.iter().zip(gems) {
            assert_eq!(dst[import], None, "{feature}");
        }
        assert_eq!(dst[local], file_uid("lib/rails_ext.rb"));
    }

    #[test]
    fn c10_ruby_a_path_gem_adds_its_load_roots_and_names_no_external_gem() {
        // Gemfile: `gem "billing", path: "engines/billing"` and `gem
        // "billing-api", require: "billing/api"`; the lockfile lists billing
        // and invoicing. engines/billing/billing.gemspec: name "invoicing".
        // Both the directory's name and the gemspec's are local: app/a.rb
        // `require "billing/invoice"` and `require "invoicing/tax"`.
        let mut g = Generation::default();
        let manifest = [
            path_gem("engines/billing"),
            gem("billing-api", Some("billing/api")),
        ];
        let name = Fact::GemspecName {
            name: "invoicing".to_owned(),
        };
        g.facts("Gemfile", &manifest)
            .facts(
                "Gemfile.lock",
                &[lock_gem("billing"), lock_gem("invoicing")],
            )
            .facts("engines/billing/billing.gemspec", &[name])
            .file("engines/billing/lib/billing/invoice.rb")
            .file("engines/billing/lib/invoicing/tax.rb");
        let invoice = g.require("app/a.rb", 1, "billing/invoice");
        let tax = g.require("app/a.rb", 2, "invoicing/tax");
        let dst = g.resolve();
        assert_eq!(
            dst[invoice],
            file_uid("engines/billing/lib/billing/invoice.rb")
        );
        assert_eq!(dst[tax], file_uid("engines/billing/lib/invoicing/tax.rb"));

        g.facts("Gemfile", &[]);
        assert_eq!(g.resolve()[invoice], None);
    }

    #[test]
    fn c10_ruby_a_file_on_two_load_roots_is_unresolved() {
        // Gemfile: `gem "billing", path: "engines/billing"`; both lib/ and
        // engines/billing/lib/ hold reports/pdf.rb. Ruby takes the first on
        // `$LOAD_PATH`, an order the manifests do not fix.
        let mut g = Generation::default();
        g.facts("Gemfile", &[path_gem("engines/billing")])
            .facts("engines/billing/billing.gemspec", &[])
            .file("lib/reports/pdf.rb")
            .file("engines/billing/lib/reports/pdf.rb");
        let import = g.require("app/order.rb", 1, "reports/pdf");
        assert_eq!(g.resolve()[import], None);
    }

    #[test]
    fn c10_ruby_a_load_root_listed_twice_is_one_root() {
        // Gemfile: `gemspec path: "."`; app.gemspec: require_paths
        // ["lib", "ext"]; lib/foo.rb, ext/bar.rb; app/a.rb requires both.
        let mut g = Generation::default();
        g.facts("Gemfile", &[path_gem(".")])
            .facts("app.gemspec", &[require_paths(&["lib", "ext"])])
            .file("lib/foo.rb")
            .file("ext/bar.rb");
        let foo = g.require("app/a.rb", 1, "foo");
        let bar = g.require("app/a.rb", 2, "bar");
        let dst = g.resolve();
        assert_eq!(dst[foo], file_uid("lib/foo.rb"));
        assert_eq!(dst[bar], file_uid("ext/bar.rb"));
    }

    #[test]
    fn c10_ruby_the_first_gemspec_of_a_directory_gives_the_require_paths() {
        // a.gemspec: require_paths ["src"]; b.gemspec: require_paths
        // ["lib"]; src/foo.rb and lib/foo.rb; app/a.rb: `require "foo"`.
        let mut g = Generation::default();
        g.facts("b.gemspec", &[require_paths(&["lib"])])
            .facts("a.gemspec", &[require_paths(&["src"])])
            .file("src/foo.rb")
            .file("lib/foo.rb");
        let foo = g.require("app/a.rb", 1, "foo");
        assert_eq!(g.resolve()[foo], file_uid("src/foo.rb"));
    }

    #[test]
    fn c10_ruby_require_relative_resolves_from_the_importers_directory() {
        // No Gemfile: a relative load needs no project. app/models/order.rb:
        // `require_relative "../services/checkout"` and `require_relative
        // "absent"` (stored "./absent"); bin/rails: `require_relative
        // "../config/boot"`; Rakefile: `require_relative
        // "config/application"` (stored "./config/application").
        let mut g = Generation::default();
        g.file("app/services/checkout.rb")
            .file("config/boot.rb")
            .file("config/application.rb");
        let (order, relative) = ("app/models/order.rb", "require_relative");
        let checkout = "../services/checkout";
        let up = g.import(order, 1, relative, checkout, checkout);
        let absent = g.import(order, 2, relative, "absent", "./absent");
        let boot = "../config/boot";
        let binstub = g.import("bin/rails", 2, relative, boot, boot);
        let application = "config/application";
        let rakefile = g.import("Rakefile", 1, relative, application, "./config/application");
        let dst = g.resolve();
        assert_eq!(dst[up], file_uid("app/services/checkout.rb"));
        assert_eq!(dst[absent], None);
        assert_eq!(dst[binstub], file_uid("config/boot.rb"));
        assert_eq!(dst[rakefile], file_uid("config/application.rb"));
    }

    #[test]
    fn c10_ruby_a_load_from_the_working_directory_or_an_absolute_path_is_unresolved() {
        // app.gemspec: require_paths ["."], so the root is a load root;
        // cwd_relative.rb and lib/x.rb exist. app/a.rb: `require
        // "./cwd_relative"` (stored path ""), `load "/srv/x.rb"` (stored
        // ""), `require_relative "/lib/x"` (stored "/lib/x").
        let mut g = Generation::default();
        g.facts("app.gemspec", &[require_paths(&["."])])
            .file("cwd_relative.rb")
            .file("lib/x.rb");
        let cwd = g.import("app/a.rb", 1, "require", "./cwd_relative", "");
        let load = g.import("app/a.rb", 2, "load", "/srv/x.rb", "");
        let absolute = g.import("app/a.rb", 3, "require_relative", "/lib/x", "/lib/x");
        let dst = g.resolve();
        assert_eq!(dst[cwd], None);
        assert_eq!(dst[load], None);
        assert_eq!(dst[absolute], None);
    }

    #[test]
    fn c10_ruby_a_file_outside_every_project_resolves_no_load_path_require() {
        // tools/Gemfile is the only manifest; lib/foo.rb exists; app/a.rb
        // (outside tools/): `require "foo"`.
        let mut g = Generation::default();
        g.facts("tools/Gemfile", &[]).file("lib/foo.rb");
        let foo = g.require("app/a.rb", 1, "foo");
        assert_eq!(g.resolve()[foo], None);
    }

    #[test]
    fn c10_ruby_an_engine_file_searches_its_own_project_only() {
        // Root Gemfile with the path gem engines/billing; lib/reports/pdf.rb
        // at the root. engines/billing/billing.gemspec (require_paths
        // ["lib"]); engines/billing/lib/billing/charge.rb: `require
        // "billing/invoice"`, `require "reports/pdf"`.
        let mut g = Generation::default();
        g.facts("Gemfile", &[path_gem("engines/billing")])
            .file("lib/reports/pdf.rb")
            .facts(
                "engines/billing/billing.gemspec",
                &[require_paths(&["lib"])],
            )
            .file("engines/billing/lib/billing/invoice.rb");
        let charge = "engines/billing/lib/billing/charge.rb";
        let own = g.require(charge, 1, "billing/invoice");
        let app = g.require(charge, 2, "reports/pdf");
        let dst = g.resolve();
        assert_eq!(dst[own], file_uid("engines/billing/lib/billing/invoice.rb"));
        assert_eq!(dst[app], None);
    }
}
