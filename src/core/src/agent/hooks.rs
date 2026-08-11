//! Tool-call hooks: interception points the SDK exposes so an embedder can gate,
//! rewrite, or audit tool calls without touching the agent loop. This is bob's
//! analog of the Claude Agent SDK's `PreToolUse` / `PostToolUse` hooks.
//!
//! Unlike the [`EventBus`](crate::core::events::EventBus) — which only *observes*
//! — a hook runs INLINE in the tool-dispatch path and can change what happens: a
//! `PreToolUse` hook may deny a call or rewrite its input before the tool runs; a
//! `PostToolUse` hook may rewrite the result the model sees. Hooks are a root-agent
//! concern (an embedder's guardrail); they default to empty (zero overhead).

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// A `PreToolUse` hook's decision about a pending tool call.
pub enum PreToolDecision {
    /// Let the call proceed with this (possibly rewritten) input.
    Proceed(Value),
    /// Block the call. The tool is not run; this message becomes its error result,
    /// so the model sees a real tool-error it can react to.
    Deny(String),
}

/// Runs before a tool executes. Return `Proceed(input)` to allow (optionally with
/// a rewritten input), or `Deny(reason)` to block it.
#[async_trait]
pub trait PreToolUse: Send + Sync {
    async fn on_pre_tool(&self, tool: &str, input: &Value) -> PreToolDecision;
}

/// Runs after a tool executes, with its `(name, output, is_error)`. Return the
/// output to record — unchanged, or rewritten (e.g. redacted).
#[async_trait]
pub trait PostToolUse: Send + Sync {
    async fn on_post_tool(&self, tool: &str, output: String, is_error: bool) -> String;
}

/// The set of hooks an agent applies to every tool call. Empty by default, so an
/// agent with no hooks pays nothing. Cheap to clone (shared `Arc`s).
#[derive(Clone, Default)]
pub struct Hooks {
    pub pre: Vec<Arc<dyn PreToolUse>>,
    pub post: Vec<Arc<dyn PostToolUse>>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }

    /// Run the pre-tool hooks in registration order. The FIRST `Deny` short-circuits
    /// (the tool won't run); otherwise each hook's `Proceed(input)` feeds the next,
    /// so rewrites chain. Returns the final input to run with, or the deny reason.
    pub async fn run_pre(&self, tool: &str, mut input: Value) -> Result<Value, String> {
        for hook in &self.pre {
            match hook.on_pre_tool(tool, &input).await {
                PreToolDecision::Proceed(next) => input = next,
                PreToolDecision::Deny(reason) => return Err(reason),
            }
        }
        Ok(input)
    }

    /// Run the post-tool hooks in registration order, chaining output rewrites.
    pub async fn run_post(&self, tool: &str, mut output: String, is_error: bool) -> String {
        for hook in &self.post {
            output = hook.on_post_tool(tool, output, is_error).await;
        }
        output
    }
}
