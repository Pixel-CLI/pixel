// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The crates and modules of the workspace, from the facts alone (spec
//! option 16.2(b), replacing v1's M-1, M-2 and P).
//!
//! A file belongs to the nearest `Cargo.toml` above it. Each manifest with
//! a `[package]` declares targets by Cargo's standard layout: the library
//! (`src/lib.rs`, or `[lib] path`), binaries (`src/main.rs`,
//! `src/bin/*.rs`, `src/bin/*/main.rs`, `[[bin]] path`), and the tests,
//! examples and benches (`tests/*.rs`, `tests/*/main.rs`, and the same under
//! `examples/` and `benches/`) and `build.rs`, each a crate of its own that
//! sees the library by its crate name (`[lib] name`, else the package name
//! with `-` read as `_`). Another package's library is a crate name only
//! where a dependency puts it (D22): a path dependency, direct or inherited
//! from `[workspace.dependencies]`, known by its key when renamed; a
//! registry or git dependency is outside the workspace, whatever library of
//! the workspace shares its name. A target root grows its module tree through its
//! `mod` declarations: `mod x;` in a module whose directory is `d` is
//! `d/x.rs` or `d/x/mod.rs` (both present: neither, as for Rust); a root's
//! or a `mod.rs`'s directory is its own, `d/b.rs`'s is `d/b`, and an inline
//! `mod x { … }` adds `x`. `#[path = "p"]` on an inline module makes its
//! directory `p` under the base of the module around it: the directory of
//! a file module (`d` for `d/b.rs`, not `d/b`), an inline module's own
//! directory (rustc's rule, D38). A `#[path]` file module is not followed,
//! nor one the rules cannot read: what it names is unresolved rather than
//! wrong.
//!
//! A file several targets declare (`mod util;` in both `lib.rs` and
//! `main.rs`) is a module of each. A file no target reaches is a crate of
//! its own with no name: its items resolve inside it, `crate::` and the
//! names of other crates do not.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::cfg;
use super::resolve::SourceFile;
use super::{DepKind, DepSource, DepSourceRef, DependencyRef, ItemKind, Manifest, ModPath};

pub type ModId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Lib,
    Bin,
    Test,
    Example,
    Bench,
    Build,
}

#[derive(Debug)]
pub struct Crate {
    pub target: Target,
    pub package: usize,
    pub root: ModId,
}

#[derive(Debug)]
pub struct Module {
    pub file: usize,
    /// The module's scope in its file: 0 for a file, an inline module's
    /// body otherwise.
    pub scope: u32,
    pub parent: Option<ModId>,
    pub krate: Option<usize>,
    /// Where `mod x;` declared directly in this module looks for `x`;
    /// `None` when the rules cannot tell (a `#[path]` they cannot read).
    dir: Option<String>,
    /// Where a `#[path]` on an inline module of this module is relative to:
    /// rustc's directory of the module, `d` for `d/b.rs`, without the `b`
    /// that `mod x;` in `d/b.rs` adds.
    base: Option<String>,
    /// The submodule each `mod` item of this module declares, by item.
    pub children: HashMap<u32, ModId>,
    /// The `#[cfg]` conjuncts of the `mod x;` declarations that lead to
    /// this module's file from its crate root (F2); what the file itself
    /// holds is in its facts.
    pub cfg: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Tree {
    pub crates: Vec<Crate>,
    pub modules: Vec<Module>,
    /// The modules a file's module scope is, one per target reaching it.
    pub at: HashMap<(usize, u32), Vec<ModId>>,
    /// The library crates by crate name.
    pub libs: HashMap<String, Vec<usize>>,
    /// The `#[macro_export]` macros of each crate, by name.
    pub exported_macros: HashMap<(usize, String), Vec<(usize, u32)>>,
    /// Each package's dependencies (see [`Wired`]): a workspace may
    /// declare many.
    pub externs: HashMap<usize, Wired>,
    /// The crate names dependencies give, numbered.
    pub extern_names: HashMap<String, u32>,
    /// The crate name each library crate gives its dependents, as its
    /// number in `extern_names`, by crate (`u32::MAX` for none).
    pub lib_name: Vec<u32>,
}

/// A package's dependencies, each with the crate name it gives the
/// package's code: four bytes for one whose name the dependency itself
/// tells (a workspace library's own, an outside crate's), eight for a
/// renamed workspace library.
#[derive(Debug, Default)]
pub struct Wired {
    /// Sorted by crate name.
    own: Vec<Extern>,
    /// With the crate name each gives, as its number in `extern_names`,
    /// sorted by it.
    renamed: Vec<(u32, Extern)>,
}

/// A crate name a package's dependency puts in its code's extern prelude:
/// its kind in the top two bits, then whether it is outside the
/// workspace, then the crate of the workspace library it is, or for one
/// outside, the crate name it gives, as its number in `extern_names`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extern(u32);

impl Extern {
    /// The bit of a crate outside the workspace.
    const OUTSIDE: u32 = 1 << 29;
    /// The bits of its crate or name.
    const ID: u32 = Extern::OUTSIDE - 1;

