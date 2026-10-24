// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! C-10's oracle: "everything in the map is true of the tree", checked
//! against a tree-sitter re-parse of each file, independently of the rule
//! files (`design/testing.md`, "Unit and property").
//!
//! The oracle checks what any correct indexer must satisfy: every path
//! exists with the content hashed; every symbol's signature is the text at
//! its line, where a definition node starts; every uid names one symbol;
//! every site sits inside its owner's span, at a line where the name it
//! targets is written; every resolved target exists. Re-parsing with the
//! same grammar would repeat its mistakes, so whether a call resolves to
//! the right definition is checked by the hand-labelled fixtures of each
//! rule file and by the generated trees below, whose right answers are
//! known by construction, each target by its full identity.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use pixel_model::hash::Hash;
use pixel_model::{EdgeKind, FileRows, Kept, Lang, SymbolKind};
use proptest::prelude::*;

use crate::tree::{Tree, TreeFile};
use crate::{extract, link, plan, text};

/// Node kinds a definition of `kind` starts with, per language.
/// Whether a definition of `kind` starts among the node kinds `starts`. The
/// generic languages have no kind list: any declaration or definition node
/// (or an Elixir `call`, which `def` is) counts.
fn defined_on(lang: Lang, kind: SymbolKind, starts: &HashSet<String>) -> bool {
    if matches!(
        lang,
        Lang::Php | Lang::C | Lang::Swift | Lang::Elixir | Lang::Lua | Lang::Kotlin
    ) {
        return starts.iter().any(|k| {
            k.ends_with("_declaration")
                || k.ends_with("_definition")
                || k.ends_with("_statement")
                || k == "call"
        });
    }
    let kinds = definition_kinds(lang, kind);
    kinds.iter().any(|k| starts.contains(*k))
}

fn definition_kinds(lang: Lang, kind: SymbolKind) -> &'static [&'static str] {
    match lang {
        // An assignment defines a constant (`X = 1`, `X ||= 2`), or
        // `const_set(:X, 1)`, a call.
        Lang::Ruby if kind == SymbolKind::Const => &["assignment", "operator_assignment", "call"],
        // A class or module a constant is assigned (`X =` with `Struct.new
        // do` on the next line) starts on the assignment.
        Lang::Ruby if matches!(kind, SymbolKind::Class | SymbolKind::Module) => {
            &["class", "module", "call", "assignment"]
        }
        // `call` covers the DSL lines that generate methods (`attr_reader`,
        // `delegate`, `scope`) and the constructors that make a class
        // (`X = Struct.new do`, `concerning :X do`).
        Lang::Ruby => &[
            "class",
            "module",
            "method",
            "singleton_method",
            "call",
            "alias",
        ],
        Lang::Rust => &[
            "function_item",
            "function_signature_item",
            "struct_item",
            "union_item",
            "enum_item",
            "enum_variant",
            "trait_item",
            "const_item",
            "static_item",
            "type_item",
            "macro_definition",
            "mod_item",
        ],
        Lang::Python => &["function_definition", "class_definition"],
        Lang::Go => &["function_declaration", "method_declaration", "type_spec"],
        Lang::Java => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "method_declaration",
            "constructor_declaration",
        ],
        Lang::CSharp => &[
            "namespace_declaration",
            "class_declaration",
            "record_declaration",
            "struct_declaration",
            "interface_declaration",
            "enum_declaration",
            "delegate_declaration",
            "method_declaration",
            "constructor_declaration",
            "local_function_statement",
            "property_declaration",
        ],
        Lang::Php | Lang::C | Lang::Swift | Lang::Elixir | Lang::Lua | Lang::Kotlin => &[],
        Lang::TypeScript => &[
            "function_declaration",
            "generator_function_declaration",
            "class_declaration",
            "abstract_class_declaration",
            "interface_declaration",
            "enum_declaration",
            "method_definition",
            "public_field_definition",
            "field_definition",
            "variable_declarator",
        ],
    }
}

