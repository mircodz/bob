//! Focused example: CUSTOM TOOLS. Implement the `Tool` trait and register it with
//! `.tool(...)`. The model sees it alongside the builtins and calls it by name —
//! this is how you give an agent domain-specific capabilities (query a database,
//! hit an internal API, control hardware, …).
//!
//! Run with: `ANTHROPIC_API_KEY=sk-... cargo run -p bob-sdk --example custom_tool`

use bob_sdk::prelude::*;
use serde_json::{json, Value};
use std::sync::Arc;

/// A tiny custom tool the model can call. Real tools would do I/O here.
struct WordCount;

#[async_trait::async_trait]
impl Tool for WordCount {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "word_count".to_string(),
            description: "Count the words in a piece of text.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let text = input.get("text").and_then(Value::as_str).unwrap_or("");
        Ok(text.split_whitespace().count().to_string())
    }

    // Read-only tools can run concurrently and are safe to auto-allow.
    fn is_read_only(&self) -> bool {
        true
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut agent = Agent::builder()
        .model("anthropic/claude-sonnet-4-5-20250929")
        .credential(ProviderAuth::ApiKey(std::env::var("ANTHROPIC_API_KEY")?))
        .permission_default(Decision::Allow)
        .tool(Arc::new(WordCount))
        .build()
        .await?;

    // The model should reach for `word_count` rather than counting by hand.
    let reply = agent
        .run("Use the word_count tool to count the words in: 'the quick brown fox jumps'.")
        .await?;
    println!("{reply}");
    Ok(())
}
