// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The literal Rails routing DSL of `config/routes.rb` and the files it
//! draws (`config/routes/*.rb`), read statically: verb declarations
//! (`get`, `post`, `put`, `patch`, `delete`, `match ... via:`), `root`,
//! `resources`/`resource` with `only:`/`except:`/`controller:`/`path:`/
//! `param:`, `member`/`collection`, `namespace`, `scope` (path and
//! `module:`), `controller` blocks, and `mount`.
//!
//! Each route keeps its verb, its path as Rails composes it and, when the
//! declaration names it literally, the `controller#action` it dispatches to.
//! A path or target that is not a literal (a variable, an interpolated
//! string, a lambda `to:`) yields no route, never a guessed one; a `mount`
//! keeps the mounted constant with no action; `concern`/`concerns`,
//! `direct`, `resolve` and `draw` (the drawn file is read on its own, with
//! no prefix) are not modelled.
//! <https://guides.rubyonrails.org/routing.html>

use tree_sitter::Node;

/// One statically known route.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// The handler as a reader names it: `admin/orders#create
    /// (Admin::OrdersController#create)`, `mount Sidekiq::Web`.
    pub fn handler(&self) -> String {
        if let Some(mounted) = &self.mounted {
            return format!("mount {mounted}");
        }
        match (&self.controller, &self.action, self.controller_constant()) {
            (Some(c), Some(a), Some(k)) => format!("{c}#{a} ({k}#{a})"),
            _ => "handler not static".to_string(),
        }
    }
}

/// True iff `path` is a routes file Rails loads: `config/routes.rb`, or a
/// file under `config/routes/` that it `draw`s.
pub fn is_routes_file(path: &str) -> bool {
    let rel = path.rsplit_once("config/").map_or("", |(before, rest)| {
        if before.is_empty() || before.ends_with('/') {
            rest
        } else {
            ""
        }
    });
    rel == "routes.rb" || (rel.starts_with("routes/") && rel.ends_with(".rb"))
}

/// The routes a routes file declares, in source order.
pub fn routes(content: &[u8]) -> Vec<Route> {
    let Some(tree) = super::parse_file("config/routes.rb", content) else {
        return Vec::new();
    };
    let mut walker = RouteWalker {
        src: content,
        routes: Vec::new(),
        visits: 0,
    };
    walker.walk(tree.root_node(), &Scope::default(), 0);
    walker.routes
}

/// Where a declaration inside a resource block lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum On {
    /// `member do`: `/orders/:id/...`.
    Member,
    /// `collection do`: `/orders/...`.
    Collection,
    /// Directly in the block: a nested scope, `/orders/:order_id/...`.
    Nested,
}

#[derive(Debug, Clone)]
struct Resource {
    collection: String,
    member: String,
    nested: String,
    controller: String,
}

#[derive(Debug, Clone, Default)]
struct Scope {
    path: String,
    /// `namespace`/`scope module:` segments, joined by `/`.
    module: String,
    controller: Option<String>,
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
            name.trim_start_matches('/').to_string()
        } else {
            format!("{}/{name}", self.module)
        }
    }
}

const VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];
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
/// per name, so nesting such declarations multiplies the work; past this many
/// routes (far above any real routes file) the rest of the file is not read.
pub const MAX_ROUTES: usize = 10_000;
/// Most syntax nodes the walk visits in one file, for the same reason.
const MAX_VISITS: usize = 1_000_000;

struct RouteWalker<'a> {
    src: &'a [u8],
    routes: Vec<Route>,
    visits: usize,
}

