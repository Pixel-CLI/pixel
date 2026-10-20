// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-mvp's Ruby answer, merged site by site with this graph's own: the
//! hybrid of ADR-0037 of the pixel-mvp research repository.
//!
//! pixel-mvp links a Ruby site only when its rules prove the target (C-10
//! there) and abstains everywhere else, which is most sites; this graph
//! links many more by name, a good share of them wrongly. Neither replaces
//! the other, so the merge is per site:
//!
//! - **pixel-mvp resolves the site**: its targets replace this graph's
//!   edges there (a call `exact`, a reference `probable`, the tier every
//!   reference has here). A target with no symbol in this graph leaves the
//!   site an `unresolved_calls` row, so `impact`'s `lower_bound` still
//!   counts it;
//! - **pixel-mvp abstains, or has no such site**: this graph's own rows stay
//!   as they are, edges in the tier this graph gave them;
//! - **both at once** (two sites of one name on one line, one resolved and
//!   one not): which row is which is unknown, so this graph's rows stay and
//!   pixel-mvp's targets are added beside them;
//! - **a site only pixel-mvp has** (an ancestor, a route this graph does not
//!   read): its targets are added, on the symbol enclosing the line.
//!
//! An edge this graph has is therefore dropped only where pixel-mvp proves
//! another target at the same site: every right edge pixel-mvp cannot prove
//! stays (ADR-0037's "at least iso"), and so do the wrong ones it abstains
//! on.
//!
//! A site is a kind (call or reference), a file, a line and the name the
//! site wrote: the method (`pack` for `Box.new.pack`), the class leaf of an
//! association (`Tag`), the `Controller#action` of a route. Sites are paired
//! on all four first. What is left is paired by kind, file, name and
//! enclosing symbol, in line order, when both sides have as many and each of
//! this graph's lines is at or before pixel-mvp's: on a call split over
//! lines this graph records the statement's first line, pixel-mvp the
//! method name's. Anything else stays unpaired, which keeps this graph's
//! rows.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::BTreeSet;

use mvp_model::{Graph, Lang};

use super::{BoxErr, Sym, Unresolved, common_kind, leaf, pair, uid_file};
use crate::store::{EdgeKind, EdgeRow, GraphStore, Tier};

/// One row of this graph at a Ruby site: an edge, or an `unresolved_calls`
/// row when `edge` is `None`.
#[derive(Debug, Clone)]
pub struct V1Row {
    /// The row's id in its table (`edges` or `unresolved_calls`).
    pub id: i64,
    pub file_id: i64,
    pub kind: EdgeKind,
    pub line: u32,
    /// The site's name ([`site_name`]).
    pub name: String,
    pub enclosing: Option<i64>,
    pub receiver: Option<String>,
    /// The name the row stores (`edges.callee`, `unresolved_calls.name`).
    pub callee: String,
    pub edge: Option<EdgeRow>,
}

/// What pixel-mvp holds at one of its Ruby sites, in this graph's ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvpSite {
    pub file_id: i64,
    pub kind: EdgeKind,
    pub line: u32,
    pub name: String,
    /// The receiver and name as this graph spells them, for a row written
    /// at a site this graph does not have.
    pub receiver: Option<String>,
    pub callee: String,
    pub target: Target,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// pixel-mvp left the site unresolved.
    Abstains,
    /// pixel-mvp resolved it, to this symbol of the graph, or to one the
    /// graph does not have.
    Resolved(Option<i64>),
}

/// How one of this graph's sites was decided.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Sites {
    /// pixel-mvp resolved every site of the key.
    pub resolved: usize,
    pub abstained: usize,
    /// Some sites of the key resolved, some not.
    pub mixed: usize,
    /// pixel-mvp has no such site.
    pub no_site: usize,
}

/// What the merge does to the edges of one kind of site.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Edges {
    /// This graph's edges before the merge, and after it.
    pub v1: usize,
    pub hybrid: usize,
    /// Edges both have: this graph's row stays, or is raised to `exact`.
    pub both: usize,
    pub raised_to_exact: usize,
    /// This graph's edges at a site pixel-mvp resolves to another target.
    pub v1_replaced: usize,
    /// This graph's edges at a site pixel-mvp abstains on, mixes, or lacks.
    pub v1_kept: usize,
    /// pixel-mvp's targets added: at a site this graph left unresolved, at a
    /// mixed site, at a site only pixel-mvp has.
    pub mvp_at_unresolved: usize,
    pub mvp_at_mixed: usize,
    pub mvp_only_site: usize,
}

/// The merge, as rows to remove and rows to add ([`merge`]).
#[derive(Debug, Default, Clone)]
pub struct RubyPlan {
    /// Ids of `edges` rows to delete.
    pub remove_edges: Vec<i64>,
    /// The file of each site of `remove_edges`, in its order: what an
    /// update reads to give a file back its rows (`ruby_switch::Undo`),
    /// whether or not the row's symbols still stand.
    pub remove_edge_files: Vec<i64>,
    /// Ids of `unresolved_calls` rows to delete.
    pub remove_unresolved: Vec<i64>,
    pub add_edges: Vec<EdgeRow>,
    /// The file of each site of `add_edges`, in its order.
    pub add_edge_files: Vec<i64>,
    pub add_unresolved: Vec<Unresolved>,
    /// Sites, by how pixel-mvp decided them.
    pub sites: Sites,
    /// Calls, routes, associations and other references.
    pub calls: Edges,
    pub routes: Edges,
    pub associations: Edges,
    pub references: Edges,
    /// pixel-mvp's resolved sites left out: on a target this graph does not
    /// have, or at a line no symbol of this graph encloses.
    pub no_target: usize,
    pub no_caller: usize,
}

