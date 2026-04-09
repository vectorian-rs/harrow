//! Shared handlers and utilities for load-test examples.

use std::net::{IpAddr, SocketAddr};

use harrow::{Request, Response};

pub fn parse_args(program: &str) -> SocketAddr {
    let args: Vec<String> = std::env::args().collect();
    let mut bind: IpAddr = [0, 0, 0, 0].into();
    let mut port: u16 = 3000;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                bind = args
                    .get(i + 1)
                    .expect("--bind requires an address")
                    .parse()
                    .expect("invalid bind address");
                i += 2;
            }
            "--port" => {
                port = args
                    .get(i + 1)
                    .expect("--port requires a number")
                    .parse()
                    .expect("invalid port number");
                i += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("usage: {program} [--bind ADDR] [--port PORT]");
                std::process::exit(1);
            }
        }
    }
    SocketAddr::new(bind, port)
}

pub async fn readiness(_req: Request) -> Response {
    Response::json(&serde_json::json!({ "ready": true }))
}

pub async fn get_user(req: Request) -> Response {
    let user_id = req.param("id");
    Response::json(&serde_json::json!({
        "id": user_id,
        "name": format!("User {}", user_id),
    }))
}

pub async fn get_user_posts(req: Request) -> Response {
    let user_id = req.param("user_id");
    let post_id = req.param("post_id");
    Response::json(&serde_json::json!({
        "user_id": user_id,
        "post_id": post_id,
        "title": format!("Post {} by user {}", post_id, user_id),
    }))
}

pub async fn create_user(req: Request) -> Response {
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

pub async fn echo(req: Request) -> Response {
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

pub async fn cpu_intensive(_req: Request) -> Response {
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
