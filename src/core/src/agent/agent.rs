//! The core agent loop. Runs a provider ↔ tool conversation to completion and
//! returns the final text. Communicates progress ONLY through the event bus.

use crate::agent::compaction::{maybe_compact, CompactionOptions};
use crate::core::events::{AgentEvent, EventBus};
use crate::core::types::{
    Completion, ContentBlock, GenerateOptions, Message, Role, StopReason, StreamEvent, Usage,
};
use crate::providers::provider::Provider;
use crate::tools::file_tracker::FileTracker;
use crate::tools::registry::{ToolContext, ToolRegistry};
use crate::tools::todo::TodoStore;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static AGENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The canonical id/name of the top-level agent. Subagents report back to it and
/// the UI keys the main transcript on it, so it must not be a bare string literal
/// scattered across files.
pub const ROOT_AGENT_ID: &str = "root";

/// Tuning knobs shared by EVERY agent (root + subagents), centralized so the
/// frontends and the subagent-spawning tools can't silently disagree.
///
/// The context window is NOT here — it's model-specific and derived per provider
/// via [`crate::providers::provider::Provider::context_window`]. A provider that
/// can't tell falls back to [`crate::providers::provider::context_window_for`],
/// which is the single source of model→window knowledge.
pub const COMPACT_THRESHOLD: f64 = 0.8;
pub const KEEP_RECENT: usize = 6;
/// Turn budgets: the root gets a large budget; coordinated/`task` subagents a
/// moderate one; the read-only `explore` agent a smaller one (it only searches).
pub const DEFAULT_MAX_TURNS: u32 = 200;
pub const SUBAGENT_MAX_TURNS: u32 = 100;
pub const EXPLORE_MAX_TURNS: u32 = 60;

/// What a spawning tool must supply to build a child agent. The shared tuning
/// constants (context window, compaction, etc.) are filled in by [`build_subagent`]
/// so the three spawn sites (task, explore, spawn_agent) can't drift.
pub struct SubagentSpec {
    pub provider: Arc<dyn Provider>,
    pub tools: ToolRegistry,
    pub bus: EventBus,
    pub system: Option<String>,
    pub cwd: String,
    pub jobs: crate::tools::jobs::JobRegistry,
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    /// The child's id + team name (equal for coordinated agents).
    pub name: String,
    pub max_turns: u32,
    pub depth: usize,
    /// Team membership: an inbox + roster for coordinated agents; `None` for the
    /// fire-and-forget `task`/`explore` children.
    pub inbox: Option<crate::agent::team::AgentInbox>,
    pub team: Option<crate::agent::team::AgentRegistry>,
    /// The spawner's cancel flag, so a Cancel on the root cascades into this child.
    /// `None` only for the root itself.
    pub parent_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Build a child agent from a [`SubagentSpec`], filling the shared tuning
/// constants. The one place subagents are constructed, so their configuration
/// stays consistent.
pub fn build_subagent(spec: SubagentSpec) -> Agent {
    let context_window = spec.provider.context_window();
    Agent::new(AgentConfig {
        provider: spec.provider,
        tools: spec.tools,
        bus: spec.bus,
        system: spec.system,
        cwd: spec.cwd,
        max_turns: spec.max_turns,
        id: Some(spec.name.clone()),
        context_window,
        compact_threshold: COMPACT_THRESHOLD,
        keep_recent: KEEP_RECENT,
        jobs: spec.jobs,
        user_asker: None,
        lsp: spec.lsp,
        inbox: spec.inbox,
        team: spec.team,
        name: spec.name,
        depth: spec.depth,
        parent_cancel: spec.parent_cancel,
        cancel: None,
        // Hooks are a root-agent guardrail; subagents don't inherit them (yet).
        hooks: crate::agent::hooks::Hooks::default(),
    })
}

/// Whether a turn should stop: the agent's OWN cancel flag is set, OR any ancestor
/// flag it observes is set. Split out as a pure function so the cascade semantics
/// (self OR parent) can be unit-tested without standing up a full provider.
fn cancel_requested(
    own: &std::sync::atomic::AtomicBool,
    parent: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> bool {
    own.load(std::sync::atomic::Ordering::Relaxed)
        || parent.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
}

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
    /// An ancestor's cancel flag, observed (read-only) in addition to this agent's
    /// own. Set for spawned children so a Cancel on the root cascades to the whole
    /// tree — each agent still owns its own flag (cleared at its own turn start),
    /// but `is_cancelled` also honors any ancestor's. `None` for the root.
    pub parent_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The agent's OWN cancel flag. Normally `None` (a fresh flag is created), but
    /// the root injects one so it can hand the same flag to its subagent-spawning
    /// tools as their `parent_cancel` — one Cancel then reaches root and every child.
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Tool-call hooks (SDK `.on_pre_tool` / `.on_post_tool`). Empty by default;
    /// applied in `run_one` to gate/rewrite every tool call.
    pub hooks: crate::agent::hooks::Hooks,
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
    /// Highest context-usage warning threshold already emitted this run, so a graded
    /// warning fires once per level as usage climbs (and re-arms after a compaction
    /// drops usage back down).
    warned_pct: u8,
}

impl Agent {
    pub fn new(cfg: AgentConfig) -> Self {
        let id = cfg.id.clone().unwrap_or_else(next_agent_id);
        let cancel = cfg
            .cancel
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        Agent {
            id,
            cfg,
            history: Vec::new(),
            full_history: Vec::new(),
            files: Arc::new(FileTracker::new()),
            todos: Arc::new(TodoStore::new()),
            cancel,
            reasoning: crate::core::types::ReasoningEffort::default(),
            warned_pct: 0,
        }
    }

