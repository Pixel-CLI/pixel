//! `pixel rename` — IDE-style symbol rename, graph-driven and tree-sitter
//! verified.
//!
//! The graph says WHERE a symbol is named (its declaration line, every
//! resolved call/reference `site_line`, the import bindings that pull it out
//! of its file). For each candidate site the file is re-parsed and the
//! identifier's *role* is verified before its bytes are touched — a name the
//! graph asserted sits on a line but that the parse finds only inside a
//! comment, a string, or an alias target is skipped, not rewritten. Sites the
//! graph could not resolve (`unresolved_calls` carrying the old name) are
//! reported in the plan so the caller sees the honest boundary of the rename.
//!
//! No regex replaces, no whole-word text scan: two same-named symbols in one
//! file only collide if the graph itself confused them.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use serde::Serialize;
use tree_sitter::{Node, Tree};

use crate::extract;
use crate::store::{EdgeKind, GraphStore, SymbolRow};

/// Why a site is being rewritten — the graph evidence that nominated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteKind {
    /// The symbol's own declaration name.
    Definition,
    /// A resolved `Calls` edge's callee identifier.
    Call,
    /// A resolved `References` edge (passed-as-value uses).
    Reference,
    /// The imported name inside a `use`/`import` binding.
    Import,
}

impl SiteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Call => "call",
            Self::Reference => "reference",
            Self::Import => "import",
        }
    }
}

/// One verified identifier rewrite: byte range plus the 1-based line the
/// graph pointed at.
#[derive(Debug, Clone, Serialize)]
pub struct RenameEdit {
    pub line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: SiteKind,
}

/// A site the rename refused to guess at.
#[derive(Debug, Clone, Serialize)]
pub struct RenameSkip {
    pub path: String,
    pub line: u32,
    pub reason: String,
}

/// Per-file edit list plus the sites that were reported instead of rewritten.
#[derive(Debug, Default, Serialize)]
pub struct RenamePlan {
    /// path -> verified edits for that file.
    pub files: BTreeMap<String, Vec<RenameEdit>>,
    pub skipped: Vec<RenameSkip>,
    /// Occurrences of the old name the graph never claimed (comments,
    /// strings, macro text, dynamic dispatch): counted per file so the
    /// answer admits they exist instead of looking byte-clean.
    pub unclaimed_text: BTreeMap<String, u32>,
}

/// A candidate site the graph nominated, before parse verification.
struct Site {
    /// Repo-relative path.
    path: String,
    line: u32,
    kind: SiteKind,
    /// For `Import`: the import's spec text, used to find the right
    /// statement when a file has several, and the path it resolved, which
    /// picks the leaf of a Rust `use` naming several.
    spec: Option<(String, String)>,
}

/// Compute the verified edit set for renaming `sym` to `new_name`.
///
/// `new_name` must already be a plausible identifier (the caller validates);
/// this only checks it differs and does not collide with a sibling symbol of
/// the same name in the same file+kind (which would silently merge two
/// declarations).
pub fn plan(
    store: &GraphStore,
    root: &Path,
    sym: &SymbolRow,
    new_name: &str,
) -> Result<RenamePlan, String> {
    if sym.name == new_name {
        return Err("rename: new name is the old name".to_string());
    }
    let files = file_paths(store)?;
    let def_path = files
        .get(&sym.file_id)
        .cloned()
        .ok_or_else(|| "rename: symbol's file is not in the graph".to_string())?;

    // A same-named symbol in the same file makes the rename self-colliding:
    // after the write both declarations share a name and one file.
    let collision = store
        .symbols_in_file(sym.file_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|s| s.id != sym.id && s.name == new_name);
    if collision {
        return Err(format!(
            "rename: {def_path} already declares a {new_name:?} — renaming would collide"
        ));
    }

    let mut sites: Vec<Site> = vec![Site {
        path: def_path.clone(),
        line: sym.start_line,
        kind: SiteKind::Definition,
        spec: None,
    }];

    for kind in [EdgeKind::Calls, EdgeKind::References] {
        let site_kind = if kind == EdgeKind::Calls {
            SiteKind::Call
        } else {
            SiteKind::Reference
        };
        for edge in store
            .edges_to(sym.id, Some(kind))
            .map_err(|e| e.to_string())?
        {
            // A site that calls the symbol through an import alias writes the
            // alias, which the rename leaves valid: neither an edit nor a skip.
            // Per edge, so a direct call on the same line is still renamed.
            if edge.callee.as_deref().is_some_and(|c| c != sym.name) {
                continue;
            }
            // The edge names the enclosing symbol; its file holds the site.
            let Some(src) = symbol_by_id(store, edge.src_id) else {
                continue;
            };
            let Some(path) = files.get(&src.file_id) else {
                continue;
            };
            sites.push(Site {
                path: path.clone(),
                line: edge.site_line,
                kind: site_kind,
                spec: None,
            });
        }
    }

    for import in store
        .imports_to_file(sym.file_id)
        .map_err(|e| e.to_string())?
    {
        if !import.bindings.iter().any(|b| b.source == sym.name) {
            continue;
        }
        let Some(path) = files.get(&import.file_id) else {
            continue;
        };
        sites.push(Site {
            path: path.clone(),
            line: 0, // located by spec text, not a stored line
            kind: SiteKind::Import,
            spec: Some((import.spec, import.path)),
        });
    }

    let mut plan = RenamePlan::default();
    for u in store
        .unresolved_named(&sym.name)
        .map_err(|e| e.to_string())?
    {
        if let Some(path) = files.get(&u.file_id) {
            plan.skipped.push(RenameSkip {
                path: path.clone(),
                line: u.site_line,
                reason: format!(
                    "unresolved {}: the graph could not prove this is the renamed symbol",
                    u.kind
                ),
            });
        }
    }

    // Group sites by file, then verify each against a fresh parse.
    let mut by_file: BTreeMap<String, Vec<Site>> = BTreeMap::new();
    for site in sites {
        by_file.entry(site.path.clone()).or_default().push(site);
    }
    for (path, sites) in by_file {
        let content = match read_confined(root, &path) {
            Ok(c) => c,
            Err(e) => {
                for s in &sites {
                    plan.skipped.push(RenameSkip {
                        path: path.clone(),
                        line: s.line,
                        reason: format!("file unreadable: {e}"),
                    });
                }
                continue;
            }
        };
        let Some(tree) = extract::parse_file(&path, &content) else {
            for s in &sites {
                plan.skipped.push(RenameSkip {
                    path: path.clone(),
                    line: s.line,
                    reason: "file does not parse with its extension's grammar".to_string(),
                });
            }
            continue;
        };
        let mut edits = Vec::new();
        for site in sites {
            verify_site(&tree, &content, &site, sym, &mut edits, &mut plan, &path);
        }
        // Residual same-name bytes the graph never claimed (comments,
        // strings, macro bodies): counted, not touched.
        let leftover = count_unclaimed_text(&content, &sym.name, &edits);
        if leftover > 0 {
            plan.unclaimed_text.insert(path.clone(), leftover);
        }
        if !edits.is_empty() {
            edits.sort_by_key(|e| e.start_byte);
            edits.dedup_by_key(|e| (e.start_byte, e.end_byte));
            plan.files.insert(path, edits);
        }
    }
    Ok(plan)
}