/// The name a site wrote, the same on both sides: a route's
/// `Controller#action`, otherwise the last segment of what was called.
pub fn site_name(kind: EdgeKind, receiver: Option<&str>, callee: &str) -> String {
    if kind == EdgeKind::References
        && let Some(controller) = receiver.and_then(|r| r.strip_prefix(ROUTE))
    {
        return format!("{controller}#{callee}");
    }
    leaf(callee.trim_start_matches(['.', '#'])).to_string()
}

/// `receiver` prefix of a route's reference (`extract::ruby_callbacks`).
const ROUTE: &str = ":route ";
/// `receiver` prefix of an association's reference.
const ASSOCIATION: &str = ":association ";

/// Which count a row of `kind` with `receiver` belongs to.
fn counts_of<'a>(plan: &'a mut RubyPlan, kind: EdgeKind, receiver: Option<&str>) -> &'a mut Edges {
    match (kind, receiver) {
        (EdgeKind::Calls, _) => &mut plan.calls,
        (_, Some(r)) if r.starts_with(ROUTE) => &mut plan.routes,
        (_, Some(r)) if r.starts_with(ASSOCIATION) => &mut plan.associations,
        _ => &mut plan.references,
    }
}

/// The tier pixel-mvp's edge takes: a proven call is `exact`; a reference
/// is `probable`, as every reference of this graph is.
fn tier_of(kind: EdgeKind) -> Tier {
    if kind == EdgeKind::Calls {
        Tier::Exact
    } else {
        Tier::Probable
    }
}

/// A site: its kind (`calls`, `references`), file, line and name.
type Key<'s> = (&'static str, i64, u32, &'s str);

/// Sites left unpaired by key, grouped for pairing in line order: kind,
/// file, name and enclosing symbol.
type Bucket<'s> = (&'static str, i64, &'s str, i64);

fn key_of(kind: EdgeKind, file_id: i64, line: u32, name: &str) -> Key<'_> {
    (kind.as_str(), file_id, line, name)
}

/// pixel-mvp's sites of one key.
#[derive(Default)]
struct MvpGroup<'a> {
    sites: Vec<&'a MvpSite>,
}

impl MvpGroup<'_> {
    fn targets(&self) -> Vec<Option<i64>> {
        let mut out: Vec<Option<i64>> = Vec::new();
        for site in &self.sites {
            if let Target::Resolved(t) = site.target
                && !out.contains(&t)
            {
                out.push(t);
            }
        }
        out
    }
    fn abstains(&self) -> bool {
        self.sites.iter().any(|s| s.target == Target::Abstains)
    }
}

/// The enclosing symbol of a group of this graph's rows, when they agree.
fn enclosing_of(rows: &[&V1Row]) -> Option<i64> {
    let first = rows.first()?.enclosing;
    rows.iter().all(|r| r.enclosing == first).then_some(first)?
}

