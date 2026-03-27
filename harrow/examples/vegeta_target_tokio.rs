//! Comprehensive test server for Vegeta load testing (Tokio backend).
//!
//! This server exposes all Harrow features for load testing:
//! - All HTTP methods (GET, POST, PUT, PATCH, DELETE)
//! - Path parameters
//! - JSON/text responses with compression
//! - Middleware chain (timeout, request-id, CORS, compression)
//! - Health/liveness/readiness probes
//! - Error responses (404, 405)
//!
//! Run with: cargo run --example vegeta_target_tokio --features tokio,timeout,request-id,cors,compression,json

use std::time::Duration;

use harrow::{
    cors_middleware, request_id_middleware, session_middleware, timeout_middleware,
    InMemorySessionStore, SameSite, Session, SessionConfig, App, ProblemDetail, Request, Response,
};

// Basic handlers
async fn root(_req: Request) -> Response {
    Response::text("hello, world")
}

async fn health(_req: Request) -> Response {
    Response::json(&serde_json::json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339(),
    }))
}

async fn liveness(_req: Request) -> Response {
    Response::text("alive")
}

async fn readiness(_req: Request) -> Response {
    Response::json(&serde_json::json!({ "ready": true }))
}

// Path parameter handlers
async fn get_user(req: Request) -> Response {
    let user_id = req.param("id");
    Response::json(&serde_json::json!({
        "id": user_id,
        "name": format!("User {}", user_id),
    }))
}

async fn get_user_posts(req: Request) -> Response {
    let user_id = req.param("user_id");
    let post_id = req.param("post_id");
    Response::json(&serde_json::json!({
        "user_id": user_id,
        "post_id": post_id,
        "title": format!("Post {} by user {}", post_id, user_id),
    }))
}

// CRUD handlers for /users resource
async fn create_user(req: Request) -> Response {
    match req.body_json::<serde_json::Value>().await {
        Ok(body) => Response::json(&serde_json::json!({
            "id": 123,
            "created": true,
            "data": body,
        }))
        .status(201),
        Err(_) => Response::text("invalid json").status(400),
    }
}

async fn update_user(req: Request) -> Response {
    let user_id = req.param("id").to_string();
    match req.body_json::<serde_json::Value>().await {
        Ok(body) => Response::json(&serde_json::json!({
            "id": user_id,
            "updated": true,
            "data": body,
        })),
        Err(_) => Response::text("invalid json").status(400),
    }
}

async fn patch_user(req: Request) -> Response {
    let user_id = req.param("id").to_string();
    match req.body_json::<serde_json::Value>().await {
        Ok(body) => Response::json(&serde_json::json!({
            "id": user_id,
            "patched": true,
            "partial": true,
            "data": body,
        })),
        Err(_) => Response::text("invalid json").status(400),
    }
}

async fn delete_user(req: Request) -> Response {
    let user_id = req.param("id");
    Response::json(&serde_json::json!({
        "id": user_id,
        "deleted": true,
    }))
}

// Echo handler for all methods
async fn echo(req: Request) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.path().to_string();
    let body = match req.body_json::<serde_json::Value>().await {
        Ok(json) => json,
        Err(_) => serde_json::json!(null),
    };
    
    Response::json(&serde_json::json!({
        "method": method,
        "path": path,
        "body": body,
    }))
}

// Compression test - returns large text that benefits from compression
async fn compression_test(_req: Request) -> Response {
    let large_text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(100);
    Response::text(large_text)
}

// Compression test - JSON
async fn compression_json(_req: Request) -> Response {
    let data: Vec<_> = (0..100).map(|i| {
        serde_json::json!({
            "id": i,
            "name": format!("Item {}", i),
            "description": "Lorem ipsum dolor sit amet, consectetur adipiscing elit.",
            "active": i % 2 == 0,
        })
    }).collect();
    
    Response::json(&serde_json::json!({ "items": data }))
}

// Error handlers
async fn not_found_handler(req: Request) -> ProblemDetail {
    ProblemDetail::new(http::StatusCode::NOT_FOUND)
        .detail(format!("no route for {} {}", req.method(), req.path()))
}

// Slow handler for timeout testing
async fn slow_handler(_req: Request) -> Response {
    tokio::time::sleep(Duration::from_secs(10)).await;
    Response::text("this should time out")
}

// CPU-intensive handler - meaningful work to stress CPU
async fn cpu_intensive(_req: Request) -> Response {
    // Perform actual computation to stress CPU
    // Calculate fibonacci(35) recursively (about 29M operations)
    fn fib(n: u32) -> u64 {
        match n {
            0 => 0,
            1 => 1,
            _ => fib(n - 1) + fib(n - 2),
        }
    }
    
    let result = fib(35);
    Response::json(&serde_json::json!({ "fib": result }))
}

