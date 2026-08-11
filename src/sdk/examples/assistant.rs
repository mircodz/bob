//! A fuller tour of the bob-sdk surface — everything the `minimal` example leaves
//! out. It builds a configured agent, registers a specialized subagent the model
//! can delegate to, installs tool-call HOOKS (a guardrail + an audit log), STREAMS
//! the first turn (rendering events as they arrive), shows how to interrupt a run,
//! and then takes a SECOND turn on the same agent to demonstrate multi-turn state.
//!
//! Run with: `ANTHROPIC_API_KEY=sk-... cargo run -p bob-sdk --example assistant`

use bob_sdk::{prelude::*, AgentEvent};
use serde_json::Value;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Build a configured agent.
    //
    // Beyond the model + credential, this sets a working directory, an interaction
    // mode (AutoAccept lets file edits through without a prompt), an explicit
    // allow-list so common read tools never prompt, and a persistent SQLite store
    // so the conversation survives across runs. Everything omitted has a sane
    // default (in-memory store, Ask permissions).
    let mut agent = Agent::builder()
        .model("anthropic/claude-sonnet-4-5-20250929")
        .credential(ProviderAuth::ApiKey(std::env::var("ANTHROPIC_API_KEY")?))
        .cwd(".")
        .permission_mode(Mode::AutoAccept)
        .allow_tools(["read_file", "grep", "list_dir"])
        // 2. Register a specialized subagent the model can delegate to by name.
        //    It gets its OWN prompt and is confined to read-only tools, so it can
        //    analyze but never mutate. The model auto-delegates by matching a task
        //    to `description`, or you can ask for it explicitly in the prompt.
        .agent(
            "code-reviewer",
            AgentDefinition {
                description: "Expert code reviewer. Use for correctness and design review.".into(),
                prompt: "You are a meticulous code reviewer. Report each issue as file:line — problem — suggested fix. Be concise.".into(),
                tools: None,
                read_only: true,
                model: None,
            },
        )
        // 3. Install tool-call hooks. Hooks run INLINE in the tool-dispatch path,
        //    so unlike event listeners they can change what happens:
        //    - a PreToolUse hook can DENY a call or rewrite its input,
        //    - a PostToolUse hook can rewrite the recorded output.
        //    Here: a guardrail that blocks writes to sensitive files, and an audit
        //    log that records every call. Both are just trait impls (below).
        .on_pre_tool(Arc::new(ProtectSecrets))
        .on_post_tool(Arc::new(AuditLog))
        .build()
        .await?;

    // 4. Stream the first turn, rendering events live instead of waiting for the
    //    final text. `run_streamed` returns immediately; the run drives on a
    //    background task while we drain its event channel.
    println!("── streaming first turn ──");
    let mut stream = agent
        .run_streamed("Give me a one-paragraph overview of this repository's architecture.")
        .await;

    // A cancel handle: interrupt the run from another task if it runs too long.
    // (Here it's a demonstration; the turn will normally finish well within 60s.)
    let handle = agent_interrupt_after(&agent, std::time::Duration::from_secs(60));

    while let Some(event) = stream.events.recv().await {
        match event {
            AgentEvent::TextDelta { text, .. } => print!("{text}"),
            AgentEvent::ToolCall { name, .. } => println!("\n  [tool: {name}]"),
            AgentEvent::SubagentSpawn { task, .. } => println!("\n  [delegated: {task}]"),
            AgentEvent::TurnEnd { usage, .. } => {
                println!(
                    "\n  [turn end — {} in / {} out tokens]",
                    usage.total_input(),
                    usage.output_tokens
                );
            }
            _ => {}
        }
    }
    handle.abort(); // the run finished; stop the interrupt timer.
    let first = stream.finish().await?;
    println!("\n── final ──\n{first}\n");

    // 5. A second turn on the SAME agent — it retains full context from the first,
    //    so this follow-up can reference "the architecture" without repeating it.
    println!("── second turn ──");
    let second = agent
        .run("Now name the single biggest risk in that architecture, in one sentence.")
        .await?;
    println!("{second}");

    Ok(())
}

/// A `PreToolUse` guardrail: deny any write/edit whose `path` targets a sensitive
/// file, so the model can never touch `.env` or secrets even in AutoAccept mode.
/// Read-only tools and non-file tools pass straight through.
struct ProtectSecrets;

#[async_trait::async_trait]
impl PreToolUse for ProtectSecrets {
    async fn on_pre_tool(&self, tool: &str, input: &Value) -> PreToolDecision {
        let is_write = matches!(tool, "write_file" | "edit_file" | "multi_edit");
        let path = input.get("path").and_then(Value::as_str).unwrap_or("");
        if is_write && (path.contains(".env") || path.contains("secrets")) {
            PreToolDecision::Deny(format!(
                "policy: refusing to modify a sensitive file ({path})"
            ))
        } else {
            PreToolDecision::Proceed(input.clone())
        }
    }
}

/// A `PostToolUse` audit hook: log every tool result to stderr (compliance /
/// debugging) and pass the output through unchanged.
struct AuditLog;

#[async_trait::async_trait]
impl PostToolUse for AuditLog {
    async fn on_post_tool(&self, tool: &str, output: String, is_error: bool) -> String {
        let status = if is_error { "error" } else { "ok" };
        eprintln!("[audit] {tool} → {status} ({} bytes)", output.len());
        output
    }
}

/// Spawn a timer that interrupts the agent after `after`, returning the task
/// handle so the caller can cancel the timer once the run finishes. Shows the
/// `interrupter()` API — cooperative cancellation from another task.
fn agent_interrupt_after(agent: &Agent, after: std::time::Duration) -> tokio::task::JoinHandle<()> {
    let interrupter = agent.interrupter();
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        interrupter.interrupt();
    })
}
