//! The bob-sdk "hello world": build an agent in a few lines and run one turn.
//! This doubles as a regression guard on the public builder surface.
//!
//! Run with: `cargo run -p bob-sdk --example minimal`
//! (Requires a configured provider — e.g. `ANTHROPIC_API_KEY` set, or
//! `bob login anthropic`.)

use bob_sdk::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut agent = Agent::builder()
        .model("anthropic/claude-sonnet-4-5-20250929")
        .cwd(".")
        .build()
        .await?;

    let reply = agent
        .run("In one sentence, what is this repository?")
        .await?;
    println!("{reply}");
    Ok(())
}