impl RouteWalker<'_> {
    /// Keep `route` unless the file already gave [`MAX_ROUTES`].
    fn push(&mut self, route: Route) {
        if self.routes.len() < MAX_ROUTES {
            self.routes.push(route);
        }
    }

    fn text(&self, node: Node) -> String {
        String::from_utf8_lossy(&self.src[node.byte_range()]).into_owned()
    }

    /// A literal string or symbol (`:a`, `:"a"`) without interpolation.
    fn literal(&self, node: Node) -> Option<String> {
        match node.kind() {
            "string" | "delimited_symbol" => {
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
            "simple_symbol" => Some(self.text(node)[1..].to_string()),
            _ => None,
        }
    }

    /// Literal symbols/strings of a value that is one, or an array of them;
    /// `None` when any element is not literal.
    fn literal_list(&self, node: Node) -> Option<Vec<String>> {
        if node.kind() == "array" {
            named(node).into_iter().map(|n| self.literal(n)).collect()
        } else {
            self.literal(node).map(|l| vec![l])
        }
    }

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

    fn walk(&mut self, node: Node, scope: &Scope, depth: usize) {
        self.visits += 1;
        if depth > MAX_DEPTH || self.visits > MAX_VISITS || self.routes.len() >= MAX_ROUTES {
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

    /// Handle one routing DSL call; true iff it was one (its block, if
    /// any, has been walked with the scope it opens).
    fn declaration(&mut self, call: Node, method: &str, scope: &Scope, depth: usize) -> bool {
        let args = call.child_by_field_name("arguments");
        let line = call.start_position().row as u32 + 1;
        match method {
            "namespace" => {
                let Some(name) = self.positional(args).first().and_then(|n| self.literal(*n))
                else {
                    return true;
                };
                let path = self
                    .option_literal(args, "path")
                    .unwrap_or(Some(name.clone()));
                let module = self.option_literal(args, "module").unwrap_or(Some(name));
                let (Some(path), Some(module)) = (path, module) else {
                    return true;
                };
                let inner = Scope {
                    path: join_path(&scope.base_path(), &path),
                    module: join_module(&scope.module, &module),
                    controller: None,
                    resource: None,
                };
                self.walk_block(call, &inner, depth);
                true
            }
            "scope" => {
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
                    Some(None) => return true,
                    None => {}
                }
                match self.option_literal(args, "module") {
                    Some(Some(module)) => inner.module = join_module(&scope.module, &module),
                    Some(None) => return true,
                    None => {}
                }
                match self.option_literal(args, "controller") {
                    Some(Some(controller)) => inner.controller = Some(controller),
                    Some(None) => return true,
                    None => {}
                }
                self.walk_block(call, &inner, depth);
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
                    "/".to_string()
                } else {
                    scope.path.clone()
                };
                self.push_target("GET", path, target.as_deref(), args, scope, line);
                true
            }
            "match" => {
                let Some(via) = self.option(args, "via").and_then(|v| self.literal_list(v)) else {
                    return true;
                };
                let verbs: Vec<String> = via
                    .iter()
                    .map(|v| {
                        if v == "all" {
                            "ANY".to_string()
                        } else {
                            v.to_uppercase()
                        }
                    })
                    .collect();
                for verb in verbs {
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
            // Templates applied elsewhere, URL helpers, other files.
            "concern" | "concerns" | "direct" | "resolve" | "draw" => true,
            _ => false,
        }
    }

    /// `to: "c#a"`, or `controller:` with `action:`, from the options. An
    /// `action:` alone names an action of the enclosing `controller` block,
    /// else of the resource's controller (absolute, already namespaced).
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
            // A `to:`, `action:` or `controller:` the interpreter could not
            // read (a redirect, a lambda, a Rack app, a variable) names the
            // handler; the path does not, so nothing is guessed from it.
            let explicit = ["to", "action", "controller"]
                .iter()
                .any(|key| self.option(args, key).is_some());
            let target = self.target(args, scope).or_else(|| {
                if explicit {
                    return None;
                }
                // No target: in a resource scope the name is an action of
                // the resource's controller; elsewhere `get "a/b"` is
                // `a#b`, and `get :x` in a `controller` block is `#x`. A
                // dynamic segment (`:id`, `*path`, `(.:format)`) names no
                // action: Rails raises for a missing `:action` there.
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
            });
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
        self.push_target(verb, path, target.as_deref(), args, scope, line);
    }

    /// `"c#a"` as a hash-rocket target, or a bare `:action` in a
    /// `controller` block.
    fn rocket_target(&self, target: &str, scope: &Scope) -> String {
        match (target.contains('#'), &scope.controller) {
            (false, Some(controller)) => format!("{controller}#{target}"),
            _ => target.to_string(),
        }
    }

    fn push_target(
        &mut self,
        verb: &str,
        path: String,
        target: Option<&str>,
        _args: Option<Node>,
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
            absolute.to_string()
        } else {
            scope.controller_path(controller)
        };
        self.push(Route {
            verb: verb.to_string(),
            path,
            controller: Some(controller),
            action: Some(action.to_string()),
            mounted: None,
            line,
        });
    }

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
                .unwrap_or(Some("id".to_string())),
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
                    verb: (*verb).to_string(),
                    path,
                    controller: Some(controller.clone()),
                    action: Some((*action).to_string()),
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
                        controller: controller.clone(),
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
            a.kind() == "pair" && {
                a.child_by_field_name("key")
                    .is_some_and(|k| matches!(k.kind(), "constant" | "scope_resolution"))
            }
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
            verb: "MOUNT".to_string(),
            path: join_path(&scope.base_path(), &at),
            controller: None,
            action: None,
            mounted: Some(constant),
            line,
        });
    }
}

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

