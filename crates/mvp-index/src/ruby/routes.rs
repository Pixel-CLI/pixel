// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Rails routes: `config/routes.rb` and drawn files to controller actions.
//!
//! The literal Rails routing DSL, read statically (v1
//! `crates/pixel-graph/src/extract/ruby_routes.rs`): verb declarations
//! (`get`, `post`, `put`, `patch`, `delete`, `match ... via:`), `root`,
//! `resources` and `resource` with `only:`, `except:`, `controller:`,
//! `path:`, `param:`, `member` and `collection`, `namespace`, `scope` (path
//! and `module:`), `controller` blocks, and `mount`. Each route keeps its
//! verb, its path as Rails composes it and, when the declaration names it
//! literally, the `controller#action` it dispatches to. A path or target
//! that is not a literal (a variable, an interpolated string, a lambda
//! `to:`) yields no route, never a guessed one; `concern`, `concerns`,
//! `direct` and `resolve` are not modelled. `draw :name` reads
//! `config/routes/name.rb` in the scope of the draw (D04): a drawn file's
//! controllers are read in the module [`DRAWN`], which resolution replaces
//! with the module of the one scope every draw of the file is written in
//! ([`Fact::Draw`]).
//!
//! Two changes from v1: a `%i[...]` or `%w[...]` list is read like `[...]`
//! wherever a literal list is (`only:`, `except:`, `via:`); v1 dropped those
//! declarations whole (MVP decision 7). A region the grammar could not
//! parse yields no route, where v1 read its declarations in the scope
//! around it.
//! <https://guides.rubyonrails.org/routing.html>

use std::collections::HashSet;

use mvp_model::EdgeKind;
use tree_sitter::Node;

use super::{Context, Fact, ROUTE};
use crate::lang::{FILE_OWNER, Site};

/// The module a file under `config/routes/` is read in: its controllers
/// are relative to the scope of the `draw` that reads it, which only
/// resolution knows (D04).
pub const DRAWN: &str = "<draw>";

/// One statically known route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// `GET`, `POST`, ..., `ANY` for `via: :all`, `MOUNT` for an engine.
    pub verb: String,
    /// The composed path (`/admin/orders/:id`), always starting with `/`.
    pub path: String,
    /// The controller path (`admin/orders`), when known.
    pub controller: Option<String>,
    /// The action (`create`), when known.
    pub action: Option<String>,
    /// The mounted constant of a `mount`.
    pub mounted: Option<String>,
    /// The line of the declaring call's first line.
    pub line: u32,
}

impl Route {
    /// The controller class the controller path names: `admin/orders` →
    /// `Admin::OrdersController`. Rails camelizes each segment; an acronym
    /// inflection (`API`) is not known here.
    pub fn controller_constant(&self) -> Option<String> {
        self.controller
            .as_deref()
            .map(|c| format!("{}Controller", camelize_path(c)))
    }
}

/// The directory of the application `path` (a routes file) belongs to,
/// and what follows its `config/`.
fn under_config(path: &str) -> Option<(&str, &str)> {
    let (before, rest) = path.rsplit_once("config/")?;
    (before.is_empty() || before.ends_with('/')).then(|| (before.trim_end_matches('/'), rest))
}

/// True iff `path` is a routes file Rails loads: `config/routes.rb`, or a
/// file under `config/routes/` that it draws.
pub fn is_routes_file(path: &str) -> bool {
    under_config(path).is_some_and(|(_, rel)| rel == "routes.rb" || is_drawn(rel))
}

fn is_drawn(rel: &str) -> bool {
    rel.starts_with("routes/") && rel.ends_with(".rb")
}

/// The application and the name `draw` gives a file under
/// `config/routes/`: `engines/x/config/routes/api/v1.rb` is `api/v1` of
/// `engines/x`.
pub fn drawn_name(path: &str) -> Option<(&str, &str)> {
    let (app, rel) = under_config(path)?;
    let name = rel.strip_prefix("routes/")?.strip_suffix(".rb")?;
    Some((app, name))
}

/// The application of a routes file, drawn or not.
pub fn app_of(path: &str) -> Option<&str> {
    under_config(path).map(|(app, _)| app)
}