/// Apply a plan: rewrite each file's verified byte ranges, ascending file
/// order and descending byte order inside a file so earlier rewrites never
/// shift later offsets. Returns the paths written.
pub fn apply(
    root: &Path,
    plan: &RenamePlan,
    old_name: &str,
    new_name: &str,
) -> Result<Vec<String>, String> {
    let mut written = Vec::new();
    for (path, edits) in &plan.files {
        let mut content =
            read_confined(root, path).map_err(|e| format!("rename: cannot read {path}: {e}"))?;
        for edit in edits.iter().rev() {
            // Re-verify before touching bytes: the file may have changed
            // between plan and apply (watcher-driven reindex, a user edit).
            if content.get(edit.start_byte..edit.end_byte) != Some(old_name.as_bytes()) {
                return Err(format!(
                    "rename: {path}:{} no longer holds {old_name:?} — file changed under the rename",
                    edit.line
                ));
            }
            content.splice(
                edit.start_byte..edit.end_byte,
                new_name.as_bytes().iter().copied(),
            );
        }
        write_confined(root, path, &content)
            .map_err(|e| format!("rename: cannot write {path}: {e}"))?;
        written.push(path.clone());
    }
    Ok(written)
}

/// Read the source file at `rel` under `root`. The path comes from the
/// graph database, which a hostile clone can supply: it must stay inside
/// the root and must not be a link.
fn read_confined(root: &Path, rel: &str) -> std::io::Result<Vec<u8>> {
    let mut content = Vec::new();
    pixel_git::nofollow::open_read(&pixel_git::repo_path::confine(root, rel)?)?
        .read_to_end(&mut content)?;
    Ok(content)
}

/// Rewrite the source file at `rel` under `root` in place (its mode and
/// inode kept), under the same confinement as [`read_confined`].
fn write_confined(root: &Path, rel: &str, content: &[u8]) -> std::io::Result<()> {
    pixel_git::nofollow::open_rewrite(&pixel_git::repo_path::confine(root, rel)?)?
        .write_all(content)
}

/// Verify one nominated site against the fresh parse and push its edits.
fn verify_site(
    tree: &Tree,
    content: &[u8],
    site: &Site,
    sym: &SymbolRow,
    edits: &mut Vec<RenameEdit>,
    plan: &mut RenamePlan,
    path: &str,
) {
    let skip = |plan: &mut RenamePlan, reason: &str| {
        plan.skipped.push(RenameSkip {
            path: path.to_string(),
            line: site.line,
            reason: reason.to_string(),
        });
    };
    match site.kind {
        SiteKind::Import => {
            let Some((spec, import_path)) = &site.spec else {
                return;
            };
            let found = import_name_nodes(tree, content, spec, import_path, &sym.name);
            if found.is_empty() {
                skip(
                    plan,
                    "import binding not found in a matching statement (stale graph?)",
                );
                return;
            }
            for node in found {
                edits.push(edit_of(node, site));
            }
        }
        _ => {
            let candidates = identifier_nodes_on_line(tree, content, site.line, &sym.name);
            let chosen = match site.kind {
                SiteKind::Definition => pick_definition(&candidates),
                SiteKind::Call => pick_callee(&candidates),
                SiteKind::Reference => pick_reference(&candidates),
                SiteKind::Import => unreachable!(),
            };
            if chosen.is_empty() {
                skip(
                    plan,
                    "no verified identifier for the asserted site (comment/string/stale graph)",
                );
                return;
            }
            for node in chosen {
                edits.push(edit_of(node, site));
            }
        }
    }
}

fn edit_of(node: Node, site: &Site) -> RenameEdit {
    RenameEdit {
        line: site.line,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        kind: site.kind,
    }
}

/// A node names the symbol textually: named, identifier-ish, exact text.
fn is_identifier_named(node: Node, name: &str, content: &[u8]) -> bool {
    if !node.is_named() {
        return false;
    }
    let kind = node.kind();
    let identifierish = kind.ends_with("identifier")
        || matches!(
            kind,
            "constant" | "simple_identifier" | "alias" | "name" | "property_identifier_pattern"
        );
    identifierish && node.utf8_text(content).is_ok_and(|t| t == name)
}