fn join_module(base: &str, module: &str) -> String {
    let module = module.trim_matches('/');
    match (base.is_empty(), module.is_empty()) {
        (_, true) => base.to_string(),
        (true, false) => module.to_string(),
        (false, false) => format!("{base}/{module}"),
    }
}

/// `admin/order_items` → `Admin::OrderItems`.
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
/// spell with `es` (`alias`/`status` and `bus` rules).
const ES_STEMS: &[&str] = &["alias", "status", "bus"];

/// The regular English singular Rails' default inflections give the common
/// cases (`orders` → `order`, `categories` → `category`, `boxes` → `box`);
/// irregular words (`people`) keep their plural. The words Rails' rules
/// single out before the trailing `s` (`statuses` → `status`, `aliases`,
/// `buses`) keep their stem, and are already singular without the `es`.
/// <https://github.com/rails/rails/blob/main/activesupport/lib/active_support/inflections.rb>
pub fn singularize(word: &str) -> String {
    if let Some(stem) = ES_STEMS.iter().find_map(|stem| {
        word.strip_suffix("es")
            .filter(|w| w.ends_with(stem))
            .or_else(|| word.ends_with(stem).then_some(word))
    }) {
        stem.to_string()
    } else if let Some(stem) = word.strip_suffix("ies") {
        format!("{stem}y")
    } else if let Some(stem) = ["sses", "shes", "ches", "xes"]
        .iter()
        .find_map(|suffix| word.strip_suffix(suffix).map(|s| (s, suffix)))
        .map(|(stem, suffix)| format!("{stem}{}", &suffix[..suffix.len() - 2]))
    {
        stem
    } else if word.ends_with("ss") {
        word.to_string()
    } else {
        word.strip_suffix('s').unwrap_or(word).to_string()
    }
}

