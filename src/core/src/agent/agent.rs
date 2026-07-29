//! The core agent loop. Runs a provider ↔ tool conversation to completion and
//! returns the final text. Communicates progress ONLY through the event bus.

use crate::agent::compaction::{maybe_compact, CompactionOptions};
use crate::core::events::{AgentEvent, EventBus};
use crate::core::types::{ContentBlock, GenerateOptions, Message, Role, StreamEvent, Usage};
use crate::providers::provider::Provider;
use crate::tools::file_tracker::FileTracker;
use crate::tools::registry::{ToolContext, ToolRegistry};
use crate::tools::todo::TodoStore;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static AGENT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_agent_id() -> String {
    format!(
        "agent_{}",
        AGENT_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
    )
}

pub struct AgentConfig {
    pub provider: Arc<dyn Provider>,
    pub tools: ToolRegistry,
    pub bus: EventBus,
    pub system: Option<String>,
    pub cwd: String,
    pub max_turns: u32,
    pub id: Option<String>,
    pub context_window: usize,
    pub compact_threshold: f64,
    pub keep_recent: usize,
    /// Shared background-job registry (so the `task`/job tools and the UI see
    /// the same jobs). Defaults to a fresh empty registry.
    pub jobs: crate::tools::jobs::JobRegistry,
    /// UI hook for ask_user / exit_plan. None → those tools return a note.
    pub user_asker: Option<Arc<dyn crate::tools::registry::UserAsker>>,
    /// Language servers for this project. None if none configured. Shared with
    /// every agent (root + subagents) so all see the same server processes.
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            provider: panic_provider(),
            tools: ToolRegistry::new(None),
            bus: EventBus::new(),
            system: None,
            cwd: ".".to_string(),
            max_turns: 20,
            id: None,
            context_window: 200_000,
            compact_threshold: 0.8,
            keep_recent: 6,
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
        }
    }
}

fn panic_provider() -> Arc<dyn Provider> {
    // AgentConfig::default is only used field-by-field in practice; never call
    // this. Present so `..Default::default()` compiles if ever needed.
    unreachable!("AgentConfig requires a provider")
}

pub struct Agent {
    pub id: String,
    cfg: AgentConfig,
    history: Vec<Message>,
    files: Arc<FileTracker>,
    todos: Arc<TodoStore>,
    /// Cooperative cancel flag. When set, the run loop finishes the current
    /// step, ensures the history is left in a valid state, and returns early.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Reasoning intensity requested for each generation (runtime-settable).
    reasoning: crate::core::types::ReasoningEffort,
}

impl Agent {
    pub fn new(cfg: AgentConfig) -> Self {
        let id = cfg.id.clone().unwrap_or_else(next_agent_id);
        Agent {
            id,
            cfg,
            history: Vec::new(),
            files: Arc::new(FileTracker::new()),
            todos: Arc::new(TodoStore::new()),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reasoning: crate::core::types::ReasoningEffort::default(),
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.history
    }

    pub fn todos(&self) -> Arc<TodoStore> {
        self.todos.clone()
    }

    /// A handle to this agent's cancel flag. Set it to `true` (from the UI on
    /// another task) to cooperatively interrupt an in-flight `run`.
    pub fn cancel_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.cancel.clone()
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn load_history(&mut self, messages: Vec<Message>) {
        self.history = messages;
    }

    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.cfg.provider = provider;
    }

    /// Set the reasoning intensity used for subsequent generations.
    pub fn set_reasoning(&mut self, effort: crate::core::types::ReasoningEffort) {
        self.reasoning = effort;
    }

    /// The current reasoning intensity.
    pub fn reasoning(&self) -> crate::core::types::ReasoningEffort {
        self.reasoning
    }

    /// The current provider (for listing models, showing the active model, etc.).
    pub fn provider(&self) -> Arc<dyn Provider> {
        self.cfg.provider.clone()
    }

