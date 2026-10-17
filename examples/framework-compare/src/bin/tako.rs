use framework_compare::{tako_app, domain::Store};

fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let addr = std::env::var("ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    tako_app::run(addr.parse().expect("ADDR"), Store::seeded());
}