/// Node kinds whose interior is text, not code: comments and strings. An
/// identifier inside one (`// foo`, an interpolated `"#{foo}"`) is prose,
/// never a reference.
fn is_text_kind(kind: &str) -> bool {
    kind.contains("comment") || kind.contains("string") || kind == "interpreted_string_literal"
}

/// True when an ancestor is a comment or string — bytes that are text, not
/// code, and must never be rewritten as a reference.
fn in_text_node(node: Node) -> bool {
    let mut cur = node;
    loop {
        if is_text_kind(cur.kind()) {
            return true;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

/// Every named identifier-ish node on `line` (1-based) whose text is `name`,
/// excluding comment/string interiors.
fn identifier_nodes_on_line<'t>(
    tree: &'t Tree,
    content: &[u8],
    line: u32,
    name: &str,
) -> Vec<Node<'t>> {
    let row = line.saturating_sub(1) as usize;
    let mut out = Vec::new();
    collect_line_nodes(tree.root_node(), content, row, name, &mut out);
    out
}

/// A node whose line span excludes `row` cannot contain a site on it.
fn node_outside_row(node: Node, row: usize) -> bool {
    node.start_position().row > row || node.end_position().row < row
}

fn collect_line_nodes<'t>(
    node: Node<'t>,
    content: &[u8],
    row: usize,
    name: &str,
    out: &mut Vec<Node<'t>>,
) {
    // Prune subtrees that cannot contain a site on `row`.
    if node_outside_row(node, row) {
        return;
    }
    if is_identifier_named(node, name, content)
        && node.start_position().row == row
        && !in_text_node(node)
    {
        out.push(node);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_line_nodes(child, content, row, name, out);
        }
    }
}

/// The declaration's own name: the node that is the `name`/`declarator`
/// field of a declaration-shaped ancestor. Falls back to a single
/// candidate inside a declaration-shaped parent when grammars use other
/// field names.
fn pick_definition<'t>(candidates: &[Node<'t>]) -> Vec<Node<'t>> {
    for &node in candidates {
        if let Some(parent) = node.parent() {
            for field in ["name", "declarator", "left"] {
                if parent.child_by_field_name(field) == Some(node) {
                    return vec![node];
                }
            }
        }
    }
    let decl_like: Vec<Node> = candidates
        .iter()
        .copied()
        .filter(|n| n.parent().is_some_and(|p| is_decl_parent_kind(p.kind())))
        .collect();
    if decl_like.len() == 1 {
        decl_like
    } else {
        Vec::new()
    }
}

/// The callee of a call on this line: a node that IS its parent's
/// function/name/field/property/attribute/method field, or the first named
/// child of a call-shaped parent.
fn pick_callee<'t>(candidates: &[Node<'t>]) -> Vec<Node<'t>> {
    let callees: Vec<Node> = candidates
        .iter()
        .copied()
        .filter(|n| is_callee_position(*n))
        .collect();
    if !callees.is_empty() {
        return callees;
    }
    // The graph asserted a call here; a lone same-name identifier is that
    // call (covers grammars whose callee field is not in the table).
    if candidates.len() == 1 {
        return vec![candidates[0]];
    }
    Vec::new()
}

/// Parent kinds that declare a name: declarations, definitions, items,
/// declarators, specs, assignments (`let x = …` has no `name` field — the
/// fallback below needs this to find its identifier).
fn is_decl_parent_kind(kind: &str) -> bool {
    kind.contains("declaration")
        || kind.contains("definition")
        || kind.contains("_item")
        || kind.contains("declarator")
        || kind.contains("spec")
        || kind.contains("assignment")
}

/// Parent kinds that wrap a call: `call_expression`, `method_invocation`,
/// `macro_invocation`, …
fn is_call_kind(kind: &str) -> bool {
    kind.contains("call") || kind.contains("invocation")
}

fn is_callee_position(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    for field in [
        "function",
        "name",
        "field",
        "property",
        "attribute",
        "method",
        "type_arguments",
    ] {
        if parent.child_by_field_name(field) == Some(node) {
            return true;
        }
    }
    // `foo(...)`, `foo!(...)`: the callee is the first named child of the
    // call-shaped node when the grammar uses no named field for it.
    is_call_kind(parent.kind()) && parent.named_child(0) == Some(node)
}

/// A passed-as-value reference: identifier in expression position that is
/// neither a declaration name nor the callee the Calls edge already owns.
/// Every same-named identifier on the asserted line in use position is a
/// reference to the renamed symbol.
fn pick_reference<'t>(candidates: &[Node<'t>]) -> Vec<Node<'t>> {
    // Every use-position occurrence is a reference; the declaration's own
    // `name` node is not (the Definition site already owns it).
    candidates
        .iter()
        .copied()
        .filter(|n| !is_declaration_name(*n))
        .collect()
}

fn is_declaration_name(node: Node) -> bool {
    node.parent().is_some_and(|p| {
        p.child_by_field_name("name") == Some(node)
            && (p.kind().contains("declaration")
                || p.kind().contains("definition")
                || p.kind().contains("_item"))
    })
}

/// Inside the file's import statements that carry `spec`, the identifier
/// nodes naming `name` in binding position — never the `as` alias, which is
/// the importer's local name and stays valid as-is. In a Rust `use`, only the
/// leaf whose full path is `path` (the import row's): `use crate::{left::push,
/// right::push as p};` holds two `push` items from two files, and renaming
/// one must leave the other.
fn import_name_nodes<'t>(
    tree: &'t Tree,
    content: &[u8],
    spec: &str,
    path: &str,
    name: &str,
) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    collect_import_nodes(tree.root_node(), content, spec, path, name, &mut out);
    out
}