    /// `None` for a crate or a name past the bits (half a billion).
    fn new(kind: DepKind, outside: bool, id: u32) -> Option<Extern> {
        let kind = match kind {
            DepKind::Normal => 0,
            DepKind::Dev => 1,
            DepKind::Build => 2,
        };
        let outside = if outside { Extern::OUTSIDE } else { 0 };
        (id <= Extern::ID).then_some(Extern(kind << 30 | outside | id))
    }

    pub fn kind(&self) -> DepKind {
        match self.0 >> 30 {
            1 => DepKind::Dev,
            2 => DepKind::Build,
            _ => DepKind::Normal,
        }
    }

    /// The crate of the workspace library it is, or `None` for a crate
    /// outside the workspace (a registry or git dependency, a path out of
    /// the tree).
    pub fn lib(&self) -> Option<u32> {
        (self.0 & Extern::OUTSIDE == 0).then_some(self.0 & Extern::ID)
    }
}

/// Whether a dependency of `kind` is in the extern prelude of `target`:
/// `[dependencies]` everywhere but the build script, `[dev-dependencies]`
/// in tests, examples, benches and the unit tests of the library and
/// binaries, `[build-dependencies]` in the build script only.
pub fn applies(kind: DepKind, target: Target) -> bool {
    match kind {
        DepKind::Normal | DepKind::Dev => target != Target::Build,
        DepKind::Build => target == Target::Build,
    }
}

impl Tree {
    /// The dependencies of `package` that give its code the crate name
    /// `name`.
    pub fn externs_named(&self, package: usize, name: &str) -> impl Iterator<Item = &Extern> {
        let (own, renamed) = match (self.externs.get(&package), self.extern_names.get(name)) {
            (Some(wired), Some(&id)) => {
                let named = |e: &Extern| self.extern_name(e);
                let start = wired.own.partition_point(|e| named(e) < id);
                let end = wired.own.partition_point(|e| named(e) <= id);
                let first = wired.renamed.partition_point(|(n, _)| *n < id);
                let last = wired.renamed.partition_point(|(n, _)| *n <= id);
                (&wired.own[start..end], &wired.renamed[first..last])
            }
            _ => (&[][..], &[][..]),
        };
        own.iter()
            .chain(renamed.iter().map(|(_, dependency)| dependency))
    }

    /// The crate name a dependency of a package's own list gives.
    fn extern_name(&self, dependency: &Extern) -> u32 {
        match dependency.lib() {
            Some(lib) => self.lib_name.get(lib as usize).copied().unwrap_or(u32::MAX),
            None => dependency.0 & Extern::ID,
        }
    }

    /// Every dependency of `package`.
    pub fn dependencies(&self, package: usize) -> impl Iterator<Item = &Extern> {
        self.externs.get(&package).into_iter().flat_map(|wired| {
            wired
                .own
                .iter()
                .chain(wired.renamed.iter().map(|(_, dependency)| dependency))
        })
    }

    /// True when `module` is `ancestor` or inside it.
    pub fn within(&self, module: ModId, ancestor: ModId) -> bool {
        let mut current = Some(module);
        while let Some(m) = current {
            if m == ancestor {
                return true;
            }
            current = self.modules[m].parent;
        }
        false
    }
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_owned()
    } else {
        format!("{dir}/{rest}")
    }
}

fn normalize(path: &str) -> &str {
    path.trim_start_matches("./")
}