/// Merges this graph's rows at its Ruby sites (`v1`) with pixel-mvp's sites
/// (`mvp`) as the module comment says. `enclosing` names the symbol of this
/// graph enclosing a line of a file, for a site only pixel-mvp has.
pub fn merge(
    v1: &[V1Row],
    mvp: &[MvpSite],
    enclosing: impl Fn(i64, u32) -> Option<i64>,
) -> RubyPlan {
    let mut plan = RubyPlan::default();
    let mut ours: HashMap<Key, Vec<&V1Row>> = HashMap::default();
    for row in v1 {
        ours.entry(key_of(row.kind, row.file_id, row.line, &row.name))
            .or_default()
            .push(row);
    }
    let mut theirs: HashMap<Key, MvpGroup> = HashMap::default();
    for site in mvp {
        theirs
            .entry(key_of(site.kind, site.file_id, site.line, &site.name))
            .or_default()
            .sites
            .push(site);
    }

    // Exact keys, then the rest in line order within one enclosing symbol.
    let mut paired: HashMap<Key, Key> = ours
        .keys()
        .filter(|k| theirs.contains_key(*k))
        .map(|k| (*k, *k))
        .collect();
    let mut ours_left: HashMap<Bucket, Vec<u32>> = HashMap::default();
    for (k, rows) in &ours {
        if let (false, Some(enc)) = (paired.contains_key(k), enclosing_of(rows)) {
            ours_left.entry((k.0, k.1, k.3, enc)).or_default().push(k.2);
        }
    }
    // Only a site whose kind, file and name one of this graph's unpaired
    // sites shares can pair: the enclosing symbol is looked up for those.
    let open: HashSet<(&str, i64, &str)> = ours_left
        .keys()
        .map(|(kind, file, name, _)| (*kind, *file, *name))
        .collect();
    let mut theirs_left: HashMap<Bucket, Vec<u32>> = HashMap::default();
    for k in theirs.keys().filter(|k| !ours.contains_key(*k)) {
        if !open.contains(&(k.0, k.1, k.3)) {
            continue;
        }
        if let Some(enc) = enclosing(k.1, k.2) {
            theirs_left
                .entry((k.0, k.1, k.3, enc))
                .or_default()
                .push(k.2);
        }
    }
    for (bucket, mut lines) in ours_left {
        let Some(mut their_lines) = theirs_left.get(&bucket).cloned() else {
            continue;
        };
        lines.sort_unstable();
        their_lines.sort_unstable();
        if lines.len() == their_lines.len() && lines.iter().zip(&their_lines).all(|(a, b)| a <= b) {
            let (kind, file, name, _) = bucket;
            for (a, b) in lines.into_iter().zip(their_lines) {
                paired.insert((kind, file, a, name), (kind, file, b, name));
            }
        }
    }

    let mut used: HashSet<&Key> = HashSet::default();
    let mut keys: Vec<&Key> = ours.keys().collect();
    keys.sort();
    for key in keys {
        let rows = &ours[key];
        let edges: Vec<(&V1Row, &EdgeRow)> = rows
            .iter()
            .filter_map(|r| r.edge.as_ref().map(|e| (*r, e)))
            .collect();
        for (row, _) in &edges {
            counts_of(&mut plan, row.kind, row.receiver.as_deref()).v1 += 1;
        }
        let Some(group) = paired.get(key).and_then(|k| {
            used.insert(k);
            theirs.get(k)
        }) else {
            plan.sites.no_site += 1;
            for (row, _) in &edges {
                counts_of(&mut plan, row.kind, row.receiver.as_deref()).v1_kept += 1;
            }
            continue;
        };
        let targets = group.targets();
        let first = rows[0];
        let src = edges
            .first()
            .map_or(first.enclosing, |(_, e)| Some(e.src_id));
        let edge_to = |dst: i64| EdgeRow {
            src_id: src.unwrap_or_default(),
            dst_id: dst,
            kind: first.kind,
            tier: tier_of(first.kind),
            site_line: first.line,
            receiver: first.receiver.clone(),
            callee: Some(first.callee.clone()),
        };
        if targets.is_empty() || group.abstains() {
            if targets.is_empty() {
                plan.sites.abstained += 1;
            } else {
                plan.sites.mixed += 1;
            }
            for (row, _) in &edges {
                counts_of(&mut plan, row.kind, row.receiver.as_deref()).v1_kept += 1;
            }
            // A mixed site gains pixel-mvp's targets beside this graph's.
            for dst in targets.iter().flatten() {
                if src.is_some() && !edges.iter().any(|(_, e)| e.dst_id == *dst) {
                    counts_of(&mut plan, first.kind, first.receiver.as_deref()).mvp_at_mixed += 1;
                    plan.add_edges.push(edge_to(*dst));
                    plan.add_edge_files.push(first.file_id);
                }
            }
            continue;
        }
        plan.sites.resolved += 1;
        // One row per pixel-mvp site, as this graph keeps one per site: an
        // edge to each resolved target, this graph's own when it has it, and
        // an `unresolved_calls` row for each target outside this graph.
        let mut wanted: Vec<i64> = group
            .sites
            .iter()
            .filter_map(|s| match s.target {
                Target::Resolved(t) => t,
                Target::Abstains => None,
            })
            .collect();
        let outside = group
            .sites
            .iter()
            .filter(|s| s.target == Target::Resolved(None))
            .count();
        plan.no_target += outside;
        for (row, edge) in &edges {
            let counts = counts_of(&mut plan, row.kind, row.receiver.as_deref());
            if let Some(at) = wanted.iter().position(|t| *t == edge.dst_id) {
                wanted.swap_remove(at);
                counts.both += 1;
                if edge.tier != tier_of(row.kind) {
                    counts.raised_to_exact += 1;
                    plan.remove_edges.push(row.id);
                    plan.remove_edge_files.push(row.file_id);
                    plan.add_edges.push(EdgeRow {
                        tier: tier_of(row.kind),
                        ..(*edge).clone()
                    });
                    plan.add_edge_files.push(row.file_id);
                }
            } else {
                counts.v1_replaced += 1;
                plan.remove_edges.push(row.id);
                plan.remove_edge_files.push(row.file_id);
            }
        }
        wanted.sort_unstable();
        for dst in wanted {
            if src.is_none() {
                plan.no_caller += 1;
                continue;
            }
            counts_of(&mut plan, first.kind, first.receiver.as_deref()).mvp_at_unresolved += 1;
            plan.add_edges.push(edge_to(dst));
            plan.add_edge_files.push(first.file_id);
        }
        let open: Vec<i64> = rows
            .iter()
            .filter(|r| r.edge.is_none())
            .map(|r| r.id)
            .collect();
        plan.remove_unresolved
            .extend(open.iter().skip(outside).copied());
        for _ in open.len()..outside {
            plan.add_unresolved.push(Unresolved {
                file_id: first.file_id,
                name: first.callee.clone(),
                enclosing: src,
                site_line: first.line,
                receiver: first.receiver.clone(),
                kind: super::unresolved_kind(first.kind),
            });
        }
    }

    // Sites only pixel-mvp has.
    let mut left: Vec<(&Key, &MvpGroup)> =
        theirs.iter().filter(|(k, _)| !used.contains(k)).collect();
    left.sort_by(|a, b| a.0.cmp(b.0));
    for (key, group) in left {
        let targets = group.targets();
        plan.no_target += targets.iter().filter(|t| t.is_none()).count();
        let site = group.sites[0];
        let src = if targets.iter().any(Option::is_some) {
            enclosing(key.1, key.2)
        } else {
            None
        };
        for dst in targets.iter().flatten() {
            let Some(src) = src else {
                plan.no_caller += 1;
                continue;
            };
            counts_of(&mut plan, site.kind, site.receiver.as_deref()).mvp_only_site += 1;
            plan.add_edge_files.push(key.1);
            plan.add_edges.push(EdgeRow {
                src_id: src,
                dst_id: *dst,
                kind: site.kind,
                tier: tier_of(site.kind),
                site_line: site.line,
                receiver: site.receiver.clone(),
                callee: Some(site.callee.clone()),
            });
        }
    }
    for kind in [
        &mut plan.calls,
        &mut plan.routes,
        &mut plan.associations,
        &mut plan.references,
    ] {
        kind.hybrid = kind.v1 - kind.v1_replaced
            + kind.mvp_at_unresolved
            + kind.mvp_at_mixed
            + kind.mvp_only_site;
    }
    plan
}