    /// The full, never-compacted transcript. This is what the frontend persists so
    /// a resumed session shows the entire conversation. (The compacted working view
    /// is an internal detail of staying under the context window.)
    pub fn messages(&self) -> &[Message] {
        &self.full_history
    }

    /// The specs of the tools this agent exposes to the model — its names,
    /// descriptions, and input schemas. Lets a caller introspect the toolset (e.g.
    /// verify a registered subagent definition is advertised).
    pub fn tool_specs(&self) -> Vec<crate::core::types::ToolSpec> {
        self.cfg.tools.specs()
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

    /// A clone of this agent's event bus, so a caller can attach a listener (the
    /// SDK's streaming path forwards these to an async channel). The bus is a
    /// cheap shared handle — cloning shares the same listener set.
    pub fn bus(&self) -> EventBus {
        self.cfg.bus.clone()
    }

    fn is_cancelled(&self) -> bool {
        cancel_requested(&self.cancel, self.cfg.parent_cancel.as_ref())
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
            // Injected by ToolRegistry::execute from the registry's own engine.
            permissions: None,
        };

        // A non-empty prompt is a real user turn. An EMPTY prompt is a
        // coordination "wake" — no user message; the inbox drain below provides
        // the content (a team member's result). Skip pushing an empty user turn.
        if !prompt.is_empty() {
            let m = Message::user_text(prompt);
            self.history.push(m.clone());
            self.full_history.push(m);
            // Emit the user turn so it's part of the event stream (and the replay
            // log), not just a UI-side push. The frontend renders the user line
            // from this event.
            self.cfg.bus.emit(AgentEvent::UserPrompt {
                agent_id: self.id.clone(),
                text: prompt.to_string(),
            });
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

        // Output-cap recovery: normally we send NO max_tokens (the server enforces
        // the model's true maximum). But if a response still comes back truncated
        // (`StopReason::MaxTokens`) — e.g. a large `structured_output` — we retry the
        // SAME turn with an explicit, raised cap so the model can finish instead of
        // emitting a truncated tool call the loop would retry forever. `None` = let
        // the server decide; `Some(n)` = a recovery override for the next attempt.
        let mut max_tokens_override: Option<u32> = None;
        let mut truncation_recoveries: u32 = 0;

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
            // Graded pre-compaction warning: as estimated usage climbs past 70 / 85
            // / 95% of the window, emit ONE warning per level so the user sees
            // context filling up before an auto-compaction silently rewrites it.
            let used = crate::agent::compaction::estimate_history_tokens(&history)
                + system_overhead_tokens;
            self.warn_context(used);
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
                    summary: compaction.summary.clone(),
                    replaced_upto: compaction.replaced_upto,
                });
                // Usage just dropped; re-arm the graded warning to the new level so
                // it can warn again as history rebuilds toward the window.
                self.rearm_context_warning(compaction.after_tokens);
            }

            // Stream the assistant turn (text deltas forwarded to the bus), with a
            // small internal retry loop for transient/overflow errors. Extracted so
            // the turn loop reads at one level of abstraction.
            let completion = match self.stream_completion(max_tokens_override).await? {
                StreamOutcome::Completed(c) => c,
                StreamOutcome::Interrupted => {
                    final_text = "[interrupted]".to_string();
                    break;
                }
            };

            // Output-truncation recovery: if the response was cut off (a partial
            // tool call the loop would otherwise retry forever), retry the SAME turn
            // with a raised cap. Decided BEFORE pushing to history, so a discarded
            // partial turn never pollutes the transcript. Bounded so a genuinely
            // oversized request fails cleanly instead of looping.
            if let Some(bumped) =
                self.truncation_retry_cap(&completion, truncation_recoveries, max_tokens_override)
            {
                truncation_recoveries += 1;
                max_tokens_override = Some(bumped);
                // Silent, self-healing retry — nothing user-visible.
                continue;
            }
            // Cleared on any non-truncated (or unrecoverable) completion so the next
            // turn starts back at the server default rather than a stale override.
            max_tokens_override = None;

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

