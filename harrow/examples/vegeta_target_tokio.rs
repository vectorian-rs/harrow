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

mod common;

use std::sync::LazyLock;
use std::time::Duration;

use harrow::{
    App, InMemorySessionStore, Request, Response, SameSite, Session, SessionConfig,
    cors_middleware, request_id_middleware, session_middleware, timeout_middleware,
};

async fn root(_req: Request) -> Response {
    Response::text("hello, world")
}

async fn mutate_user(req: Request, op: &'static str) -> Response {
    let user_id = req.param("id").to_string();
    match req.body_json::<serde_json::Value>().await {
        Ok(body) => Response::json(&serde_json::json!({
            "id": user_id,
            op: true,
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

static COMPRESSION_TEXT: LazyLock<String> = LazyLock::new(|| {
    "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(100)
});

static COMPRESSION_JSON: LazyLock<String> = LazyLock::new(|| {
    let data: Vec<_> = (0..100)
        .map(|i| {
            serde_json::json!({
                "id": i,
                "name": format!("Item {}", i),
                "description": "Lorem ipsum dolor sit amet, consectetur adipiscing elit.",
                "active": i % 2 == 0,
            })
        })
        .collect();
    serde_json::json!({ "items": data }).to_string()
});

async fn compression_test(_req: Request) -> Response {
    Response::text(COMPRESSION_TEXT.as_str())
}

async fn compression_json(_req: Request) -> Response {
    Response::text(COMPRESSION_JSON.as_str()).header("content-type", "application/json")
}

async fn slow_handler(_req: Request) -> Response {
    tokio::time::sleep(Duration::from_secs(10)).await;
    Response::text("this should time out")
}

async fn middleware_test(req: Request) -> Response {
    let request_id = req.header("x-request-id").unwrap_or("none");
    Response::json(&serde_json::json!({
        "request_id": request_id,
        "cors": req.header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN.as_str()).is_some(),
    }))
}

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
        let counter: i32 = session
            .get("counter")
            .unwrap_or_else(|| "0".to_string())
            .parse()
            .unwrap_or(0);
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

    let addr = common::parse_args("vegeta_target_tokio");

    let session_config = SessionConfig::new([0u8; 32])
        .cookie_name("sid")
        .ttl(Duration::from_secs(3600))
        .same_site(SameSite::Lax)
        .secure(false); // Allow HTTP for testing
    let session_store = InMemorySessionStore::new();

    let app = App::new()
        .default_problem_details()
        .health("/health")
        .liveness("/live")
        .readiness_handler("/ready", common::readiness)
        .middleware(request_id_middleware)
        .middleware(cors_middleware(Default::default()))
        .middleware(session_middleware(session_store, session_config))
        .middleware(timeout_middleware(Duration::from_secs(5)))
        .get("/", root)
        .get("/users/:id", common::get_user)
        .post("/users", common::create_user)
        .put("/users/:id", |req| mutate_user(req, "updated"))
        .patch("/users/:id", |req| mutate_user(req, "patched"))
        .delete("/users/:id", delete_user)
        .get("/users/:user_id/posts/:post_id", common::get_user_posts)
        .get("/echo", common::echo)
        .post("/echo", common::echo)
        .put("/echo", common::echo)
        .patch("/echo", common::echo)
        .delete("/echo", common::echo)
        .get("/compress/text", compression_test)
        .get("/compress/json", compression_json)
        .get("/middleware-test", middleware_test)
        .get("/session", session_get)
        .post("/session/increment", session_increment)
        .delete("/session", session_destroy)
        .get("/slow", slow_handler)
        .get("/cpu", common::cpu_intensive);

    tracing::info!("Tokio server starting on http://{}", addr);
    harrow::runtime::tokio::serve(app, addr).await.unwrap();
}
