//! bob-relay binary: parse args and serve the router from the library.

use clap::Parser;

#[derive(Parser)]
#[command(name = "bob-relay", about = "Public WebSocket relay for bob remote control.")]
struct Args {
    /// Address to bind, e.g. 0.0.0.0:8787
    #[arg(long, default_value = "127.0.0.1:8787")]
    addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let token = std::env::var("BOB_RELAY_TOKEN").unwrap_or_else(|_| "dev-token".to_string());
    if token == "dev-token" {
        eprintln!("[relay] WARNING: using default token 'dev-token'; set BOB_RELAY_TOKEN");
    }
    let listener = tokio::net::TcpListener::bind(&args.addr).await?;
    eprintln!("[relay] listening on {}", args.addr);
    axum::serve(listener, bob_relay::router(token)).await?;
    Ok(())
}
