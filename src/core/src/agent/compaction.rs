//! History compaction. Rough token estimate (chars/4); when history exceeds a
//! fraction of the context window, summarize older messages into one synthetic
//! message and keep the recent tail verbatim.

use crate::core::types::{ContentBlock, GenerateOptions, Message, Role};
use crate::providers::provider::Provider;
use std::sync::Arc;

pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

pub fn estimate_message_tokens(m: &Message) -> usize {
    let mut n = 4; // per-message overhead
    for b in &m.content {
        match b {
            ContentBlock::Text { text } => n += estimate_tokens(text),
            ContentBlock::ToolUse { name, input, .. } => {
                n += estimate_tokens(&input.to_string()) + estimate_tokens(name)
            }
            ContentBlock::ToolResult { content, .. } => n += estimate_tokens(content),
            ContentBlock::Thinking { thinking, .. } => n += estimate_tokens(thinking),
            ContentBlock::RedactedThinking { data } => n += estimate_tokens(data),
            ContentBlock::ReasoningItem { .. } => {}
        }
    }
    n
}

pub fn estimate_history_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

pub struct CompactionOptions {
    pub context_window: usize,
    pub threshold: f64,
    pub keep_recent: usize,
    /// Estimated tokens for the fixed request overhead that is NOT in `messages`:
    /// the system prompt plus every tool schema. Counted toward the budget so we
    /// compact before the real request (system + tools + history) overflows.
    pub system_overhead_tokens: usize,
}

pub struct CompactionResult {
    pub messages: Vec<Message>,
    pub compacted: bool,
    pub before_tokens: usize,
    pub after_tokens: usize,
}

const SUMMARY_MARKER: &str = "[conversation summary]";

pub async fn maybe_compact(
    messages: Vec<Message>,
    provider: &Arc<dyn Provider>,
    opts: &CompactionOptions,
) -> CompactionResult {
    let before_tokens = estimate_history_tokens(&messages) + opts.system_overhead_tokens;
    let limit = (opts.context_window as f64 * opts.threshold) as usize;

    if before_tokens <= limit || messages.len() <= opts.keep_recent + 1 {
        return CompactionResult {
            messages,
            compacted: false,
            before_tokens,
            after_tokens: before_tokens,
        };
    }

    let split = messages.len().saturating_sub(opts.keep_recent);
    // Guard: never split a tool_use from its following tool_result.
    let adjusted_split = if split > 0
        && messages
            .get(split)
            .map(|m| m.role == Role::Tool)
            .unwrap_or(false)
    {
        split - 1
    } else {
        split
    };

    let older: Vec<Message> = messages[..adjusted_split].to_vec();
    let recent: Vec<Message> = messages[adjusted_split..].to_vec();

    // If summarizing fails (transient API error), keep the full history verbatim
    // rather than discarding the older half — a dropped request must never destroy
    // conversation context.
    let summary_text = match summarize(&older, provider).await {
        Ok(text) => text,
        Err(_) => {
            return CompactionResult {
                messages,
                compacted: false,
                before_tokens,
                after_tokens: before_tokens,
            };
        }
    };
    let summary_message = Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!("{}\n{}", SUMMARY_MARKER, summary_text),
        }],
    };

    let mut compacted = vec![summary_message];
    compacted.extend(recent);
    let after_tokens = estimate_history_tokens(&compacted) + opts.system_overhead_tokens;
    CompactionResult {
        messages: compacted,
        compacted: true,
        before_tokens,
        after_tokens,
    }
}

async fn summarize(messages: &[Message], provider: &Arc<dyn Provider>) -> anyhow::Result<String> {
    let transcript = messages
        .iter()
        .map(render_for_summary)
        .collect::<Vec<_>>()
        .join("\n\n");
    let instruction = "Summarize the following conversation transcript so an AI coding agent can \
        continue the work without the original. Preserve: the user's goals and constraints, \
        decisions made, files created or modified (with paths), key facts discovered, and any \
        unfinished tasks or open questions. Be concise but complete. Output only the summary.";

    let opts = GenerateOptions {
        messages: vec![Message::user_text(format!(
            "{}\n\n---\n{}",
            instruction, transcript
        ))],
        max_tokens: Some(1024),
        ..Default::default()
    };

    let completion = provider.generate(opts).await?;
    Ok(completion.message.text().trim().to_string())
}

fn render_for_summary(m: &Message) -> String {
    let parts: Vec<String> = m
        .content
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::ToolUse { name, input, .. } => {
                format!("[tool_use {} {}]", name, input)
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => format!(
                "[tool_result{}]\n{}",
                if is_error.unwrap_or(false) {
                    " error"
                } else {
                    ""
                },
                content
            ),
            ContentBlock::Thinking { thinking, .. } => format!("[thinking]\n{}", thinking),
            ContentBlock::RedactedThinking { .. } => "[redacted_thinking]".to_string(),
            ContentBlock::ReasoningItem { .. } => String::new(),
        })
        .collect();
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    format!("{}: {}", role, parts.join("\n"))
}
