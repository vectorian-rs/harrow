use std::collections::HashMap;
use std::sync::Arc;

use http::Method;

use crate::handler::{self, HandlerFn};
use crate::middleware::Middleware;
use crate::path::{PathMatch, PathPattern, to_matchit_pattern};
use crate::request::Request;
use crate::response::Response;

// ---------------------------------------------------------------------------
// MethodMap — maps HTTP methods to route indices for a single path pattern
// ---------------------------------------------------------------------------

struct MethodMap {
    entries: Vec<(Method, usize)>,
}

impl MethodMap {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(2),
        }
    }

    fn insert(&mut self, method: Method, route_idx: usize) {
        self.entries.push((method, route_idx));
    }

    fn lookup(&self, method: &Method) -> Option<usize> {
        self.entries
            .iter()
            .find(|(m, _)| m == method)
            .map(|(_, idx)| *idx)
    }

    fn has_any(&self) -> bool {
        !self.entries.is_empty()
    }
}

/// Convert matchit params to a PathMatch, stripping the leading `/` from catch-all values.
fn params_to_path_match(params: &matchit::Params) -> PathMatch {
    let mut pm = PathMatch::with_capacity(params.iter().count());
    for (key, value) in params.iter() {
        let value = value.strip_prefix('/').unwrap_or(value);
        pm.push(key.to_string(), value.to_string());
    }
    pm
}

/// Metadata attached to a route, queryable at runtime.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "json", derive(serde::Serialize))]
pub struct RouteMetadata {
    pub name: Option<String>,
    pub tags: Vec<String>,
    pub deprecated: bool,
    pub custom: HashMap<String, String>,
}

/// A single route entry. Concrete struct, not a trait object graph.
pub struct Route {
    pub method: Method,
    pub pattern: PathPattern,
    pub handler: HandlerFn,
    pub metadata: RouteMetadata,
    /// Middleware scoped to this route (from route groups).
    /// Runs after global middleware, before the handler.
    /// Stored as `Arc` so group middleware can be shared across routes cheaply.
    pub middleware: Vec<Arc<dyn Middleware>>,
}