/// Which Ruby files a plan covers.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// Every Ruby file of the graph: a full build's merge.
    All,
    /// These Ruby files, by id: an update's, which merges again only the
    /// files whose rows on either side may have moved
    /// (`ruby_switch::merge_again`). The merge is decided file by file, so
    /// a file's plan is the same whichever others are planned with it.
    Files(&'a HashSet<i64>),
}

impl Scope<'_> {
    /// The file ids this scope reads, among `ruby`: `None` for all.
    fn ids(&self) -> Option<Vec<i64>> {
        match self {
            Scope::All => None,
            Scope::Files(ids) => {
                let mut ids: Vec<i64> = ids.iter().copied().collect();
                ids.sort_unstable();
                Some(ids)
            }
        }
    }
}

const V1_EDGES: &str =
    "SELECT e.id, s.file_id, e.src_id, e.dst_id, e.kind, e.tier, e.site_line, e.receiver,
        coalesce(e.callee, d.name)
   FROM edges e JOIN symbols s ON s.id = e.src_id JOIN files f ON f.id = s.file_id
   JOIN symbols d ON d.id = e.dst_id
  WHERE f.lang = 'ruby' AND e.kind IN ('calls', 'references')";

const V1_OPEN: &str =
    "SELECT u.id, u.file_id, u.name, u.enclosing_symbol_id, u.site_line, u.receiver, u.kind
   FROM unresolved_calls u JOIN files f ON f.id = u.file_id WHERE f.lang = 'ruby'";

fn v1_edge(r: &rusqlite::Row<'_>) -> rusqlite::Result<V1Row> {
    let kind = EdgeKind::parse(&r.get::<_, String>(4)?);
    let receiver: Option<String> = r.get(7)?;
    let callee: String = r.get(8)?;
    let edge = EdgeRow {
        src_id: r.get(2)?,
        dst_id: r.get(3)?,
        kind,
        tier: Tier::parse(&r.get::<_, String>(5)?),
        site_line: r.get(6)?,
        receiver: receiver.clone(),
        callee: Some(callee.clone()),
    };
    Ok(V1Row {
        id: r.get(0)?,
        file_id: r.get(1)?,
        kind,
        line: edge.site_line,
        name: site_name(kind, receiver.as_deref(), &callee),
        enclosing: Some(edge.src_id),
        receiver,
        callee,
        edge: Some(edge),
    })
}

fn v1_open(r: &rusqlite::Row<'_>) -> rusqlite::Result<V1Row> {
    let kind = EdgeKind::parse(&r.get::<_, String>(6)?);
    let receiver: Option<String> = r.get(5)?;
    let callee: String = r.get(2)?;
    Ok(V1Row {
        id: r.get(0)?,
        file_id: r.get(1)?,
        kind,
        line: r.get(4)?,
        name: site_name(kind, receiver.as_deref(), &callee),
        enclosing: r.get(3)?,
        receiver,
        callee,
        edge: None,
    })
}

/// This graph's rows at its Ruby sites: every `calls` and `references`
/// edge from a symbol of a Ruby file, every `unresolved_calls` row of one.
pub fn v1_rows(store: &GraphStore) -> Result<Vec<V1Row>, BoxErr> {
    v1_rows_in(store, Scope::All)
}

/// [`v1_rows`] of the files of `scope`.
pub fn v1_rows_in(store: &GraphStore, scope: Scope<'_>) -> Result<Vec<V1Row>, BoxErr> {
    let conn = store.conn();
    let mut out = Vec::new();
    match scope.ids() {
        None => {
            let mut stmt = conn.prepare(V1_EDGES)?;
            for row in stmt.query_map([], v1_edge)? {
                out.push(row?);
            }
            let mut stmt = conn.prepare(V1_OPEN)?;
            for row in stmt.query_map([], v1_open)? {
                out.push(row?);
            }
        }
        Some(ids) => {
            let mut edges = conn.prepare(&format!("{V1_EDGES} AND s.file_id = ?1"))?;
            let mut open = conn.prepare(&format!("{V1_OPEN} AND u.file_id = ?1"))?;
            for id in ids {
                for row in edges.query_map([id], v1_edge)? {
                    out.push(row?);
                }
                for row in open.query_map([id], v1_open)? {
                    out.push(row?);
                }
            }
        }
    }
    Ok(out)
}