/// The `Route` sites of a routes file at `path`, and the `draw`s it makes
/// ([`Fact::Draw`]): one site per declaration line, controller and action
/// (`resources` declares several routes to one action, `update` twice),
/// with the verb and path of the first (v1 `push_route_references`). A
/// `mount` names no action and yields none. A drawn file's controller is
/// recorded as its path in [`DRAWN`]'s module, for resolution to place.
pub fn read(root: Node, src: &[u8], path: &str) -> (Vec<Site>, Vec<Fact>) {
    let drawn = drawn_name(path).is_some();
    let mut seen = HashSet::new();
    let mut sites = Vec::new();
    let (routes, draws) = walk(root, src, drawn);
    for route in routes {
        let (Some(action), Some(constant)) = (route.action.clone(), route.controller_constant())
        else {
            continue;
        };
        let controller = route.controller.clone().unwrap_or_default();
        let (qualifier, shown) = match controller.strip_prefix(DRAWN) {
            Some(relative) => {
                let relative = relative.trim_start_matches('/');
                let shown = format!("{}Controller", camelize_path(relative));
                (controller.clone(), shown)
            }
            None => (constant.clone(), constant),
        };
        if !seen.insert((route.line, qualifier.clone(), action.clone())) {
            continue;
        }
        let context = Context {
            name: action.clone(),
            qualifier: format!("{ROUTE}{qualifier}"),
            cref: Vec::new(),
            extra: vec![route.verb, route.path],
        };
        sites.push(Site {
            kind: EdgeKind::Route,
            line: route.line,
            owner: Some(FILE_OWNER),
            target: format!("{shown}#{action}"),
            context: context.encode(),
        });
    }
    (sites, draws)
}

/// The routes a routes file declares, in source order, read at the top
/// level (a drawn file in [`DRAWN`]'s module).
#[cfg(test)]
pub fn routes(root: Node, src: &[u8], drawn: bool) -> Vec<Route> {
    walk(root, src, drawn).0
}

fn walk(root: Node, src: &[u8], drawn: bool) -> (Vec<Route>, Vec<Fact>) {
    let mut walker = RouteWalker {
        src,
        routes: Vec::new(),
        draws: Vec::new(),
        visits: 0,
    };
    let scope = Scope {
        module: if drawn {
            DRAWN.to_owned()
        } else {
            String::new()
        },
        ..Scope::default()
    };
    walker.walk(root, &scope, 0);
    (walker.routes, walker.draws)
}

/// Where a declaration inside a resource block lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum On {
    /// `member do`: `/orders/:id/...`.
    Member,
    /// `collection do`: `/orders/...`.
    Collection,
    /// Directly in the block: a nested scope, `/orders/:order_id/...`.
    Nested,
}

#[derive(Clone, Debug)]
struct Resource {
    collection: String,
    member: String,
    nested: String,
    /// The module-qualified controller path, `admin/orders`.
    controller: String,
}

#[derive(Clone, Debug, Default)]
struct Scope {
    /// The URL prefix.
    path: String,
    /// `namespace` and `scope module:` segments, joined by `/`.
    module: String,
    /// From a `controller` block or `scope controller:`.
    controller: Option<String>,
    /// The innermost resource, and where in its block the walk is.
    resource: Option<(Resource, On)>,
}

impl Scope {
    fn base_path(&self) -> String {
        match &self.resource {
            Some((r, On::Member)) => r.member.clone(),
            Some((r, On::Collection)) => r.collection.clone(),
            Some((r, On::Nested)) => r.nested.clone(),
            None => self.path.clone(),
        }
    }

    fn controller_path(&self, name: &str) -> String {
        if self.module.is_empty() || name.starts_with('/') {
            name.trim_start_matches('/').to_owned()
        } else {
            format!("{}/{name}", self.module)
        }
    }
}

const VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// `(action, verb, suffix)`: a suffix starting with `:member` is appended to
/// the member path, any other to the collection path.
const PLURAL_ACTIONS: &[(&str, &str, &str)] = &[
    ("index", "GET", ""),
    ("create", "POST", ""),
    ("new", "GET", "/new"),
    ("edit", "GET", ":member/edit"),
    ("show", "GET", ":member"),
    ("update", "PATCH", ":member"),
    ("update", "PUT", ":member"),
    ("destroy", "DELETE", ":member"),
];
const SINGULAR_ACTIONS: &[(&str, &str, &str)] = &[
    ("create", "POST", ""),
    ("new", "GET", "/new"),
    ("edit", "GET", "/edit"),
    ("show", "GET", ""),
    ("update", "PATCH", ""),
    ("update", "PUT", ""),
    ("destroy", "DELETE", ""),
];