/// The directory a module file's `mod x;` looks in: its own for `mod.rs`,
/// `d/b` for `d/b.rs`.
fn module_dir(path: &str) -> String {
    let dir = parent_dir(path);
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.strip_suffix(".rs") {
        Some("mod") | None => dir.to_owned(),
        Some(stem) => join(dir, stem),
    }
}

/// The target `rel` (a path relative to its package) is the root of.
fn target_of(rel: &str, manifest: &Manifest) -> Option<Target> {
    let lib = manifest.lib_path.as_deref().map_or("src/lib.rs", normalize);
    if rel == lib {
        return Some(Target::Lib);
    }
    if rel == "src/main.rs"
        || manifest
            .bins
            .iter()
            .any(|(_, path)| !path.is_empty() && normalize(path) == rel)
    {
        return Some(Target::Bin);
    }
    let parts: Vec<&str> = rel.split('/').collect();
    let kind = |dir: &str| match dir {
        "tests" => Some(Target::Test),
        "examples" => Some(Target::Example),
        "benches" => Some(Target::Bench),
        _ => None,
    };
    match parts.as_slice() {
        ["build.rs"] => Some(Target::Build),
        ["src", "bin", file] if file.ends_with(".rs") => Some(Target::Bin),
        ["src", "bin", _, "main.rs"] => Some(Target::Bin),
        [dir, file] if file.ends_with(".rs") => kind(dir),
        [dir, _, "main.rs"] => kind(dir),
        _ => None,
    }
}

struct Builder<'f, 'w> {
    files: &'f [SourceFile<'w>],
    by_path: HashMap<&'w str, usize>,
    /// Each file's package: the index of its manifest, when that manifest
    /// declares a package.
    package_of: Vec<Option<usize>>,
    tree: Tree,
    visited: HashSet<(usize, usize)>,
}

