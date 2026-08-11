//! Focused example: SESSIONS. Persist the conversation to a store and resume it in
//! a later run. The default store is in-memory (nothing written to disk); pass a
//! `SqliteStore` to persist under `~/.bob`, then `.resume_latest()` to continue.
//!
//! Run with: `ANTHROPIC_API_KEY=sk-... cargo run -p bob-sdk --example sessions`
//! Run it twice: the second run resumes the first conversation.

use bob_sdk::prelude::*;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A persistent store — survives across process runs. (Omit `.store(...)` to
    // keep everything in memory instead.)
    let store = Arc::new(SqliteStore::default());

    let mut agent = Agent::builder()
        .model("anthropic/claude-sonnet-4-5-20250929")
        .credential(ProviderAuth::ApiKey(std::env::var("ANTHROPIC_API_KEY")?))
        .cwd(".")
        .store(store)
        // Pick up the most recent session in this cwd, if one exists. On the first
        // run there's nothing to resume, so it starts fresh.
        .resume_latest()
        .build()
        .await?;

    // On a second run, the agent already has the earlier exchange in context.
    let reply = agent
        .run("If we've talked before, briefly recall what about. Otherwise, say hello.")
        .await?;
    println!("{reply}");
    Ok(())
}