const MAX_DEPTH: usize = 256;
/// Most routes one file yields. A `resources :a, :b do` block is walked once
/// per name, so nesting such declarations multiplies the work; past this
/// many routes (far above any real routes file) the rest is not read.
pub const MAX_ROUTES: usize = 10_000;
/// Most syntax nodes the walk visits in one file, for the same reason.
const MAX_VISITS: usize = 1_000_000;

struct RouteWalker<'a> {
    src: &'a [u8],
    routes: Vec<Route>,
    draws: Vec<Fact>,
    visits: usize,
}

impl RouteWalker<'_> {
    /// Keeps `route` unless the file already gave [`MAX_ROUTES`].
    fn push(&mut self, route: Route) {
        if self.routes.len() < MAX_ROUTES {
            self.routes.push(route);
        }
    }

    fn text(&self, node: Node) -> String {
        String::from_utf8_lossy(&self.src[node.byte_range()]).into_owned()
    }

    /// A literal string or symbol (`:a`, `:"a"`) without interpolation; an
    /// escape sequence is skipped, as v1 does.
    fn literal(&self, node: Node) -> Option<String> {
        match node.kind() {
            "string" | "delimited_symbol" => self.content(node),
            "simple_symbol" => Some(self.text(node)[1..].to_owned()),
            _ => None,
        }
    }

    /// The text of a string-like node's content; `None` when interpolated.
    fn content(&self, node: Node) -> Option<String> {
        let mut out = String::new();
        let mut cursor = node.walk();
        for part in node.children(&mut cursor) {
            match part.kind() {
                "string_content" => out.push_str(&self.text(part)),
                "interpolation" => return None,
                _ => {}
            }
        }
        Some(out)
    }

    /// The literals of a value that is one, an array of them, or a `%i[]` or
    /// `%w[]` list of plain words; `None` when any element is not literal.
    fn literal_list(&self, node: Node) -> Option<Vec<String>> {
        match node.kind() {
            "array" => named(node).into_iter().map(|n| self.literal(n)).collect(),
            "symbol_array" | "string_array" => named(node)
                .into_iter()
                .map(|n| {
                    matches!(n.kind(), "bare_symbol" | "bare_string")
                        .then(|| self.content(n))
                        .flatten()
                })
                .collect(),
            _ => self.literal(node).map(|l| vec![l]),
        }
    }

    /// The value of a `key: value` (or `:key => value`) argument; a
    /// shorthand `key:` has none.
    fn option<'t>(&self, args: Option<Node<'t>>, key: &str) -> Option<Node<'t>> {
        named(args?).into_iter().find_map(|arg| {
            if arg.kind() != "pair" {
                return None;
            }
            let k = self.text(arg.child_by_field_name("key")?);
            (k.trim_start_matches(':').trim_end_matches(':') == key)
                .then(|| arg.child_by_field_name("value"))
                .flatten()
        })
    }

    /// Absent, present but not literal, or literal.
    fn option_literal(&self, args: Option<Node>, key: &str) -> Option<Option<String>> {
        self.option(args, key).map(|v| self.literal(v))
    }

    fn positional<'t>(&self, args: Option<Node<'t>>) -> Vec<Node<'t>> {
        args.map(named)
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !matches!(a.kind(), "pair" | "block_argument"))
            .collect()
    }

    /// Only a receiver-less call is a routing declaration; any other node,
    /// and an unknown method (`nested`, `with_options`, an `if`), is
    /// transparent: its children are read in the same scope. A region the
    /// grammar could not parse yields nothing: the `namespace` or resource
    /// block around its declarations may be lost, and with it their path
    /// and controller.
    fn walk(&mut self, node: Node, scope: &Scope, depth: usize) {
        self.visits += 1;
        if depth > MAX_DEPTH
            || self.visits > MAX_VISITS
            || self.routes.len() >= MAX_ROUTES
            || node.is_error()
        {
            return;
        }
        if node.kind() == "call" && node.child_by_field_name("receiver").is_none() {
            let method = node
                .child_by_field_name("method")
                .map(|m| self.text(m))
                .unwrap_or_default();
            if self.declaration(node, &method, scope, depth) {
                return;
            }
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children {
            self.walk(child, scope, depth + 1);
        }
    }

    fn walk_block(&mut self, call: Node, scope: &Scope, depth: usize) {
        if let Some(block) = call.child_by_field_name("block") {
            self.walk(block, scope, depth + 1);
        }
    }

    /// Handles one routing declaration; true iff it was one (its block, if
    /// any, has been walked with the scope it opens).
    fn declaration(&mut self, call: Node, method: &str, scope: &Scope, depth: usize) -> bool {
        let args = call.child_by_field_name("arguments");
        let line = call.start_position().row as u32 + 1;
        match method {
            "namespace" => {
                self.namespace(call, scope, depth);
                true
            }
            "scope" => {
                self.scope(call, scope, depth);
                true
            }
            "controller" => {
                let Some(name) = self.positional(args).first().and_then(|n| self.literal(*n))
                else {
                    return true;
                };
                let mut inner = scope.clone();
                inner.controller = Some(name);
                self.walk_block(call, &inner, depth);
                true
            }
            "constraints" | "defaults" => {
                self.walk_block(call, scope, depth);
                true
            }
            "member" | "collection" => {
                // A stray `member do` outside a resource is no declaration:
                // its routes keep the outer scope.
                let Some((resource, _)) = &scope.resource else {
                    return false;
                };
                let mut inner = scope.clone();
                let on = if method == "member" {
                    On::Member
                } else {
                    On::Collection
                };
                inner.resource = Some((resource.clone(), on));
                self.walk_block(call, &inner, depth);
                true
            }
            "resources" | "resource" => {
                self.resources(call, method == "resources", scope, depth, line);
                true
            }
            "root" => {
                let target = match self.positional(args).first() {
                    Some(first) => self.literal(*first),
                    None => self.target(args, scope),
                };
                let path = if scope.path.is_empty() {
                    "/".to_owned()
                } else {
                    scope.path.clone()
                };
                self.push_target("GET", path, target.as_deref(), scope, line);
                true
            }
            "match" => {
                let Some(via) = self.option(args, "via").and_then(|v| self.literal_list(v)) else {
                    return true;
                };
                for verb in via {
                    let verb = if verb == "all" {
                        "ANY".to_owned()
                    } else {
                        verb.to_uppercase()
                    };
                    self.verb(call, &verb, scope, line);
                }
                true
            }
            m if VERBS.contains(&m) => {
                self.verb(call, &m.to_uppercase(), scope, line);
                true
            }
            "mount" => {
                self.mount(args, scope, line);
                true
            }
            // Another file, read in this scope (D04): its module, unless
            // the scope holds a resource or a controller, which the rules
            // do not carry over.
            "draw" => {
                let name = self.positional(args).first().and_then(|n| self.literal(*n));
                let carried = scope.resource.is_none() && scope.controller.is_none();
                self.draws.push(Fact::Draw {
                    name: name.unwrap_or_default(),
                    module: carried.then(|| scope.module.clone()),
                });
                true
            }
            // Templates applied elsewhere, URL helpers.
            "concern" | "concerns" | "direct" | "resolve" => true,
            _ => false,
        }
    }

    /// `namespace :admin` (with `path:` and `module:` defaulting to the
    /// name): a path and module prefix, out of any resource.
    fn namespace(&mut self, call: Node, scope: &Scope, depth: usize) {
        let args = call.child_by_field_name("arguments");
        let Some(name) = self.positional(args).first().and_then(|n| self.literal(*n)) else {
            return;
        };
        let path = self
            .option_literal(args, "path")
            .unwrap_or(Some(name.clone()));
        let module = self.option_literal(args, "module").unwrap_or(Some(name));
        let (Some(path), Some(module)) = (path, module) else {
            return;
        };
        let inner = Scope {
            path: join_path(&scope.base_path(), &path),
            module: join_module(&scope.module, &module),
            controller: None,
            resource: None,
        };
        self.walk_block(call, &inner, depth);
    }

    /// `scope "/path"`, `scope path:`, `module:`, `controller:`; any other
    /// option (`as:`, `defaults:`, `constraints:`) is ignored.
    fn scope(&mut self, call: Node, scope: &Scope, depth: usize) {
        let args = call.child_by_field_name("arguments");
        let mut inner = scope.clone();
        let path = match self.positional(args).first() {
            Some(first) => Some(self.literal(*first)),
            None => self.option_literal(args, "path"),
        };
        match path {
            Some(Some(path)) => {
                inner.path = join_path(&scope.base_path(), &path);
                inner.resource = None;
            }
            Some(None) => return,
            None => {}
        }
        match self.option_literal(args, "module") {
            Some(Some(module)) => inner.module = join_module(&scope.module, &module),
            Some(None) => return,
            None => {}
        }
        match self.option_literal(args, "controller") {
            Some(Some(controller)) => inner.controller = Some(controller),
            Some(None) => return,
            None => {}
        }
        self.walk_block(call, &inner, depth);
    }

    /// `to: "c#a"`, or `controller:` with `action:`. An `action:` alone
    /// names an action of the enclosing `controller` block, else of the
    /// resource's controller (absolute, already namespaced).
    fn target(&self, args: Option<Node>, scope: &Scope) -> Option<String> {
        if let Some(to) = self.option(args, "to") {
            return self.literal(to);
        }
        let action = self.option_literal(args, "action")??;
        let controller = match self.option_literal(args, "controller") {
            Some(c) => c?,
            None => scope.controller.clone().or_else(|| {
                scope
                    .resource
                    .as_ref()
                    .map(|(resource, _)| format!("/{}", resource.controller))
            })?,
        };
        Some(format!("{controller}#{action}"))
    }

    fn verb(&mut self, call: Node, verb: &str, scope: &Scope, line: u32) {
        let args = call.child_by_field_name("arguments");
        // `get "path" => "c#a"`: the path and target are a hash pair.
        let rocket = args.map(named).unwrap_or_default().into_iter().find(|a| {
            a.kind() == "pair"
                && a.child_by_field_name("key")
                    .is_some_and(|k| k.kind() == "string")
        });
        let (segment, target) = if let Some(pair) = rocket {
            let Some(segment) = pair
                .child_by_field_name("key")
                .and_then(|k| self.literal(k))
            else {
                return;
            };
            let target = pair
                .child_by_field_name("value")
                .and_then(|v| self.literal(v));
            (segment, target.map(|t| self.rocket_target(&t, scope)))
        } else {
            let Some(first) = self.positional(args).first().copied() else {
                return;
            };
            let Some(segment) = self.literal(first) else {
                return;
            };
            let target = self
                .target(args, scope)
                .or_else(|| self.inferred_target(args, &segment, scope));
            (segment, target)
        };
        let mut route_scope = scope.clone();
        if let Some(on) = self.option(args, "on").and_then(|o| self.literal(o))
            && let Some((resource, _)) = &scope.resource
        {
            let on = match on.as_str() {
                "member" => On::Member,
                "collection" => On::Collection,
                _ => On::Nested,
            };
            route_scope.resource = Some((resource.clone(), on));
        }
        let path = join_path(&route_scope.base_path(), &segment);
        self.push_target(verb, path, target.as_deref(), scope, line);
    }

    /// No target: in a resource scope the name is an action of the
    /// resource's controller; elsewhere `get "a/b"` is `a#b`, and `get :x`
    /// in a `controller` block is `#x`. A dynamic segment (`:id`, `*path`,
    /// `(.:format)`) names no action: Rails raises for a missing `:action`
    /// there. A `to:`, `action:` or `controller:` that could not be read (a
    /// redirect, a lambda, a Rack app, a variable) names the handler; the
    /// path does not, so nothing is guessed from it.
    fn inferred_target(&self, args: Option<Node>, segment: &str, scope: &Scope) -> Option<String> {
        if ["to", "action", "controller"]
            .iter()
            .any(|key| self.option(args, key).is_some())
        {
            return None;
        }
        let bare = segment.trim_start_matches('/');
        if bare.contains([':', '*', '(']) {
            None
        } else if let Some((resource, _)) = &scope.resource {
            (!bare.contains('/')).then(|| format!("/{}#{bare}", resource.controller))
        } else if let Some(controller) = &scope.controller {
            (!bare.contains('/')).then(|| format!("{controller}#{bare}"))
        } else {
            bare.rsplit_once('/').map(|(c, a)| format!("{c}#{a}"))
        }
    }

    /// `"c#a"` as a hash-rocket target, or a bare `:action` in a
    /// `controller` block.
    fn rocket_target(&self, target: &str, scope: &Scope) -> String {
        match (target.contains('#'), &scope.controller) {
            (false, Some(controller)) => format!("{controller}#{target}"),
            _ => target.to_owned(),
        }
    }

    fn push_target(
        &mut self,
        verb: &str,
        path: String,
        target: Option<&str>,
        scope: &Scope,
        line: u32,
    ) {
        let Some((controller, action)) = target.and_then(|t| t.split_once('#')) else {
            return;
        };
        if controller.is_empty() || action.is_empty() {
            return;
        }
        // A target already resolved against the resource (`/orders`) is
        // absolute; any other is relative to the enclosing module.
        let controller = if let Some(absolute) = controller.strip_prefix('/') {
            absolute.to_owned()
        } else {
            scope.controller_path(controller)
        };
        self.push(Route {
            verb: verb.to_owned(),
            path,
            controller: Some(controller),
            action: Some(action.to_owned()),
            mounted: None,
            line,
        });
    }

    /// `resources :orders` (or `resource :profile`): the seven (or six)
    /// REST routes, filtered by `only:` and `except:`, and the block walked
    /// once per name in the resource's scope. Ignored options: `shallow:`,
    /// `as:`, `concerns:`, `constraints:`, `defaults:`, `format:`,
    /// `path_names:`.
    fn resources(&mut self, call: Node, plural: bool, scope: &Scope, depth: usize, line: u32) {
        let args = call.child_by_field_name("arguments");
        let names: Vec<String> = match self
            .positional(args)
            .into_iter()
            .map(|n| self.literal(n))
            .collect::<Option<Vec<_>>>()
        {
            Some(names) if !names.is_empty() => names,
            _ => return,
        };
        let only = match self.option(args, "only").map(|v| self.literal_list(v)) {
            Some(Some(only)) => Some(only),
            Some(None) => return,
            None => None,
        };
        let except = match self.option(args, "except").map(|v| self.literal_list(v)) {
            Some(Some(except)) => except,
            Some(None) => return,
            None => Vec::new(),
        };
        let (Some(segment_opt), Some(controller_opt), Some(param)) = (
            self.option_literal(args, "path")
                .map_or(Some(None), |p| p.map(Some)),
            self.option_literal(args, "controller")
                .map_or(Some(None), |c| c.map(Some)),
            self.option_literal(args, "param")
                .unwrap_or(Some("id".to_owned())),
        ) else {
            return;
        };
        let module = match self.option_literal(args, "module") {
            Some(Some(module)) => join_module(&scope.module, &module),
            Some(None) => return,
            None => scope.module.clone(),
        };
        for name in names {
            let segment = segment_opt.clone().unwrap_or_else(|| name.clone());
            let collection = join_path(&scope.base_path(), &segment);
            let controller_name = controller_opt.clone().unwrap_or_else(|| {
                if plural {
                    name.clone()
                } else {
                    pluralize(&name)
                }
            });
            let controller = if module.is_empty() {
                controller_name
            } else {
                format!("{module}/{controller_name}")
            };
            let (member, nested) = if plural {
                (
                    format!("{collection}/:{param}"),
                    format!("{collection}/:{}_{param}", singularize(&name)),
                )
            } else {
                (collection.clone(), collection.clone())
            };
            let table = if plural {
                PLURAL_ACTIONS
            } else {
                SINGULAR_ACTIONS
            };
            for (action, verb, suffix) in table {
                if only
                    .as_ref()
                    .is_some_and(|o| !o.iter().any(|a| a == action))
                    || except.iter().any(|a| a == action)
                {
                    continue;
                }
                let path = match suffix.strip_prefix(":member") {
                    Some(rest) => format!("{member}{rest}"),
                    None => format!("{collection}{suffix}"),
                };
                self.push(Route {
                    verb: (*verb).to_owned(),
                    path,
                    controller: Some(controller.clone()),
                    action: Some((*action).to_owned()),
                    mounted: None,
                    line,
                });
            }
            // Rails applies a resource's `module:` as a scope around its
            // block too, so nested resources and relative targets inherit it.
            let inner = Scope {
                path: scope.path.clone(),
                module: module.clone(),
                controller: None,
                resource: Some((
                    Resource {
                        collection,
                        member,
                        nested,
                        controller,
                    },
                    On::Nested,
                )),
            };
            self.walk_block(call, &inner, depth);
        }
    }

    /// `mount Engine => "/at"` or `mount Engine, at: "/at"`.
    fn mount(&mut self, args: Option<Node>, scope: &Scope, line: u32) {
        let all = args.map(named).unwrap_or_default();
        let (constant, at) = if let Some(pair) = all.iter().find(|a| {
            a.kind() == "pair"
                && a.child_by_field_name("key")
                    .is_some_and(|k| matches!(k.kind(), "constant" | "scope_resolution"))
        }) {
            (
                pair.child_by_field_name("key").map(|k| self.text(k)),
                pair.child_by_field_name("value")
                    .and_then(|v| self.literal(v)),
            )
        } else {
            (
                all.first()
                    .filter(|n| matches!(n.kind(), "constant" | "scope_resolution"))
                    .map(|n| self.text(*n)),
                self.option_literal(args, "at").flatten(),
            )
        };
        let (Some(constant), Some(at)) = (constant, at) else {
            return;
        };
        self.push(Route {
            verb: "MOUNT".to_owned(),
            path: join_path(&scope.base_path(), &at),
            controller: None,
            action: None,
            mounted: Some(constant),
            line,
        });
    }
}