/// The receiver and name of a pixel-mvp Ruby target as this graph writes
/// them: `Box.new.pack` is `Box.new` and `pack`; a route `C#action` is
/// `:route C` and `action`; an association to `Billing::Invoice` is
/// `:association ? Billing::Invoice` and `Invoice` (pixel-mvp's target does
/// not say which macro, which this graph's own rows put where `?` is).
fn spelled(kind: mvp_model::EdgeKind, target: &str) -> (Option<String>, String) {
    if kind == mvp_model::EdgeKind::Route
        && let Some((controller, action)) = target.rsplit_once('#')
    {
        return (Some(format!("{ROUTE}{controller}")), action.to_string());
    }
    if kind == mvp_model::EdgeKind::Association {
        return (
            Some(format!("{ASSOCIATION}? {target}")),
            leaf(target).to_string(),
        );
    }
    let name = leaf(target).to_string();
    let receiver = target
        .strip_suffix(name.as_str())
        .map(|r| r.trim_end_matches(['.', ':', '#']))
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    (receiver, name)
}

/// What pixel-mvp's index (`mvp`) holds at the Ruby sites of `store`, in
/// `store`'s ids. Reads both, writes neither.
pub fn mvp_sites(store: &GraphStore, mvp: &mvp_store::Store) -> Result<Vec<MvpSite>, BoxErr> {
    mvp_sites_in(store, mvp, Scope::All)
}

/// This graph's Ruby symbols of the files `paths` names (all, for
/// `None`), as the pairing reads them.
fn v1_symbols(
    store: &GraphStore,
    paths: Option<&BTreeSet<String>>,
) -> Result<Vec<Sym<i64>>, BoxErr> {
    const SQL: &str = "SELECT s.id, f.path, s.qualified, s.kind, s.start_line FROM symbols s
       JOIN files f ON f.id = s.file_id WHERE f.lang = 'ruby'";
    let sym = |r: &rusqlite::Row<'_>| {
        Ok(Sym {
            key: r.get::<_, i64>(0)?,
            path: r.get(1)?,
            name: r.get(2)?,
            kind: common_kind(&r.get::<_, String>(3)?).to_string(),
            line: r.get(4)?,
        })
    };
    let mut out = Vec::new();
    match paths {
        None => {
            let mut stmt = store.conn().prepare(SQL)?;
            for row in stmt.query_map([], sym)? {
                out.push(row?);
            }
        }
        Some(paths) => {
            let mut stmt = store.conn().prepare(&format!("{SQL} AND f.path = ?1"))?;
            for path in paths {
                for row in stmt.query_map([path], sym)? {
                    out.push(row?);
                }
            }
        }
    }
    Ok(out)
}

/// [`mvp_sites`] of the files of `scope`. The symbols are paired file by
/// file (`pair`), so pairing the files of the scope and those its sites
/// resolve into gives each the pairs a pairing of every file gives it.
pub fn mvp_sites_in(
    store: &GraphStore,
    mvp: &mvp_store::Store,
    scope: Scope<'_>,
) -> Result<Vec<MvpSite>, BoxErr> {
    let files: HashMap<String, i64> = store
        .files()?
        .into_iter()
        .filter(|f| f.lang == "ruby")
        .map(|f| (f.path, f.id))
        .collect();
    let (edges, paths) = match scope.ids() {
        None => (mvp.all_edges()?, None),
        Some(ids) => {
            let wanted: HashSet<i64> = ids.into_iter().collect();
            let mut scoped: Vec<&String> = files
                .iter()
                .filter(|(_, id)| wanted.contains(id))
                .map(|(path, _)| path)
                .collect();
            scoped.sort();
            let mut edges = Vec::new();
            for path in scoped {
                edges.extend(mvp.edges_of(path)?);
            }
            let mut paths: BTreeSet<String> = BTreeSet::new();
            for edge in &edges {
                paths.insert(uid_file(&edge.src));
                if let Some(dst) = edge.dst.as_deref() {
                    paths.insert(uid_file(dst));
                }
            }
            paths.retain(|path| files.contains_key(path));
            (edges, Some(paths))
        }
    };
    let ours = v1_symbols(store, paths.as_ref())?;
    let mut theirs: Vec<Sym<String>> = Vec::new();
    // The files pixel-mvp reads as Ruby, as the pairing of every file
    // takes them.
    let mvp_files: Vec<String> = mvp
        .files()?
        .into_iter()
        .filter(|f| f.lang == Lang::Ruby && files.contains_key(&f.path))
        .filter(|f| paths.as_ref().is_none_or(|paths| paths.contains(&f.path)))
        .map(|f| f.path)
        .collect();
    for path in mvp_files {
        for symbol in mvp.symbols(&path)? {
            theirs.push(Sym {
                kind: common_kind(symbol.kind.as_str()).to_string(),
                key: symbol.uid,
                path: symbol.file,
                name: symbol.name,
                line: symbol.line,
            });
        }
    }
    let paired = pair(&ours, &theirs);
    let mut out = Vec::new();
    for edge in edges {
        let kind = match edge.kind {
            mvp_model::EdgeKind::Call => EdgeKind::Calls,
            mvp_model::EdgeKind::Reference
            | mvp_model::EdgeKind::Route
            | mvp_model::EdgeKind::Association => EdgeKind::References,
            mvp_model::EdgeKind::Import => continue,
        };
        let Some(&file_id) = files.get(&uid_file(&edge.src)) else {
            continue;
        };
        let (receiver, callee) = spelled(edge.kind, &edge.target);
        let name = site_name(kind, receiver.as_deref(), &callee);
        let target = match edge.dst.as_deref() {
            None => Target::Abstains,
            Some(dst) => Target::Resolved(paired.get(dst).copied()),
        };
        out.push(MvpSite {
            file_id,
            kind,
            line: edge.line,
            name,
            receiver,
            callee,
            target,
        });
    }
    Ok(out)
}