/// The crates and modules of `files`, given the workspace's manifests as
/// `(path, manifest)` and the `dependencies` of the manifest of each
/// index, read one at a time.
pub fn build(
    files: &[SourceFile<'_>],
    manifests: &[(&str, Manifest)],
    dependencies: impl Fn(usize, &mut dyn FnMut(DependencyRef<'_>)),
) -> Tree {
    let dirs: HashMap<&str, usize> = manifests
        .iter()
        .enumerate()
        .map(|(i, (path, _))| (parent_dir(path), i))
        .collect();
    let package_of: Vec<Option<usize>> = files
        .iter()
        .map(|file| nearest(&dirs, file.path).filter(|m| manifests[*m].1.package.is_some()))
        .collect();
    let mut builder = Builder {
        files,
        by_path: files.iter().enumerate().map(|(i, f)| (f.path, i)).collect(),
        package_of,
        tree: Tree::default(),
        visited: HashSet::default(),
    };
    let mut targets: Vec<(usize, Target, usize)> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let Some(package) = builder.package_of[index] else {
            continue;
        };
        let (manifest_path, manifest) = &manifests[package];
        let dir = parent_dir(manifest_path);
        let rel = if dir.is_empty() {
            file.path
        } else {
            match file
                .path
                .strip_prefix(dir)
                .and_then(|r| r.strip_prefix('/'))
            {
                Some(rel) => rel,
                None => continue,
            }
        };
        if let Some(target) = target_of(rel, manifest) {
            targets.push((package, target, index));
        }
    }
    targets.sort_by(|a, b| (a.0, a.1, files[a.2].path).cmp(&(b.0, b.1, files[b.2].path)));
    for (package, target, root) in targets {
        let manifest = &manifests[package].1;
        let name = match target {
            Target::Lib => manifest
                .lib_name
                .clone()
                .or_else(|| manifest.package.clone())
                .unwrap_or_default()
                .replace('-', "_"),
            _ => String::new(),
        };
        let krate = builder.tree.crates.len();
        let dir = Some(parent_dir(files[root].path).to_owned());
        let module = builder.module(root, 0, None, Some(krate), (dir.clone(), dir), Vec::new());
        builder.tree.crates.push(Crate {
            target,
            package,
            root: module,
        });
        if target == Target::Lib {
            builder.tree.libs.entry(name).or_default().push(krate);
        }
        builder.visited.insert((krate, root));
        builder.grow(module);
    }
    for index in 0..files.len() {
        if !builder.tree.at.contains_key(&(index, 0)) {
            let module = builder.module(index, 0, None, None, (None, None), Vec::new());
            builder.grow(module);
        }
    }
    builder.exported_macros();
    externs(manifests, dependencies, &mut builder.tree);
    builder.tree
}

/// `dir/rel` with `.` and `..` read, or `None` when it leaves the tree or
/// is absolute.
fn normalize_path(dir: &str, rel: &str) -> Option<String> {
    let mut out = String::new();
    normalize_into(dir, rel, &mut out).then_some(out)
}

/// [`normalize_path`] into `out`, which it clears first: `false` for
/// `None`. A workspace may declare millions of path dependencies.
fn normalize_into(dir: &str, rel: &str, out: &mut String) -> bool {
    out.clear();
    if rel.starts_with('/') {
        return false;
    }
    // A directory with no `.`, empty or `..` segment is read whole.
    let plain = !dir.contains('.') && !dir.contains("//") && !dir.starts_with('/');
    let dir = if plain {
        out.push_str(dir.trim_end_matches('/'));
        ""
    } else {
        dir
    };
    for part in dir.split('/').chain(rel.split('/')) {
        match part {
            "" | "." => {}
            ".." if out.is_empty() => return false,
            ".." => out.truncate(out.rfind('/').unwrap_or(0)),
            part => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(part);
            }
        }
    }
    true
}

/// The number of a crate name among those dependencies give.
fn number(names: &mut HashMap<String, u32>, name: &str) -> u32 {
    if let Some(&id) = names.get(name) {
        return id;
    }
    let next = names.len() as u32;
    names.insert(name.to_owned(), next);
    next
}

/// The crate name each package's dependencies give it, and the workspace
/// library each is (D22). A path dependency (direct, or a workspace
/// dependency with a path) names the package whose manifest is in that
/// directory; any other is outside the workspace. A renamed dependency
/// (`package = …`) is known by its key, any other by the library's own
/// crate name, which outside the workspace is taken to be its key. Each
/// package's are read in turn, borrowed from its facts, and kept in
/// `tree.externs`, four bytes each where the dependency tells its crate
/// name: a workspace may hold thousands of packages, each declaring many.
fn externs(
    manifests: &[(&str, Manifest)],
    dependencies: impl Fn(usize, &mut dyn FnMut(DependencyRef<'_>)),
    tree: &mut Tree,
) {
    let by_dir: HashMap<&str, usize> = manifests
        .iter()
        .enumerate()
        .map(|(i, (path, _))| (parent_dir(path), i))
        .collect();
    let mut names = HashMap::default();
    // Each package's library and its crate name, and each workspace
    // root's dependencies by key, the first of a repeated one.
    let mut libs: Vec<Option<usize>> = vec![None; manifests.len()];
    for (krate, c) in tree.crates.iter().enumerate() {
        if c.target == Target::Lib {
            libs[c.package].get_or_insert(krate);
        }
    }
    let lib_names: Vec<Option<u32>> = manifests
        .iter()
        .map(|(_, manifest)| {
            let name = manifest.lib_name.as_ref().or(manifest.package.as_ref())?;
            Some(number(&mut names, &name.replace('-', "_")))
        })
        .collect();
    tree.lib_name = vec![u32::MAX; tree.crates.len()];
    for (package, krate) in libs.iter().enumerate() {
        if let (Some(krate), Some(name)) = (krate, lib_names[package]) {
            tree.lib_name[*krate] = name;
        }
    }
    // The package of the workspace a path names, relative to a directory.
    let mut target = String::new();
    let mut package_at = |base: &str, rel: &str| {
        normalize_into(base, rel, &mut target)
            .then(|| by_dir.get(target.as_str()).copied())
            .flatten()
            .filter(|m| manifests[*m].1.package.is_some())
    };
    // Each workspace root's dependencies by key, the first of a repeated
    // one, with that package: read once, not once per member.
    let mut inherited: HashMap<usize, Inherited> = HashMap::default();
    for (index, (path, manifest)) in manifests.iter().enumerate() {
        for dependency in &manifest.workspace_dependencies {
            let keyed = inherited.entry(index).or_default();
            if keyed.contains_key(dependency.key.as_str()) {
                continue;
            }
            let package = match &dependency.source {
                DepSource::Path(rel) => package_at(parent_dir(path), rel),
                DepSource::Workspace | DepSource::Other => None,
            };
            keyed.insert(&dependency.key, (dependency.package.is_some(), package));
        }
    }
    // A package's dependencies, each with its crate name, before they are
    // sorted by it: those whose name the dependency tells, and the others.
    let mut own: Vec<(u32, Extern)> = Vec::new();
    let mut renamed: Vec<(u32, Extern)> = Vec::new();
    let mut key = String::new();
    for (index, (path, manifest)) in manifests.iter().enumerate() {
        if manifest.package.is_none() {
            continue;
        }
        let dir = parent_dir(path);
        let keyed =
            workspace_root(manifests, &by_dir, dir, manifest).and_then(|root| inherited.get(&root));
        dependencies(index, &mut |dependency| {
            // Whether it is renamed, as declared or as the workspace
            // declares it, and the package of the workspace its path names.
            let (declared, package) = match dependency.source {
                DepSourceRef::Workspace => keyed
                    .and_then(|keyed| keyed.get(dependency.key).copied())
                    .unwrap_or((false, None)),
                DepSourceRef::Path(rel) => (false, package_at(dir, rel)),
                DepSourceRef::Other => (false, None),
            };
            let name = match package {
                Some(m) if !(declared || dependency.package.is_some()) => lib_names[m],
                _ => {
                    key.clear();
                    key.extend(
                        dependency
                            .key
                            .chars()
                            .map(|c| if c == '-' { '_' } else { c }),
                    );
                    Some(number(&mut names, &key))
                }
            };
            let Some(name) = name else { return };
            // A library past the bits reads as outside the workspace,
            // which proves less; an outside crate's name past them is
            // kept beside.
            let kind = dependency.kind;
            let lib = package.and_then(|package| libs[package]);
            let library = lib.and_then(|lib| Extern::new(kind, false, u32::try_from(lib).ok()?));
            match (library, Extern::new(kind, true, name)) {
                (Some(library), _) if lib.map(|lib| tree.lib_name[lib]) == Some(name) => {
                    own.push((name, library));
                }
                (Some(library), _) => renamed.push((name, library)),
                (None, Some(outside)) => own.push((name, outside)),
                (None, None) => {
                    renamed.push((name, Extern::new(kind, true, 0).expect("zero fits")));
                }
            }
        });
        own.sort_by_key(|(name, _)| *name);
        renamed.sort_by_key(|(name, _)| *name);
        let mut wired = Wired {
            own: own.drain(..).map(|(_, dependency)| dependency).collect(),
            renamed: std::mem::take(&mut renamed),
        };
        wired.renamed.shrink_to_fit();
        tree.externs.insert(index, wired);
    }
    tree.extern_names = names;
}

/// A workspace root's dependencies by key, each with whether it is
/// renamed and the package of the workspace its path names.
type Inherited<'m> = HashMap<&'m str, (bool, Option<usize>)>;

/// The workspace root a package's `workspace = true` reads, by its index
/// in `manifests`: the one its `[package] workspace` names, else the
/// nearest manifest at or above it with a `[workspace]` table.
fn workspace_root(
    manifests: &[(&str, Manifest)],
    by_dir: &HashMap<&str, usize>,
    dir: &str,
    manifest: &Manifest,
) -> Option<usize> {
    let found = match &manifest.workspace_root {
        Some(rel) => normalize_path(dir, rel).and_then(|root| by_dir.get(root.as_str()).copied()),
        None => {
            let mut current = dir;
            loop {
                if let Some(m) = by_dir.get(current).filter(|m| manifests[**m].1.workspace) {
                    break Some(*m);
                }
                if current.is_empty() {
                    break None;
                }
                current = parent_dir(current);
            }
        }
    };
    found.filter(|m| manifests[*m].1.workspace)
}

/// The manifest of the nearest directory above `path` that has one.
fn nearest(dirs: &HashMap<&str, usize>, path: &str) -> Option<usize> {
    let mut dir = parent_dir(path);
    loop {
        if let Some(manifest) = dirs.get(dir) {
            return Some(*manifest);
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent_dir(dir);
    }
}

impl Builder<'_, '_> {
    fn module(
        &mut self,
        file: usize,
        scope: u32,
        parent: Option<ModId>,
        krate: Option<usize>,
        (dir, base): (Option<String>, Option<String>),
        cfg: Vec<String>,
    ) -> ModId {
        let id = self.tree.modules.len();
        self.tree.modules.push(Module {
            file,
            scope,
            parent,
            krate,
            dir,
            base,
            children: HashMap::default(),
            cfg,
        });
        self.tree.at.entry((file, scope)).or_default().push(id);
        id
    }

    /// Adds the submodules `root` declares, and theirs.
    fn grow(&mut self, root: ModId) {
        let mut pending = vec![root];
        while let Some(module) = pending.pop() {
            let (file, scope, krate) = {
                let m = &self.tree.modules[module];
                (m.file, m.scope, m.krate)
            };
            let source = &self.files[file];
            for &index in source.declared_in(scope) {
                let item = &source.facts.items[index as usize];
                let ItemKind::Module { body, path } = &item.kind else {
                    continue;
                };
                let (dir, base, chain) = {
                    let m = &self.tree.modules[module];
                    (m.dir.clone(), m.base.clone(), m.cfg.clone())
                };
                let child = match (body, path) {
                    // An inline module's own directory: its name under this
                    // module's, or its `#[path]` under this module's base
                    // (D38, as rustc 1.99 reads it).
                    (Some(body), _) => {
                        let own = match path {
                            ModPath::Layout => dir.map(|d| join(&d, &item.name)),
                            ModPath::Literal(rel) => base.and_then(|b| normalize_path(&b, rel)),
                            ModPath::Unknown => None,
                        };
                        let at = (own.clone(), own);
                        Some(self.module(file, *body, Some(module), krate, at, chain))
                    }
                    (None, ModPath::Layout) if item.scope == scope => dir
                        .and_then(|dir| self.module_file(file, &dir, &item.name, krate))
                        .map(|child| {
                            let path = self.files[child].path;
                            let at = (Some(module_dir(path)), Some(parent_dir(path).to_owned()));
                            let cfg = cfg::union(&chain, &item.cfg);
                            self.module(child, 0, Some(module), krate, at, cfg)
                        }),
                    (None, _) => None,
                };
                if let Some(child) = child {
                    self.tree.modules[module].children.insert(index, child);
                    pending.push(child);
                }
            }
        }
    }

    /// The file `mod name;` names from a module of `file` whose directory
    /// is `dir`: exactly one of `dir/name.rs` and `dir/name/mod.rs`, in the
    /// same package, not yet part of the crate.
    fn module_file(
        &mut self,
        file: usize,
        dir: &str,
        name: &str,
        krate: Option<usize>,
    ) -> Option<usize> {
        let krate = krate?;
        let package = self.package_of[file];
        let found: Vec<usize> = [
            join(dir, &format!("{name}.rs")),
            join(dir, &format!("{name}/mod.rs")),
        ]
        .iter()
        .filter_map(|candidate| self.by_path.get(candidate.as_str()).copied())
        .filter(|candidate| self.package_of[*candidate] == package)
        .collect();
        match found.as_slice() {
            [child] if self.visited.insert((krate, *child)) => Some(*child),
            _ => None,
        }
    }

    fn exported_macros(&mut self) {
        for module in &self.tree.modules {
            let Some(krate) = module.krate else { continue };
            let source = &self.files[module.file];
            for &index in source.declared_in(module.scope) {
                let item = &source.facts.items[index as usize];
                if matches!(item.kind, ItemKind::Macro { exported: true, .. }) {
                    self.tree
                        .exported_macros
                        .entry((krate, item.name.clone()))
                        .or_default()
                        .push((module.file, index));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_path;

    #[test]
    fn a_path_is_read_from_its_directory_whatever_the_directory_holds() {
        for (dir, rel, normal) in [
            ("a/b", "../c", Some("a/c")),
            ("a/b", "../../c/./d/", Some("c/d")),
            ("a/b", "../../../c", None),
            ("", "c", Some("c")),
            ("", "../c", None),
            ("a/b/", "c", Some("a/b/c")),
            ("a/./b", "../c", Some("a/c")),
            ("a/../b", "c", Some("b/c")),
            ("a//b", "c", Some("a/b/c")),
            ("v1.2/b", "../c", Some("v1.2/c")),
            ("a", "/c", None),
        ] {
            assert_eq!(normalize_path(dir, rel).as_deref(), normal, "{dir} {rel}");
        }
    }
}
