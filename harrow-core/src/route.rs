use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::{Method, StatusCode};

use crate::handler::{self, HandlerFn};
use crate::middleware::Middleware;
use crate::path::{PathMatch, PathPattern, to_matchit_pattern};
use crate::problem::ProblemDetail;
use crate::request::Request;
use crate::response::{IntoResponse, Response};

pub(crate) type NotFoundHandlerFn =
    Box<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync>;

pub(crate) type MethodNotAllowedHandlerFn = Box<
    dyn Fn(Request, Vec<Method>) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync,
>;

pub(crate) struct NotFoundHandler(pub NotFoundHandlerFn);

pub(crate) struct MethodNotAllowedHandler(pub MethodNotAllowedHandlerFn);

fn wrap_not_found_handler<F, Fut, T>(f: F) -> NotFoundHandlerFn
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: IntoResponse + 'static,
{
    handler::wrap(f)
}

fn wrap_method_not_allowed_handler<F, Fut, T>(f: F) -> MethodNotAllowedHandlerFn
where
    F: Fn(Request, Vec<Method>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: IntoResponse + 'static,
{
    Box::new(move |req, methods| {
        let fut = f(req, methods);
        Box::pin(async move { fut.await.into_response() })
    })
}

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
        let mut methods: Vec<Method> = self.method_maps[*matched.value]
            .entries
            .iter()
            .map(|(m, _)| m.clone())
            .collect();
        // RFC 9110 §9.3.2: HEAD is implicitly supported when GET is.
        if methods.contains(&Method::GET) && !methods.contains(&Method::HEAD) {
            methods.push(Method::HEAD);
        }
        methods
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
    max_body_size: usize,
    not_found_handler: Option<NotFoundHandlerFn>,
    method_not_allowed_handler: Option<MethodNotAllowedHandlerFn>,
}

impl App {
    pub fn new() -> Self {
        Self {
            route_table: RouteTable::new(),
            middleware: Vec::new(),
            state: crate::state::TypeMap::new(),
            max_body_size: crate::request::DEFAULT_MAX_BODY_SIZE,
            not_found_handler: None,
            method_not_allowed_handler: None,
        }
    }