/// The route table. A `Vec` you can iterate, filter, print, serialize.
/// Uses matchit's compressed radix trie for O(path_length) lookups.
pub struct RouteTable {
    routes: Vec<Route>,
    router: matchit::Router<usize>,
    method_maps: Vec<MethodMap>,
    pattern_index: HashMap<String, usize>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            router: matchit::Router::new(),
            method_maps: Vec::new(),
            pattern_index: HashMap::new(),
        }
    }

    pub fn push(&mut self, route: Route) {
        let idx = self.routes.len();
        let matchit_pat = to_matchit_pattern(route.pattern.as_str());

        if let Some(&map_idx) = self.pattern_index.get(&matchit_pat) {
            self.method_maps[map_idx].insert(route.method.clone(), idx);
        } else {
            let map_idx = self.method_maps.len();
            let mut mm = MethodMap::new();
            mm.insert(route.method.clone(), idx);
            self.method_maps.push(mm);
            self.pattern_index.insert(matchit_pat.clone(), map_idx);
            self.router
                .insert(matchit_pat, map_idx)
                .expect("duplicate or conflicting route pattern");
        }

        self.routes.push(route);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Route> {
        self.routes.iter()
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Get a route by index.
    pub fn get(&self, idx: usize) -> Option<&Route> {
        self.routes.get(idx)
    }

    /// Find the best matching route for the given method and path.
    #[cfg_attr(feature = "profiling", inline(never))]
    pub fn match_route(&self, method: &Method, path: &str) -> Option<(&Route, PathMatch)> {
        let (idx, path_match) = self.match_route_idx(method, path)?;
        Some((&self.routes[idx], path_match))
    }

    /// Find the best matching route index for the given method and path.
    #[cfg_attr(feature = "profiling", inline(never))]
    pub fn match_route_idx(&self, method: &Method, path: &str) -> Option<(usize, PathMatch)> {
        let matched = self.router.at(path).ok()?;
        let map_idx = *matched.value;
        let route_idx = self.method_maps[map_idx].lookup(method)?;
        Some((route_idx, params_to_path_match(&matched.params)))
    }

    /// Check whether any route (regardless of method) matches this path.
    /// Used for 405 vs 404 distinction.
    pub fn any_route_matches_path(&self, path: &str) -> bool {
        self.router
            .at(path)
            .ok()
            .is_some_and(|m| self.method_maps[*m.value].has_any())
    }

    /// Return the HTTP methods registered for the given path.
    /// Used to populate the `Allow` header on 405 responses (RFC 9110 §15.5.6).
    pub fn allowed_methods(&self, path: &str) -> Vec<Method> {
        let matched = match self.router.at(path).ok() {
            Some(m) => m,
            None => return Vec::new(),
        };
        self.method_maps[*matched.value]
            .entries
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }
}

impl Default for RouteTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for the application. Owns route table, middleware, and state.
pub struct App {
    route_table: RouteTable,
    middleware: Vec<Box<dyn Middleware>>,
    state: crate::state::TypeMap,
}

impl App {
    pub fn new() -> Self {
        Self {
            route_table: RouteTable::new(),
            middleware: Vec::new(),
            state: crate::state::TypeMap::new(),
        }
    }

    /// Register a route (no route-level middleware).
    fn route<F, Fut>(mut self, method: Method, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route_table.push(Route {
            method,
            pattern: PathPattern::parse(pattern),
            handler: handler::wrap(handler),
            metadata: RouteMetadata::default(),
            middleware: Vec::new(), // no route-level middleware for top-level routes
        });
        self
    }

    pub fn get<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::GET, pattern, handler)
    }

    pub fn post<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::POST, pattern, handler)
    }

    pub fn put<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::PUT, pattern, handler)
    }

    pub fn delete<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::DELETE, pattern, handler)
    }

    pub fn patch<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::PATCH, pattern, handler)
    }

    /// Attach metadata to the most recently added route matching this pattern.
    pub fn with_metadata(mut self, pattern: &str, f: impl FnOnce(&mut RouteMetadata)) -> Self {
        if let Some(route) = self
            .route_table
            .routes
            .iter_mut()
            .rev()
            .find(|r| r.pattern.as_str() == pattern)
        {
            f(&mut route.metadata);
        }
        self
    }

    /// Add a global middleware. Runs on every request before route-level middleware.
    pub fn middleware<M: Middleware + 'static>(mut self, m: M) -> Self {
        self.middleware.push(Box::new(m));
        self
    }

    /// Register application state of type `T`.
    pub fn state<T: Send + Sync + 'static>(mut self, val: T) -> Self {
        self.state.insert(val);
        self
    }

    /// Create a route group with a shared prefix and optional scoped middleware.
    ///
    /// Routes defined inside the group get the prefix prepended and any
    /// middleware added to the group attached. Group middleware runs after
    /// global middleware but before the handler.
    ///
    /// ```ignore
    /// let app = App::new()
    ///     .get("/health", health)
    ///     .group("/api/v1", |g| {
    ///         g.middleware(auth_middleware)
    ///          .get("/users", list_users)
    ///          .get("/users/:id", get_user)
    ///     });
    /// ```
    pub fn group(mut self, prefix: &str, f: impl FnOnce(Group) -> Group) -> Self {
        let g = Group::new(prefix);
        let g = f(g);
        for route in g.into_routes() {
            self.route_table.push(route);
        }
        self
    }

    /// Access the route table for introspection.
    pub fn route_table(&self) -> &RouteTable {
        &self.route_table
    }

    /// Consume the builder, returning the parts needed by the server.
    pub fn into_parts(self) -> (RouteTable, Vec<Box<dyn Middleware>>, crate::state::TypeMap) {
        (self.route_table, self.middleware, self.state)
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Route Group
// ---------------------------------------------------------------------------

/// A group of routes sharing a common path prefix and scoped middleware.
///
/// Created via `App::group()` or `Group::group()` for nesting.
pub struct Group {
    prefix: String,
    middleware: Vec<Arc<dyn Middleware>>,
    routes: Vec<Route>,
}

impl Group {
    fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.trim_end_matches('/').to_string(),
            middleware: Vec::new(),
            routes: Vec::new(),
        }
    }

    /// Add middleware scoped to this group. Runs after global middleware,
    /// before the handler, only for routes in this group.
    pub fn middleware<M: Middleware + 'static>(mut self, m: M) -> Self {
        self.middleware.push(Arc::new(m));
        self
    }

    /// Register a route within this group. The group prefix is prepended.
    /// Group middleware is attached later in `into_routes()`.
    fn route<F, Fut>(mut self, method: Method, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        let full_pattern = format!("{}{}", self.prefix, pattern);
        self.routes.push(Route {
            method,
            pattern: PathPattern::parse(&full_pattern),
            handler: handler::wrap(handler),
            metadata: RouteMetadata::default(),
            middleware: Vec::new(),
        });
        self
    }

    pub fn get<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::GET, pattern, handler)
    }

    pub fn post<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::POST, pattern, handler)
    }

    pub fn put<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::PUT, pattern, handler)
    }

    pub fn delete<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::DELETE, pattern, handler)
    }

    pub fn patch<F, Fut>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response> + Send + 'static,
    {
        self.route(Method::PATCH, pattern, handler)
    }

    /// Nest a sub-group. The sub-group's prefix is appended to this group's
    /// prefix, and middleware from both groups is combined (outer group first).
    ///
    /// ```ignore
    /// app.group("/api", |g| {
    ///     g.middleware(auth)
    ///      .group("/v1", |v1| {
    ///          v1.middleware(rate_limit)
    ///            .get("/users", list_users)  // /api/v1/users — auth + rate_limit
    ///      })
    ///      .get("/health", health)           // /api/health — auth only
    /// })
    /// ```
    pub fn group(mut self, prefix: &str, f: impl FnOnce(Group) -> Group) -> Self {
        let nested_prefix = format!("{}{}", self.prefix, prefix.trim_end_matches('/'));
        let sub = Group::new(&nested_prefix);
        let sub = f(sub);
        for mut route in sub.into_routes() {
            // Prepend this group's middleware before the sub-group's middleware.
            let mut combined: Vec<Arc<dyn Middleware>> = Vec::new();
            for mw in &self.middleware {
                combined.push(Arc::clone(mw));
            }
            combined.append(&mut route.middleware);
            route.middleware = combined;
            self.routes.push(route);
        }
        self
    }

    /// Consume the group, attaching group middleware to each route.
    fn into_routes(self) -> Vec<Route> {
        let mut routes = self.routes;
        for route in &mut routes {
            // Prepend group middleware before any existing per-route middleware
            // (which may come from nested sub-groups).
            let mut combined: Vec<Arc<dyn Middleware>> = Vec::new();
            for mw in &self.middleware {
                combined.push(Arc::clone(mw));
            }
            combined.append(&mut route.middleware);
            route.middleware = combined;
        }
        routes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler;

    async fn dummy(_req: Request) -> Response {
        Response::text("ok")
    }

    fn make_route(method: Method, pattern: &str) -> Route {
        Route {
            method,
            pattern: PathPattern::parse(pattern),
            handler: handler::wrap(dummy),
            metadata: RouteMetadata::default(),
            middleware: Vec::new(),
        }
    }

    #[test]
    fn trie_exact_match() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/health"));
        table.push(make_route(Method::GET, "/users"));

        let (idx, _) = table.match_route_idx(&Method::GET, "/health").unwrap();
        assert_eq!(idx, 0);
        let (idx, _) = table.match_route_idx(&Method::GET, "/users").unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn trie_param_match() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/users/:id"));

        let (idx, pm) = table.match_route_idx(&Method::GET, "/users/42").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(pm.get("id"), Some("42"));
    }

    #[test]
    fn trie_glob_match() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/files/*path"));

        let (idx, pm) = table
            .match_route_idx(&Method::GET, "/files/a/b/c.txt")
            .unwrap();
        assert_eq!(idx, 0);
        assert_eq!(pm.get("path"), Some("a/b/c.txt"));
    }

    #[test]
    fn trie_literal_over_param_priority() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/users/:id"));
        table.push(make_route(Method::GET, "/users/me"));

        // Literal "/users/me" should win even though param was registered first
        let (idx, _) = table.match_route_idx(&Method::GET, "/users/me").unwrap();
        assert_eq!(idx, 1);

        // Other values still match param
        let (idx, pm) = table.match_route_idx(&Method::GET, "/users/42").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(pm.get("id"), Some("42"));
    }

    #[test]
    fn trie_method_filtering() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/users"));
        table.push(make_route(Method::POST, "/users"));

        let (idx, _) = table.match_route_idx(&Method::GET, "/users").unwrap();
        assert_eq!(idx, 0);
        let (idx, _) = table.match_route_idx(&Method::POST, "/users").unwrap();
        assert_eq!(idx, 1);
        assert!(table.match_route_idx(&Method::DELETE, "/users").is_none());
    }

    #[test]
    fn trie_404_miss() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/health"));

        assert!(table.match_route_idx(&Method::GET, "/nope").is_none());
        assert!(
            table
                .match_route_idx(&Method::GET, "/health/extra")
                .is_none()
        );
    }

    #[test]
    fn trie_root_path() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/"));

        let (idx, _) = table.match_route_idx(&Method::GET, "/").unwrap();
        assert_eq!(idx, 0);
    }

    #[test]
    fn trie_any_route_matches_path() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/users"));
        table.push(make_route(Method::POST, "/users/:id"));

        assert!(table.any_route_matches_path("/users"));
        assert!(table.any_route_matches_path("/users/42"));
        assert!(!table.any_route_matches_path("/nope"));
    }

    #[test]
    fn trie_introspection_unchanged() {
        let mut table = RouteTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);

        table.push(make_route(Method::GET, "/a"));
        table.push(make_route(Method::GET, "/b"));

        assert!(!table.is_empty());
        assert_eq!(table.len(), 2);
        assert!(table.get(0).is_some());
        assert!(table.get(2).is_none());
        assert_eq!(table.iter().count(), 2);
    }

    #[test]
    fn trie_glob_zero_segments() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/files/*path"));

        // matchit catch-all requires at least one character; `/files/` does not match.
        // This matches axum/actix/salvo behavior.
        assert!(table.match_route_idx(&Method::GET, "/files/").is_none());

        // But a single segment does match
        let (idx, pm) = table.match_route_idx(&Method::GET, "/files/x").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(pm.get("path"), Some("x"));
    }

    #[test]
    fn allowed_methods_returns_registered_methods() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/users"));
        table.push(make_route(Method::POST, "/users"));
        table.push(make_route(Method::DELETE, "/users"));

        let mut methods: Vec<String> = table
            .allowed_methods("/users")
            .iter()
            .map(|m| m.to_string())
            .collect();
        methods.sort();
        assert_eq!(methods, vec!["DELETE", "GET", "POST"]);

        // Non-existent path returns empty
        assert!(table.allowed_methods("/nope").is_empty());
    }

    #[test]
    fn trie_match_route_returns_route() {
        let mut table = RouteTable::new();
        table.push(make_route(Method::GET, "/users/:id"));

        let (route, pm) = table.match_route(&Method::GET, "/users/99").unwrap();
        assert_eq!(route.pattern.as_str(), "/users/:id");
        assert_eq!(pm.get("id"), Some("99"));
    }
}
