//! Focused example: STREAMING. Drain the event channel to render an agent turn
//! live — reasoning text as it arrives, plus tool calls and the turn summary —
//! instead of blocking for the final string.
//!
//! Run with: `ANTHROPIC_API_KEY=sk-... cargo run -p bob-sdk --example streaming`

use bob_sdk::{prelude::*, AgentEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut agent = Agent::builder()
        .model("anthropic/claude-sonnet-4-5-20250929")
        .credential(ProviderAuth::ApiKey(std::env::var("ANTHROPIC_API_KEY")?))
        .build()
        .await?;

    // `run_streamed` returns immediately; the run drives on a background task.
    let mut stream = agent
        .run_streamed("Count from 1 to 5 with a word each.")
        .await;

    while let Some(event) = stream.events.recv().await {
        match event {
            AgentEvent::TextDelta { text, .. } => print!("{text}"),
            AgentEvent::ToolCall { name, .. } => println!("\n[tool: {name}]"),
            _ => {}
        }
    }

    // The final text is also available once the stream closes.
    let final_text = stream.finish().await?;
    println!("\n--- final ---\n{final_text}");
    Ok(())
}