/// The smallest symbol of `store` enclosing each line of each Ruby file,
/// as [`merge`] asks for it.
pub fn spans(store: &GraphStore) -> Result<impl Fn(i64, u32) -> Option<i64>, BoxErr> {
    spans_in(store, Scope::All)
}

/// [`spans`] of the files of `scope`.
pub fn spans_in(
    store: &GraphStore,
    scope: Scope<'_>,
) -> Result<impl Fn(i64, u32) -> Option<i64> + use<>, BoxErr> {
    const SQL: &str = "SELECT s.file_id, s.start_line, s.end_line, s.id FROM symbols s
       JOIN files f ON f.id = s.file_id WHERE f.lang = 'ruby'";
    let mut by_file: HashMap<i64, Vec<(u32, u32, i64)>> = HashMap::default();
    let span = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, u32, u32, i64)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    };
    let mut rows = Vec::new();
    match scope.ids() {
        None => {
            let mut stmt = store.conn().prepare(SQL)?;
            for row in stmt.query_map([], span)? {
                rows.push(row?);
            }
        }
        Some(ids) => {
            let mut stmt = store.conn().prepare(&format!("{SQL} AND s.file_id = ?1"))?;
            for id in ids {
                for row in stmt.query_map([id], span)? {
                    rows.push(row?);
                }
            }
        }
    }
    for (file, start, end, id) in rows {
        by_file.entry(file).or_default().push((start, end, id));
    }
    Ok(move |file: i64, line: u32| {
        by_file
            .get(&file)?
            .iter()
            .filter(|(start, end, _)| *start <= line && line <= *end)
            .min_by_key(|(start, end, id)| (end - start, *id))
            .map(|(_, _, id)| *id)
    })
}

/// The merge of `store`'s Ruby rows with pixel-mvp's index (`mvp`). Reads
/// both, writes neither.
pub fn ruby_plan(store: &GraphStore, mvp: &mvp_store::Store) -> Result<RubyPlan, BoxErr> {
    ruby_plan_in(store, mvp, Scope::All)
}

