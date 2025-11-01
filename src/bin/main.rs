use std::{env, error::Error};
use ring::run;

/// Resolve the listening address from CLI arg or PORT env.
/// Accepts either:
///   - "7001"                    -> becomes "127.0.0.1:7001"
///   - "127.0.0.1:7001"          -> used as-is
/// Defaults to 127.0.0.1:9000 if neither is set.
fn listen_addr() -> String {
    let arg = env::args().nth(1);
    let from_env = env::var("PORT").ok();

    let raw = arg.or(from_env);
    match raw.as_deref() {
        Some(val) if val.contains(':') => val.to_string(),
        Some(port) => format!("127.0.0.1:{port}"),
        None => "127.0.0.1:9000".to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // 1. Resolve the address
    let addr = listen_addr();

    // 2. Run the server
    run(&addr).await
}