    /// Register a route (no route-level middleware).
    fn route<F, Fut, T>(mut self, method: Method, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
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

    pub fn get<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::GET, pattern, handler)
    }

    pub fn post<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::POST, pattern, handler)
    }

    pub fn put<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::PUT, pattern, handler)
    }

    pub fn delete<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::DELETE, pattern, handler)
    }

    pub fn patch<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::PATCH, pattern, handler)
    }

    fn probe<F, Fut, T>(self, kind: &'static str, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::GET, pattern, handler)
            .with_metadata(pattern, |metadata| {
                metadata.name = Some(kind.to_string());
                metadata.tags.push("probe".to_string());
                metadata.tags.push(kind.to_string());
            })
    }

    /// Register a static health endpoint that returns `200 OK` with body `ok`.
    pub fn health(self, pattern: &str) -> Self {
        self.health_handler(pattern, |_req| async { Response::text("ok") })
    }

    /// Register a custom health endpoint.
    pub fn health_handler<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.probe("health", pattern, handler)
    }

    /// Register a static liveness endpoint that returns `200 OK` with body `alive`.
    pub fn liveness(self, pattern: &str) -> Self {
        self.liveness_handler(pattern, |_req| async { Response::text("alive") })
    }

    /// Register a custom liveness endpoint.
    pub fn liveness_handler<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.probe("liveness", pattern, handler)
    }

    /// Register a static readiness endpoint that returns `200 OK` with body `ready`.
    pub fn readiness(self, pattern: &str) -> Self {
        self.readiness_handler(pattern, |_req| async { Response::text("ready") })
    }

    /// Register a custom readiness endpoint.
    pub fn readiness_handler<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.probe("readiness", pattern, handler)
    }

    /// Customize the framework-generated 404 response.
    pub fn not_found_handler<F, Fut, T>(mut self, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.not_found_handler = Some(wrap_not_found_handler(handler));
        self
    }

    /// Customize the framework-generated 405 response.
    pub fn method_not_allowed_handler<F, Fut, T>(mut self, handler: F) -> Self
    where
        F: Fn(Request, Vec<Method>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.method_not_allowed_handler = Some(wrap_method_not_allowed_handler(handler));
        self
    }

    /// Use RFC 9457 problem details for framework-generated 404 and 405 responses.
    pub fn default_problem_details(self) -> Self {
        self.not_found_handler(|req| async move {
            let path = req.path().to_string();
            ProblemDetail::new(StatusCode::NOT_FOUND)
                .detail(format!("no route for {path}"))
                .instance(path)
        })
        .method_not_allowed_handler(|req, allowed| async move {
            let path = req.path().to_string();
            let allow = allowed
                .iter()
                .map(|method| method.as_str())
                .collect::<Vec<_>>()
                .join(", ");

            ProblemDetail::new(StatusCode::METHOD_NOT_ALLOWED)
                .detail(format!("allowed methods: {allow}"))
                .instance(path)
                .extension("allow", allow)
        })
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

    /// Set the maximum request body size in bytes. Default: 2 MiB.
    /// Set to 0 to disable the limit.
    pub fn max_body_size(mut self, bytes: usize) -> Self {
        self.max_body_size = bytes;
        self
    }

    /// Access the route table for introspection.
    pub fn route_table(&self) -> &RouteTable {
        &self.route_table
    }

    /// Create a test client that dispatches requests through the full
    /// middleware + routing pipeline without TCP.
    pub fn client(self) -> crate::client::Client {
        let (route_table, middleware, state, max_body_size) = self.into_parts();
        let shared = Arc::new(crate::dispatch::SharedState {
            route_table,
            middleware,
            state: Arc::new(state),
            max_body_size,
        });
        crate::client::Client::new(shared)
    }

    /// Consume the builder, returning the parts needed by the server.
    pub fn into_parts(
        mut self,
    ) -> (
        RouteTable,
        Vec<Box<dyn Middleware>>,
        crate::state::TypeMap,
        usize,
    ) {
        if let Some(handler) = self.not_found_handler.take() {
            self.state.insert(NotFoundHandler(handler));
        }
        if let Some(handler) = self.method_not_allowed_handler.take() {
            self.state.insert(MethodNotAllowedHandler(handler));
        }

        (
            self.route_table,
            self.middleware,
            self.state,
            self.max_body_size,
        )
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
    fn route<F, Fut, T>(mut self, method: Method, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
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

    pub fn get<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::GET, pattern, handler)
    }

    pub fn post<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::POST, pattern, handler)
    }

    pub fn put<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::PUT, pattern, handler)
    }

    pub fn delete<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
    {
        self.route(Method::DELETE, pattern, handler)
    }

    pub fn patch<F, Fut, T>(self, pattern: &str, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: IntoResponse + 'static,
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
    use crate::response::Response;
    use http::StatusCode;

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
        // HEAD is implicitly added because GET is registered (RFC 9110 §9.3.2).
        assert_eq!(methods, vec!["DELETE", "GET", "HEAD", "POST"]);

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

    struct TestError;

    impl IntoResponse for TestError {
        fn into_response(self) -> Response {
            Response::new(StatusCode::BAD_REQUEST, "bad request")
        }
    }

    #[tokio::test]
    async fn app_get_accepts_fallible_handlers() {
        async fn fallible(_req: Request) -> Result<Response, TestError> {
            Err(TestError)
        }

        let client = App::new().get("/fallible", fallible).client();
        let resp = client.get("/fallible").await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text(), "bad request");
    }

    #[tokio::test]
    async fn group_get_accepts_fallible_handlers() {
        async fn fallible(_req: Request) -> Result<Response, TestError> {
            Err(TestError)
        }

        let client = App::new()
            .group("/api", |g| g.get("/fallible", fallible))
            .client();
        let resp = client.get("/api/fallible").await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text(), "bad request");
    }

    #[tokio::test]
    async fn probe_helpers_register_routes_and_attach_metadata() {
        let app = App::new()
            .health("/health")
            .liveness("/live")
            .readiness("/ready");

        let routes: Vec<(String, String, Vec<String>)> = app
            .route_table()
            .iter()
            .map(|route| {
                (
                    route.pattern.as_str().to_string(),
                    route.metadata.name.clone().unwrap_or_default(),
                    route.metadata.tags.to_vec(),
                )
            })
            .collect();

        assert_eq!(
            routes,
            vec![
                (
                    "/health".to_string(),
                    "health".to_string(),
                    vec!["probe".to_string(), "health".to_string()],
                ),
                (
                    "/live".to_string(),
                    "liveness".to_string(),
                    vec!["probe".to_string(), "liveness".to_string()],
                ),
                (
                    "/ready".to_string(),
                    "readiness".to_string(),
                    vec!["probe".to_string(), "readiness".to_string()],
                ),
            ]
        );

        let client = app.client();

        let resp = client.get("/health").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text(), "ok");

        let resp = client.get("/live").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text(), "alive");

        let resp = client.get("/ready").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text(), "ready");
    }

    #[tokio::test]
    async fn custom_not_found_handler_is_used_but_status_stays_404() {
        let client = App::new()
            .not_found_handler(|req| async move {
                Response::text(format!("missing {}", req.path())).status(200)
            })
            .client();

        let resp = client.get("/nope").await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.text(), "missing /nope");
    }

    #[tokio::test]
    async fn custom_method_not_allowed_handler_is_used_and_allow_is_enforced() {
        let client = App::new()
            .get("/users", dummy)
            .post("/users", dummy)
            .method_not_allowed_handler(|_req, methods| async move {
                let body = methods
                    .iter()
                    .map(|method| method.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");

                Response::text(body).status(200).header("allow", "CUSTOM")
            })
            .client();

        let resp = client.put("/users", "").await;

        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(resp.text(), "GET, POST, HEAD");

        let allow = resp.header("allow").expect("expected Allow header on 405");
        let mut methods: Vec<&str> = allow.split(", ").collect();
        methods.sort();
        assert_eq!(methods, vec!["GET", "HEAD", "POST"]);
    }

    #[tokio::test]
    async fn default_problem_details_formats_framework_404_and_405() {
        let client = App::new()
            .default_problem_details()
            .get("/users", dummy)
            .client();

        let resp = client.get("/nope").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.header("content-type"),
            Some("application/problem+json")
        );
        assert!(resp.text().contains("\"status\":404"));
        assert!(resp.text().contains("\"detail\":\"no route for /nope\""));

        let resp = client.post("/users", "").await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            resp.header("content-type"),
            Some("application/problem+json")
        );
        assert_eq!(resp.header("allow"), Some("GET, HEAD"));
        assert!(resp.text().contains("\"status\":405"));
        assert!(resp.text().contains("\"allow\":\"GET, HEAD\""));
    }
}
