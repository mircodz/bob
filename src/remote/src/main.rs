//! bob-remote: two roles over the relay.
//! - `host` (default): drives a bob-core Agent, streams events to the relay,
//!   and takes prompts/answers back. The network analogue of bob-tui.
//! - `test-client`: a dumb terminal controller to exercise the whole path
//!   before the iOS app exists.

mod client;
mod host;
mod session;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bob-remote", about = "Remote-control host + test client for bob.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the agent host: connect to the relay and drive a bob session.
    Host {
        /// Relay WebSocket URL, e.g. ws://127.0.0.1:8787/ws
        #[arg(long, default_value = "ws://127.0.0.1:8787/ws")]
        relay: String,
        /// Session id shared with the controller.
        #[arg(long)]
        session: String,
        /// Provider spec, e.g. anthropic:claude-sonnet-4-5 (defaults to config).
        #[arg(short = 'p', long)]
        provider: Option<String>,
    },
    /// Run a terminal controller: send stdin lines as prompts, print events.
    TestClient {
        #[arg(long, default_value = "ws://127.0.0.1:8787/ws")]
        relay: String,
        #[arg(long)]
        session: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token = std::env::var("BOB_RELAY_TOKEN").unwrap_or_else(|_| "dev-token".to_string());
    match Cli::parse().command {
        Command::Host {
            relay,
            session,
            provider,
        } => host::run(relay, session, token, provider).await,
        Command::TestClient { relay, session } => client::run(relay, session, token).await,
    }
}