    /// Run the agent on a single user prompt, looping over tool calls.
    pub async fn run(&mut self, prompt: &str) -> anyhow::Result<String> {
        let ctx = ToolContext {
            cwd: self.cfg.cwd.clone(),
            files: self.files.clone(),
            todos: self.todos.clone(),
            jobs: self.cfg.jobs.clone(),
            user_asker: self.cfg.user_asker.clone(),
            lsp: self.cfg.lsp.clone(),
        };

        self.history.push(Message::user_text(prompt));
        self.cfg.bus.emit(AgentEvent::TurnStart {
            agent_id: self.id.clone(),
        });
        // Fresh run: clear any stale cancel signal.
        self.cancel
            .store(false, std::sync::atomic::Ordering::Relaxed);

        let mut total = Usage::default();
        let mut final_text = String::new();

        for _turn in 0..self.cfg.max_turns {
            // Interrupt before starting a new turn — history ends on a valid
            // boundary here (a tool_result message or the initial user prompt),
            // so we can stop cleanly.
            if self.is_cancelled() {
                final_text = "[interrupted]".to_string();
                break;
            }
            // Compact history if approaching the context window.
            let history = std::mem::take(&mut self.history);
            let compaction = maybe_compact(
                history,
                &self.cfg.provider,
                &CompactionOptions {
                    context_window: self.cfg.context_window,
                    threshold: self.cfg.compact_threshold,
                    keep_recent: self.cfg.keep_recent,
                },
            )
            .await;
            self.history = compaction.messages;
            if compaction.compacted {
                self.cfg.bus.emit(AgentEvent::Compaction {
                    agent_id: self.id.clone(),
                    before_tokens: compaction.before_tokens,
                    after_tokens: compaction.after_tokens,
                });
            }

            // Stream the assistant turn, forwarding text deltas to the bus.
            let opts = GenerateOptions {
                system: self.cfg.system.clone(),
                messages: self.history.clone(),
                tools: self.cfg.tools.specs(),
                cache: true,
                reasoning: self.reasoning,
                ..Default::default()
            };
            let mut rx = self.cfg.provider.stream(opts).await?;

            let mut completion = None;
            while let Some(evt) = rx.recv().await {
                match evt {
                    StreamEvent::TextDelta { text } => {
                        self.cfg.bus.emit(AgentEvent::TextDelta {
                            agent_id: self.id.clone(),
                            text,
                        });
                    }
                    StreamEvent::MessageStop { completion: c } => {
                        completion = Some(c);
                    }
                    _ => {}
                }
                // Stop consuming the stream promptly on interrupt. Dropping `rx`
                // ends the provider's send side.
                if self.is_cancelled() {
                    break;
                }
            }
            // Interrupted mid-stream before the model finished: stop cleanly. The
            // history still ends on a valid boundary (no assistant message pushed
            // yet), so the next request is well-formed.
            if self.is_cancelled() && completion.is_none() {
                final_text = "[interrupted]".to_string();
                break;
            }
            let completion =
                completion.ok_or_else(|| anyhow::anyhow!("stream ended without completion"))?;

            total.input_tokens += completion.usage.input_tokens;
            total.output_tokens += completion.usage.output_tokens;
            total.cache_creation_input_tokens += completion.usage.cache_creation_input_tokens;
            total.cache_read_input_tokens += completion.usage.cache_read_input_tokens;

            // Per-completion usage event (precise, one per model call).
            self.cfg.bus.emit(AgentEvent::Completion {
                agent_id: self.id.clone(),
                model: self.cfg.provider.model().to_string(),
                usage: completion.usage,
            });

            self.history.push(completion.message.clone());
            self.cfg.bus.emit(AgentEvent::Message {
                agent_id: self.id.clone(),
                message: completion.message.clone(),
            });

            // Collect tool_use blocks.
            let tool_uses: Vec<(String, String, serde_json::Value)> = completion
                .message
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            if tool_uses.is_empty() {
                final_text = completion.message.text();
                break;
            }

            // Interrupt requested while the model was streaming: don't run the
            // tools, but we MUST still answer every tool_use with a result or the
            // history becomes invalid for the next request. Feed back a cancel
            // notice for each, then stop.
            if self.is_cancelled() {
                let results = tool_uses
                    .iter()
                    .map(|(id, _, _)| ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: "interrupted by user".to_string(),
                        is_error: Some(true),
                    })
                    .collect();
                self.history.push(Message {
                    role: Role::Tool,
                    content: results,
                });
                final_text = "[interrupted]".to_string();
                break;
            }

            // Execute all requested tools concurrently and feed results back.
            let mut futs = Vec::new();
            for (id, name, input) in tool_uses {
                self.cfg.bus.emit(AgentEvent::ToolCall {
                    agent_id: self.id.clone(),
                    tool_use_id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                let tools = self.cfg.tools.clone();
                let bus = self.cfg.bus.clone();
                let agent_id = self.id.clone();
                let ctx = ctx.clone();
                futs.push(async move {
                    let output = tools.execute(&name, input, &ctx).await;
                    let is_error = output.starts_with("error:");
                    bus.emit(AgentEvent::ToolResult {
                        agent_id,
                        tool_use_id: id.clone(),
                        output: output.clone(),
                        is_error,
                    });
                    ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: output,
                        is_error: Some(is_error),
                    }
                });
            }
            let results = futures::future::join_all(futs).await;
            self.history.push(Message {
                role: Role::Tool,
                content: results,
            });
        }

        self.cfg.bus.emit(AgentEvent::TurnEnd {
            agent_id: self.id.clone(),
            usage: total,
        });
        Ok(final_text)
    }
}