// Middleware chain test - reports which middleware ran
async fn middleware_test(req: Request) -> Response {
    let request_id = req.header("x-request-id").unwrap_or("none");
    Response::json(&serde_json::json!({
        "request_id": request_id,
        "cors": req.header("access-control-allow-origin").is_some(),
    }))
}

// Session test handlers
async fn session_get(req: Request) -> Response {
    if let Ok(session) = req.require_ext::<Session>() {
        let counter = session.get("counter").unwrap_or_else(|| "0".to_string());
        Response::json(&serde_json::json!({
            "counter": counter,
            "session_id": session.id(),
        }))
    } else {
        Response::json(&serde_json::json!({"error": "no session"})).status(500)
    }
}

async fn session_increment(req: Request) -> Response {
    if let Ok(session) = req.require_ext::<Session>() {
        let counter: i32 = session.get("counter")
            .unwrap_or_else(|| "0".to_string())
            .parse().unwrap_or(0);
        session.set("counter", &(counter + 1).to_string());
        Response::json(&serde_json::json!({
            "counter": counter + 1,
            "session_id": session.id(),
        }))
    } else {
        Response::json(&serde_json::json!({"error": "no session"})).status(500)
    }
}

async fn session_destroy(req: Request) -> Response {
    if let Ok(session) = req.require_ext::<Session>() {
        session.destroy();
        Response::json(&serde_json::json!({"destroyed": true}))
    } else {
        Response::json(&serde_json::json!({"error": "no session"})).status(500)
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let addr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string())
        .parse()
        .unwrap();

    // Session configuration
    let session_config = SessionConfig::new([0u8; 32])
        .cookie_name("sid")
        .ttl(Duration::from_secs(3600))
        .same_site(SameSite::Lax)
        .secure(false); // Allow HTTP for testing
    let session_store = InMemorySessionStore::new();

    let app = App::new()
        // Default handlers
        .not_found_handler(not_found_handler)
        // Probes
        .health("/health")
        .liveness("/live")
        .readiness_handler("/ready", readiness)
        // Middleware (applied in order - outermost first)
        .middleware(request_id_middleware)
        .middleware(cors_middleware(Default::default()))
        .middleware(session_middleware(session_store, session_config))
        .middleware(timeout_middleware(Duration::from_secs(5)))
        // Routes
        .get("/", root)
        .get("/health", health)
        .get("/live", liveness)
        // User CRUD API
        .get("/users/:id", get_user)
        .post("/users", create_user)
        .put("/users/:id", update_user)
        .patch("/users/:id", patch_user)
        .delete("/users/:id", delete_user)
        .get("/users/:user_id/posts/:post_id", get_user_posts)
        // Echo for all methods
        .get("/echo", echo)
        .post("/echo", echo)
        .put("/echo", echo)
        .patch("/echo", echo)
        .delete("/echo", echo)
        // Compression tests
        .get("/compress/text", compression_test)
        .get("/compress/json", compression_json)
        // Middleware test
        .get("/middleware-test", middleware_test)
        // Session test endpoints
        .get("/session", session_get)
        .post("/session/increment", session_increment)
        .delete("/session", session_destroy)
        // Load test scenarios
        .get("/slow", slow_handler)
        .get("/cpu", cpu_intensive);

    tracing::info!("Tokio server starting on http://{}", addr);
    tracing::info!("Endpoints:");
    tracing::info!("  GET    /                    - Root/hello");
    tracing::info!("  GET    /health              - Health check (JSON)");
    tracing::info!("  GET    /live                - Liveness probe");
    tracing::info!("  GET    /ready               - Readiness probe");
    tracing::info!("  GET    /users/:id           - Get user");
    tracing::info!("  POST   /users               - Create user");
    tracing::info!("  PUT    /users/:id           - Update user");
    tracing::info!("  PATCH  /users/:id           - Patch user");
    tracing::info!("  DELETE /users/:id           - Delete user");
    tracing::info!("  GET    /users/:user_id/posts/:post_id - Nested params");
    tracing::info!("  GET|POST|PUT|PATCH|DELETE  /echo - Echo method");
    tracing::info!("  GET    /compress/text       - Compressed text");
    tracing::info!("  GET    /compress/json       - Compressed JSON");
    tracing::info!("  GET    /middleware-test      - Middleware chain test");
    tracing::info!("  GET    /session               - Get session data");
    tracing::info!("  POST   /session/increment    - Increment session counter");
    tracing::info!("  DELETE /session               - Destroy session");
    tracing::info!("  GET    /slow                 - Slow handler (timeout test)");
    tracing::info!("  GET    /cpu                  - CPU intensive");

    harrow::serve(app, addr).await.unwrap();
}