fn collect_import_nodes<'t>(
    node: Node<'t>,
    content: &[u8],
    spec: &str,
    path: &str,
    name: &str,
    out: &mut Vec<Node<'t>>,
) {
    let is_import = is_import_stmt_kind(node.kind())
        || (node.kind() == "export_statement"
            && node.utf8_text(content).is_ok_and(|t| t.contains("from")));
    if is_import && node.utf8_text(content).is_ok_and(|t| t.contains(spec)) {
        if node.kind() == "use_declaration" {
            use_leaf_name_nodes(node, content, path, name, out);
            return;
        }
        // Within this statement, rewrite name-position identifiers matching
        // the old name; alias-position nodes (the `as X` target) keep the
        // importer's local name.
        collect_binding_names(node, content, name, out);
        return;
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_import_nodes(child, content, spec, path, name, out);
        }
    }
}

/// The source-name node of the leaf of Rust `use` `node` whose path is `path`
/// and whose item is `name`.
fn use_leaf_name_nodes<'t>(
    node: Node<'t>,
    content: &[u8],
    path: &str,
    name: &str,
    out: &mut Vec<Node<'t>>,
) {
    let Some(arg) = node.child_by_field_name("argument") else {
        return;
    };
    for leaf in extract::rust_use_leaves(content, arg) {
        if leaf.path == path
            && leaf.binding.is_some_and(|b| b.source == name)
            && let Some(bytes) = leaf.source_bytes
            && let Some(found) = node.descendant_for_byte_range(bytes.start, bytes.end)
        {
            out.push(found);
        }
    }
}

fn collect_binding_names<'t>(node: Node<'t>, content: &[u8], name: &str, out: &mut Vec<Node<'t>>) {
    if is_identifier_named(node, name, content) && !in_text_node(node) && !is_alias_position(node) {
        out.push(node);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_binding_names(child, content, name, out);
        }
    }
}

/// `foo as bar` — `bar` is the alias the importer chose; it is a different
/// name, not a reference to the renamed symbol.
fn is_alias_position(node: Node) -> bool {
    node.parent()
        .is_some_and(|p| p.child_by_field_name("alias") == Some(node))
}

/// Statement kinds that pull bindings from another file: `import`, `use`,
/// `using`.
fn is_import_stmt_kind(kind: &str) -> bool {
    kind.contains("import") || kind == "use_declaration" || kind.contains("using_directive")
}

/// Whole-word occurrences of `name` in `content` outside the verified edit
/// ranges — comments, doc text, macro bodies. Byte-level and honest.
fn count_unclaimed_text(content: &[u8], name: &str, edits: &[RenameEdit]) -> u32 {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let needle = name.as_bytes();
    let mut count = 0u32;
    // `windows` advances the offset itself: a mutated body cannot stall the
    // scan, and a match at `i` cannot recur inside the same word (every
    // later start in it is preceded by a word byte), so the skip the old
    // hand-rolled index made was never load-bearing.
    for (i, window) in content.windows(needle.len()).enumerate() {
        let after = i + needle.len();
        if window == needle
            && (i == 0 || !is_word(content[i - 1]))
            && (after == content.len() || !is_word(content[after]))
            && !edits
                .iter()
                .any(|e| i >= e.start_byte && after <= e.end_byte)
        {
            count += 1;
        }
    }
    count
}

fn file_paths(store: &GraphStore) -> Result<BTreeMap<i64, String>, String> {
    Ok(store
        .files()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect())
}

