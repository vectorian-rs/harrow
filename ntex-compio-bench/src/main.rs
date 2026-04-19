#[cfg(all(feature = "mimalloc", feature = "jemalloc"))]
compile_error!(
    "features `mimalloc` and `jemalloc` are mutually exclusive — \
     use `--features mimalloc` OR `--no-default-features --features jemalloc`"
);

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(target_os = "linux")]
const ALLOCATOR_NAME: &str = if cfg!(feature = "mimalloc") {
    "mimalloc"
} else if cfg!(feature = "jemalloc") {
    "jemalloc"
} else {
    "system"
};

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ntex-compio-perf-server requires Linux. Exiting.");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod server {
    use std::sync::LazyLock;

    use ntex::web;
    use serde::Serialize;

    #[derive(Debug, Clone, Serialize)]
    struct User {
        id: u32,
        name: String,
        email: String,
        active: bool,
        score: u32,
        tags: Vec<String>,
    }

    static USERS_10: LazyLock<Vec<User>> = LazyLock::new(|| users(10));
    static USERS_100: LazyLock<Vec<User>> = LazyLock::new(|| users(100));

    fn users(count: u32) -> Vec<User> {
        (0..count)
            .map(|i| User {
                id: i,
                name: format!("User {i}"),
                email: format!("user{i}@example.com"),
                active: i % 2 == 0,
                score: i * 17 + 42,
                tags: vec!["bench".into(), "test".into(), "user".into()],
            })
            .collect()
    }

    fn parse_bind_port() -> (String, u16) {
        let args: Vec<String> = std::env::args().collect();
        let mut bind = "127.0.0.1".to_string();
        let mut port: u16 = 3090;
        let mut i = 1;

        while i < args.len() {
            match args[i].as_str() {
                "--bind" => {
                    bind = args.get(i + 1).expect("--bind requires an address").clone();
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
                    eprintln!("usage: ntex-compio-perf-server [--bind ADDR] [--port PORT]");
                    std::process::exit(1);
                }
            }
        }

        (bind, port)
    }

    #[web::get("/text")]
    async fn text_handler() -> &'static str {
        "ok"
    }

    #[web::get("/json/1kb")]
    async fn json_1kb_handler() -> web::types::Json<&'static Vec<User>> {
        web::types::Json(&USERS_10)
    }

    #[web::get("/json/10kb")]
    async fn json_10kb_handler() -> web::types::Json<&'static Vec<User>> {
        web::types::Json(&USERS_100)
    }

    #[web::get("/health")]
    async fn health_handler() -> &'static str {
        "ok"
    }

    #[ntex::main]
    async fn run() -> std::io::Result<()> {
        let (bind, port) = parse_bind_port();
        let addr = format!("{bind}:{port}");

        eprintln!(
            "ntex-compio-perf-server listening on {addr} [allocator: {}]",
            crate::ALLOCATOR_NAME
        );

        web::HttpServer::new(async || {
            web::App::new()
                .service(text_handler)
                .service(json_1kb_handler)
                .service(json_10kb_handler)
                .service(health_handler)
        })
        .bind(&addr)?
        .run()
        .await
    }

    pub(super) fn main() {
        run().expect("ntex compio server failed");
    }
}

#[cfg(target_os = "linux")]
fn main() {
    server::main();
}
