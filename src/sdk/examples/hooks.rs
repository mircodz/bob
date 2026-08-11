//! Focused example: HOOKS. Intercept tool calls to enforce a guardrail. Unlike an
//! event listener (which only observes), a `PreToolUse` hook runs inline and can
//! DENY a call or rewrite its input; a `PostToolUse` hook can rewrite the output.
//!
//! Run with: `ANTHROPIC_API_KEY=sk-... cargo run -p bob-sdk --example hooks`

use bob_sdk::prelude::*;
use serde_json::Value;
use std::sync::Arc;

/// Block any write/edit that targets a sensitive file, even in AutoAccept mode.
struct ProtectSecrets;

#[async_trait::async_trait]
impl PreToolUse for ProtectSecrets {
    async fn on_pre_tool(&self, tool: &str, input: &Value) -> PreToolDecision {
        let is_write = matches!(tool, "write_file" | "edit_file" | "multi_edit");
        let path = input.get("path").and_then(Value::as_str).unwrap_or("");
        if is_write && (path.contains(".env") || path.contains("secrets")) {
            PreToolDecision::Deny(format!("policy: refusing to modify {path}"))
        } else {
            // Proceed with the (here, unchanged) input.
            PreToolDecision::Proceed(input.clone())
        }
    }
}

/// Log every tool result; pass the output through unchanged.
struct AuditLog;

#[async_trait::async_trait]
impl PostToolUse for AuditLog {
    async fn on_post_tool(&self, tool: &str, output: String, is_error: bool) -> String {
        eprintln!(
            "[audit] {tool} → {} ({} bytes)",
            if is_error { "error" } else { "ok" },
            output.len()
        );
        output
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut agent = Agent::builder()
        .model("anthropic/claude-sonnet-4-5-20250929")
        .credential(ProviderAuth::ApiKey(std::env::var("ANTHROPIC_API_KEY")?))
        .permission_mode(Mode::AutoAccept)
        .on_pre_tool(Arc::new(ProtectSecrets))
        .on_post_tool(Arc::new(AuditLog))
        .build()
        .await?;

    // Even asked directly, the guardrail blocks the write and the model sees a
    // tool-error it must reckon with.
    let reply = agent
        .run("Write the text 'x' to a file named .env in the current directory.")
        .await?;
    println!("{reply}");
    Ok(())
}