/// Named children but comments.
fn named(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

fn join_path(base: &str, segment: &str) -> String {
    let joined = format!("{base}/{segment}");
    let parts: Vec<&str> = joined.split('/').filter(|p| !p.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

pub fn join_module(base: &str, module: &str) -> String {
    let module = module.trim_matches('/');
    match (base.is_empty(), module.is_empty()) {
        (_, true) => base.to_owned(),
        (true, false) => module.to_owned(),
        (false, false) => format!("{base}/{module}"),
    }
}

/// `admin/order_items` → `Admin::OrderItems`: each `_` word's first letter
/// uppercased, the rest kept.
pub fn camelize_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            segment
                .split('_')
                .map(|word| {
                    let mut chars = word.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_uppercase().chain(chars).collect()
                    })
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("::")
}

/// Singular words ending in `s` whose plural Rails' default inflections
/// spell with `es` (the `alias`, `status` and `bus` rules).
const ES_STEMS: &[&str] = &["alias", "status", "bus"];

/// The regular English singular Rails' default inflections give the common
/// cases (`orders` → `order`, `categories` → `category`, `boxes` → `box`);
/// irregular words (`people`) keep their plural. The words Rails' rules
/// single out before the trailing `s` (`statuses`, `aliases`, `buses`) keep
/// their stem, and are already singular without the `es`.
/// <https://github.com/rails/rails/blob/main/activesupport/lib/active_support/inflections.rb>
pub fn singularize(word: &str) -> String {
    if let Some(stem) = ES_STEMS.iter().find_map(|stem| {
        word.strip_suffix("es")
            .filter(|w| w.ends_with(stem))
            .or_else(|| word.ends_with(stem).then_some(word))
    }) {
        stem.to_owned()
    } else if let Some(stem) = word.strip_suffix("ies") {
        format!("{stem}y")
    } else if let Some(stem) = ["sses", "shes", "ches", "xes"]
        .iter()
        .find_map(|suffix| word.strip_suffix(suffix).map(|s| (s, suffix)))
        .map(|(stem, suffix)| format!("{stem}{}", &suffix[..suffix.len() - 2]))
    {
        stem
    } else if word.ends_with("ss") {
        word.to_owned()
    } else {
        word.strip_suffix('s').unwrap_or(word).to_owned()
    }
}

/// The regular English plural Rails' default inflections give (`profile` →
/// `profiles`, `category` → `categories`, `box` → `boxes`, `status` →
/// `statuses`); any other word already ending in `s` (`settings`) is kept.
pub fn pluralize(word: &str) -> String {
    if ES_STEMS.iter().any(|stem| word.ends_with(stem)) {
        format!("{word}es")
    } else if let Some(stem) = word.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u'])
    {
        format!("{stem}ies")
    } else if ["ss", "sh", "ch", "x"].iter().any(|s| word.ends_with(s)) {
        format!("{word}es")
    } else if word.ends_with('s') {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

#[cfg(test)]
mod tests;
