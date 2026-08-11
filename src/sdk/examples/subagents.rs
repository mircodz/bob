//! Focused example: SUBAGENTS. Register a specialized named subagent with its own
//! prompt and tool restriction; the model delegates to it by matching a task to
//! its `description` (or you can name it explicitly in the prompt).
//!
//! Run with: `ANTHROPIC_API_KEY=sk-... cargo run -p bob-sdk --example subagents`

use bob_sdk::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut agent = Agent::builder()
        .model("anthropic/claude-sonnet-4-5-20250929")
        .credential(ProviderAuth::ApiKey(std::env::var("ANTHROPIC_API_KEY")?))
        .cwd(".")
        // A read-only "explorer" the model can hand investigation tasks to. Its
        // fresh context keeps file-reading noise out of the main conversation.
        .agent(
            "explorer",
            AgentDefinition {
                description: "Explores the codebase to answer factual questions about it.".into(),
                prompt: "You investigate a codebase and report concise, cited findings \
                         (file:line). You never modify anything."
                    .into(),
                tools: None,
                read_only: true,
                model: None,
            },
        )
        .build()
        .await?;

    // Naming the agent explicitly guarantees delegation; omitting the name lets
    // the model decide based on the definition's `description`.
    let reply = agent
        .run("Use the explorer agent to find where the SDK builder is defined.")
        .await?;
    println!("{reply}");
    Ok(())
}