fn grammar(lang: Lang) -> tree_sitter::Language {
    match lang {
        Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        // The TSX grammar reads TypeScript and JSX alike.
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::Java => tree_sitter_java::LANGUAGE.into(),
        Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Lang::C => tree_sitter_c::LANGUAGE.into(),
        Lang::Swift => tree_sitter_swift::LANGUAGE.into(),
        Lang::Elixir => tree_sitter_elixir::LANGUAGE.into(),
        Lang::Lua => tree_sitter_lua::LANGUAGE.into(),
        Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
    }
}

/// Manifests are read as data, not parsed by the grammar.
fn is_manifest(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(name, "Cargo.toml" | "Gemfile.lock")
}

/// What one line of a re-parsed file holds.
#[derive(Default)]
struct Line {
    /// Kinds of the named nodes starting on it.
    starts: HashSet<String>,
    /// Texts of the named leaves on it, symbol colons dropped.
    names: HashSet<String>,
}

/// A Ruby file is re-parsed as its rules read it: past the anonymous
/// forwarding tree-sitter-ruby cannot parse, which a byte-for-byte rewrite
/// of the same offsets reads (#107); no line moves.
fn reparse(lang: Lang, content: &str) -> HashMap<u32, Line> {
    let tree = match lang {
        Lang::Ruby => crate::ruby::tree(content.as_bytes()).expect("a parse"),
        _ => {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&grammar(lang)).expect("grammar loads");
            parser.parse(content, None).expect("a parse")
        }
    };
    let mut lines: HashMap<u32, Line> = HashMap::default();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_named() {
            let line = lines
                .entry(node.start_position().row as u32 + 1)
                .or_default();
            line.starts.insert(node.kind().to_owned());
            if node.child_count() == 0 {
                let leaf = &content[node.byte_range()];
                line.names.insert(leaf.trim_start_matches(':').to_owned());
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    lines
}

/// The last segment of a name as written: `Billing::Charge.call` → `call`,
/// `self.name=` → `name`, `crate::store::open` → `open`.
fn last_segment(target: &str) -> &str {
    let last = target
        .rsplit(['.', ':', '#', '/'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(target);
    last.strip_suffix('=').unwrap_or(last)
}

/// Whether the site's target is written on its source line: its name is a
/// leaf of the re-parse there (`save` in `user.save`), or the target is
/// the line's text itself (a `require` spec, `Struct.new(:x)`), white space
/// collapsed. A target that elides a long receiver (`…tail.name`) is
/// checked on its name.
fn written_on(target: &str, source: &str, line: Option<&Line>) -> bool {
    let name = last_segment(target);
    if matches!(name, "*" | "self") || line.is_some_and(|l| l.names.contains(name)) {
        return true;
    }
    // `link["href"]` and `link&.[]("href")` call the method `[]`, written
    // as brackets.
    if matches!(name, "[]" | "[]=") {
        return source.contains('[');
    }
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let target = target.strip_prefix('…').unwrap_or(target);
    !target.is_empty() && collapse(source).contains(&collapse(target))
}

/// Every way `files` (rows of the tree at `root`) is not true of the tree.
pub fn violations(root: &Path, files: &[FileRows]) -> Vec<String> {
    let mut found = Vec::new();
    // The store's primary key: a uid a second symbol repeats would fail
    // the publish, and a set of uids would hide it.
    let mut uids: HashSet<&str> = HashSet::default();
    for symbol in files.iter().flat_map(|rows| &rows.symbols) {
        if !uids.insert(symbol.uid.as_str()) {
            found.push(format!(
                "{}:{} symbol {}: uid {} is not unique",
                symbol.file, symbol.line, symbol.name, symbol.uid
            ));
        }
    }
    for rows in files {
        let path = &rows.file.path;
        let Ok(bytes) = std::fs::read(root.join(path)) else {
            found.push(format!("{path}: no such file"));
            continue;
        };
        if Hash::of(&bytes) != rows.file.hash {
            found.push(format!("{path}: content differs from the hash indexed"));
            continue;
        }
        let content = String::from_utf8_lossy(&bytes);
        let line_count = text::line_count(&bytes);
        let parsed = if is_manifest(path) {
            None
        } else {
            Some(reparse(rows.file.lang, &content))
        };
        let spans: HashMap<&str, (u32, u32)> = rows
            .symbols
            .iter()
            .map(|s| (s.uid.as_str(), (s.line, s.end_line)))
            .collect();
        for symbol in &rows.symbols {
            let at = |what: &str| format!("{path}:{} symbol {}: {what}", symbol.line, symbol.name);
            if symbol.file != *path {
                found.push(at("names another file"));
            }
            if symbol.kind == SymbolKind::File {
                if (symbol.line, symbol.end_line) != (1, line_count) {
                    found.push(at("the file symbol does not span the file"));
                }
                continue;
            }
            if symbol.line < 1 || symbol.end_line < symbol.line || symbol.end_line > line_count {
                found.push(at("span outside the file"));
                continue;
            }
            let source = text::line(&content, symbol.line).unwrap_or("");
            if symbol.signature.is_empty() || !source.contains(&symbol.signature) {
                found.push(at(&format!(
                    "signature {:?} is not on its line",
                    symbol.signature
                )));
            }
            if let Some(lines) = &parsed {
                let starts = lines.get(&symbol.line).map(|l| &l.starts);
                if !starts.is_some_and(|s| defined_on(rows.file.lang, symbol.kind, s)) {
                    found.push(at("no definition starts on its line"));
                }
            }
        }
        for edge in &rows.edges {
            let at = |what: &str| {
                format!(
                    "{path}:{} {} {:?}: {what}",
                    edge.line, edge.kind, edge.target
                )
            };
            match spans.get(edge.src.as_str()) {
                None => found.push(at("its source is not a symbol of this file")),
                Some((start, end)) if edge.line < *start || edge.line > *end => {
                    found.push(at("outside its source's span"))
                }
                Some(_) => {}
            }
            if let Some(dst) = &edge.dst
                && !uids.contains(dst.as_str())
            {
                found.push(at(&format!("target {dst} is not a symbol")));
            }
            let named = matches!(
                edge.kind,
                EdgeKind::Call | EdgeKind::Reference | EdgeKind::Import
            );
            if named && let Some(lines) = &parsed {
                let source = text::line(&content, edge.line).unwrap_or("");
                if !written_on(&edge.target, source, lines.get(&edge.line)) {
                    found.push(at("its target is not written on its line"));
                }
            }
        }
    }
    found
}

/// Indexes `files` written under `root`, as a run with no previous
/// generation does; no git needed.
pub fn index(root: &Path, files: &[(String, String)]) -> Vec<FileRows> {
    let tree = write_tree(root, files);
    let plan = plan(&tree, &[], None, &[]);
    let extracted = extract(root, &plan.extract, None);
    link(&tree, &plan, extracted, |_| Ok::<_, ()>(Kept::default()))
        .expect("no store to fail")
        .add
}

/// Writes `files` under `root` and returns them as a scan would.
pub fn write_tree(root: &Path, files: &[(String, String)]) -> Tree {
    let mut tree_files = Vec::new();
    for (path, content) in files {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().expect("a parent")).expect("directories");
        std::fs::write(&full, content).expect("written");
        let lang = crate::lang::wanted(path).expect("a supported file");
        tree_files.push(TreeFile {
            path: path.clone(),
            lang,
            hash: Hash::of(content.as_bytes()),
            bytes: content.len() as u64,
            stat: None,
        });
    }
    tree_files.sort_by(|a, b| a.path.cmp(&b.path));
    Tree {
        root: root.to_path_buf(),
        commit: None,
        dirty: true,
        files: tree_files,
        skipped: Vec::new(),
        untracked: Vec::new(),
        index: None,
        unsettled: false,
    }
}

/// What tells two symbols apart: `path#qualified-name#kind`, the uid
/// without its `~n`. A method and the free function of its name in one
/// file, or a definition and its file, differ.
fn identity(file: &str, name: &str, kind: SymbolKind) -> String {
    format!("{file}#{name}#{kind}")
}

/// The resolved calls of `rows` as `(caller, line, callee)`, each symbol
/// by its identity.
fn resolved_calls(rows: &[FileRows]) -> Expected {
    let by_uid: HashMap<&str, String> = rows
        .iter()
        .flat_map(|r| r.symbols.iter())
        .map(|s| (s.uid.as_str(), identity(&s.file, &s.name, s.kind)))
        .collect();
    rows.iter()
        .flat_map(|r| r.edges.iter())
        .filter(|e| e.kind == EdgeKind::Call)
        .filter_map(|e| {
            let src = by_uid.get(e.src.as_str())?;
            let dst = by_uid.get(e.dst.as_deref()?)?;
            Some((src.clone(), e.line, dst.clone()))
        })
        .collect()
}

type Expected = BTreeSet<(String, u32, String)>;

const POOL: [&str; 5] = ["alpha", "beta", "gamma", "delta", "omega"];

/// A statement of a generated method body.
#[derive(Clone, Debug)]
enum Statement {
    /// `name` with no receiver.
    Bare(usize),
    /// `self.name`.
    OnSelf(usize),
    /// `name = 1` then `name`: a local, not a call.
    Local(usize),
    /// `K{class}.new.name`.
    OnNew(usize, usize),
    /// A name defined nowhere.
    Undefined,
    /// A top-level function defined once (`helper_once`) or twice (`helper_twice`).
    Helper(bool),
}

fn statement(classes: usize) -> impl Strategy<Value = Statement> {
    prop_oneof![
        (0..POOL.len()).prop_map(Statement::Bare),
        (0..POOL.len()).prop_map(Statement::OnSelf),
        (0..POOL.len()).prop_map(Statement::Local),
        (0..classes, 0..POOL.len()).prop_map(|(c, n)| Statement::OnNew(c, n)),
        Just(Statement::Undefined),
        any::<bool>().prop_map(Statement::Helper),
    ]
}

/// One generated class: whether its file requires the helpers, and its
/// methods, each a name and a body.
type RubyClass = (bool, Vec<(usize, Vec<Statement>)>);

/// Classes `K0..Kn`.
fn ruby_classes() -> impl Strategy<Value = Vec<RubyClass>> {
    (1usize..=3).prop_flat_map(|classes| {
        prop::collection::vec(
            (
                any::<bool>(),
                prop::collection::btree_map(
                    0..POOL.len(),
                    prop::collection::vec(statement(classes), 0..4),
                    1..=3,
                )
                .prop_map(|methods| methods.into_iter().collect::<Vec<_>>()),
            ),
            classes,
        )
    })
}

/// The Ruby files of a generated tree and the calls Ruby makes, by the
/// rules of `ruby/mod.rs`: own methods through a fully known chain, a
/// top-level function defined in one file the caller's file requires,
/// `K.new.m` to `K#m`, nothing else.
fn ruby_tree(classes: &[RubyClass]) -> (Vec<(String, String)>, Expected) {
    let mut files = vec![
        (
            "lib/helpers.rb".to_owned(),
            "def helper_once\nend\n\ndef helper_twice\nend\n".to_owned(),
        ),
        (
            "lib/more_helpers.rb".to_owned(),
            "def helper_twice\nend\n".to_owned(),
        ),
    ];
    let mut expected = Expected::new();
    let defines = |class: usize, name: usize| classes[class].1.iter().any(|(m, _)| *m == name);
    let method = |class: usize, name: usize| {
        identity(
            &format!("lib/k{class}.rb"),
            &format!("K{class}#{}", POOL[name]),
            SymbolKind::Method,
        )
    };
    for (i, (requires, methods)) in classes.iter().enumerate() {
        let path = format!("lib/k{i}.rb");
        let mut lines = Vec::new();
        if *requires {
            lines.push("require_relative \"helpers\"".to_owned());
            lines.push("require_relative \"more_helpers\"".to_owned());
        }
        lines.push(format!("class K{i}"));
        for (name, body) in methods {
            let caller = method(i, *name);
            lines.push(format!("  def {}", POOL[*name]));
            // A name assigned earlier in the body is a local from then on.
            let mut locals = HashSet::default();
            for statement in body {
                let line = lines.len() as u32 + 1;
                let mut call = |callee: String| {
                    expected.insert((caller.clone(), line, callee));
                };
                match statement {
                    Statement::Bare(n) | Statement::OnSelf(n) => {
                        let prefix = if matches!(statement, Statement::OnSelf(_)) {
                            "self."
                        } else {
                            ""
                        };
                        lines.push(format!("    {prefix}{}", POOL[*n]));
                        let local = prefix.is_empty() && locals.contains(n);
                        if defines(i, *n) && !local {
                            call(method(i, *n));
                        }
                    }
                    Statement::Local(n) => {
                        lines.push(format!("    {} = 1", POOL[*n]));
                        lines.push(format!("    {}", POOL[*n]));
                        locals.insert(*n);
                    }
                    Statement::OnNew(c, n) => {
                        lines.push(format!("    K{c}.new.{}", POOL[*n]));
                        if defines(*c, *n) {
                            call(method(*c, *n));
                        }
                    }
                    Statement::Undefined => lines.push("    nowhere_defined".to_owned()),
                    Statement::Helper(once) => {
                        let name = if *once { "helper_once" } else { "helper_twice" };
                        lines.push(format!("    {name}"));
                        if *once && *requires {
                            call(identity(
                                "lib/helpers.rb",
                                "helper_once",
                                SymbolKind::Function,
                            ));
                        }
                    }
                }
            }
            lines.push("  end".to_owned());
        }
        lines.push("end".to_owned());
        files.push((path, lines.join("\n") + "\n"));
    }
    (files, expected)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn c10_every_row_of_a_generated_ruby_tree_is_true_of_it(classes in ruby_classes()) {
        let dir = tempfile::tempdir().unwrap();
        let (files, _) = ruby_tree(&classes);
        let rows = index(dir.path(), &files);
        let wrong = violations(dir.path(), &rows);
        prop_assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn c10_a_generated_ruby_tree_resolves_exactly_the_calls_ruby_makes(classes in ruby_classes()) {
        let dir = tempfile::tempdir().unwrap();
        let (files, expected) = ruby_tree(&classes);
        let rows = index(dir.path(), &files);
        prop_assert_eq!(resolved_calls(&rows), expected);
    }
}

/// A statement of a generated Rust function body.
#[derive(Clone, Debug)]
enum RustStatement {
    /// `name();` with no path.
    Bare(usize),
    /// `crate::m{module}::name();`
    Path(usize, usize),
    /// `let name = || (); name();`: a closure shadows the function.
    Closure(usize),
    /// `let v = crate::m{module}::S{module}::new(); v.name();`
    Typed(usize, usize),
    /// `nowhere_defined();`
    Undefined,
}

fn rust_statement(modules: usize) -> impl Strategy<Value = RustStatement> {
    prop_oneof![
        (0..POOL.len()).prop_map(RustStatement::Bare),
        (0..modules, 0..POOL.len()).prop_map(|(m, n)| RustStatement::Path(m, n)),
        (0..POOL.len()).prop_map(RustStatement::Closure),
        (0..modules, 0..POOL.len()).prop_map(|(m, n)| RustStatement::Typed(m, n)),
        Just(RustStatement::Undefined),
    ]
}

/// One generated module: free functions and the methods of its struct,
/// and names defined only under a `#[cfg]` Rust proves false, a function
/// and a method each, which never exist (F2).
#[derive(Clone, Debug)]
struct RustModule {
    functions: Vec<(usize, Vec<RustStatement>)>,
    methods: Vec<usize>,
    dead: Vec<usize>,
}

fn rust_modules() -> impl Strategy<Value = Vec<RustModule>> {
    (1usize..=3).prop_flat_map(|modules| {
        prop::collection::vec(
            (
                prop::collection::btree_map(
                    0..POOL.len(),
                    prop::collection::vec(rust_statement(modules), 0..4),
                    1..=3,
                ),
                prop::collection::btree_set(0..POOL.len(), 0..=3),
                prop::collection::btree_set(0..POOL.len(), 0..=2),
            )
                .prop_map(|(functions, methods, dead)| RustModule {
                    functions: functions.into_iter().collect(),
                    methods: methods.into_iter().collect(),
                    dead: dead.into_iter().collect(),
                }),
            modules,
        )
    })
}

/// A crate `gen-crate` with modules `m0..mn`, and the calls Rust makes: a
/// bare call to a function of the same module, a `crate::` path, an
/// associated `new` and a method on a value whose type the `let` states;
/// never a closure's call, never a name defined nowhere or only under a
/// false `#[cfg]`.
fn rust_tree(modules: &[RustModule]) -> (Vec<(String, String)>, Expected) {
    let mut files = vec![(
        "Cargo.toml".to_owned(),
        "[package]\nname = \"gen-crate\"\nversion = \"0.1.0\"\n".to_owned(),
    )];
    let lib: String = (0..modules.len())
        .map(|i| format!("pub mod m{i};\n"))
        .collect();
    files.push(("src/lib.rs".to_owned(), lib));
    let mut expected = Expected::new();
    for (i, module) in modules.iter().enumerate() {
        let path = format!("src/m{i}.rs");
        let mut lines = Vec::new();
        for (function, body) in &module.functions {
            let caller = identity(&path, POOL[*function], SymbolKind::Function);
            lines.push(format!("pub fn {}() {{", POOL[*function]));
            // A name a `let` binds shadows the function for the rest of the body.
            let mut shadowed = HashSet::default();
            for statement in body {
                let line = lines.len() as u32 + 1;
                let mut call = |line: u32, callee: String| {
                    expected.insert((caller.clone(), line, callee));
                };
                let function = |m: usize, n: usize| {
                    identity(&format!("src/m{m}.rs"), POOL[n], SymbolKind::Function)
                };
                let method = |m: usize, name: &str| {
                    identity(
                        &format!("src/m{m}.rs"),
                        &format!("S{m}::{name}"),
                        SymbolKind::Method,
                    )
                };
                match statement {
                    RustStatement::Bare(n) => {
                        lines.push(format!("    {}();", POOL[*n]));
                        if module.functions.iter().any(|(f, _)| f == n) && !shadowed.contains(n) {
                            call(line, function(i, *n));
                        }
                    }
                    RustStatement::Path(m, n) => {
                        lines.push(format!("    crate::m{m}::{}();", POOL[*n]));
                        if modules[*m].functions.iter().any(|(f, _)| f == n) {
                            call(line, function(*m, *n));
                        }
                    }
                    RustStatement::Closure(n) => {
                        lines.push(format!("    let {} = || ();", POOL[*n]));
                        lines.push(format!("    {}();", POOL[*n]));
                        shadowed.insert(*n);
                    }
                    RustStatement::Typed(m, n) => {
                        lines.push(format!("    let v = crate::m{m}::S{m}::new();"));
                        call(line, method(*m, "new"));
                        lines.push(format!("    v.{}();", POOL[*n]));
                        if modules[*m].methods.contains(n) {
                            call(line + 1, method(*m, POOL[*n]));
                        }
                    }
                    RustStatement::Undefined => lines.push("    nowhere_defined();".to_owned()),
                }
            }
            lines.push("}".to_owned());
        }
        lines.push(format!("pub struct S{i};"));
        lines.push(format!("impl S{i} {{"));
        lines.push(format!("    pub fn new() -> Self {{ S{i} }}"));
        for method in &module.methods {
            lines.push(format!("    pub fn {}(&self) {{}}", POOL[*method]));
        }
        lines.push("}".to_owned());
        for dead in &module.dead {
            if !module.functions.iter().any(|(f, _)| f == dead) {
                lines.push("#[cfg(any())]".to_owned());
                lines.push(format!("pub fn {}() {{}}", POOL[*dead]));
            }
            if !module.methods.contains(dead) {
                lines.push("#[cfg(not(all()))]".to_owned());
                lines.push(format!(
                    "impl S{i} {{ pub fn {}(&self) {{}} }}",
                    POOL[*dead]
                ));
            }
        }
        files.push((path, lines.join("\n") + "\n"));
    }
    (files, expected)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn c10_every_row_of_a_generated_rust_tree_is_true_of_it(modules in rust_modules()) {
        let dir = tempfile::tempdir().unwrap();
        let (files, _) = rust_tree(&modules);
        let rows = index(dir.path(), &files);
        let wrong = violations(dir.path(), &rows);
        prop_assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn c10_a_generated_rust_tree_resolves_exactly_the_calls_rust_makes(modules in rust_modules()) {
        let dir = tempfile::tempdir().unwrap();
        let (files, expected) = rust_tree(&modules);
        let rows = index(dir.path(), &files);
        prop_assert_eq!(resolved_calls(&rows), expected);
    }
}

#[test]
fn the_oracle_catches_a_signature_off_its_line_and_a_site_off_its_span() {
    let dir = tempfile::tempdir().unwrap();
    let mut rows = index(
        dir.path(),
        &[(
            "lib/a.rb".to_owned(),
            "class A\n  def run\n    stop\n  end\nend\n".to_owned(),
        )],
    );
    assert!(violations(dir.path(), &rows).is_empty());
    let file = &mut rows[0];
    let class = file.symbols.iter_mut().find(|s| s.kind != SymbolKind::File);
    if let Some(class) = class {
        class.line += 1;
    }
    file.edges.push(pixel_model::Edge {
        src: file.symbols[0].uid.clone(),
        dst: None,
        kind: EdgeKind::Call,
        tier: pixel_model::Tier::Open,
        line: 99,
        target: "stop".to_owned(),
        context: String::new(),
    });
    let wrong = violations(dir.path(), &rows);
    assert!(
        wrong
            .iter()
            .any(|w| w.contains("outside its source's span")),
        "{wrong:#?}"
    );
}

/// An assignment starts the definition of a constant, or of the class or
/// module a constant is assigned (`X =` before `Struct.new do`), only: a
/// method whose line holds nothing but `X = 1` is not true of the tree
/// (the review's M3).
#[test]
fn c10_the_oracle_takes_an_assignment_for_a_constant_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut rows = index(
        dir.path(),
        &[(
            "lib/a.rb".to_owned(),
            "class A
  LIMIT = 1
  def run; end
end
Pair =
  Struct.new(:a) do
  end
"
            .to_owned(),
        )],
    );
    assert!(violations(dir.path(), &rows).is_empty());
    let run = rows[0]
        .symbols
        .iter_mut()
        .find(|s| s.name == "A#run")
        .expect("A#run");
    run.line = 2;
    run.signature = "LIMIT = 1".to_owned();
    let wrong = violations(dir.path(), &rows);
    assert!(
        wrong
            .iter()
            .any(|w| w.contains("no definition starts on its line")),
        "{wrong:#?}"
    );
}

/// C-10 on a whole repository, as `pixel index` reads it:
/// `PIXEL_MVP_REFERENCE_REPO` names the work tree (the `reference` job of
/// `ci.yml`). Prints the time of each phase and what the rule files made of
/// the tree, per symbol kind and per site kind; with
/// `PIXEL_MVP_REFERENCE_DUMP` it also writes every site, one per line
/// (kind, `path:line`, source, target as written, target `path:line name`
/// or `-`), for sampling resolved edges against the source by hand. Then
/// every row must be true of the tree.
#[test]
#[ignore = "needs PIXEL_MVP_REFERENCE_REPO (the reference job)"]
fn c10_reference_every_row_is_true_of_the_tree() {
    use std::time::Instant;

    let root = std::env::var_os("PIXEL_MVP_REFERENCE_REPO")
        .map(std::path::PathBuf::from)
        .expect("PIXEL_MVP_REFERENCE_REPO names the reference repository");
    let started = Instant::now();
    let tree = crate::tree::scan(&root).expect("a git work tree");
    let scanned = started.elapsed();
    let plan = plan(&tree, &[], None, &[]);
    let started = Instant::now();
    let extracted = extract(&tree.root, &plan.extract, None);
    let extracted_in = started.elapsed();
    let started = Instant::now();
    let delta = link(&tree, &plan, extracted, |_| Ok::<_, ()>(Kept::default())).unwrap();
    let linked_in = started.elapsed();
    assert_eq!(
        delta.add.len(),
        tree.files.len(),
        "with no deadline, every file is read"
    );

    let mut symbols: BTreeMap<String, usize> = BTreeMap::new();
    let mut sites: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut by_uid = HashMap::default();
    for rows in &delta.add {
        for symbol in &rows.symbols {
            *symbols
                .entry(format!("{} {}", rows.file.lang, symbol.kind))
                .or_default() += 1;
            by_uid.insert(symbol.uid.as_str(), symbol);
        }
        for edge in &rows.edges {
            let count = sites
                .entry(format!("{} {}", rows.file.lang, edge.kind))
                .or_default();
            count.0 += 1;
            count.1 += usize::from(edge.dst.is_some());
        }
    }
    eprintln!("{} files at {:?}", delta.add.len(), tree.commit);
    eprintln!("time: scan {scanned:?}, extract {extracted_in:?} (every core), link {linked_in:?}");
    eprintln!("symbols: {symbols:?}");
    for (kind, (all, resolved)) in &sites {
        eprintln!("{kind}: {all} sites, {resolved} resolved");
    }
    if let Some(dump) = std::env::var_os("PIXEL_MVP_REFERENCE_DUMP") {
        let mut out = String::new();
        for edge in delta.add.iter().flat_map(|r| &r.edges) {
            let src = by_uid[edge.src.as_str()];
            let dst = edge.dst.as_deref().map_or_else(
                || "-".to_owned(),
                |uid| {
                    format!(
                        "{}:{} {}",
                        by_uid[uid].file, by_uid[uid].line, by_uid[uid].name
                    )
                },
            );
            out.push_str(&format!(
                "{}\t{}:{}\t{}\t{}\t{dst}\n",
                edge.kind, src.file, edge.line, src.name, edge.target
            ));
        }
        std::fs::write(dump, out).expect("dump written");
    }
    let wrong = violations(&tree.root, &delta.add);
    assert!(
        wrong.is_empty(),
        "{} rows not true of the tree, first: {:#?}",
        wrong.len(),
        &wrong[..wrong.len().min(20)]
    );
}

/// Indexes the generated Rust tree of `modules` and re-points the call at
/// `from` to `to`: the generated-tree check must now fail.
fn repointed_rust_tree_fails(modules: &[RustModule], from: &str, to: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (files, expected) = rust_tree(modules);
    let mut rows = index(dir.path(), &files);
    assert_eq!(resolved_calls(&rows), expected);
    let edge = rows
        .iter_mut()
        .flat_map(|r| r.edges.iter_mut())
        .find(|e| e.dst.as_deref() == Some(from))
        .unwrap_or_else(|| panic!("a call resolved to {from}"));
    edge.dst = Some(to.to_owned());
    assert!(
        rows.iter().flat_map(|r| &r.symbols).any(|s| s.uid == to),
        "{to} is a symbol: only the right answer tells the two apart"
    );
    assert_ne!(resolved_calls(&rows), expected, "{from} re-pointed to {to}");
}

#[test]
fn the_generated_trees_catch_a_method_call_re_pointed_at_a_free_function_of_its_name() {
    // D54: `v.alpha()` linked to the free `alpha` of the same file.
    let modules = [RustModule {
        functions: vec![(0, vec![RustStatement::Typed(0, 0)])],
        methods: vec![0],
        dead: Vec::new(),
    }];
    repointed_rust_tree_fails(
        &modules,
        "src/m0.rs#S0::alpha#method",
        "src/m0.rs#alpha#function",
    );
}

#[test]
fn the_generated_trees_catch_a_call_re_pointed_at_its_file() {
    // D54: any target re-pointed at the File symbol of its file.
    let modules = [RustModule {
        functions: vec![(0, vec![RustStatement::Typed(0, 0)])],
        methods: vec![0],
        dead: Vec::new(),
    }];
    repointed_rust_tree_fails(
        &modules,
        "src/m0.rs#S0::alpha#method",
        "src/m0.rs#src/m0.rs#file",
    );
    repointed_rust_tree_fails(
        &modules,
        "src/m0.rs#S0::new#method",
        "src/m0.rs#src/m0.rs#file",
    );
}

#[test]
fn the_oracle_catches_two_symbols_with_one_uid() {
    // D54, D44: a HashSet of uids must not collapse a duplicate.
    let dir = tempfile::tempdir().unwrap();
    let mut rows = index(
        dir.path(),
        &[(
            "lib/a.rb".to_owned(),
            "class A\n  def run\n  end\n\n  def stop\n  end\nend\n".to_owned(),
        )],
    );
    assert!(violations(dir.path(), &rows).is_empty());
    let symbols = &mut rows[0].symbols;
    let run = symbols
        .iter()
        .find(|s| s.name == "A#run")
        .expect("A#run")
        .uid
        .clone();
    let stop = symbols
        .iter_mut()
        .find(|s| s.name == "A#stop")
        .expect("A#stop");
    stop.uid = run;
    let wrong = violations(dir.path(), &rows);
    assert!(wrong.iter().any(|w| w.contains("not unique")), "{wrong:#?}");
}