            // Execute the requested tools (concurrent reads, sequential mutations),
            // then record the results as one Tool-role message.
            let results = self.execute_tool_uses(&ctx, &tool_uses).await;
            let msg = Message {
                role: Role::Tool,
                content: results,
            };
            self.history.push(msg.clone());
            self.full_history.push(msg);
        }

        // If the turn loop ran to exhaustion without the model finishing, give it
        // ONE final tool-less turn to wind down — summarize what it accomplished and
        // what's left — instead of returning a bare "[stopped]" stub. This is what a
        // parent agent (or the user) actually needs as a handoff.
        if !finished && final_text.is_empty() && !self.is_cancelled() {
            final_text = self.wind_down().await;
        }
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

    /// Stream one assistant turn to completion, forwarding text deltas to the bus.
    /// Owns the internal retry state machine: a context-length rejection triggers a
    /// reactive compaction + retry (once), a transient error backs off and retries
    /// up to the attempt cap, and a fatal/exhausted error propagates. Returns
    /// `Interrupted` if the run was cancelled mid-stream before a completion.
    async fn stream_completion(
        &mut self,
        max_tokens_override: Option<u32>,
    ) -> anyhow::Result<StreamOutcome> {
        // Retry a transient stream failure (dropped connection, 429/5xx, overload)
        // up to 10 times with capped exponential backoff before giving up — matches
        // Claude Code's persistence so a flaky network doesn't kill a turn.
        const MAX_STREAM_ATTEMPTS: u32 = 10;
        let mut attempt = 0u32;
        let mut recovered_overflow = false;
        loop {
            attempt += 1;
            let opts = GenerateOptions {
                system: self.cfg.system.clone(),
                messages: self.history.clone(),
                tools: self.cfg.tools.specs(),
                cache: true,
                reasoning: self.reasoning,
                // Normally None (server enforces the true max); set only while
                // recovering from a truncated response.
                max_tokens: max_tokens_override,
                ..Default::default()
            };
            let mut rx = match self.cfg.provider.stream(opts).await {
                Ok(rx) => rx,
                Err(e) => {
                    // Failure before the stream even opened (network/auth/length).
                    match classify_error(&e.to_string()) {
                        ProviderErrorKind::Overflow if !recovered_overflow => {
                            recovered_overflow = true;
                            self.compact_now().await;
                            continue;
                        }
                        ProviderErrorKind::Transient if attempt < MAX_STREAM_ATTEMPTS => {
                            self.backoff(attempt).await;
                            continue;
                        }
                        _ => return Err(e),
                    }
                }
            };

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
                    StreamEvent::MessageStop { completion: done } => {
                        completion = Some(done);
                    }
                    StreamEvent::Error { message } => {
                        stream_error = Some(message);
                        break;
                    }
                    _ => {}
                }
                if self.is_cancelled() {
                    break;
                }
            }

            // Interrupted mid-stream before the model finished: stop cleanly.
            if self.is_cancelled() && completion.is_none() {
                return Ok(StreamOutcome::Interrupted);
            }

            if let Some(message) = stream_error {
                match classify_error(&message) {
                    ProviderErrorKind::Overflow if !recovered_overflow => {
                        recovered_overflow = true;
                        self.compact_now().await;
                        continue;
                    }
                    ProviderErrorKind::Transient if attempt < MAX_STREAM_ATTEMPTS => {
                        self.backoff(attempt).await;
                        continue;
                    }
                    // Out of retries, or a genuinely fatal error.
                    _ => return Err(anyhow::anyhow!("{}", message)),
                }
            }

            return Ok(match completion {
                Some(c) => StreamOutcome::Completed(c),
                None => return Err(anyhow::anyhow!("stream ended without completion")),
            });
        }
    }

    /// Execute a turn's tool calls and return their result blocks in request order.
    /// Read-only tools (reads/searches/status) run concurrently for speed; mutating
    /// tools (edits, writes, bash, coordination) run sequentially so two edits to
    /// the same file in one turn can't race. Emits a `ToolCall` event per call
    /// first, so the UI shows every invocation before results stream back.
    async fn execute_tool_uses(
        &self,
        ctx: &ToolContext,
        tool_uses: &[(String, String, serde_json::Value)],
    ) -> Vec<ContentBlock> {
        for (id, name, input) in tool_uses {
            self.cfg.bus.emit(AgentEvent::ToolCall {
                agent_id: self.id.clone(),
                tool_use_id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
        }
        // Kick off the read-only calls concurrently, preserving their order.
        let mut concurrent = Vec::new();
        for (id, name, input) in tool_uses {
            if self.cfg.tools.is_read_only(name) {
                let tools = self.cfg.tools.clone();
                let bus = self.cfg.bus.clone();
                let hooks = self.cfg.hooks.clone();
                let agent_id = self.id.clone();
                let ctx = ctx.clone();
                let (id, name, input) = (id.clone(), name.clone(), input.clone());
                concurrent.push(async move {
                    run_one(&tools, &bus, &hooks, &agent_id, &ctx, id, name, input).await
                });
            }
        }
        let mut concurrent_results = futures::future::join_all(concurrent).await.into_iter();
        // Weave concurrent read results back into request order; run mutating tools
        // inline where they sit.
        let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in tool_uses {
            if self.cfg.tools.is_read_only(name) {
                if let Some(r) = concurrent_results.next() {
                    results.push(r);
                }
            } else {
                results.push(
                    run_one(
                        &self.cfg.tools,
                        &self.cfg.bus,
                        &self.cfg.hooks,
                        &self.id,
                        ctx,
                        id.clone(),
                        name.clone(),
                        input.clone(),
                    )
                    .await,
                );
            }
        }
        results
    }

    /// Decide whether a completion looks truncated and, if so, the raised
    /// `max_tokens` cap to retry the SAME turn with. Returns `None` to proceed
    /// (not truncated, out of recovery budget, or already at the model's ceiling).
    /// Detects truncation two ways because a stream that drops mid-tool-call may
    /// never deliver `finish_reason`: an explicit `MaxTokens` stop, OR a tool call
    /// whose arguments are the `parse_tool_input` error sentinel (truncated JSON).
    fn truncation_retry_cap(
        &self,
        completion: &Completion,
        recoveries: u32,
        current_override: Option<u32>,
    ) -> Option<u32> {
        const MAX_TRUNCATION_RECOVERIES: u32 = 2;
        if recoveries >= MAX_TRUNCATION_RECOVERIES {
            return None;
        }
        let truncated_tool_args = completion.message.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolUse { input, .. }
                if crate::providers::codec::tool_input_parse_error(input).is_some())
        });
        if completion.stop_reason != StopReason::MaxTokens && !truncated_tool_args {
            return None;
        }
        // Base the next cap on what the model actually produced, doubled, and
        // clamped to the model's real output ceiling.
        let ceiling = crate::providers::provider::max_output_tokens_for(self.cfg.provider.model());
        let produced = completion.usage.output_tokens as u32;
        let bumped = produced.saturating_mul(2).max(8_192).min(ceiling);
        // Retry only if the bumped cap actually grows the budget over what we last
        // asked for; otherwise proceed (the partial + parse-error message at least
        // gives the model something actionable).
        (Some(bumped) != current_override && bumped > produced).then_some(bumped)
    }

    /// Run the agent on `prompt` but force its FINAL answer to be a single JSON
    /// object matching `schema`, returned as a validated `serde_json::Value`. This
    /// is the seam workflow orchestration uses: code gets data it can branch on
    /// instead of prose it would have to parse.
    ///
    /// Mechanism: a one-off `structured_output` tool (whose advertised schema IS
    /// `schema`) is added to the agent's registry for the duration of the call; the
    /// prompt is suffixed with an instruction to finish by calling it; the normal
    /// `run()` loop drives everything. The tool captures the validated payload into
    /// a shared sink we read afterward. If the model finishes without calling it,
    /// we nudge once and re-run; still nothing → `Err`. The registry is restored on
    /// exit so the agent is unchanged for any later `run`.
    pub async fn run_structured(
        &mut self,
        prompt: &str,
        schema: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        use crate::tools::structured::StructuredOutputTool;

        let sink: crate::tools::structured::OutputSink = Arc::new(std::sync::Mutex::new(None));

        // Snapshot the registry so we can restore it — run() reads self.cfg.tools
        // directly, and adding the structured tool must not leak into later turns.
        let original_tools = self.cfg.tools.clone();
        self.cfg.tools.add(Arc::new(StructuredOutputTool::new(
            schema.clone(),
            sink.clone(),
        )));

        let instruction = "\n\nWhen you have everything you need, finish by calling the \
            `structured_output` tool exactly once with your final answer as its arguments. \
            That tool call IS your deliverable — do not also write a prose summary.";

        // First attempt: the real prompt + the structured-output instruction.
        // Capture the run Results WITHOUT `?` so a provider/stream error can't
        // early-return past the registry restore below — leaking the one-off
        // structured_output tool (and its finish-instruction schema) into every
        // later normal turn on this agent.
        let first = self.run(&format!("{prompt}{instruction}")).await;
        let mut captured = sink.lock().unwrap().take();

        // One corrective retry if the model didn't call the tool (empty-prompt wake
        // turn — no new user message, just the nudge folded into history).
        let mut second = Ok(String::new());
        if first.is_ok() && captured.is_none() && !self.is_cancelled() {
            let nudge = "You did not record a result. Call the `structured_output` tool now \
                with your final answer matching its schema.";
            second = self.run(nudge).await;
            captured = sink.lock().unwrap().take();
        }

        // Restore the registry regardless of outcome (success, run error, or a
        // missing result), so the agent is unchanged for any later run.
        self.cfg.tools = original_tools;

        // Surface a run error only after restoring — the restore must always happen.
        first?;
        second?;

        captured.ok_or_else(|| {
            anyhow::anyhow!("agent did not produce a structured result matching the schema")
        })
    }

    /// Emit a graded context-usage warning if `used` tokens crosses the next
    /// threshold (70 / 85 / 95% of the window) not yet warned this run. Fires at
    /// most once per level as usage climbs.
    fn warn_context(&mut self, used: usize) {
        let window = self.cfg.context_window.max(1);
        let pct = ((used * 100) / window) as u8;
        for level in [95u8, 85, 70] {
            if pct >= level && self.warned_pct < level {
                self.warned_pct = level;
                self.cfg.bus.emit(AgentEvent::ContextWarning {
                    agent_id: self.id.clone(),
                    used_tokens: used,
                    context_window: window,
                    pct: level,
                });
                break;
            }
        }
    }

    /// Reset the graded warning arm to the level `used` now sits in, so warnings
    /// can fire again as history climbs back toward the window after a compaction.
    fn rearm_context_warning(&mut self, used: usize) {
        let window = self.cfg.context_window.max(1);
        let pct = ((used * 100) / window) as u8;
        self.warned_pct = [95u8, 85, 70]
            .into_iter()
            .find(|&level| pct >= level)
            .unwrap_or(0);
    }

    /// Force a compaction of the conversation now (the `/compact` command). Public
    /// so the frontend can summarize on demand rather than waiting for the
    /// threshold. Returns `(did_compact, before_tokens, after_tokens)` for the
    /// working history, and does NOT emit a Compaction event — the caller drives
    /// its own in-chat indicator, so an event would duplicate it.
    pub async fn compact(&mut self) -> (bool, usize, usize) {
        let before = crate::agent::compaction::estimate_history_tokens(&self.history);
        let len_before = self.history.len();
        self.compact_inner(false).await;
        let after = crate::agent::compaction::estimate_history_tokens(&self.history);
        (self.history.len() < len_before, before, after)
    }

    /// Compact history NOW, emitting a Compaction event so the UI shows it happened.
    /// Used by the automatic (threshold / overflow) paths.
    async fn compact_now(&mut self) {
        self.compact_inner(true).await;
    }

    /// Compact the working history. `emit` controls whether a Compaction event is
    /// published (automatic paths emit; the manual `/compact` command does not,
    /// since the frontend renders its own live indicator).
    async fn compact_inner(&mut self, emit: bool) {
        let history = std::mem::take(&mut self.history);
        let compaction = maybe_compact(
            history,
            &self.cfg.provider,
            &CompactionOptions {
                context_window: self.cfg.context_window,
                threshold: 0.0, // force compaction regardless of the estimate
                keep_recent: self.cfg.keep_recent,
                system_overhead_tokens: 0,
            },
        )
        .await;
        self.history = compaction.messages;
        if emit && compaction.compacted {
            self.cfg.bus.emit(AgentEvent::Compaction {
                agent_id: self.id.clone(),
                before_tokens: compaction.before_tokens,
                after_tokens: compaction.after_tokens,
                summary: compaction.summary.clone(),
                replaced_upto: compaction.replaced_upto,
            });
        }
    }

    /// Sleep for an exponential backoff before retrying a transient failure.
    async fn backoff(&self, attempt: u32) {
        let secs = (0.5 * 2f64.powi(attempt as i32 - 1)).min(8.0);
        tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
    }

    /// A final tool-less generation when the turn budget is exhausted: ask the
    /// model to summarize what it did and what remains, so the caller gets a real
    /// handoff instead of a stub. Best-effort — falls back to a stub on error.
    async fn wind_down(&mut self) -> String {
        let mut messages = self.history.clone();
        messages.push(Message::user_text(
            "You've reached your turn limit and can no longer call tools. In a few \
             sentences, summarize what you accomplished, what remains unfinished, and \
             your recommended next step. Do not attempt any more tools.",
        ));
        let opts = GenerateOptions {
            system: self.cfg.system.clone(),
            messages,
            tools: Vec::new(), // no tools — force a text answer
            cache: true,
            reasoning: self.reasoning,
            max_tokens: Some(1024),
            ..Default::default()
        };
        match self.cfg.provider.generate(opts).await {
            Ok(c) => {
                let text = c.message.text().trim().to_string();
                if text.is_empty() {
                    format!(
                        "[stopped after {} turns without finishing]",
                        self.cfg.max_turns
                    )
                } else {
                    format!("[turn limit reached] {text}")
                }
            }
            Err(_) => format!(
                "[stopped after reaching the {}-turn limit without finishing]",
                self.cfg.max_turns
            ),
        }
    }
}