/// [`ruby_plan`] of the files of `scope`: the rows a plan of every file
/// gives them, and nothing for the others.
pub fn ruby_plan_in(
    store: &GraphStore,
    mvp: &mvp_store::Store,
    scope: Scope<'_>,
) -> Result<RubyPlan, BoxErr> {
    let v1 = v1_rows_in(store, scope)?;
    let sites = mvp_sites_in(store, mvp, scope)?;
    Ok(merge(&v1, &sites, spans_in(store, scope)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: i64 = 7;

    fn edge_row(id: i64, line: u32, name: &str, src: i64, dst: i64, tier: Tier) -> V1Row {
        V1Row {
            id,
            file_id: FILE,
            kind: EdgeKind::Calls,
            line,
            name: name.to_string(),
            enclosing: Some(src),
            receiver: None,
            callee: name.to_string(),
            edge: Some(EdgeRow {
                src_id: src,
                dst_id: dst,
                kind: EdgeKind::Calls,
                tier,
                site_line: line,
                receiver: None,
                callee: Some(name.to_string()),
            }),
        }
    }

    fn open_row(id: i64, line: u32, name: &str, enclosing: i64) -> V1Row {
        V1Row {
            id,
            file_id: FILE,
            kind: EdgeKind::Calls,
            line,
            name: name.to_string(),
            enclosing: Some(enclosing),
            receiver: Some("item".to_string()),
            callee: name.to_string(),
            edge: None,
        }
    }

    fn site(line: u32, name: &str, target: Target) -> MvpSite {
        MvpSite {
            file_id: FILE,
            kind: EdgeKind::Calls,
            line,
            name: name.to_string(),
            receiver: None,
            callee: name.to_string(),
            target,
        }
    }

    fn nowhere(_: i64, _: u32) -> Option<i64> {
        None
    }

    /// `(src, dst, line, tier)` of each edge a plan adds.
    fn added(plan: &RubyPlan) -> Vec<(i64, i64, u32, Tier)> {
        plan.add_edges
            .iter()
            .map(|e| (e.src_id, e.dst_id, e.site_line, e.tier))
            .collect()
    }

    #[test]
    fn a_site_pixel_mvp_proves_replaces_the_guess_this_graph_made_there() {
        // `title` in Card#label (1): this graph guessed Book#title (20) by
        // name; pixel-mvp proves Card#title (30).
        let v1 = [edge_row(100, 8, "title", 1, 20, Tier::Probable)];
        let plan = merge(
            &v1,
            &[site(8, "title", Target::Resolved(Some(30)))],
            nowhere,
        );
        assert_eq!(plan.remove_edges, vec![100]);
        assert_eq!(added(&plan), vec![(1, 30, 8, Tier::Exact)]);
        assert_eq!(plan.calls.v1_replaced, 1);
        assert_eq!((plan.calls.v1, plan.calls.hybrid), (1, 1));
        assert_eq!(plan.sites.resolved, 1);
    }

    #[test]
    fn a_site_pixel_mvp_abstains_on_or_lacks_keeps_this_graphs_edge_in_its_tier() {
        // `item.save` (line 20) abstained; `helper` (line 30) is not a site
        // of pixel-mvp at all. Both edges are this graph's to keep, as they
        // are: that is what keeps every right edge pixel-mvp cannot prove.
        let v1 = [
            edge_row(100, 20, "save", 1, 21, Tier::Probable),
            edge_row(101, 30, "helper", 1, 22, Tier::Exact),
        ];
        let plan = merge(&v1, &[site(20, "save", Target::Abstains)], nowhere);
        assert!(
            plan.remove_edges.is_empty() && plan.add_edges.is_empty(),
            "{plan:?}"
        );
        assert!(plan.remove_unresolved.is_empty() && plan.add_unresolved.is_empty());
        assert_eq!(plan.calls.v1_kept, 2);
        assert_eq!((plan.sites.abstained, plan.sites.no_site), (1, 1));
    }

    #[test]
    fn a_target_outside_this_graph_leaves_the_site_unresolved_not_guessed() {
        // pixel-mvp proves `title` is an `attr_accessor *FIELDS` reader this
        // graph has no symbol for: this graph's guess goes, and the site is
        // still counted as unresolved for `impact`'s lower bound.
        let v1 = [edge_row(100, 8, "title", 1, 20, Tier::Probable)];
        let plan = merge(&v1, &[site(8, "title", Target::Resolved(None))], nowhere);
        assert_eq!(plan.remove_edges, vec![100]);
        assert!(plan.add_edges.is_empty());
        assert_eq!(plan.add_unresolved.len(), 1);
        let u = &plan.add_unresolved[0];
        assert_eq!(
            (u.file_id, u.name.as_str(), u.enclosing, u.site_line),
            (FILE, "title", Some(1), 8)
        );
        assert_eq!(plan.no_target, 1);
    }

    #[test]
    fn a_site_this_graph_left_unresolved_takes_pixel_mvps_edge_on_its_own_row() {
        let v1 = [open_row(500, 12, "render", 2)];
        let plan = merge(
            &v1,
            &[site(12, "render", Target::Resolved(Some(40)))],
            nowhere,
        );
        assert_eq!(plan.remove_unresolved, vec![500]);
        assert_eq!(added(&plan), vec![(2, 40, 12, Tier::Exact)]);
        // The row keeps the receiver and name this graph recorded.
        assert_eq!(plan.add_edges[0].receiver.as_deref(), Some("item"));
        assert_eq!(plan.add_edges[0].callee.as_deref(), Some("render"));
        assert_eq!(plan.calls.mvp_at_unresolved, 1);
    }

    #[test]
    fn agreement_raises_a_probable_call_to_exact_and_leaves_an_exact_one_alone() {
        let v1 = [
            edge_row(100, 5, "compute", 1, 20, Tier::Probable),
            edge_row(101, 6, "total", 1, 21, Tier::Exact),
        ];
        let mvp = [
            site(5, "compute", Target::Resolved(Some(20))),
            site(6, "total", Target::Resolved(Some(21))),
        ];
        let plan = merge(&v1, &mvp, nowhere);
        assert_eq!(plan.remove_edges, vec![100]);
        assert_eq!(added(&plan), vec![(1, 20, 5, Tier::Exact)]);
        assert_eq!((plan.calls.both, plan.calls.raised_to_exact), (2, 1));
    }

    #[test]
    fn two_sites_of_one_name_on_one_line_keep_one_row_each() {
        // `f(purchase.a, purchase.b)`: two `purchase` calls on line 9.
        let v1 = [
            edge_row(100, 9, "purchase", 1, 20, Tier::Exact),
            edge_row(101, 9, "purchase", 1, 20, Tier::Exact),
        ];
        let mvp = [
            site(9, "purchase", Target::Resolved(Some(20))),
            site(9, "purchase", Target::Resolved(Some(20))),
        ];
        let plan = merge(&v1, &mvp, nowhere);
        assert!(
            plan.remove_edges.is_empty() && plan.add_edges.is_empty(),
            "{plan:?}"
        );
        assert_eq!(plan.calls.both, 2);
    }

    #[test]
    fn a_key_both_resolved_and_abstained_keeps_this_graphs_rows_and_adds_pixel_mvps() {
        // Which of this graph's rows is the resolved site is unknown.
        let v1 = [edge_row(100, 4, "save", 1, 20, Tier::Probable)];
        let mvp = [
            site(4, "save", Target::Resolved(Some(30))),
            site(4, "save", Target::Abstains),
        ];
        let plan = merge(&v1, &mvp, nowhere);
        assert!(plan.remove_edges.is_empty());
        assert_eq!(added(&plan), vec![(1, 30, 4, Tier::Exact)]);
        assert_eq!(
            (
                plan.sites.mixed,
                plan.calls.v1_kept,
                plan.calls.mvp_at_mixed
            ),
            (1, 1, 1)
        );
    }

    #[test]
    fn a_call_split_over_lines_pairs_this_graphs_first_line_with_pixel_mvps_name_line() {
        // `Box.new(\n size: 1\n).pack`: this graph writes line 22, pixel-mvp
        // line 24. Paired, they are one edge, not two.
        let v1 = [edge_row(100, 22, "pack", 1, 20, Tier::Probable)];
        let in_boxed =
            |file: i64, line: u32| (file == FILE && (21..=25).contains(&line)).then_some(1);
        let plan = merge(
            &v1,
            &[site(24, "pack", Target::Resolved(Some(20)))],
            in_boxed,
        );
        assert_eq!(added(&plan), vec![(1, 20, 22, Tier::Exact)]);
        assert_eq!(plan.calls.both, 1);
        assert_eq!(plan.calls.mvp_only_site, 0);
        // pixel-mvp's line before this graph's is not that shape: unpaired,
        // this graph's edge stays and pixel-mvp's is its own site.
        let plan = merge(
            &v1,
            &[site(21, "pack", Target::Resolved(Some(20)))],
            in_boxed,
        );
        assert!(plan.remove_edges.is_empty());
        assert_eq!(added(&plan), vec![(1, 20, 21, Tier::Exact)]);
        assert_eq!((plan.calls.v1_kept, plan.calls.mvp_only_site), (1, 1));
    }

    #[test]
    fn a_site_only_pixel_mvp_has_hangs_on_the_enclosing_symbol_or_is_dropped() {
        let mut route = site(2, "CardsController#show", Target::Resolved(Some(50)));
        route.kind = EdgeKind::References;
        route.receiver = Some(":route CardsController".to_string());
        route.callee = "show".to_string();
        let file_symbol = |_: i64, _: u32| Some(9);
        let plan = merge(&[], std::slice::from_ref(&route), file_symbol);
        assert_eq!(added(&plan), vec![(9, 50, 2, Tier::Probable)]);
        assert_eq!(
            plan.add_edges[0].receiver.as_deref(),
            Some(":route CardsController")
        );
        assert_eq!((plan.routes.mvp_only_site, plan.routes.hybrid), (1, 1));
        let plan = merge(&[], &[route], nowhere);
        assert!(plan.add_edges.is_empty());
        assert_eq!(plan.no_caller, 1);
    }

    #[test]
    fn a_site_is_named_alike_on_both_sides() {
        assert_eq!(site_name(EdgeKind::Calls, Some("Box.new"), "pack"), "pack");
        assert_eq!(
            site_name(
                EdgeKind::References,
                Some(":route Admin::OrdersController"),
                "show"
            ),
            "Admin::OrdersController#show"
        );
        assert_eq!(
            site_name(
                EdgeKind::References,
                Some(":association has_many Push::Subscription"),
                "Subscription"
            ),
            "Subscription"
        );
        assert_eq!(
            spelled(mvp_model::EdgeKind::Call, "Box.new.pack"),
            (Some("Box.new".to_string()), "pack".to_string())
        );
        assert_eq!(
            spelled(mvp_model::EdgeKind::Call, "title"),
            (None, "title".to_string())
        );
        assert_eq!(
            spelled(mvp_model::EdgeKind::Route, "Admin::OrdersController#show"),
            (
                Some(":route Admin::OrdersController".to_string()),
                "show".to_string()
            )
        );
        assert_eq!(
            spelled(mvp_model::EdgeKind::Association, "Push::Subscription"),
            (
                Some(":association ? Push::Subscription".to_string()),
                "Subscription".to_string()
            )
        );
    }
}

#[cfg(test)]
mod audited_contract_tests {
    use super::*;

    #[test]
    fn file_scopes_should_sort_the_exact_ids_and_keep_an_empty_scope_distinct_from_all() {
        assert_eq!(Scope::All.ids(), None);
        assert_eq!(Scope::Files(&HashSet::default()).ids(), Some(vec![]));
        assert_eq!(
            Scope::Files(&HashSet::from_iter([7, 2, 4])).ids(),
            Some(vec![2, 4, 7])
        );
    }

    #[test]
    fn v1_rows_should_read_resolved_and_unresolved_ruby_sites() {
        let store = GraphStore::open_in_memory().unwrap();
        store.conn().execute_batch("INSERT INTO files(id,path,blob_oid,lang) VALUES(1,'a.rb','oid','ruby'),(2,'b.rs','oid','rust');
          INSERT INTO symbols(id,uid,file_id,name,qualified,kind,start_line,end_line) VALUES(1,'a',1,'a','A#a','method',1,5),(2,'b',1,'b','A#b','method',6,8);
          INSERT INTO edges(src_id,dst_id,kind,tier,site_line,callee) VALUES(1,2,'calls','exact',2,'b');").unwrap();
        store
            .insert_unresolved_call(1, "missing", Some(1), 3, None, "references")
            .unwrap();
        let rows = v1_rows(&store).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            v1_rows_in(&store, Scope::Files(&HashSet::default()))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            v1_rows_in(&store, Scope::Files(&HashSet::from_iter([1])))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn mvp_sites_should_pair_real_ruby_calls_and_ignore_files_absent_from_the_graph() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        crate::testutil::init_repo(
            &root,
            &[
                (
                    "app/a.rb",
                    "class A\n def save; end\n def run; save; end\nend\n",
                ),
                ("orphan.rb", "def other; other; end\n"),
            ],
        );
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("app/a.rb", "oid", "ruby").unwrap();
        let save = store
            .insert_symbol(
                file,
                "save",
                "save",
                "A#save",
                crate::store::SymbolKind::Method,
                2,
                2,
                "def save; end",
            )
            .unwrap();
        store
            .insert_symbol(
                file,
                "run",
                "run",
                "A#run",
                crate::store::SymbolKind::Method,
                3,
                3,
                "def run; save; end",
            )
            .unwrap();
        let mvp = super::super::refresh(&root, &root.join(".pixel")).unwrap();
        let sites = mvp_sites(&store, &mvp).unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(
            (sites[0].file_id, sites[0].line, sites[0].name.as_str()),
            (file, 3, "save")
        );
        assert!(matches!(sites[0].target,Target::Resolved(Some(id)) if id==save));
    }
}
