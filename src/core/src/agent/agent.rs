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
    /// This agent's mailbox for inter-agent coordination. None → the agent is not
    /// part of a team (no inbox drain).
    pub inbox: Option<crate::agent::team::AgentInbox>,
    /// The shared team roster, so coordination tools can address other agents.
    pub team: Option<crate::agent::team::AgentRegistry>,
    /// This agent's own name in the team + its spawn depth (root = 0).
    pub name: String,
    pub depth: usize,
}

pub struct Agent {
    pub id: String,
    cfg: AgentConfig,
    /// The working history sent to the provider. May be compacted in place once it
    /// approaches the context window (older turns collapsed into a summary).
    history: Vec<Message>,
    /// The full, never-compacted transcript — every user/assistant/tool message in
    /// order. This is what gets persisted so a resumed session shows the WHOLE
    /// conversation, not the compacted view the model happened to last run on.
    full_history: Vec<Message>,
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
            full_history: Vec::new(),
            files: Arc::new(FileTracker::new()),
            todos: Arc::new(TodoStore::new()),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reasoning: crate::core::types::ReasoningEffort::default(),
        }
    }

    /// The full, never-compacted transcript. This is what the frontend persists so
    /// a resumed session shows the entire conversation. (The compacted working view
    /// is an internal detail of staying under the context window.)
    pub fn messages(&self) -> &[Message] {
        &self.full_history
    }

    pub fn todos(&self) -> Arc<TodoStore> {
        self.todos.clone()
    }

    /// Whether this agent should be woken to process coordination messages. It
    /// wakes only when there is a pending message AND none of its OWN children are
    /// still running — so when it spawned several agents, it wakes ONCE after they
    /// have all reported, sees every result together, and can write a single
    /// synthesized reply rather than one dribbled paragraph per agent. (If a
    /// message is waiting but one of its children is still running, we hold off;
    /// that child's completion will re-trigger the check.) Gating on its OWN
    /// children — not the whole team — means an unrelated slow agent elsewhere
    /// can't stall this agent. The frontend polls this on an idle agent and, if
    /// true, drives a fresh empty-prompt "wake" turn.
    pub fn has_pending_coordination(&mut self) -> bool {
        let own_children_running = self
            .cfg
            .team
            .as_ref()
            .is_some_and(|team| team.has_running_children(&self.cfg.name));
        if own_children_running {
            return false;
        }
        self.cfg
            .inbox
            .as_mut()
            .is_some_and(|inbox| inbox.has_pending())
    }

    /// Whether this agent still has coordination work outstanding: either a message
    /// waiting to be processed, OR one of its own children still running (whose
    /// result will arrive later). A driver loop keeps the agent alive until this is
    /// false. (`has_pending_coordination` is the stricter "ready to wake NOW" gate;
    /// this is the looser "not done yet" gate the remote host polls on.)
    pub fn has_outstanding_coordination(&mut self) -> bool {
        let own_children_running = self
            .cfg
            .team
            .as_ref()
            .is_some_and(|team| team.has_running_children(&self.cfg.name));
        own_children_running
            || self
                .cfg
                .inbox
                .as_mut()
                .is_some_and(|inbox| inbox.has_pending())
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
        self.history = messages.clone();
        self.full_history = messages;
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
            coord: self
                .cfg
                .team
                .as_ref()
                .map(|team| crate::tools::registry::CoordContext {
                    name: self.cfg.name.clone(),
                    depth: self.cfg.depth,
                    team: team.clone(),
                }),
        };

        // A non-empty prompt is a real user turn. An EMPTY prompt is a
        // coordination "wake" — no user message; the inbox drain below provides
        // the content (a team member's result). Skip pushing an empty user turn.
        if !prompt.is_empty() {
            let m = Message::user_text(prompt);
            self.history.push(m.clone());
            self.full_history.push(m);
        }
        self.cfg.bus.emit(AgentEvent::TurnStart {
            agent_id: self.id.clone(),
        });
        // Fresh run: clear any stale cancel signal.
        self.cancel
            .store(false, std::sync::atomic::Ordering::Relaxed);

        let mut total = Usage::default();
        let mut final_text = String::new();
        // Set true when the model finishes on its own (no more tool calls) or we
        // stop deliberately. If the turn loop runs to exhaustion instead, we report
        // that explicitly rather than returning an empty result.
        let mut finished = false;

        for _turn in 0..self.cfg.max_turns {
            // Interrupt before starting a new turn — history ends on a valid
            // boundary here (a tool_result message or the initial user prompt),
            // so we can stop cleanly.
            if self.is_cancelled() {
                final_text = "[interrupted]".to_string();
                break;
            }
            // Coordination seam: fold any messages from other agents into history
            // as a synthetic user turn, so the model sees and can act on them at
            // this turn boundary. No-op when the agent has no inbox (not a team).
            if let Some(inbox) = self.cfg.inbox.as_mut() {
                let messages = inbox.drain();
                for m in messages {
                    let msg = Message::user_text(crate::agent::team::format_coord_message(
                        &m.from, &m.text,
                    ));
                    self.history.push(msg.clone());
                    self.full_history.push(msg);
                    self.cfg.bus.emit(AgentEvent::AgentMessage {
                        to: self.id.clone(),
                        from: m.from.clone(),
                        text: m.text.clone(),
                    });
                }
            }
            // Compact history if approaching the context window. The overhead of
            // the system prompt + every tool schema is counted toward the budget so
            // we compact before the real request (system + tools + history) exceeds
            // the window, not just when the message text alone does.
            let system_overhead_tokens = self
                .cfg
                .system
                .as_deref()
                .map(crate::agent::compaction::estimate_tokens)
                .unwrap_or(0)
                + self
                    .cfg
                    .tools
                    .specs()
                    .iter()
                    .map(|s| {
                        crate::agent::compaction::estimate_tokens(&s.name)
                            + crate::agent::compaction::estimate_tokens(&s.description)
                            + crate::agent::compaction::estimate_tokens(&s.input_schema.to_string())
                    })
                    .sum::<usize>();
            let history = std::mem::take(&mut self.history);
            let compaction = maybe_compact(
                history,
                &self.cfg.provider,
                &CompactionOptions {
                    context_window: self.cfg.context_window,
                    threshold: self.cfg.compact_threshold,
                    keep_recent: self.cfg.keep_recent,
                    system_overhead_tokens,
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
            let mut stream_error = None;
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
                    StreamEvent::Error { message } => {
                        stream_error = Some(message);
                        break;
                    }
                    _ => {}
                }
                // Stop consuming the stream promptly on interrupt. Dropping `rx`
                // ends the provider's send side.
                if self.is_cancelled() {
                    break;
                }
            }
            // A real mid-stream API failure: surface it as an error so the caller
            // can retry/report, rather than poisoning history with a fake turn.
            if let Some(message) = stream_error {
                anyhow::bail!("{}", message);
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
            self.full_history.push(completion.message.clone());
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
                finished = true;
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
                let msg = Message {
                    role: Role::Tool,
                    content: results,
                };
                self.history.push(msg.clone());
                self.full_history.push(msg);
                final_text = "[interrupted]".to_string();
                break;
            }

            // Execute the requested tools. Read-only tools (reads/searches/status)
            // run concurrently for speed; mutating tools (edits, writes, bash,
            // refactors, coordination) run sequentially so two edits to the same
            // file in one turn can't race and corrupt each other. Order within the
            // turn is preserved so results map back to their tool_use ids.
            for (id, name, input) in &tool_uses {
                self.cfg.bus.emit(AgentEvent::ToolCall {
                    agent_id: self.id.clone(),
                    tool_use_id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
            }
            let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
            let mut concurrent = Vec::new();
            for (id, name, input) in &tool_uses {
                if is_read_only(name) {
                    let tools = self.cfg.tools.clone();
                    let bus = self.cfg.bus.clone();
                    let agent_id = self.id.clone();
                    let ctx = ctx.clone();
                    let (id, name, input) = (id.clone(), name.clone(), input.clone());
                    concurrent.push(async move {
                        run_one(&tools, &bus, &agent_id, &ctx, id, name, input).await
                    });
                }
            }
            let mut concurrent_results = futures::future::join_all(concurrent).await.into_iter();
            for (id, name, input) in &tool_uses {
                if is_read_only(name) {
                    if let Some(r) = concurrent_results.next() {
                        results.push(r);
                    }
                } else {
                    results.push(
                        run_one(
                            &self.cfg.tools,
                            &self.cfg.bus,
                            &self.id,
                            &ctx,
                            id.clone(),
                            name.clone(),
                            input.clone(),
                        )
                        .await,
                    );
                }
            }
            let msg = Message {
                role: Role::Tool,
                content: results,
            };
            self.history.push(msg.clone());
            self.full_history.push(msg);
        }

        // If the turn loop ran to exhaustion without the model finishing, return
        // a clear message rather than an empty string — otherwise a subagent that
        // hits the cap looks like it silently returned nothing.
        if !finished && final_text.is_empty() {
            final_text = format!(
                "[stopped after reaching the {}-turn limit without finishing]",
                self.cfg.max_turns
            );
        }

        self.cfg.bus.emit(AgentEvent::TurnEnd {
            agent_id: self.id.clone(),
            usage: total,
        });
        Ok(final_text)
    }
}

/// Whether a tool only observes state (safe to run concurrently with siblings) or
/// mutates the workspace / shared state (must be serialized within a turn). Unknown
/// tools are treated as mutating — the conservative default.
fn is_read_only(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "list_dir"
            | "glob"
            | "grep"
            | "lsp"
            | "job_status"
            | "job_output"
            | "list_agents"
            | "web_fetch"
            | "web_search"
    )
}

/// Execute one tool call, emit its result event, and return the tool_result block.
/// `is_error` comes from the typed Result, not from sniffing the text.
async fn run_one(
    tools: &ToolRegistry,
    bus: &EventBus,
    agent_id: &str,
    ctx: &ToolContext,
    id: String,
    name: String,
    input: serde_json::Value,
) -> ContentBlock {
    let (content, is_error) = match tools.execute(&name, input, ctx).await {
        Ok(output) => (output, false),
        Err(e) => (e.wire(), true),
    };
    bus.emit(AgentEvent::ToolResult {
        agent_id: agent_id.to_string(),
        tool_use_id: id.clone(),
        output: content.clone(),
        is_error,
    });
    ContentBlock::ToolResult {
        tool_use_id: id,
        content,
        is_error: Some(is_error),
    }
}