/// The regular English plural Rails' default inflections give (`profile` →
/// `profiles`, `category` → `categories`, `box` → `boxes`, `address` →
/// `addresses`, `status` → `statuses`); any other word already ending in
/// `s` (`settings`) is kept.
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
        word.to_string()
    } else {
        format!("{word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(source: &str) -> Vec<String> {
        routes(source.as_bytes())
            .iter()
            .map(|r| format!("{} {} {}", r.verb, r.path, r.handler()))
            .collect()
    }

    #[test]
    fn routes_should_compose_namespaces_resources_and_verbs() {
        let source = r#"Rails.application.routes.draw do
  root "home#index"
  namespace :admin do
    root to: "dashboard#show"
    resources :orders, only: [:index, :create] do
      member do
        post :refund
      end
      get :export, on: :collection
      get "preview", action: :show_preview, on: :member
      resources :line_items, only: :destroy
    end
    resource :profile, except: [:new, :create, :destroy]
  end
  resources :orders, only: [:show]
  resources :posts, only: [], module: :blog do
    resources :comments, only: :index
    get "feed", to: "feeds#show"
  end
  scope "/shop", module: "store" do
    get "cart" => "carts#show"
    get "checkout", to: "checkouts#new"
  end
  scope module: :api do
    patch "items/:id", controller: "items", action: :update
  end
  controller :pages do
    get "about" => :about
  end
  get "help/faq"
  match "search", to: "search#query", via: [:get, :post]
  mount Sidekiq::Web => "/sidekiq"
  mount Blog::Engine, at: "/blog"
end
"#;
        assert_eq!(
            table(source),
            [
                "GET / home#index (HomeController#index)",
                "GET /admin admin/dashboard#show (Admin::DashboardController#show)",
                "GET /admin/orders admin/orders#index (Admin::OrdersController#index)",
                "POST /admin/orders admin/orders#create (Admin::OrdersController#create)",
                "POST /admin/orders/:id/refund admin/orders#refund (Admin::OrdersController#refund)",
                "GET /admin/orders/export admin/orders#export (Admin::OrdersController#export)",
                "GET /admin/orders/:id/preview admin/orders#show_preview (Admin::OrdersController#show_preview)",
                "DELETE /admin/orders/:order_id/line_items/:id admin/line_items#destroy (Admin::LineItemsController#destroy)",
                "GET /admin/profile/edit admin/profiles#edit (Admin::ProfilesController#edit)",
                "GET /admin/profile admin/profiles#show (Admin::ProfilesController#show)",
                "PATCH /admin/profile admin/profiles#update (Admin::ProfilesController#update)",
                "PUT /admin/profile admin/profiles#update (Admin::ProfilesController#update)",
                "GET /orders/:id orders#show (OrdersController#show)",
                "GET /posts/:post_id/comments blog/comments#index (Blog::CommentsController#index)",
                "GET /posts/:post_id/feed blog/feeds#show (Blog::FeedsController#show)",
                "GET /shop/cart store/carts#show (Store::CartsController#show)",
                "GET /shop/checkout store/checkouts#new (Store::CheckoutsController#new)",
                "PATCH /items/:id api/items#update (Api::ItemsController#update)",
                "GET /about pages#about (PagesController#about)",
                "GET /help/faq help#faq (HelpController#faq)",
                "GET /search search#query (SearchController#query)",
                "POST /search search#query (SearchController#query)",
                "MOUNT /sidekiq mount Sidekiq::Web",
                "MOUNT /blog mount Blog::Engine",
            ]
        );
    }

    #[test]
    fn quoted_symbols_should_read_like_plain_ones() {
        let source = "Rails.application.routes.draw do\n  resources :\"orders\", only: [:\"show\"]\n  get :\"ping\", to: \"health#ping\"\n  get :\"x#{y}\", to: \"a#b\"\nend\n";
        assert_eq!(
            table(source),
            [
                "GET /orders/:id orders#show (OrdersController#show)",
                "GET /ping health#ping (HealthController#ping)",
            ]
        );
    }

    #[test]
    fn nested_multi_name_resources_should_stop_at_the_route_cap() {
        // Each level names two resources, so the innermost block is reached
        // 2^20 times without a bound.
        let mut source = String::from("Rails.application.routes.draw do\n");
        for depth in 0..20 {
            source.push_str(&format!("resources :a{depth}, :b{depth} do\n"));
        }
        source.push_str(&"end\n".repeat(21));
        let started = std::time::Instant::now();
        let found = routes(source.as_bytes());
        assert_eq!(found.len(), MAX_ROUTES);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn dynamic_routes_should_yield_nothing_rather_than_a_guess() {
        let source = r#"Rails.application.routes.draw do
  get path_var, to: "a#b"
  get "x/#{y}", to: "a#b"
  get "lambda", to: ->(env) { [200, {}, []] }
  get "rack", to: RackApp
  resources resource_name
  resources :orders, only: allowed
  namespace ns do
    get "inside", to: "a#b"
  end
  match "any", to: "a#b"
  concern :commentable do
    resources :comments
  end
  draw :admin
  mount engine, at: "/x"
  get "orders/:id"
  get "files/*path"
  get "feed(.:format)"
  get "old/path", to: redirect("/new")
  match "a/b", to: redirect("/c"), via: :get
  get "c/d", action: some_action
  get "e/f", controller: some_controller
  controller :pages do
    get "orders/:id"
    get "a/b"
    get "health", to: HealthApp
  end
  resources :photos, only: [] do
    get "nested/path"
    get "legacy", to: redirect("/x")
  end
end
"#;
        assert_eq!(table(source), Vec::<String>::new());
    }

    #[test]
    fn routes_files_should_be_the_main_one_and_drawn_ones() {
        for path in [
            "config/routes.rb",
            "engines/billing/config/routes.rb",
            "config/routes/admin.rb",
            "config/routes/api/v1.rb",
        ] {
            assert!(is_routes_file(path), "{path}");
        }
        for path in [
            "app/config/routes.rbx",
            "config/router.rb",
            "myconfig/routes.rb",
            "config/routes/readme.md",
            "lib/routes.rb",
        ] {
            assert!(!is_routes_file(path), "{path}");
        }
    }

    #[test]
    fn inflections_should_follow_the_regular_english_rules() {
        for (plural, singular) in [
            ("orders", "order"),
            ("categories", "category"),
            ("boxes", "box"),
            ("addresses", "address"),
            ("glass", "glass"),
            ("sizes", "size"),
            ("statuses", "status"),
            ("order_statuses", "order_status"),
            ("aliases", "alias"),
            ("buses", "bus"),
            ("status", "status"),
            ("alias", "alias"),
            ("bus", "bus"),
        ] {
            assert_eq!(singularize(plural), singular, "{plural}");
        }
        for (singular, plural) in [
            ("profile", "profiles"),
            ("category", "categories"),
            ("key", "keys"),
            ("box", "boxes"),
            ("address", "addresses"),
            ("settings", "settings"),
            ("status", "statuses"),
            ("order_status", "order_statuses"),
            ("alias", "aliases"),
            ("bus", "buses"),
        ] {
            assert_eq!(pluralize(singular), plural, "{singular}");
        }
        assert_eq!(camelize_path("admin/order_items"), "Admin::OrderItems");
    }
}