/// Whether a provider error string indicates the request exceeded the model's
/// context window (so a reactive compaction + retry is worth attempting).
/// How the agent loop should react to a provider error, decided from its message
/// text (the errors flowing here are all `String`/`anyhow` — a typed error at the
/// provider source is a separate, larger change). Classification is centralized
/// here instead of scattered `msg.contains(...)` checks, and tightened so a stray
/// digit run like "1500 tokens" no longer reads as an HTTP 500.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderErrorKind {
    /// Context-length rejection → compact history and retry once.
    Overflow,
    /// Rate limit / overload / transient network hiccup → back off and retry.
    Transient,
    /// Anything else → fatal, propagate to the caller.
    Fatal,
}

/// Whether `hay` contains `code` as a standalone HTTP status token (bounded by a
/// non-digit or the string edge on both sides), so "500" matches "http 500" /
/// "error 500" / "status: 500" but NOT "1500 tokens" or "cost 5008".
fn has_status_code(hay: &str, code: &str) -> bool {
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(pos) = hay[from..].find(code) {
        let start = from + pos;
        let end = start + code.len();
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_digit();
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_digit();
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Classify a provider error message into the loop's reaction. Overflow is checked
/// before Transient so a length rejection compacts rather than blindly retrying.
fn classify_error(msg: &str) -> ProviderErrorKind {
    let m = msg.to_ascii_lowercase();

    // Context-length rejections — specific phrases, no false-positive risk.
    let overflow = m.contains("context length")
        || m.contains("context_length")
        || m.contains("maximum context")
        || m.contains("too many tokens")
        || m.contains("prompt is too long")
        || m.contains("reduce the length")
        || (m.contains("token") && m.contains("exceed"));
    if overflow {
        return ProviderErrorKind::Overflow;
    }

    // Transient: rate limits, overload, retryable HTTP statuses (as whole tokens),
    // timeouts, and dropped streams. "connection" alone is too broad, so require a
    // network qualifier alongside it.
    let transient = m.contains("rate limit")
        || m.contains("overloaded")
        || m.contains("timeout")
        || m.contains("timed out")
        || m.contains("stream ended")
        || ["429", "500", "502", "503", "529"]
            .iter()
            .any(|c| has_status_code(&m, c))
        || (m.contains("connection")
            && (m.contains("reset")
                || m.contains("refused")
                || m.contains("closed")
                || m.contains("error")
                || m.contains("aborted")));
    if transient {
        return ProviderErrorKind::Transient;
    }

    ProviderErrorKind::Fatal
}

/// The result of [`Agent::stream_completion`]: a finished completion, or a clean
/// stop because the run was interrupted mid-stream.
enum StreamOutcome {
    Completed(Completion),
    Interrupted,
}

/// Execute one tool call, emit its result event, and return the tool_result block.
/// `is_error` comes from the typed Result, not from sniffing the text. `hooks`
/// gate/rewrite the call: a `PreToolUse` deny skips execution, and `PostToolUse`
/// hooks fold the recorded output.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    tools: &ToolRegistry,
    bus: &EventBus,
    hooks: &crate::agent::hooks::Hooks,
    agent_id: &str,
    ctx: &ToolContext,
    id: String,
    name: String,
    input: serde_json::Value,
) -> ContentBlock {
    // If the streamed/returned tool arguments failed to parse as JSON, the codec
    // tagged them with a sentinel rather than silently substituting `{}` (which
    // used to make the tool fail downstream with a misleading "missing field"
    // error the model couldn't diagnose). Surface the real cause so it retries
    // with well-formed arguments.
    let (mut content, is_error) =
        if let Some(explain) = crate::providers::codec::tool_input_parse_error(&input) {
            (explain, true)
        } else {
            // PreToolUse hooks: may deny (skip execution) or rewrite the input.
            match hooks.run_pre(&name, input).await {
                Err(reason) => (reason, true),
                Ok(input) => match tools.execute(&name, input, ctx).await {
                    Ok(output) => (output, false),
                    Err(e) => (e.wire(), true),
                },
            }
        };
    // PostToolUse hooks: may rewrite the recorded output (e.g. redact secrets).
    if !hooks.post.is_empty() {
        content = hooks.run_post(&name, content, is_error).await;
    }
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

#[cfg(test)]
mod classify_tests {
    use super::{classify_error, ProviderErrorKind};

    #[test]
    fn overflow_phrases_classify_as_overflow() {
        for m in [
            "This model's maximum context length is 200000 tokens",
            "prompt is too long",
            "reduce the length of the messages",
            "input tokens exceed the limit",
        ] {
            assert_eq!(classify_error(m), ProviderErrorKind::Overflow, "{m}");
        }
    }

    #[test]
    fn transient_statuses_and_phrases_classify_as_transient() {
        for m in [
            "HTTP 500: internal server error",
            "error 502 bad gateway",
            "status: 503",
            "429 Too Many Requests",
            "the model is overloaded",
            "rate limit exceeded",
            "request timed out",
            "stream ended unexpectedly",
            "connection reset by peer",
        ] {
            assert_eq!(classify_error(m), ProviderErrorKind::Transient, "{m}");
        }
    }

    #[test]
    fn digit_run_containing_500_is_not_a_status_code() {
        // Regression: `contains("500")` used to force a spurious retry on messages
        // that merely mention a number containing 500.
        assert_eq!(
            classify_error("model produced 1500 tokens but the schema was invalid"),
            ProviderErrorKind::Fatal,
        );
        assert_eq!(
            classify_error("cost was 5008 microcents"),
            ProviderErrorKind::Fatal,
        );
    }

    #[test]
    fn bare_connection_word_is_not_transient() {
        // "connection" needs a network qualifier (reset/refused/closed/error/…).
        assert_eq!(
            classify_error("the tool returned a database connection handle"),
            ProviderErrorKind::Fatal,
        );
        assert_eq!(
            classify_error("connection refused"),
            ProviderErrorKind::Transient,
        );
    }

    #[test]
    fn provider_stream_drop_messages_are_retryable() {
        // Contract: the transport-failure messages the providers emit on a dropped
        // stream (openai.rs / anthropic.rs / responses.rs) must classify as Transient
        // so the loop retries instead of surfacing a fabricated success. They embed
        // "stream ended" so the classification holds regardless of the reqwest tail.
        for m in [
            "openai stream ended: error decoding response body",
            "anthropic stream ended: connection reset by peer",
            "responses stream ended: unexpected EOF during chunked read",
        ] {
            assert_eq!(classify_error(m), ProviderErrorKind::Transient, "{m}");
        }
    }

    #[test]
    fn unknown_errors_are_fatal() {
        assert_eq!(classify_error("invalid api key"), ProviderErrorKind::Fatal,);
        assert_eq!(classify_error(""), ProviderErrorKind::Fatal);
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::cancel_requested;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn own_flag_cancels() {
        let own = AtomicBool::new(false);
        assert!(!cancel_requested(&own, None));
        own.store(true, Ordering::Relaxed);
        assert!(cancel_requested(&own, None));
    }

    #[test]
    fn parent_flag_cascades_to_child() {
        // A child's own flag is clear, but cancelling the parent (root) must make
        // the child observe cancellation — the core of #5.
        let parent = Arc::new(AtomicBool::new(false));
        let child_own = AtomicBool::new(false);
        assert!(!cancel_requested(&child_own, Some(&parent)));
        parent.store(true, Ordering::Relaxed);
        assert!(cancel_requested(&child_own, Some(&parent)));
    }

    #[test]
    fn childs_own_run_start_clear_does_not_undo_parent_cancel() {
        // A child clears its OWN flag at each turn start; that must not resurrect it
        // once the parent has been cancelled (the "won't die" bug).
        let parent = Arc::new(AtomicBool::new(true));
        let child_own = AtomicBool::new(false); // freshly cleared at turn start
        assert!(cancel_requested(&child_own, Some(&parent)));
    }
}

#[cfg(test)]
mod structured_tests {
    use super::{Agent, AgentConfig, DEFAULT_MAX_TURNS};
    use crate::core::events::EventBus;
    use crate::providers::mock::{MockProvider, MockReply, MockRule};
    use crate::providers::provider::Provider;
    use crate::tools::registry::ToolRegistry;
    use serde_json::json;
    use std::sync::Arc;

    /// Build a minimal root-ish agent over a given provider, with an empty tool set.
    fn agent_with(provider: Arc<dyn Provider>) -> Agent {
        Agent::new(AgentConfig {
            provider,
            tools: ToolRegistry::new(None),
            bus: EventBus::new(),
            system: None,
            cwd: ".".to_string(),
            max_turns: DEFAULT_MAX_TURNS,
            id: Some("root".to_string()),
            context_window: 200_000,
            compact_threshold: 0.8,
            keep_recent: 6,
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
            inbox: None,
            team: None,
            name: "root".to_string(),
            depth: 0,
            parent_cancel: None,
            cancel: None,
            hooks: crate::agent::hooks::Hooks::default(),
        })
    }

    #[tokio::test]
    async fn structured_run_captures_the_tool_call() {
        // The model's first move is to call structured_output with a valid object.
        let schema = json!({
            "type": "object",
            "required": ["answer"],
            "properties": { "answer": {"type": "string"} }
        });
        let provider = MockProvider::new(vec![MockRule {
            needle: "structured_output".to_string(),
            reply: MockReply::ToolCall {
                name: "structured_output".to_string(),
                input: json!({"answer": "42"}),
            },
        }]);
        let mut agent = agent_with(Arc::new(provider));
        let out = agent
            .run_structured("what is the answer?", schema)
            .await
            .unwrap();
        assert_eq!(out, json!({"answer": "42"}));
    }

    #[tokio::test]
    async fn structured_run_errors_when_never_called() {
        // The model just talks and never calls the tool — after the retry we get Err.
        let schema = json!({ "type": "object", "required": ["answer"] });
        let provider = MockProvider::new(vec![]).with_default(MockReply::Text("hi".into()));
        let mut agent = agent_with(Arc::new(provider));
        assert!(agent.run_structured("go", schema).await.is_err());
    }

    #[tokio::test]
    async fn truncated_response_recovers_a_bounded_number_of_times_then_stops() {
        // A provider that ALWAYS returns a MaxTokens-truncated tool call (as the
        // 4096-cap bug did) must not loop forever: the output-truncation recovery
        // retries with a raised cap a bounded number of times, then lets the turn
        // proceed (the parse-error path handles the partial). The whole run must
        // TERMINATE — a regression here would hang the test via the timeout.
        let provider = MockProvider::new(vec![]).with_default(MockReply::Truncated {
            name: "some_tool".to_string(),
            // Truncated JSON — exactly the sentinel `parse_tool_input` produces.
            input: json!({ "__bob_tool_args_parse_error__": { "raw": "{\"x\":", "error": "eof" } }),
            output_tokens: 4096,
        });
        let provider = Arc::new(provider);
        // A tiny max_turns so that, once recovery is exhausted, the loop still exits
        // promptly instead of grinding to 200 turns.
        let mut agent = Agent::new(AgentConfig {
            provider: provider.clone(),
            tools: ToolRegistry::new(None),
            bus: EventBus::new(),
            system: None,
            cwd: ".".to_string(),
            max_turns: 4,
            id: Some("root".to_string()),
            context_window: 200_000,
            compact_threshold: 0.8,
            keep_recent: 6,
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
            inbox: None,
            team: None,
            name: "root".to_string(),
            depth: 0,
            parent_cancel: None,
            cancel: None,
            hooks: crate::agent::hooks::Hooks::default(),
        });
        // Must return (not hang). The exact text isn't important — that it completes
        // is. The 10s guard turns a regression (infinite loop) into a failure.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            agent.run("do something big"),
        )
        .await;
        assert!(res.is_ok(), "run must terminate, not loop on truncation");
        assert!(res.unwrap().is_ok());
        // Recovery is bounded: at most (max_turns) turns × a small constant of
        // recovery retries — nowhere near an unbounded loop.
        assert!(
            provider.call_count() <= 12,
            "expected a bounded number of provider calls, got {}",
            provider.call_count()
        );
    }
}

/// End-to-end agent-loop tests: a real turn that calls a tool, feeds the result
/// back, and finishes — driven entirely by MockProvider + a fake tool, asserting
/// the loop terminates and the transcript/events are what a frontend would render.
#[cfg(test)]
mod e2e_tests {
    use super::{Agent, AgentConfig, DEFAULT_MAX_TURNS};
    use crate::core::events::{AgentEvent, EventBus};
    use crate::core::types::{Role, ToolSpec};
    use crate::providers::mock::{MockProvider, MockReply, MockRule};
    use crate::tools::registry::{Tool, ToolContext, ToolRegistry, ToolResult};
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    /// A trivial tool that records the input it was called with and echoes a fixed
    /// result — enough to exercise the full tool-call → tool-result → text turn.
    struct EchoTool {
        seen: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".to_string(),
                description: "echo back".to_string(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
            self.seen.lock().unwrap().push(input.clone());
            Ok(format!("echoed: {}", input["msg"].as_str().unwrap_or("")))
        }
        fn is_read_only(&self) -> bool {
            true
        }
    }

    fn agent_with_echo(
        provider: Arc<dyn crate::providers::provider::Provider>,
        bus: EventBus,
    ) -> (Agent, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new(None);
        tools.add(Arc::new(EchoTool { seen: seen.clone() }));
        let agent = Agent::new(AgentConfig {
            provider,
            tools,
            bus,
            system: None,
            cwd: ".".to_string(),
            max_turns: DEFAULT_MAX_TURNS,
            id: Some("root".to_string()),
            context_window: 200_000,
            compact_threshold: 0.8,
            keep_recent: 6,
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
            inbox: None,
            team: None,
            name: "root".to_string(),
            depth: 0,
            parent_cancel: None,
            cancel: None,
            hooks: crate::agent::hooks::Hooks::default(),
        });
        (agent, seen)
    }

    #[tokio::test]
    async fn full_turn_calls_a_tool_then_finishes() {
        // The model calls `echo` once (keyed off the user prompt), then — seeing the
        // tool result — replies with final text. This is the canonical agent loop.
        // The model calls `echo` once (keyed off the user prompt), then — once the
        // tool result comes back (a Tool-role message whose `.text()` is empty, so
        // the "please echo" rule no longer matches) — falls through to the default
        // reply with final text.
        let provider = MockProvider::new(vec![MockRule {
            needle: "please echo".to_string(),
            reply: MockReply::ToolCall {
                name: "echo".to_string(),
                input: json!({"msg": "hello"}),
            },
        }])
        .with_default(MockReply::Text("all done".to_string()));

        // Capture the event stream a frontend would render.
        let bus = EventBus::new();
        let kinds = Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let kinds = kinds.clone();
            bus.on(Arc::new(move |e: &AgentEvent| {
                kinds.lock().unwrap().push(e.kind().to_string());
            }));
        }

        let (mut agent, seen) = agent_with_echo(Arc::new(provider), bus);
        let out = agent.run("please echo").await.unwrap();

        // The final assistant text is returned.
        assert_eq!(out, "all done");
        // The tool actually ran, with the model's input.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["msg"], "hello");

        // History ends coherently: user → assistant(tool_use) → tool(result) →
        // assistant(text). The last message is the final assistant turn.
        let msgs = agent.messages();
        assert_eq!(msgs.first().unwrap().role, Role::User);
        assert_eq!(msgs.last().unwrap().role, Role::Assistant);
        assert_eq!(msgs.last().unwrap().text(), "all done");

        // The event stream carried the turn: a tool call, a tool result, and a
        // final message — the signals the TUI/remote render from.
        let kinds = kinds.lock().unwrap();
        assert!(kinds.iter().any(|k| k == "ToolCall"), "kinds: {kinds:?}");
        assert!(kinds.iter().any(|k| k == "ToolResult"), "kinds: {kinds:?}");
        assert!(kinds.iter().any(|k| k == "TurnEnd"), "kinds: {kinds:?}");
    }

    #[tokio::test]
    async fn plain_text_turn_returns_immediately() {
        // No tool call: the model answers directly and the loop ends in one turn.
        let provider = MockProvider::new(vec![]).with_default(MockReply::Text("hi there".into()));
        let (mut agent, _seen) = agent_with_echo(Arc::new(provider), EventBus::new());
        let out = agent.run("hello").await.unwrap();
        assert_eq!(out, "hi there");
        // user + assistant only.
        assert_eq!(agent.messages().len(), 2);
    }

    // ---- hooks ----
    use crate::agent::hooks::{Hooks, PostToolUse, PreToolDecision, PreToolUse};

    /// Build an echo-tool agent with a specific hook set.
    fn agent_with_hooks(
        provider: Arc<dyn crate::providers::provider::Provider>,
        hooks: Hooks,
    ) -> (Agent, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new(None);
        tools.add(Arc::new(EchoTool { seen: seen.clone() }));
        let agent = Agent::new(AgentConfig {
            provider,
            tools,
            bus: EventBus::new(),
            system: None,
            cwd: ".".to_string(),
            max_turns: DEFAULT_MAX_TURNS,
            id: Some("root".to_string()),
            context_window: 200_000,
            compact_threshold: 0.8,
            keep_recent: 6,
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
            inbox: None,
            team: None,
            name: "root".to_string(),
            depth: 0,
            parent_cancel: None,
            cancel: None,
            hooks,
        });
        (agent, seen)
    }

    fn echo_provider() -> MockProvider {
        // Calls `echo` once on the first prompt, then finishes with text.
        MockProvider::new(vec![MockRule {
            needle: "go".to_string(),
            reply: MockReply::ToolCall {
                name: "echo".to_string(),
                input: json!({"msg": "hi"}),
            },
        }])
        .with_default(MockReply::Text("done".into()))
    }

    struct DenyEcho;
    #[async_trait]
    impl PreToolUse for DenyEcho {
        async fn on_pre_tool(&self, tool: &str, _input: &Value) -> PreToolDecision {
            if tool == "echo" {
                PreToolDecision::Deny("blocked by policy".into())
            } else {
                PreToolDecision::Proceed(_input.clone())
            }
        }
    }

    #[tokio::test]
    async fn pre_tool_hook_denies_and_tool_never_runs() {
        let hooks = Hooks {
            pre: vec![Arc::new(DenyEcho)],
            post: vec![],
        };
        let (mut agent, seen) = agent_with_hooks(Arc::new(echo_provider()), hooks);
        agent.run("go").await.unwrap();
        // The tool was blocked before executing.
        assert!(seen.lock().unwrap().is_empty(), "denied tool must not run");
    }

    struct RewriteMsg;
    #[async_trait]
    impl PreToolUse for RewriteMsg {
        async fn on_pre_tool(&self, _tool: &str, input: &Value) -> PreToolDecision {
            let mut v = input.clone();
            v["msg"] = json!("rewritten");
            PreToolDecision::Proceed(v)
        }
    }

    #[tokio::test]
    async fn pre_tool_hook_rewrites_the_input() {
        let hooks = Hooks {
            pre: vec![Arc::new(RewriteMsg)],
            post: vec![],
        };
        let (mut agent, seen) = agent_with_hooks(Arc::new(echo_provider()), hooks);
        agent.run("go").await.unwrap();
        // The tool saw the hook's rewritten input, not the model's original.
        assert_eq!(seen.lock().unwrap()[0]["msg"], "rewritten");
    }

    struct ShoutOutput;
    #[async_trait]
    impl PostToolUse for ShoutOutput {
        async fn on_post_tool(&self, _tool: &str, output: String, _is_error: bool) -> String {
            output.to_uppercase()
        }
    }

    #[tokio::test]
    async fn post_tool_hook_rewrites_the_output() {
        let hooks = Hooks {
            pre: vec![],
            post: vec![Arc::new(ShoutOutput)],
        };
        // Capture the tool-result event to inspect the recorded output.
        let (agent_provider, recorded) = (Arc::new(echo_provider()), Arc::new(Mutex::new(None)));
        let (mut agent, _seen) = agent_with_hooks(agent_provider, hooks);
        {
            let recorded = recorded.clone();
            agent.bus().on(Arc::new(move |e: &AgentEvent| {
                if let AgentEvent::ToolResult { output, .. } = e {
                    *recorded.lock().unwrap() = Some(output.clone());
                }
            }));
        }
        agent.run("go").await.unwrap();
        // EchoTool returns "echoed: hi"; the post hook uppercased it.
        assert_eq!(recorded.lock().unwrap().as_deref(), Some("ECHOED: HI"),);
    }
}
