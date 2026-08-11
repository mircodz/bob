//! Focused example: PERMISSIONS. Control what runs without a prompt via the
//! interaction mode and allow/deny tool lists. Without an interactive asker,
//! `Ask`/`Deny` decisions decline — so this is a safe, headless configuration.
//!
//! Run with: `ANTHROPIC_API_KEY=sk-... cargo run -p bob-sdk --example permissions`

use bob_sdk::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut agent = Agent::builder()
        .model("anthropic/claude-sonnet-4-5-20250929")
        .credential(ProviderAuth::ApiKey(std::env::var("ANTHROPIC_API_KEY")?))
        .cwd(".")
        // Plan mode: the agent researches read-only and cannot mutate anything —
        // ideal for "analyze, don't touch" tasks.
        .permission_mode(Mode::Plan)
        // Auto-allow the read tools so they never prompt...
        .allow_tools(["read_file", "grep", "list_dir"])
        // ...and force `bash` to always require approval (declined here, since no
        // asker is set) even if some rule would otherwise allow it.
        .deny_tools(["bash"])
        .build()
        .await?;

    let reply = agent
        .run("Summarize what this project does, using only reads. Do not run any commands.")
        .await?;
    println!("{reply}");
    Ok(())
}