fn symbol_by_id(store: &GraphStore, id: i64) -> Option<SymbolRow> {
    store
        .conn()
        .query_row(
            "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
             FROM symbols WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok(SymbolRow {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    file_id: r.get(2)?,
                    name: r.get(3)?,
                    qualified: r.get(4)?,
                    kind: crate::store::SymbolKind::parse(&r.get::<_, String>(5)?),
                    start_line: r.get(6)?,
                    end_line: r.get(7)?,
                    sig: r.get(8)?,
                })
            },
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Word-boundary counting must not match inside a longer identifier.
    #[test]
    fn unclaimed_text_respects_word_boundaries() {
        let content = b"foo foo_bar bar_foo foo\n".as_slice();
        assert_eq!(count_unclaimed_text(content, "foo", &[]), 2);
        let edits = vec![RenameEdit {
            line: 1,
            start_byte: 0,
            end_byte: 3,
            kind: SiteKind::Definition,
        }];
        assert_eq!(count_unclaimed_text(content, "foo", &edits), 1);
    }

    /// The verified edit ranges are applied back-to-front so a same-line
    /// earlier rewrite cannot shift a later one.
    #[test]
    fn apply_writes_ranges_descending() {
        let dir = std::env::temp_dir().join(format!("pixel-rename-apply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), b"fn foo() {}\nfn bar() { foo() }\n").unwrap();
        let mut plan = RenamePlan::default();
        plan.files.insert(
            "a.rs".to_string(),
            vec![
                RenameEdit {
                    line: 1,
                    start_byte: 3,
                    end_byte: 6,
                    kind: SiteKind::Definition,
                },
                RenameEdit {
                    line: 2,
                    start_byte: 23,
                    end_byte: 26,
                    kind: SiteKind::Call,
                },
            ],
        );
        let written = apply(&dir, &plan, "foo", "baz").unwrap();
        assert_eq!(written, vec!["a.rs".to_string()]);
        assert_eq!(
            std::fs::read(dir.join("a.rs")).unwrap(),
            b"fn baz() {}\nfn bar() { baz() }\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file changed between plan and apply is refused, not half-rewritten.
    #[test]
    fn apply_refuses_a_file_that_moved() {
        let dir = std::env::temp_dir().join(format!("pixel-rename-moved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), b"fn foo() {}\n").unwrap();
        let mut plan = RenamePlan::default();
        plan.files.insert(
            "a.rs".to_string(),
            vec![RenameEdit {
                line: 1,
                start_byte: 3,
                end_byte: 6,
                kind: SiteKind::Definition,
            }],
        );
        std::fs::write(dir.join("a.rs"), b"fn qux() {}\n").unwrap();
        assert!(apply(&dir, &plan, "foo", "baz").is_err());
        assert_eq!(std::fs::read(dir.join("a.rs")).unwrap(), b"fn qux() {}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Paths in a rename plan come from the graph database, which a hostile
    /// clone can supply: a link at the file, a `..` path and a path through
    /// a directory link all leave the file outside the root untouched,
    /// content and mode.
    #[test]
    fn apply_should_not_write_outside_the_root_or_through_a_link() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("repo");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim.rs");
        std::fs::write(&victim, b"fn foo() {}\n").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&victim, root.join("a.rs")).unwrap();
        symlink(&outside, root.join("dir")).unwrap();
        for path in ["a.rs", "../outside/victim.rs", "dir/victim.rs"] {
            let mut plan = RenamePlan::default();
            plan.files.insert(
                path.to_string(),
                vec![RenameEdit {
                    line: 1,
                    start_byte: 3,
                    end_byte: 6,
                    kind: SiteKind::Definition,
                }],
            );
            assert!(apply(&root, &plan, "foo", "baz").is_err(), "{path} written");
            assert_eq!(std::fs::read(&victim).unwrap(), b"fn foo() {}\n", "{path}");
            let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o644, "{path}");
        }
    }

    /// The rewrite is in place: an executable file stays executable.
    #[test]
    fn apply_should_keep_the_mode_of_the_rewritten_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool.rs");
        std::fs::write(&file, b"fn foo() {}\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut plan = RenamePlan::default();
        plan.files.insert(
            "tool.rs".to_string(),
            vec![RenameEdit {
                line: 1,
                start_byte: 3,
                end_byte: 6,
                kind: SiteKind::Definition,
            }],
        );
        apply(dir.path(), &plan, "foo", "baz").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"fn baz() {}\n");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    /// A real graph built from source files: a definition, a resolved call,
    /// a named import, and a comment decoy carrying the old name. The extra
    /// `unusedHelper` sibling proves the collision check keys on the NAME,
    /// not on the file merely having another symbol.
    fn fixture() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/login.ts"),
            "export function loginUser(name: string): boolean {\n    return name.length > 0;\n}\n\
             export function unusedHelper(): number { return 0; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/caller.ts"),
            "import { loginUser } from \"./login\";\n\nexport function go(): boolean {\n    // loginUser is validated upstream\n    return loginUser(\"someone\");\n}\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        (dir, GraphStore::open(&db).unwrap())
    }

    fn login_sym(store: &GraphStore) -> SymbolRow {
        store
            .symbols_by_name("loginUser", None, 10)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "loginUser")
            .expect("loginUser must be in the fixture graph")
    }

    /// The plan the CLI test proved end-to-end, asserted at the engine
    /// level: def + call + import edits, comment untouched and counted.
    #[test]
    fn plan_covers_definition_call_and_import_but_not_the_comment() {
        let (dir, store) = fixture();
        let sym = login_sym(&store);
        let plan = plan(&store, dir.path(), &sym, "authenticate").unwrap();
        let caller = plan.files.get("src/caller.ts").expect("caller edits");
        assert!(
            caller.iter().any(|e| e.kind == SiteKind::Import),
            "{caller:?}"
        );
        assert!(
            caller.iter().any(|e| e.kind == SiteKind::Call),
            "{caller:?}"
        );
        let login = plan.files.get("src/login.ts").expect("def edits");
        assert!(
            login.iter().any(|e| e.kind == SiteKind::Definition),
            "{login:?}"
        );
        // The comment occurrence is unclaimed text, not an edit.
        let content = std::fs::read(dir.path().join("src/caller.ts")).unwrap();
        assert!(
            !caller
                .iter()
                .any(|e| &content[e.start_byte..e.end_byte] != b"loginUser"),
            "every edit range must sit on the old name"
        );
        assert_eq!(plan.unclaimed_text.get("src/caller.ts"), Some(&1));
        // login.ts holds only claimed occurrences — no unclaimed entry.
        assert!(!plan.unclaimed_text.contains_key("src/login.ts"));

        let written = apply(dir.path(), &plan, "loginUser", "authenticate").unwrap();
        assert_eq!(written.len(), 2);
        let after = std::fs::read(dir.path().join("src/caller.ts")).unwrap();
        let after = String::from_utf8(after).unwrap();
        assert!(after.contains("authenticate("), "{after}");
        assert!(after.contains("import { authenticate }"), "{after}");
        assert!(after.contains("// loginUser"), "{after}");
    }

    /// `import { loginUser as auth }` then `auth()`: the import's source
    /// name is rewritten, and the call — an Exact edge since the resolver
    /// follows the alias — writes `auth`, which stays valid. It is neither an
    /// edit nor a skip: a skip would report a site the rename cannot verify
    /// when there is nothing to rename there.
    #[test]
    fn plan_rewrites_an_aliased_import_and_leaves_the_aliased_call() {
        let (dir, _store) = fixture();
        std::fs::write(
            dir.path().join("src/caller.ts"),
            "import { loginUser as auth } from \"./login\";\n\nexport function go(): boolean {\n    return auth(\"someone\");\n}\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let sym = login_sym(&store);
        let plan = plan(&store, dir.path(), &sym, "authenticate").unwrap();
        let caller = plan.files.get("src/caller.ts").expect("caller edits");
        let kinds: Vec<SiteKind> = caller.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [SiteKind::Import], "{caller:?}");
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);

        apply(dir.path(), &plan, "loginUser", "authenticate").unwrap();
        let after = std::fs::read_to_string(dir.path().join("src/caller.ts")).unwrap();
        assert!(after.contains("import { authenticate as auth }"), "{after}");
        assert!(after.contains("return auth("), "{after}");
    }

    /// `import { loginUser, loginUser as auth }`, then `auth()` and
    /// `loginUser()` on one line: two edges from one caller and one line. The
    /// alias's call stays, the direct call is renamed — skipping by
    /// `(caller, line)` left it stale while its import was rewritten.
    #[test]
    fn plan_renames_a_direct_call_that_shares_its_line_with_an_aliased_one() {
        let (dir, _store) = fixture();
        std::fs::write(
            dir.path().join("src/caller.ts"),
            "import { loginUser, loginUser as auth } from \"./login\";\n\nexport function go(): boolean {\n    return auth(\"a\") && loginUser(\"b\");\n}\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let sym = login_sym(&store);
        let plan = plan(&store, dir.path(), &sym, "authenticate").unwrap();
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
        apply(dir.path(), &plan, "loginUser", "authenticate").unwrap();
        let after = std::fs::read_to_string(dir.path().join("src/caller.ts")).unwrap();
        assert!(
            after.contains("import { authenticate, authenticate as auth }"),
            "{after}"
        );
        assert!(
            after.contains("return auth(\"a\") && authenticate(\"b\");"),
            "{after}"
        );
    }

    /// `use crate::{left::push, right::push as p};` imports two `push`
    /// items from two files. Renaming `left.rs`'s rewrites its leaf only:
    /// matching the statement by spec alone rewrote every `push` in it.
    #[test]
    fn plan_rewrites_only_the_leaf_of_a_grouped_use_that_names_the_symbol() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub mod left;\npub mod right;\npub mod ship;\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/left.rs"), "pub fn push() {}\n").unwrap();
        std::fs::write(dir.path().join("src/right.rs"), "pub fn push() {}\n").unwrap();
        std::fs::write(
            dir.path().join("src/ship.rs"),
            "use crate::{left::push, right::push as p};\npub fn ship() { push(); p(); }\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let left = store.file_by_path("src/left.rs").unwrap().unwrap().id;
        let sym = store
            .symbols_in_file(left)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "push")
            .unwrap();
        let plan = plan(&store, dir.path(), &sym, "shove").unwrap();
        apply(dir.path(), &plan, "push", "shove").unwrap();
        let after = std::fs::read_to_string(dir.path().join("src/ship.rs")).unwrap();
        assert_eq!(
            after,
            "use crate::{left::shove, right::push as p};\npub fn ship() { shove(); p(); }\n"
        );
    }

    #[test]
    fn plan_rejects_renaming_to_the_same_name() {
        let (dir, store) = fixture();
        let sym = login_sym(&store);
        assert!(plan(&store, dir.path(), &sym, "loginUser").is_err());
    }

    /// A same-file sibling already named `new_name` would silently merge
    /// two declarations; the plan must refuse.
    #[test]
    fn plan_rejects_a_name_that_collides_in_the_defining_file() {
        let (dir, _store) = fixture();
        std::fs::write(
            dir.path().join("src/login.ts"),
            "export function loginUser(name: string): boolean {\n    return name.length > 0;\n}\n\
             export function authenticate(): boolean { return false; }\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let sym = login_sym(&store);
        assert!(plan(&store, dir.path(), &sym, "authenticate").is_err());
    }

    #[test]
    fn site_kind_names_are_stable() {
        assert_eq!(SiteKind::Definition.as_str(), "definition");
        assert_eq!(SiteKind::Call.as_str(), "call");
        assert_eq!(SiteKind::Reference.as_str(), "reference");
        assert_eq!(SiteKind::Import.as_str(), "import");
    }

    /// Kind predicates are the contract the per-grammar verification stands
    /// on; they are asserted directly so a flipped connector cannot hide.
    #[test]
    fn kind_predicates() {
        assert!(is_text_kind("comment"));
        assert!(is_text_kind("block_comment"));
        assert!(is_text_kind("string"));
        assert!(is_text_kind("interpreted_string_literal"));
        assert!(!is_text_kind("identifier"));
        assert!(!is_text_kind("call_expression"));

        assert!(is_decl_parent_kind("function_declaration"));
        assert!(is_decl_parent_kind("let_declaration"));
        assert!(is_decl_parent_kind("class_definition"));
        assert!(is_decl_parent_kind("function_item"));
        assert!(is_decl_parent_kind("variable_declarator"));
        assert!(is_decl_parent_kind("type_spec"));
        assert!(is_decl_parent_kind("assignment"));
        assert!(!is_decl_parent_kind("call_expression"));
        assert!(!is_decl_parent_kind("identifier"));

        assert!(is_call_kind("call_expression"));
        assert!(is_call_kind("method_invocation"));
        assert!(is_call_kind("macro_invocation"));
        assert!(!is_call_kind("identifier"));
        assert!(!is_call_kind("member_expression"));

        assert!(is_import_stmt_kind("import_statement"));
        assert!(is_import_stmt_kind("use_declaration"));
        assert!(is_import_stmt_kind("using_directive"));
        assert!(!is_import_stmt_kind("export_statement"));
        assert!(!is_import_stmt_kind("identifier"));
    }

    /// The line-span prune must fire for a node entirely above or below the
    /// target row — both directions, not just both-at-once.
    #[test]
    fn outside_row_prunes_above_and_below() {
        let src = b"fn a() {}\nfn b() {}\nfn c() {}\n";
        let tree = extract::parse_file("a.rs", src).unwrap();
        let root = tree.root_node();
        let a = root.named_child(0).unwrap();
        let c = root.named_child(2).unwrap();
        assert!(node_outside_row(a, 1)); // row 1 (line 2): a is line 1
        assert!(node_outside_row(c, 1));
        assert!(!node_outside_row(a, 0));
        assert!(!node_outside_row(root, 1));
    }

    /// An identifier interpolated into a string literal is still text.
    #[test]
    fn interpolation_inside_a_string_is_text() {
        let src = b"name = 1\nx = f\"hi {name}\"\n";
        let tree = extract::parse_file("a.py", src).unwrap();
        // Line 2's interpolated `name` is inside a string — not a candidate.
        assert!(identifier_nodes_on_line(&tree, src, 2, "name").is_empty());
        assert_eq!(identifier_nodes_on_line(&tree, src, 1, "name").len(), 1);
    }

    /// A line with only a call has no definition — the field check must not
    /// return the callee just because it is *some* node.
    #[test]
    fn definition_picker_ignores_call_sites() {
        let src = b"foo();\nlet foo = 1;\n";
        let tree = extract::parse_file("a.rs", src).unwrap();
        let call_line = identifier_nodes_on_line(&tree, src, 1, "foo");
        assert_eq!(call_line.len(), 1);
        assert!(pick_definition(&call_line).is_empty());
        // `let foo = 1` — let_declaration has no `name` field; the fallback
        // still finds the declared identifier.
        let let_line = identifier_nodes_on_line(&tree, src, 2, "foo");
        assert_eq!(let_line.len(), 1);
        assert_eq!(pick_definition(&let_line), let_line);
    }

    /// `foo!(...)`: the macro callee is the first named child of
    /// `macro_invocation`, reached only through the call-kind branch.
    #[test]
    fn macro_invocation_callee_is_first_named_child() {
        let src = b"foo!(x);\nfn main() {}\n";
        let tree = extract::parse_file("a.rs", src).unwrap();
        let cands = identifier_nodes_on_line(&tree, src, 1, "foo");
        assert_eq!(cands.len(), 1);
        assert!(is_callee_position(cands[0]));
    }

    /// The `name` field of a NON-declaration parent (an import specifier)
    /// is not a declaration name.
    #[test]
    fn import_binding_is_not_a_declaration_name() {
        let src = b"import { loginUser } from \"./login\";\n";
        let tree = extract::parse_file("a.ts", src).unwrap();
        let nodes = import_name_nodes(&tree, src, "./login", "./login", "loginUser");
        assert_eq!(nodes.len(), 1);
        assert!(!is_declaration_name(nodes[0]));
        // And it IS a reference-position identifier.
        assert_eq!(pick_reference(&nodes), nodes);
    }

    /// `import { other as loginUser }` — the alias carries the OLD name but
    /// is the importer's local binding, not a reference to the symbol.
    #[test]
    fn alias_with_same_name_is_not_rewritten() {
        let src = b"import { other as loginUser } from \"./login\";\n";
        let tree = extract::parse_file("a.ts", src).unwrap();
        assert!(import_name_nodes(&tree, src, "./login", "./login", "loginUser").is_empty());
        // The alias node itself reports as alias position.
        let tree2 = extract::parse_file("a.ts", src).unwrap();
        let all = identifier_nodes_on_line(&tree2, src, 1, "loginUser");
        assert_eq!(all.len(), 1);
        assert!(is_alias_position(all[0]));

        // `import m.n as alias`: the dotted module name is a child of the
        // aliased_import but NOT its last named child — not the alias.
        let src = b"import os.path as loginUser\n";
        let tree = extract::parse_file("a.py", src).unwrap();
        let all = identifier_nodes_on_line(&tree, src, 1, "loginUser");
        assert_eq!(all.len(), 1);
        assert!(is_alias_position(all[0]));
        // `os.path` itself is in name position, never alias position.
        let tree = extract::parse_file("a.py", src).unwrap();
        let os = identifier_nodes_on_line(&tree, src, 1, "os");
        assert!(!os.is_empty());
        assert!(os.iter().all(|n| !is_alias_position(*n)));

        // `from pkg import loginUser as auth` — `loginUser` is the first
        // named child of `aliased_import`, not its last: it names the
        // imported symbol, so it must NOT read as alias position.
        let src = b"from pkg import loginUser as auth\n";
        let tree = extract::parse_file("a.py", src).unwrap();
        let all = identifier_nodes_on_line(&tree, src, 1, "loginUser");
        assert_eq!(all.len(), 1);
        assert!(!is_alias_position(all[0]));
    }

    /// `export { x } from "./m"` is an import-like binding site; a bare
    /// `export { x }` re-exports a local and is not.
    #[test]
    fn reexport_from_is_an_import_site_bare_export_is_not() {
        let src = b"export { loginUser } from \"./login\";\nexport { loginUser };\n";
        let tree = extract::parse_file("a.ts", src).unwrap();
        let nodes = import_name_nodes(&tree, src, "./login", "./login", "loginUser");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].start_position().row, 0);
    }

    /// Word-boundary counting at the exact end of content and inside a
    /// doubled identifier.
    #[test]
    fn unclaimed_text_end_boundary_and_overlap() {
        assert_eq!(count_unclaimed_text(b"x foo", "foo", &[]), 1);
        let edits = vec![RenameEdit {
            line: 1,
            start_byte: 0,
            end_byte: 3,
            kind: SiteKind::Definition,
        }];
        // The second `foo` starts past the first's edit range — claimedness
        // requires the occurrence to fit INSIDE an edit, not merely touch it.
        assert_eq!(count_unclaimed_text(b"foo foo", "foo", &edits), 1);
    }

    /// Call-site position: `foo()` and `x.foo()` are callees, `map(foo)`
    /// passes `foo` as a value, `// foo` is text.
    #[test]
    fn callee_and_reference_positions_differ() {
        let src = b"function f() {\n    foo();\n    x.foo();\n    map(foo);\n    // foo\n}\n";
        let tree = extract::parse_file("a.ts", src).unwrap();
        let bare = identifier_nodes_on_line(&tree, src, 2, "foo");
        assert_eq!(bare.len(), 1);
        assert!(is_callee_position(bare[0]));
        let member = identifier_nodes_on_line(&tree, src, 3, "foo");
        assert_eq!(member.len(), 1, "only the field, not receiver x");
        assert!(is_callee_position(member[0]));
        let arg = identifier_nodes_on_line(&tree, src, 4, "foo");
        assert_eq!(arg.len(), 1);
        assert!(!is_callee_position(arg[0]));
        assert!(identifier_nodes_on_line(&tree, src, 5, "foo").is_empty());
        let picked = pick_callee(&arg);
        assert_eq!(picked, vec![arg[0]]);
        assert_eq!(pick_reference(&arg), vec![arg[0]]);
        assert!(!pick_reference(&bare).is_empty()); // `foo()` also reads as a use
    }

    /// A comment or string containing the name is never a candidate.
    #[test]
    fn comments_and_strings_are_not_identifiers() {
        let src = b"// loginUser\nconst s = \"loginUser\";\nloginUser();\n";
        let tree = extract::parse_file("a.ts", src).unwrap();
        assert!(identifier_nodes_on_line(&tree, src, 1, "loginUser").is_empty());
        assert!(identifier_nodes_on_line(&tree, src, 2, "loginUser").is_empty());
        assert_eq!(
            identifier_nodes_on_line(&tree, src, 3, "loginUser").len(),
            1
        );
    }

    /// `import { foo as bar }` — the binding name is rewritten, the alias
    /// is the importer's local name and stays.
    #[test]
    fn import_alias_target_is_never_rewritten() {
        let src = b"import { loginUser as auth } from \"./login\";\nimport { loginUser } from \"./login\";\n";
        let tree = extract::parse_file("a.ts", src).unwrap();
        let nodes = import_name_nodes(&tree, src, "./login", "./login", "loginUser");
        assert_eq!(nodes.len(), 2);
        // First statement: only the `name` side (`loginUser`), never `auth`.
        assert_eq!(nodes[0].start_position().row, 0);
        assert_eq!(nodes[1].start_position().row, 1);
        for n in &nodes {
            assert_eq!(n.utf8_text(src).unwrap(), "loginUser");
            assert!(!is_alias_position(*n));
        }
    }

    /// A statement importing a different spec must not produce edits.
    #[test]
    fn import_nodes_only_match_the_resolved_spec() {
        let src = b"import { loginUser } from \"./other\";\n";
        let tree = extract::parse_file("a.ts", src).unwrap();
        assert!(import_name_nodes(&tree, src, "./login", "./login", "loginUser").is_empty());
    }

    /// The definition picker picks the `name` field of the declaration —
    /// never a same-named call on the same line.
    #[test]
    fn definition_is_the_declarations_name_field() {
        let src = b"export function loginUser(): boolean { return loginUser(); }\n";
        let tree = extract::parse_file("a.ts", src).unwrap();
        let cands = identifier_nodes_on_line(&tree, src, 1, "loginUser");
        assert_eq!(cands.len(), 2);
        let def = pick_definition(&cands);
        assert_eq!(def.len(), 1);
        assert!(is_declaration_name(def[0]));
        assert!(!is_declaration_name(cands[1]));

        // Rust `fn` items sit under `function_item` — a kind matching only
        // the `_item` alternative, so a connector flip there is observable.
        let src = b"fn loginUser() {}\n";
        let tree = extract::parse_file("a.rs", src).unwrap();
        let cands = identifier_nodes_on_line(&tree, src, 1, "loginUser");
        assert_eq!(cands.len(), 1);
        assert!(is_declaration_name(cands[0]));
    }

    /// Every `unresolved_calls` row carrying the old name lands in
    /// `skipped` — reported, never guessed. And nothing the graph never
    /// resolved produces an edit: only edges and imports nominate sites.
    #[test]
    fn unresolved_same_name_calls_are_reported_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/login.ts"),
            "export function loginUser(): boolean { return true; }\n",
        )
        .unwrap();
        // A call through an object the resolver cannot type — and one it
        // can resolve by name (only one `loginUser` exists in the graph).
        std::fs::write(
            dir.path().join("src/dyn.ts"),
            "declare const api: { loginUser(): boolean };\nexport function f() { return api.loginUser(); }\n",
        )
        .unwrap();
        // A second same-named declaration makes the call unresolvable:
        // the graph cannot tell which `loginUser` `api.loginUser()` means.
        std::fs::write(
            dir.path().join("src/other.ts"),
            "export function loginUser(): number { return 1; }\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let sym = login_sym(&store);
        let unresolved = store.unresolved_named("loginUser").unwrap();
        assert!(
            !unresolved.is_empty(),
            "the ambiguous call must leave an unresolved row"
        );
        let plan = plan(&store, dir.path(), &sym, "authenticate").unwrap();
        for row in &unresolved {
            assert!(
                plan.skipped
                    .iter()
                    .any(|s| s.line == row.site_line && s.reason.contains(&row.kind)),
                "unresolved {}:{} must be reported: {:?}",
                row.file_id,
                row.site_line,
                plan.skipped
            );
        }
        // And each verified edit's bytes still say the old name.
        for (path, edits) in &plan.files {
            let content = std::fs::read(dir.path().join(path)).unwrap();
            for e in edits {
                assert_eq!(&content[e.start_byte..e.end_byte], b"loginUser");
            }
        }
    }
}
