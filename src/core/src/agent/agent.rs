use crate::agent::compaction::{input_safe_budget, maybe_compact, CompactionOptions};
use crate::core::events::{AgentEvent, EventBus};
use crate::core::types::{
    Completion, ContentBlock, GenerateOptions, Message, Role, StopReason, StreamEvent, ToolOutput,
    Usage,
};
#[cfg(test)]
use crate::providers::provider::has_status_code;
use crate::providers::provider::{is_transient_error, Provider, MAX_RETRIES, RETRY_DELAY};
use crate::tools::file_tracker::FileTracker;
use crate::tools::registry::{ToolContext, ToolRegistry};
use crate::tools::todo::TodoStore;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[path = "streaming.rs"]
mod streaming;

static AGENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Canonical root identity used by coordination and transcript routing.
pub const ROOT_AGENT_ID: &str = "root";

pub const COMPACT_THRESHOLD: f64 = 0.8;
pub const KEEP_RECENT: usize = 6;
pub const DEFAULT_MAX_TURNS: u32 = 200;
pub const SUBAGENT_MAX_TURNS: u32 = 100;
pub const EXPLORE_MAX_TURNS: u32 = 60;

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
    pub inbox: Option<crate::agent::team::AgentInbox>,
    pub team: Option<crate::agent::team::AgentRegistry>,
    /// Ancestor cancellation, observed but never cleared by the child.
    pub parent_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub hooks: crate::agent::hooks::Hooks,
    pub memory: Option<Arc<dyn crate::core::memory::MemoryRepository>>,
}

pub fn build_subagent(spec: SubagentSpec) -> Agent {
    let context_window = spec.provider.context_window();
    let mut agent = Agent::new(AgentConfig {
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
        hooks: spec.hooks,
    });
    agent.set_memory_repository(spec.memory);
    agent
}

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
    /// Shared job and output ownership across this session.
    pub jobs: crate::tools::jobs::JobRegistry,
    /// UI hook for ask_user / exit_plan. None → those tools return a note.
    pub user_asker: Option<Arc<dyn crate::tools::registry::UserAsker>>,
    /// Shared server processes for the root and subagents.
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    pub inbox: Option<crate::agent::team::AgentInbox>,
    pub team: Option<crate::agent::team::AgentRegistry>,
    pub name: String,
    pub depth: usize,
    /// Ancestor cancellation, observed but never cleared by this agent.
    pub parent_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Optional shared local cancellation flag; otherwise created by the agent.
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub hooks: crate::agent::hooks::Hooks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStopReason {
    Completed,
    Cancelled,
    TurnLimit,
    TimedOut,
    OutputLimit,
    ToolPanic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    pub text: String,
    pub stop_reason: RunStopReason,
    pub turns: u32,
}

#[derive(Debug)]
pub struct RunStopped(pub RunOutcome);

impl std::fmt::Display for RunStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.0.stop_reason {
            RunStopReason::Completed => "completed",
            RunStopReason::Cancelled => "cancelled",
            RunStopReason::TurnLimit => "turn limit reached",
            RunStopReason::TimedOut => "timed out",
            RunStopReason::OutputLimit => "output limit reached",
            RunStopReason::ToolPanic => "tool panicked",
        };
        write!(f, "agent stopped ({reason}): {}", self.0.text)
    }
}

impl std::error::Error for RunStopped {}

impl RunOutcome {
    pub fn into_completed_text(self) -> anyhow::Result<String> {
        if self.stop_reason == RunStopReason::Completed {
            Ok(self.text)
        } else {
            Err(RunStopped(self).into())
        }
    }
}

pub struct Agent {
    pub id: String,
    cfg: AgentConfig,
    /// Provider-facing context; older messages may be compacted.
    history: Vec<Message>,
    /// Uncompacted transcript retained for persistence and display.
    full_history: Vec<Message>,
    files: Arc<FileTracker>,
    todos: Arc<TodoStore>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    reasoning: crate::core::types::ReasoningEffort,
    /// Highest warning emitted; compaction rearms lower thresholds.
    warned_pct: u8,
    turn_limit_summary: Option<String>,
    memory: Option<Arc<dyn crate::core::memory::MemoryRepository>>,
    request_system: Option<String>,
    context_estimate: crate::agent::context::ContextEstimate,
    structured_sink: Option<crate::tools::structured::OutputSink>,
    structured_error: std::sync::Mutex<Option<String>>,
    #[cfg(test)]
    skip_retry_backoff: bool,
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
            request_system: cfg.system.clone(),
            cfg,
            history: Vec::new(),
            full_history: Vec::new(),
            files: Arc::new(FileTracker::new()),
            todos: Arc::new(TodoStore::new()),
            cancel,
            reasoning: crate::core::types::ReasoningEffort::default(),
            warned_pct: 0,
            turn_limit_summary: None,
            memory: None,
            context_estimate: Default::default(),
            structured_sink: None,
            structured_error: std::sync::Mutex::new(None),
            #[cfg(test)]
            skip_retry_backoff: false,
        }
    }

    /// Full transcript, distinct from the compacted provider context.
    pub fn messages(&self) -> &[Message] {
        &self.full_history
    }

    pub fn working_messages(&self) -> &[Message] {
        &self.history
    }

    pub fn context_usage(&self) -> (usize, usize) {
        (
            self.context_estimate.used(
                crate::agent::compaction::estimate_history_tokens(&self.history)
                    + self.system_overhead_tokens(),
            ),
            self.cfg.context_window,
        )
    }

    pub fn tool_specs(&self) -> Vec<crate::core::types::ToolSpec> {
        self.cfg.tools.specs()
    }

    pub fn todos(&self) -> Arc<TodoStore> {
        self.todos.clone()
    }

    /// Actionable messages wake immediately; completion reports may be batched.
    pub fn has_pending_coordination(&mut self) -> bool {
        if self
            .cfg
            .inbox
            .as_mut()
            .is_some_and(|inbox| inbox.has_actionable())
        {
            return true;
        }
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

    /// Keeps the driver alive while child results are pending, even before a wake is ready.
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

    pub fn drain_coordination(&mut self) {
        if let Some(inbox) = self.cfg.inbox.as_mut() {
            for message in inbox.drain() {
                if matches!(
                    message.kind,
                    crate::agent::team::AgentMessageKind::Actionable
                ) {
                    if let Some(sink) = &self.structured_sink {
                        sink.lock().unwrap().take();
                    }
                }
                let msg = Message::user_text(crate::agent::team::format_coord_message(
                    &message.from,
                    &message.text,
                ));
                self.history.push(msg.clone());
                self.full_history.push(msg);
                self.cfg.bus.emit(AgentEvent::AgentMessage {
                    to: self.id.clone(),
                    from: message.from,
                    text: message.text,
                });
            }
        }
    }

    pub fn discard_pending_coordination(&mut self) {
        if let Some(inbox) = self.cfg.inbox.as_mut() {
            inbox.drain();
        }
    }

    pub(crate) async fn settle_owned_mutations(&self) -> bool {
        if self.cfg.depth == 0 {
            return self.cfg.jobs.cancel_blocking_mutations_and_wait().await;
        }
        let mut owners = vec![self.cfg.name.clone()];
        if let Some(team) = &self.cfg.team {
            owners.extend(team.descendant_names(&self.cfg.name));
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for owner in owners {
                if !self
                    .cfg
                    .jobs
                    .cancel_blocking_mutations_for_and_wait(&owner)
                    .await
                {
                    return false;
                }
            }
            true
        })
        .await
        .unwrap_or(false)
    }

    pub fn try_close_inbox(&mut self) -> bool {
        let Some(handle) = self
            .cfg
            .team
            .as_ref()
            .and_then(|team| team.get(&self.cfg.name))
        else {
            return true;
        };
        self.cfg
            .inbox
            .as_mut()
            .is_none_or(|inbox| inbox.try_close(&handle))
    }

    pub fn set_memory_repository(
        &mut self,
        memory: Option<Arc<dyn crate::core::memory::MemoryRepository>>,
    ) {
        self.memory = memory;
        self.request_system = self.cfg.system.clone();
        self.context_estimate.reset();
    }

    async fn refresh_system(&mut self) -> anyhow::Result<bool> {
        let Some(system) = until_cancelled(
            &self.cancel,
            self.cfg.parent_cancel.as_ref(),
            crate::core::memory::with_memory_context(
                self.cfg.system.clone().unwrap_or_default(),
                self.cfg.cwd.clone(),
                self.memory.clone(),
            ),
        )
        .await
        else {
            return Ok(false);
        };
        let system = system?;
        self.request_system = if self.cfg.system.is_some() || !system.is_empty() {
            Some(system)
        } else {
            None
        };
        Ok(true)
    }

    pub fn cancel_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.cancel.clone()
    }

    pub fn bus(&self) -> EventBus {
        self.cfg.bus.clone()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        cancel_requested(&self.cancel, self.cfg.parent_cancel.as_ref())
    }

    pub fn load_history(&mut self, messages: Vec<Message>) {
        self.load_histories(messages.clone(), messages);
    }

    pub fn load_histories(&mut self, full: Vec<Message>, working: Vec<Message>) {
        self.full_history = full;
        self.history = working;
        self.context_estimate.reset();
        self.warned_pct = 0;
    }

    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.cfg.context_window = provider.context_window();
        self.cfg.provider = provider;
        self.context_estimate.reset();
        self.warned_pct = 0;
    }

    pub fn set_reasoning(&mut self, effort: crate::core::types::ReasoningEffort) {
        self.reasoning = effort;
    }

    pub fn reasoning(&self) -> crate::core::types::ReasoningEffort {
        self.reasoning
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        self.cfg.provider.clone()
    }

    pub(crate) fn turn_limit_summary(&self) -> Option<&str> {
        self.turn_limit_summary.as_deref()
    }

    pub fn turn_budget(&self) -> u32 {
        self.cfg.max_turns
    }

    pub async fn run(&mut self, prompt: &str) -> anyhow::Result<String> {
        self.cancel.store(false, Ordering::Relaxed);
        self.run_with_budget(prompt, self.turn_budget())
            .await?
            .into_completed_text()
    }

    /// Run without resetting cancellation, consuming at most the supplied budget.
    pub async fn run_with_budget(
        &mut self,
        prompt: &str,
        max_turns: u32,
    ) -> anyhow::Result<RunOutcome> {
        self.turn_limit_summary = None;
        let max_turns = max_turns.min(self.cfg.max_turns);
        let mut turns = 0;
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

        // Empty prompts are coordination wakes, not user turns.
        if !prompt.is_empty() {
            let m = Message::user_text(prompt);
            self.history.push(m.clone());
            self.full_history.push(m);
            self.cfg.bus.emit(AgentEvent::UserPrompt {
                agent_id: self.id.clone(),
                text: prompt.to_string(),
            });
        }
        self.cfg.bus.emit(AgentEvent::TurnStart {
            agent_id: self.id.clone(),
        });

        let mut turn = TurnEndGuard {
            bus: self.cfg.bus.clone(),
            agent_id: self.id.clone(),
            usage: Usage::default(),
        };
        let mut final_text = String::new();
        let mut finished = false;
        let mut failure = None;

        let mut max_tokens_override: Option<u32> = None;
        let mut truncation_recoveries: u32 = 0;

        'turns: for _turn in 0..max_turns {
            // History is at a tool-safe boundary here.
            if self.is_cancelled() {
                final_text = "[interrupted]".to_string();
                break;
            }
            turns += 1;
            self.drain_coordination();
            let (used, _) = self.context_usage();
            self.warn_context(used);
            if self
                .compact_with_threshold(self.cfg.compact_threshold)
                .await?
                .is_none()
            {
                break;
            }

            let completion = match self.stream_completion(max_tokens_override).await? {
                StreamOutcome::Completed(c) => c,
                StreamOutcome::Interrupted(text) => {
                    if !text.is_empty() {
                        self.record_summary(&text);
                    }
                    final_text = text;
                    break;
                }
            };

            self.record_usage(completion.usage, &mut turn.usage);

            if completion_truncated(&completion) {
                if turns < max_turns {
                    if let Some(bumped) = self.truncation_retry_cap(
                        &completion,
                        truncation_recoveries,
                        max_tokens_override,
                    ) {
                        truncation_recoveries += 1;
                        max_tokens_override = Some(bumped);
                        continue;
                    }
                }
                let mut message = completion.message.clone();
                message
                    .content
                    .retain(|block| !matches!(block, ContentBlock::ToolUse { .. }));
                final_text = format!(
                    "[output limit reached; response incomplete and no truncated tool calls were executed]\n{}",
                    message.text(),
                );
                if !message.content.is_empty() {
                    self.history.push(message.clone());
                    self.full_history.push(message.clone());
                    self.cfg.bus.emit(AgentEvent::Message {
                        agent_id: self.id.clone(),
                        message,
                    });
                }
                self.record_summary("[output limit reached; response incomplete]");
                failure = Some(RunStopReason::OutputLimit);
                break;
            }
            // Recovery overrides must not carry into the next turn.
            max_tokens_override = None;

            self.history.push(completion.message.clone());
            self.full_history.push(completion.message.clone());
            self.cfg.bus.emit(AgentEvent::Message {
                agent_id: self.id.clone(),
                message: completion.message.clone(),
            });

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
                // A steering message may have arrived during the final provider
                // response. Give it a model step instead of dropping the inbox.
                if !self.is_cancelled()
                    && self
                        .cfg
                        .inbox
                        .as_mut()
                        .is_some_and(|inbox| inbox.has_pending())
                {
                    continue;
                }
                if self.cfg.name != ROOT_AGENT_ID
                    && self
                        .structured_sink
                        .as_ref()
                        .is_none_or(|sink| sink.lock().unwrap().is_some())
                {
                    loop {
                        if self.is_cancelled() {
                            final_text = completion.message.text();
                            break 'turns;
                        }
                        if self.has_pending_coordination() {
                            continue 'turns;
                        }
                        let children_running = self
                            .cfg
                            .team
                            .as_ref()
                            .is_some_and(|team| team.has_running_children(&self.cfg.name));
                        if !children_running {
                            if self.try_close_inbox() {
                                break;
                            }
                            continue 'turns;
                        }
                        until_cancelled(
                            &self.cancel,
                            self.cfg.parent_cancel.as_ref(),
                            tokio::time::sleep(std::time::Duration::from_millis(10)),
                        )
                        .await;
                    }
                }
                final_text = completion.message.text();
                finished = true;
                break;
            }

            let (results, panicked) = self.execute_tool_uses(&ctx, &tool_uses).await;
            let msg = Message {
                role: Role::Tool,
                content: results,
            };
            self.history.push(msg.clone());
            self.full_history.push(msg);
            if panicked {
                final_text = "[stopped after a tool panic; outcome unknown, changes may have taken effect. Inspect state before retrying.]".into();
                self.record_summary(&final_text);
                failure = Some(RunStopReason::ToolPanic);
                break;
            }
        }

        // One tool-less handoff is allowed after budget exhaustion.
        if failure.is_none()
            && !finished
            && final_text.is_empty()
            && !self.is_cancelled()
            && max_turns > 0
        {
            final_text = self.wind_down(max_turns, &mut turn.usage).await;
        }
        let stop_reason = if let Some(reason) = failure {
            reason
        } else if self.is_cancelled() {
            if final_text.is_empty() {
                final_text = "[interrupted]".to_string();
            }
            RunStopReason::Cancelled
        } else if finished {
            RunStopReason::Completed
        } else {
            if final_text.is_empty() {
                final_text = format!(
                    "[stopped after reaching the {max_turns}-turn limit without finishing]"
                );
                self.record_summary(&final_text);
            }
            self.turn_limit_summary = Some(final_text.clone());
            RunStopReason::TurnLimit
        };

        drop(turn);
        Ok(RunOutcome {
            text: final_text,
            stop_reason,
            turns,
        })
    }

    /// Reads run concurrently; mutations are serialized to avoid races.
    /// Results retain request order, with all call events emitted before results.
    async fn execute_tool_uses(
        &self,
        ctx: &ToolContext,
        tool_uses: &[(String, String, serde_json::Value)],
    ) -> (Vec<ContentBlock>, bool) {
        for (id, name, input) in tool_uses {
            self.cfg.bus.emit(AgentEvent::ToolCall {
                agent_id: self.id.clone(),
                tool_use_id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
        }
        let mut concurrent = Vec::new();
        for (id, name, input) in tool_uses {
            if self.cfg.tools.is_read_only(name) {
                concurrent.push(self.run_tool(ctx, id, name, input));
            }
        }
        let concurrent_results = futures::future::join_all(concurrent).await;
        let mut panicked = concurrent_results.iter().any(|(_, panicked)| *panicked);
        let mut concurrent_results = concurrent_results.into_iter();
        let mut results = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in tool_uses {
            if self.cfg.tools.is_read_only(name) {
                if let Some((result, _)) = concurrent_results.next() {
                    results.push(result);
                }
            } else if panicked {
                results.push(self.tool_result(
                    id,
                    "not executed: an earlier tool panicked; inspect state before retrying".into(),
                    true,
                ));
            } else {
                let (result, failed) = self.run_tool(ctx, id, name, input).await;
                panicked |= failed;
                results.push(result);
            }
        }
        (results, panicked)
    }

    async fn run_tool(
        &self,
        ctx: &ToolContext,
        id: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> (ContentBlock, bool) {
        use crate::tools::registry::{ToolError, ToolErrorKind};
        let work = async {
            if name == "structured_output" {
                if let Some(sink) = &self.structured_sink {
                    sink.lock().unwrap().take();
                }
            }
            if let Some(explain) = crate::providers::codec::tool_input_parse_error(input) {
                return Err(ToolError::invalid_input(explain));
            }
            let input = self
                .cfg
                .hooks
                .run_pre(name, input.clone())
                .await
                .map_err(ToolError::failed)?;
            self.cfg.tools.execute(name, input, ctx).await
        };
        let started = !self.is_cancelled();
        let work = crate::core::panic::catch(work);
        tokio::pin!(work);
        let outcome =
            until_cancelled(&self.cancel, self.cfg.parent_cancel.as_ref(), &mut work).await;
        let outcome =
            if started && outcome.is_none() && matches!(name, "task" | "explore" | "workflow") {
                tokio::time::timeout(std::time::Duration::from_secs(2), &mut work)
                    .await
                    .ok()
            } else {
                outcome
            };
        let (mut content, is_error, mut panicked) = match outcome {
            Some(Ok(Ok(output))) => (output, false, false),
            Some(Ok(Err(error))) => {
                let panicked = error.kind == ToolErrorKind::Panic;
                (error.into_output(), true, panicked)
            }
            Some(Err(_)) => ("panic: tool execution panicked; outcome unknown, changes may have taken effect; do not automatically retry".into(), true, true),
            None => {
                let settled = self.settle_owned_mutations().await;
                let text = if !settled {
                    "interrupted, but memory mutation cleanup exceeded its deadline; changes may still complete"
                } else if name == "memory" {
                    "interrupted by user; any memory mutation already started has settled and may have taken effect"
                } else {
                    "interrupted by user"
                };
                return (self.tool_result(id, text.into(), true), false);
            }
        };
        if !panicked && !self.cfg.hooks.post.is_empty() {
            let post =
                crate::core::panic::catch(self.cfg.hooks.run_post(name, content.clone(), is_error));
            match until_cancelled(&self.cancel, self.cfg.parent_cancel.as_ref(), post).await {
                Some(Ok(output)) => content = output,
                Some(Err(_)) => panicked = true,
                None => {}
            }
        }
        if name == "structured_output" {
            *self.structured_error.lock().unwrap() =
                is_error.then(|| content.model_text().into_owned());
        }
        let bounded = crate::core::panic::catch(async {
            crate::agent::output::bound_tool_output(content.clone(), &ctx.jobs)
        })
        .await;
        match bounded {
            Ok(output) => content = output,
            Err(_) => panicked = true,
        }
        (self.tool_result(id, content, is_error), panicked)
    }

    fn tool_result(&self, id: &str, content: ToolOutput, is_error: bool) -> ContentBlock {
        self.cfg.bus.emit(AgentEvent::ToolResult {
            agent_id: self.id.clone(),
            tool_use_id: id.to_string(),
            output: content.clone(),
            is_error,
        });
        ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content,
            is_error: Some(is_error),
        }
    }

    /// Dropped streams may omit `finish_reason`; malformed arguments also signal truncation.
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
        if !completion_truncated(completion) {
            return None;
        }
        let ceiling = crate::providers::provider::max_output_tokens_for(self.cfg.provider.model());
        let produced = u32::try_from(completion.usage.output_tokens).unwrap_or(u32::MAX);
        let previous = current_override.unwrap_or(produced).max(produced);
        let bumped = previous.saturating_mul(2).max(8_192).min(ceiling);
        (bumped > previous).then_some(bumped)
    }

    /// Capture a schema-validated result through a temporary tool, nudging once
    /// if omitted. Always restore the registry after the run completes or fails.
    pub async fn run_structured(
        &mut self,
        prompt: &str,
        schema: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        use crate::tools::structured::StructuredOutputTool;
        crate::tools::schema::CompiledSchema::compile(&schema).map_err(anyhow::Error::msg)?;

        let sink: crate::tools::structured::OutputSink = Arc::new(std::sync::Mutex::new(None));
        self.structured_sink = Some(sink.clone());
        self.structured_error.lock().unwrap().take();

        let original_tools = self.cfg.tools.clone();
        self.cfg.tools.add(Arc::new(StructuredOutputTool::new(
            schema.clone(),
            sink.clone(),
        )));

        let instruction = "\n\nWhen you have everything you need, finish by calling the \
            `structured_output` tool exactly once with your final answer as its arguments. \
            That tool call IS your deliverable — do not also write a prose summary.";

        // Capture errors without `?` so the temporary registry is always restored.
        let first = self.run(&format!("{prompt}{instruction}")).await;
        let mut captured = sink.lock().unwrap().take();

        let mut second = Ok(String::new());
        if first.is_ok()
            && captured.is_none()
            && !self.is_cancelled()
            && self.turn_limit_summary.is_none()
        {
            let nudge = "You did not record a result. Call the `structured_output` tool now \
                with your final answer matching its schema.";
            second = self.run(nudge).await;
            captured = sink.lock().unwrap().take();
        }

        self.cfg.tools = original_tools;
        self.structured_sink = None;

        first?;
        second?;

        captured.ok_or_else(|| {
            let detail = self.structured_error.lock().unwrap().take();
            anyhow::anyhow!(
                "agent did not produce a structured result matching the schema{}",
                detail
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default()
            )
        })
    }

    fn warn_context(&mut self, used: usize) {
        let window = self.cfg.context_window.max(1);
        let pct = used.saturating_mul(100) / window;
        for level in [95u8, 85, 70] {
            if pct >= usize::from(level) && self.warned_pct < level {
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

    fn rearm_context_warning(&mut self, used: usize) {
        let window = self.cfg.context_window.max(1);
        let pct = used.saturating_mul(100) / window;
        self.warned_pct = [95u8, 85, 70]
            .into_iter()
            .find(|&level| pct >= usize::from(level))
            .unwrap_or(0);
    }

    /// Compact the working context, preserving the full transcript and logging
    /// successful compaction for resume. Cancellation or failure leaves it intact.
    pub async fn compact(&mut self) -> anyhow::Result<(bool, usize, usize)> {
        self.cancel.store(false, Ordering::Relaxed);
        self.compact_with_threshold(0.0)
            .await?
            .ok_or_else(|| anyhow::anyhow!("compaction cancelled; history unchanged"))
    }

    async fn compact_now(&mut self) -> anyhow::Result<bool> {
        match self.compact_with_threshold(0.0).await? {
            None => Ok(false),
            Some((true, _, _)) => Ok(true),
            Some((false, _, _)) => anyhow::bail!(
                "context window exceeded, but no history could be compacted; use a larger-context model or start a new session"
            ),
        }
    }

    fn system_overhead_tokens(&self) -> usize {
        use crate::agent::compaction::estimate_tokens;
        self.request_system
            .as_deref()
            .map(estimate_tokens)
            .unwrap_or(0)
            + self
                .cfg
                .tools
                .specs()
                .iter()
                .map(|s| {
                    estimate_tokens(&s.name)
                        + estimate_tokens(&s.description)
                        + estimate_tokens(&s.input_schema.to_string())
                })
                .sum::<usize>()
    }

    async fn compact_with_threshold(
        &mut self,
        threshold: f64,
    ) -> anyhow::Result<Option<(bool, usize, usize)>> {
        if !self.refresh_system().await? {
            return Ok(None);
        }
        let (used, _) = self.context_usage();
        self.warn_context(used);
        let threshold =
            if threshold > 0.0 && used > (self.cfg.context_window as f64 * threshold) as usize {
                0.0
            } else {
                threshold
            };
        let options = CompactionOptions {
            context_window: self.cfg.context_window,
            threshold,
            keep_recent: self.cfg.keep_recent,
            system_overhead_tokens: self.system_overhead_tokens(),
        };
        let on_retry = |attempt| {
            self.cfg.bus.emit(AgentEvent::StreamRetry {
                agent_id: self.id.clone(),
                attempt,
                max_attempts: MAX_RETRIES,
            });
        };
        let Some(result) = until_cancelled(
            &self.cancel,
            self.cfg.parent_cancel.as_ref(),
            maybe_compact(
                self.history.clone(),
                &self.cfg.provider,
                &options,
                &on_retry,
            ),
        )
        .await
        else {
            return Ok(None);
        };
        let result = result
            .map_err(|error| anyhow::anyhow!("compaction failed; history unchanged: {error:#}"))?;
        if self.is_cancelled() {
            return Ok(None);
        }
        if threshold > 0.0 {
            anyhow::ensure!(
                result.after_tokens <= input_safe_budget(self.cfg.context_window),
                "context still exceeds the safe request budget; use /compact or a larger-context model"
            );
        }
        let outcome = (result.compacted, result.before_tokens, result.after_tokens);
        if result.compacted {
            self.history = result.messages;
            self.context_estimate.reset();
            self.rearm_context_warning(result.after_tokens);
            self.cfg.bus.emit(AgentEvent::Compaction {
                agent_id: self.id.clone(),
                before_tokens: result.before_tokens,
                after_tokens: result.after_tokens,
                summary: result.summary,
                replaced_upto: result.replaced_upto,
            });
        }
        Ok(Some(outcome))
    }

    async fn backoff(&self) {
        #[cfg(test)]
        if self.skip_retry_backoff {
            return;
        }
        until_cancelled(
            &self.cancel,
            self.cfg.parent_cancel.as_ref(),
            tokio::time::sleep(RETRY_DELAY),
        )
        .await;
    }

    fn record_usage(&mut self, usage: Usage, total: &mut Usage) {
        self.context_estimate.observe(
            usage,
            crate::agent::compaction::estimate_history_tokens(&self.history)
                + self.system_overhead_tokens(),
        );
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        total.cache_creation_input_tokens += usage.cache_creation_input_tokens;
        total.cache_read_input_tokens += usage.cache_read_input_tokens;
        self.cfg.bus.emit(AgentEvent::Completion {
            agent_id: self.id.clone(),
            model: self.cfg.provider.model().to_string(),
            usage,
        });
    }

    fn record_summary(&mut self, text: &str) {
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        };
        self.history.push(message.clone());
        self.full_history.push(message.clone());
        self.cfg.bus.emit(AgentEvent::Message {
            agent_id: self.id.clone(),
            message,
        });
    }

    async fn wind_down(&mut self, max_turns: u32, total: &mut Usage) -> String {
        match self
            .compact_with_threshold(self.cfg.compact_threshold)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => return String::new(),
            Err(error) => {
                let text =
                    format!("[stopped after reaching the {max_turns}-turn limit; {error:#}]");
                self.record_summary(&text);
                return text;
            }
        }
        let mut messages = self.history.clone();
        messages.push(Message::user_text(
            "You've reached your turn limit and can no longer call tools. In a few \
             sentences, summarize what you accomplished, what remains unfinished, and \
             your recommended next step. Do not attempt any more tools.",
        ));
        let opts = GenerateOptions {
            system: self.request_system.clone(),
            messages,
            tools: Vec::new(),
            cache: true,
            reasoning: self.reasoning,
            max_tokens: Some(1024),
            ..Default::default()
        };
        let on_retry = |attempt| {
            self.cfg.bus.emit(AgentEvent::StreamRetry {
                agent_id: self.id.clone(),
                attempt,
                max_attempts: MAX_RETRIES,
            });
        };
        let Some(completion) = until_cancelled(
            &self.cancel,
            self.cfg.parent_cancel.as_ref(),
            crate::providers::provider::generate_with_retry(
                self.cfg.provider.as_ref(),
                opts,
                &on_retry,
            ),
        )
        .await
        else {
            return String::new();
        };
        let fallback =
            format!("[stopped after reaching the {max_turns}-turn limit without finishing]");
        let text = match completion {
            Ok(c) => {
                self.record_usage(c.usage, total);
                let text = c.message.text().trim().to_string();
                if text.is_empty() {
                    fallback
                } else {
                    format!("[turn limit reached] {text}")
                }
            }
            Err(error) => format!(
                "[stopped after reaching the {max_turns}-turn limit; summary failed: {error:#}]"
            ),
        };
        self.record_summary(&text);
        text
    }
}

struct TurnEndGuard {
    bus: EventBus,
    agent_id: String,
    usage: Usage,
}

impl Drop for TurnEndGuard {
    fn drop(&mut self) {
        self.bus.emit(AgentEvent::TurnEnd {
            agent_id: self.agent_id.clone(),
            usage: self.usage,
        });
    }
}

async fn until_cancelled<T>(
    own: &std::sync::atomic::AtomicBool,
    parent: Option<&Arc<std::sync::atomic::AtomicBool>>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::pin!(future);
    loop {
        if cancel_requested(own, parent) {
            return None;
        }
        tokio::select! {
            biased;
            output = &mut future => return Some(output),
            _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderErrorKind {
    Overflow,
    Transient,
    Fatal,
}

/// Overflow precedes transient errors so length rejections compact before retrying.
fn classify_error(msg: &str) -> ProviderErrorKind {
    let m = msg.to_ascii_lowercase();

    let overflow = m.contains("context length")
        || m.contains("context_length")
        || m.contains("maximum context")
        || m.contains("context_window_exceeded")
        || m.contains("exceeds the context window")
        || m.contains("too many tokens")
        || m.contains("prompt is too long")
        || m.contains("reduce the length")
        || (m.contains("token") && m.contains("exceed"));
    if overflow {
        return ProviderErrorKind::Overflow;
    }

    if is_transient_error(&m) {
        return ProviderErrorKind::Transient;
    }

    ProviderErrorKind::Fatal
}

enum StreamOutcome {
    Completed(Completion),
    Interrupted(String),
}

fn completion_truncated(completion: &Completion) -> bool {
    completion.stop_reason == StopReason::MaxTokens
        || completion.message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolUse { input, .. }
                if crate::providers::codec::tool_input_parse_error(input).is_some())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod run {
        use super::*;
        use crate::agent::team::{mailbox, AgentMessage};
        use crate::core::types::ToolSpec;
        use crate::test_support::agent::{
            completion, config, text, tool, ScriptProvider, Step, Summary,
        };
        use crate::tools::registry::{Tool, ToolResult};
        use async_trait::async_trait;
        use serde_json::{json, Value};
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use std::sync::Mutex;
        use std::time::Duration;
        use tokio::sync::Notify;

        pub(super) const BOUND: Duration = Duration::from_secs(2);

        pub(super) fn agent(provider: Arc<dyn Provider>) -> Agent {
            let mut cfg = config(provider);
            cfg.max_turns = 3;
            Agent::new(cfg)
        }

        pub(super) fn capture(agent: &Agent) -> Arc<Mutex<Vec<AgentEvent>>> {
            let events = Arc::new(Mutex::new(Vec::new()));
            let recorded = events.clone();
            agent.bus().on(Arc::new(move |event| {
                recorded.lock().unwrap().push(event.clone());
            }));
            events
        }

        pub(super) async fn bounded_run(
            agent: &mut Agent,
            prompt: &str,
            budget: u32,
        ) -> RunOutcome {
            tokio::time::timeout(BOUND, agent.run_with_budget(prompt, budget))
                .await
                .expect("run must settle promptly")
                .unwrap()
        }

        #[tokio::test]
        async fn live_memory_refreshes_after_save_forget_and_external_updates() {
            use crate::core::memory::{
                InMemoryMemories, MemoryRepository, MemoryScope, SqliteMemories,
            };
            use crate::test_support::TestDir;
            use crate::tools::memory::MemoryTool;

            const BASE: &str = "Preserve human instructions and verify changes.";
            const SAVED: &str = "Live saved preference.";
            const EXTERNAL: &str = "External update between turns.";

            for sqlite in [false, true] {
                let dir = TestDir::new();
                let repository: Arc<dyn MemoryRepository> = if sqlite {
                    Arc::new(SqliteMemories {
                        path: dir.join("memory.db"),
                    })
                } else {
                    Arc::new(InMemoryMemories::default())
                };
                let scope = MemoryScope::project(dir.path()).unwrap();
                let mut cfg = config(Arc::new(ScriptProvider::new(vec![])));
                cfg.cwd = dir.path().to_string_lossy().into_owned();
                cfg.system = Some(BASE.into());
                cfg.tools.add(Arc::new(MemoryTool {
                    repository: repository.clone(),
                }));
                let mut agent = Agent::new(cfg);
                agent.set_memory_repository(Some(repository.clone()));
                let old = repository
                    .remember(&scope, "Added after the agent was built.")
                    .unwrap();
                let provider = Arc::new(ScriptProvider::new(vec![
                    Step::Complete(vec![ContentBlock::ToolUse {
                        id: "save".into(),
                        name: "memory".into(),
                        input: json!({"content": SAVED}),
                    }]),
                    Step::Complete(vec![text("saved")]),
                    Step::Complete(vec![ContentBlock::ToolUse {
                        id: "forget".into(),
                        name: "memory".into(),
                        input: json!({"action": "forget", "id": old.id}),
                    }]),
                    Step::Complete(vec![text("forgotten")]),
                    Step::Complete(vec![text("external update seen")]),
                ]));
                agent.set_provider(provider.clone());

                assert_eq!(
                    bounded_run(&mut agent, "save a preference", 3).await.text,
                    "saved"
                );
                assert_eq!(
                    bounded_run(&mut agent, "forget the earlier fact", 3)
                        .await
                        .text,
                    "forgotten"
                );
                repository.remember(&scope, EXTERNAL).unwrap();
                assert_eq!(
                    bounded_run(&mut agent, "check current memory", 3)
                        .await
                        .text,
                    "external update seen"
                );
                provider.assert_exhausted();

                let systems = provider.systems.lock().unwrap();
                assert_eq!(systems.len(), 5);
                for (index, system) in systems.iter().enumerate() {
                    let system = system.as_ref().unwrap();
                    assert_eq!(system.matches("# Learned memory").count(), 1);
                    assert_eq!(system.matches(BASE).count(), 1);
                    assert!(system.ends_with(BASE));
                    for (fact, present) in [
                        (old.content.as_str(), index < 3),
                        (SAVED, index >= 1),
                        (EXTERNAL, index == 4),
                    ] {
                        assert_eq!(
                            system.matches(fact).count(),
                            usize::from(present),
                            "request {index}: {fact}"
                        );
                    }
                }
            }
        }

        #[test]
        fn warnings_fire_at_boundaries_once_and_rearm_after_usage_drops() {
            let mut agent = agent(Arc::new(ScriptProvider::new(vec![])));
            agent.cfg.context_window = 1000;
            let events = capture(&agent);
            for used in [699, 700, 700, 849, 850, 949, 950, 1200] {
                agent.warn_context(used);
            }
            agent.rearm_context_warning(850);
            agent.warn_context(850);
            agent.warn_context(950);
            agent.rearm_context_warning(699);
            agent.warn_context(700);
            let observed: Vec<_> = events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::ContextWarning {
                        used_tokens,
                        context_window,
                        pct,
                        ..
                    } => Some((*used_tokens, *context_window, *pct)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                observed,
                vec![
                    (700, 1000, 70),
                    (850, 1000, 85),
                    (950, 1000, 95),
                    (950, 1000, 95),
                    (700, 1000, 70)
                ]
            );
        }

        #[test]
        fn loading_distinct_histories_clears_stale_usage_without_losing_transcript() {
            let mut agent = agent(Arc::new(ScriptProvider::new(vec![])));
            agent.context_estimate.observe(
                Usage {
                    input_tokens: 9000,
                    ..Usage::default()
                },
                0,
            );
            agent.warned_pct = 95;
            let full = vec![
                Message::user_text("original request"),
                Message::user_text("follow-up"),
            ];
            let working = vec![Message::user_text("saved context")];
            agent.load_histories(full.clone(), working.clone());
            assert_eq!(
                serde_json::to_value(agent.messages()).unwrap(),
                serde_json::to_value(full).unwrap()
            );
            assert_eq!(
                serde_json::to_value(agent.working_messages()).unwrap(),
                serde_json::to_value(&working).unwrap()
            );
            assert_eq!(
                agent.context_usage().0,
                crate::agent::compaction::estimate_history_tokens(&working)
            );
            assert_eq!(agent.warned_pct, 0);
        }

        #[tokio::test]
        async fn cancellation_gate_never_polls_work_when_either_flag_is_already_set() {
            for (own, parent) in [(true, false), (false, true), (true, true)] {
                let own = AtomicBool::new(own);
                let parent = Arc::new(AtomicBool::new(parent));
                let polled = AtomicBool::new(false);
                let result = until_cancelled(&own, Some(&parent), async {
                    polled.store(true, Ordering::Relaxed);
                    42
                })
                .await;
                assert_eq!(result, None);
                assert!(!polled.load(Ordering::Relaxed));
            }
            assert_eq!(
                until_cancelled(&AtomicBool::new(false), None, async { 42 }).await,
                Some(42)
            );
        }

        #[tokio::test(start_paused = true)]
        async fn cancellation_gate_drops_pending_work_for_local_and_parent_cancellation() {
            struct Dropped(Arc<AtomicBool>);
            impl Drop for Dropped {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            for cancel_parent in [false, true] {
                let own = AtomicBool::new(false);
                let parent = Arc::new(AtomicBool::new(false));
                let started = Notify::new();
                let dropped = Arc::new(AtomicBool::new(false));
                let work = async {
                    let _guard = Dropped(dropped.clone());
                    started.notify_one();
                    std::future::pending::<()>().await;
                };
                let (result, ()) = tokio::time::timeout(BOUND, async {
                    tokio::join!(until_cancelled(&own, Some(&parent), work), async {
                        started.notified().await;
                        if cancel_parent {
                            parent.store(true, Ordering::Relaxed);
                        } else {
                            own.store(true, Ordering::Relaxed);
                        }
                    })
                })
                .await
                .expect("pending work must be cancelled");
                assert_eq!(result, None);
                assert!(dropped.load(Ordering::Relaxed));
                assert_eq!(own.load(Ordering::Relaxed), !cancel_parent);
                assert_eq!(parent.load(Ordering::Relaxed), cancel_parent);
            }
        }

        #[tokio::test]
        async fn typed_completion_does_not_parse_status_from_text() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Complete(vec![text(
                "[interrupted] [turn limit reached]",
            )])]));
            let mut agent = agent(provider);
            let outcome = bounded_run(&mut agent, "go", 1).await;
            assert_eq!(outcome.stop_reason, RunStopReason::Completed);
            assert_eq!(outcome.turns, 1);
            assert_eq!(outcome.text, "[interrupted] [turn limit reached]");
            assert!(agent.turn_limit_summary().is_none());
        }

        #[tokio::test]
        async fn typed_runs_preserve_cancel_flags_legacy_clears_only_own() {
            for (name, own, parent) in [
                ("uncancelled root", false, None),
                ("cancelled root", true, None),
                ("uncancelled child", false, Some(false)),
                ("child own cancellation", true, Some(false)),
                ("parent cancellation", false, Some(true)),
                ("both cancellations", true, Some(true)),
            ] {
                let cancelled = own || parent == Some(true);
                let expected_calls = usize::from(!cancelled) + usize::from(parent != Some(true));
                let provider = Arc::new(ScriptProvider::new(
                    (0..expected_calls)
                        .map(|_| Step::Complete(vec![text("done")]))
                        .collect(),
                ));
                let mut agent = agent(provider.clone());
                agent.cancel.store(own, Ordering::Relaxed);
                agent.cfg.parent_cancel = parent.map(|value| Arc::new(AtomicBool::new(value)));
                assert_eq!(
                    cancel_requested(&agent.cancel, agent.cfg.parent_cancel.as_ref()),
                    cancelled,
                    "{name}"
                );
                let outcome = bounded_run(&mut agent, "go", 3).await;
                assert_eq!(
                    outcome.stop_reason,
                    if cancelled {
                        RunStopReason::Cancelled
                    } else {
                        RunStopReason::Completed
                    },
                    "{name}"
                );
                assert_eq!(outcome.turns, u32::from(!cancelled), "{name}");
                assert_eq!(
                    provider.calls.load(Ordering::Relaxed),
                    usize::from(!cancelled),
                    "{name}"
                );
                assert_eq!(agent.cancel.load(Ordering::Relaxed), own, "{name}");
                let result = tokio::time::timeout(BOUND, agent.run("go")).await.unwrap();
                if parent == Some(true) {
                    assert!(
                        result
                            .unwrap_err()
                            .downcast_ref::<RunStopped>()
                            .is_some_and(
                                |stopped| stopped.0.stop_reason == RunStopReason::Cancelled
                            ),
                        "{name}"
                    );
                } else {
                    assert_eq!(result.unwrap(), "done", "{name}");
                }
                assert!(!agent.cancel.load(Ordering::Relaxed), "{name}");
                assert_eq!(
                    agent
                        .cfg
                        .parent_cancel
                        .as_ref()
                        .map(|flag| flag.load(Ordering::Relaxed)),
                    parent,
                    "{name}"
                );
                assert_eq!(
                    provider.calls.load(Ordering::Relaxed),
                    expected_calls,
                    "{name}"
                );
                provider.assert_exhausted();
            }
        }

        #[tokio::test]
        async fn remaining_budget_across_coordination_wakes_never_restarts() {
            let provider = Arc::new(
                ScriptProvider::new(vec![
                    Step::Complete(vec![text("first")]),
                    Step::Complete(vec![tool("second", "missing")]),
                ])
                .with_summary(Summary::Complete),
            );
            let mut agent = agent(provider.clone());
            agent.cfg.max_turns = 2;
            let (inbox, sender) = mailbox();
            agent.cfg.inbox = Some(inbox);
            let mut remaining = agent.turn_budget();
            let first = bounded_run(&mut agent, "go", remaining).await;
            remaining -= first.turns;
            sender
                .send(AgentMessage {
                    kind: crate::agent::team::AgentMessageKind::Actionable,
                    from: "child".into(),
                    text: "finished".into(),
                })
                .unwrap();
            assert!(agent.has_pending_coordination());
            let wake = bounded_run(&mut agent, "", remaining).await;
            remaining -= wake.turns;
            assert_eq!(wake.stop_reason, RunStopReason::TurnLimit);
            assert_eq!(remaining, 0);
            let calls = provider.calls.load(Ordering::Relaxed);
            let zero = bounded_run(&mut agent, "", remaining).await;
            assert_eq!(zero.stop_reason, RunStopReason::TurnLimit);
            assert_eq!(zero.turns, 0);
            assert_eq!(provider.calls.load(Ordering::Relaxed), calls);
            assert_eq!(agent.turn_limit_summary(), Some(zero.text.as_str()));
            assert_eq!(agent.messages().last().unwrap().text(), zero.text);
            assert!(agent
                .messages()
                .iter()
                .any(|m| m.text() == "[message from child]: finished"));
        }

        #[tokio::test]
        async fn coordination_arriving_during_completion_consumes_another_iteration() {
            let provider = Arc::new(ScriptProvider::new(vec![
                Step::Complete(vec![text("first")]),
                Step::Complete(vec![text("synthesized")]),
            ]));
            let mut agent = agent(provider);
            let (inbox, sender) = mailbox();
            agent.cfg.inbox = Some(inbox);
            agent.bus().on(Arc::new(move |event| {
                if matches!(event, AgentEvent::Message { message, .. } if message.text() == "first")
                {
                    sender
                        .send(AgentMessage {
                            kind: crate::agent::team::AgentMessageKind::Actionable,
                            from: "child".into(),
                            text: "result".into(),
                        })
                        .unwrap();
                }
            }));
            let outcome = bounded_run(&mut agent, "go", 2).await;
            assert_eq!(outcome.stop_reason, RunStopReason::Completed);
            assert_eq!(outcome.turns, 2);
            assert_eq!(outcome.text, "synthesized");
        }

        #[tokio::test]
        async fn capped_budget_records_summary_history_events_and_usage_then_resets() {
            let provider = Arc::new(
                ScriptProvider::new(vec![
                    Step::Complete(vec![tool("one", "missing")]),
                    Step::Complete(vec![text("done")]),
                ])
                .with_summary(Summary::Complete),
            );
            let mut agent = agent(provider.clone());
            agent.cfg.max_turns = 1;
            let events = capture(&agent);
            let outcome = bounded_run(&mut agent, "go", 20).await;
            assert_eq!(outcome.turns, 1);
            assert_eq!(outcome.stop_reason, RunStopReason::TurnLimit);
            assert_eq!(outcome.text, "[turn limit reached] saved handoff");
            assert_eq!(agent.turn_limit_summary(), Some(outcome.text.as_str()));
            assert_eq!(agent.history.last().unwrap().text(), outcome.text);
            assert_eq!(agent.messages().last().unwrap().text(), outcome.text);
            assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
            {
                let events = events.lock().unwrap();
                assert_eq!(
                    events
                        .iter()
                        .filter(|e| matches!(e, AgentEvent::Completion { .. }))
                        .count(),
                    2
                );
                assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::Message { message, .. } if message.text() == outcome.text)).count(), 1);
                let usage = events
                    .iter()
                    .find_map(|e| match e {
                        AgentEvent::TurnEnd { usage, .. } => Some(usage),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(usage.input_tokens, 20);
                assert_eq!(usage.output_tokens, 10);
                assert_eq!(usage.cache_creation_input_tokens, 4);
                assert_eq!(usage.cache_read_input_tokens, 6);
            }
            assert_eq!(agent.run("finish").await.unwrap(), "done");
            assert!(agent.turn_limit_summary().is_none());
            assert_eq!(provider.calls.load(Ordering::Relaxed), 3);
            provider.assert_exhausted();
        }

        #[tokio::test]
        async fn zero_budget_never_calls_provider_and_records_a_local_stop() {
            let provider = Arc::new(ScriptProvider::new(vec![]));
            let mut agent = agent(provider.clone());
            let events = capture(&agent);
            let outcome = bounded_run(&mut agent, "go", 0).await;
            assert_eq!(outcome.stop_reason, RunStopReason::TurnLimit);
            assert_eq!(outcome.turns, 0);
            assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
            assert_eq!(agent.history.last().unwrap().text(), outcome.text);
            assert_eq!(agent.turn_limit_summary(), Some(outcome.text.as_str()));
            let events = events.lock().unwrap();
            assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Message { message, .. } if message.text() == outcome.text)
        ));
            assert!(!events
                .iter()
                .any(|e| matches!(e, AgentEvent::Completion { .. })));
        }

        #[tokio::test]
        async fn drain_coordination_retains_late_reports_without_provider_or_budget() {
            let provider = Arc::new(ScriptProvider::new(vec![]));
            let mut agent = agent(provider.clone());
            let events = capture(&agent);
            let (inbox, sender) = mailbox();
            agent.cfg.inbox = Some(inbox);
            let outcome = bounded_run(&mut agent, "go", 0).await;
            agent.cancel.store(true, Ordering::Relaxed);
            sender
                .send(AgentMessage {
                    kind: crate::agent::team::AgentMessageKind::Actionable,
                    from: "child".into(),
                    text: "partial result".into(),
                })
                .unwrap();
            agent.drain_coordination();
            agent.drain_coordination();
            assert!(!agent.has_outstanding_coordination());
            assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
            assert_eq!(
                agent.history.last().unwrap().text(),
                "[message from child]: partial result"
            );
            assert_eq!(
                agent.messages().last().unwrap().text(),
                "[message from child]: partial result"
            );
            assert_eq!(agent.turn_limit_summary(), Some(outcome.text.as_str()));
            assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(
                    |e| matches!(e, AgentEvent::AgentMessage { text, .. } if text == "partial result")
                )
                .count(),
            1
        );
        }

        #[tokio::test]
        async fn cancellation_before_tool_dispatch_still_emits_paired_results() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Complete(vec![tool(
                "one", "later",
            )])]));
            let mut agent = agent(provider);
            let (_, calls) = add_tool(&mut agent, "later", false, false);
            let events = capture(&agent);
            let cancel = agent.cancel_handle();
            agent.bus().on(Arc::new(move |event| {
                if matches!(event, AgentEvent::Message { .. }) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }));
            let outcome = bounded_run(&mut agent, "go", 1).await;
            assert_eq!(outcome.stop_reason, RunStopReason::Cancelled);
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(
                matches!(&agent.messages().last().unwrap().content[0], ContentBlock::ToolResult { tool_use_id, is_error: Some(true), .. } if tool_use_id == "one")
            );
            assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(
                    |e| matches!(e, AgentEvent::ToolResult { tool_use_id, .. } if tool_use_id == "one")
                )
                .count(),
            1
        );
        }

        #[tokio::test]
        async fn cancellation_interrupts_provider_startup() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Pending]));
            let mut agent = agent(provider.clone());
            let cancel = agent.cancel_handle();
            let (outcome, ()) = tokio::join!(bounded_run(&mut agent, "go", 3), async {
                provider.started.notified().await;
                cancel.store(true, Ordering::Relaxed);
            });
            assert_eq!(outcome.stop_reason, RunStopReason::Cancelled);
            assert_eq!(outcome.turns, 1);
            assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        }

        #[tokio::test]
        async fn cancellation_preserves_partial_stream_in_history_and_events() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Partial]));
            let mut agent = agent(provider);
            let events = capture(&agent);
            let cancel = agent.cancel_handle();
            agent.bus().on(Arc::new(move |event| {
                if matches!(event, AgentEvent::TextDelta { .. }) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }));
            let outcome = bounded_run(&mut agent, "go", 3).await;
            assert_eq!(outcome.stop_reason, RunStopReason::Cancelled);
            assert_eq!(outcome.text, "partial answer");
            assert_eq!(agent.history.last().unwrap().text(), outcome.text);
            assert_eq!(agent.messages().last().unwrap().text(), outcome.text);
            assert!(events.lock().unwrap().iter().any(
            |e| matches!(e, AgentEvent::Message { message, .. } if message.text() == outcome.text)
        ));
        }

        fn failed_stream(partial: &str, error: &str) -> Step {
            Step::Events(vec![
                StreamEvent::TextDelta {
                    text: partial.into(),
                },
                StreamEvent::Error {
                    message: error.into(),
                },
            ])
        }

        fn assert_recorded_text(agent: &Agent, events: &[AgentEvent], expected: &str) {
            for history in [&agent.history[..], agent.messages()] {
                assert_eq!(history.len(), 2);
                assert_eq!(history[0].role, Role::User);
                assert_eq!(history[1].role, Role::Assistant);
                assert!(
                    matches!(&history[1].content[..], [ContentBlock::Text { text }] if text == expected)
                );
            }
            let messages: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::Message { message, .. } => Some(message),
                    _ => None,
                })
                .collect();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].role, Role::Assistant);
            assert!(
                matches!(&messages[0].content[..], [ContentBlock::Text { text }] if text == expected)
            );
            assert!(!events.iter().any(|event| matches!(
                event,
                AgentEvent::ToolCall { .. } | AgentEvent::ToolResult { .. }
            )));
        }

        fn assert_clean_retry_requests(provider: &ScriptProvider, expected_calls: usize) {
            assert_eq!(provider.calls.load(Ordering::Relaxed), expected_calls);
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), expected_calls);
            for messages in requests.iter() {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].role, Role::User);
                assert_eq!(messages[0].text(), "go");
            }
        }

        #[tokio::test]
        async fn fatal_stream_error_preserves_only_partial_text_once() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Events(vec![
                StreamEvent::TextDelta {
                    text: "partial ".into(),
                },
                StreamEvent::ToolUseStart {
                    id: "unfinished".into(),
                    name: "missing".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    id: "unfinished".into(),
                    partial_json: "{\"x\":".into(),
                },
                StreamEvent::TextDelta {
                    text: "answer".into(),
                },
                StreamEvent::Error {
                    message: "invalid api key".into(),
                },
            ])]));
            let mut agent = agent(provider.clone());
            let events = capture(&agent);
            let error = tokio::time::timeout(BOUND, agent.run_with_budget("go", 3))
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.to_string(), "invalid api key");
            assert_clean_retry_requests(&provider, 1);
            let events = events.lock().unwrap();
            assert_recorded_text(&agent, &events, "partial answer");
            assert!(!events
                .iter()
                .any(|event| matches!(event, AgentEvent::StreamRetry { .. })));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
                    .count(),
                1
            );
        }

        #[tokio::test]
        async fn closed_stream_retries_boundedly_and_preserves_only_final_partial_text() {
            let provider = Arc::new(ScriptProvider::new(
                (0..11).map(|_| Step::Partial).collect(),
            ));
            let mut agent = agent(provider.clone());
            agent.skip_retry_backoff = true;
            let events = capture(&agent);
            let error = tokio::time::timeout(BOUND, agent.run_with_budget("go", 3))
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "provider request failed after 10 retries: stream ended without completion"
            );
            assert_clean_retry_requests(&provider, 11);
            let events = events.lock().unwrap();
            assert_recorded_text(&agent, &events, "partial answer");
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::StreamRetry { .. }))
                    .count(),
                10
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
                    .count(),
                1
            );
        }

        #[tokio::test]
        async fn transient_partial_stream_then_success_records_only_completed_message() {
            let provider = Arc::new(ScriptProvider::new(vec![
                failed_stream("discarded partial", "HTTP 503"),
                Step::Events(vec![
                    StreamEvent::TextDelta {
                        text: "complete answer".into(),
                    },
                    StreamEvent::MessageStop {
                        completion: completion(vec![text("complete answer")]),
                    },
                ]),
            ]));
            let mut agent = agent(provider.clone());
            agent.skip_retry_backoff = true;
            let events = capture(&agent);
            let outcome = bounded_run(&mut agent, "go", 3).await;
            assert_eq!(outcome.stop_reason, RunStopReason::Completed);
            assert_eq!(outcome.turns, 1);
            assert_eq!(outcome.text, "complete answer");
            assert_clean_retry_requests(&provider, 2);
            let events = events.lock().unwrap();
            assert_recorded_text(&agent, &events, "complete answer");
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::StreamRetry { .. }))
                    .count(),
                1
            );
        }

        #[tokio::test]
        async fn exhausted_transient_retries_record_only_last_partial_once() {
            for final_startup_error in [false, true] {
                let mut steps: Vec<_> = (1..=10)
                    .map(|attempt| failed_stream(&format!("partial attempt {attempt}"), "HTTP 503"))
                    .collect();
                steps.push(if final_startup_error {
                    Step::Transient
                } else {
                    failed_stream("partial attempt 11", "HTTP 503")
                });
                let provider = Arc::new(ScriptProvider::new(steps));
                let mut agent = agent(provider.clone());
                agent.skip_retry_backoff = true;
                let events = capture(&agent);
                let error = tokio::time::timeout(BOUND, agent.run_with_budget("go", 3))
                    .await
                    .unwrap()
                    .unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "provider request failed after 10 retries: HTTP 503"
                );
                assert_clean_retry_requests(&provider, 11);
                let events = events.lock().unwrap();
                let expected = if final_startup_error {
                    "partial attempt 10"
                } else {
                    "partial attempt 11"
                };
                assert_recorded_text(&agent, &events, expected);
                let retries: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        AgentEvent::StreamRetry {
                            attempt,
                            max_attempts,
                            ..
                        } => Some((*attempt, *max_attempts)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    retries,
                    (1..=10).map(|attempt| (attempt, 10)).collect::<Vec<_>>()
                );
                assert!(matches!(events.last(), Some(AgentEvent::TurnEnd { .. })));
                assert!(matches!(
                    events.get(events.len() - 2),
                    Some(AgentEvent::Message { .. })
                ));
            }
        }

        #[tokio::test]
        async fn fatal_retry_without_text_does_not_record_a_discarded_attempt() {
            let provider = Arc::new(ScriptProvider::new(vec![
                failed_stream("discarded partial", "HTTP 503"),
                Step::Events(vec![StreamEvent::Error {
                    message: "invalid api key".into(),
                }]),
            ]));
            let mut agent = agent(provider.clone());
            agent.skip_retry_backoff = true;
            let events = capture(&agent);
            let error = tokio::time::timeout(BOUND, agent.run_with_budget("go", 3))
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.to_string(), "invalid api key");
            assert_clean_retry_requests(&provider, 2);
            assert_eq!(agent.history.len(), 1);
            assert_eq!(agent.messages().len(), 1);
            assert!(!events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, AgentEvent::Message { .. })));
        }

        struct TestTool {
            name: &'static str,
            read_only: bool,
            pending: bool,
            started: Arc<Notify>,
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl Tool for TestTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.name.into(),
                    description: "test".into(),
                    input_schema: json!({"type":"object"}),
                }
            }
            fn is_read_only(&self) -> bool {
                self.read_only
            }
            async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
                self.calls.fetch_add(1, Ordering::Relaxed);
                self.started.notify_one();
                if self.pending {
                    std::future::pending().await
                } else {
                    Ok("completed read".into())
                }
            }
        }

        fn add_tool(
            agent: &mut Agent,
            name: &'static str,
            read_only: bool,
            pending: bool,
        ) -> (Arc<Notify>, Arc<AtomicUsize>) {
            let started = Arc::new(Notify::new());
            let calls = Arc::new(AtomicUsize::new(0));
            agent.cfg.tools.add(Arc::new(TestTool {
                name,
                read_only,
                pending,
                started: started.clone(),
                calls: calls.clone(),
            }));
            (started, calls)
        }

        #[tokio::test]
        async fn reported_usage_can_trigger_compaction_before_the_local_estimate() {
            let provider = Arc::new(
                ScriptProvider::new(vec![Step::Complete(vec![text("done")])])
                    .with_summary(Summary::Complete),
            );
            let mut agent = agent(provider);
            agent.cfg.context_window = 8192;
            agent.load_history(vec![Message {
                role: Role::Assistant,
                content: vec![text(&"old work ".repeat(1000))],
            }]);
            let estimated = agent.context_usage().0;
            agent.context_estimate.observe(
                Usage {
                    input_tokens: 7200,
                    ..Usage::default()
                },
                estimated,
            );
            let events = capture(&agent);
            assert_eq!(agent.run("continue").await.unwrap(), "done");
            assert!(events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, AgentEvent::Compaction { .. })));
            assert!(agent.context_usage().0 < 7200);
        }

        #[tokio::test]
        async fn manual_compaction_emits_event_even_when_message_count_is_unchanged() {
            let mut agent = agent(Arc::new(
                ScriptProvider::new(vec![]).with_summary(Summary::Complete),
            ));
            let original = vec![Message {
                role: Role::Assistant,
                content: vec![text(&"old work ".repeat(2000))],
            }];
            agent.load_history(original.clone());
            let events = capture(&agent);
            let (did, before, after) = agent.compact().await.unwrap();
            assert!(did);
            assert!(after < before);
            assert_eq!(agent.working_messages().len(), original.len());
            assert_eq!(
                serde_json::to_value(agent.messages()).unwrap(),
                serde_json::to_value(&original).unwrap()
            );
            assert!(matches!(
                events.lock().unwrap().as_slice(),
                [AgentEvent::Compaction {
                    replaced_upto: 1,
                    ..
                }]
            ));
            assert!(agent.context_usage().0 < before);
        }

        #[tokio::test]
        async fn failed_auto_compaction_preserves_history_and_never_sends_oversized_request() {
            for (name, fail_at) in [("first chunk", 1), ("after successful chunk", 2)] {
                let mut script = ScriptProvider::new(vec![]);
                if fail_at == 2 {
                    script = script.with_summary(Summary::Complete);
                }
                let provider = Arc::new(script.with_summary(Summary::Error));
                let mut agent = agent(provider.clone());
                agent.cfg.context_window = 4000;
                let mut original = vec![Message {
                    role: Role::Assistant,
                    content: vec![text(&"old work ".repeat(3000))],
                }];
                agent.load_history(original.clone());
                original.push(Message::user_text("continue"));
                let events = capture(&agent);
                let error = agent.run("continue").await.unwrap_err().to_string();
                assert!(error.contains("compaction failed"), "{name}: {error}");
                assert!(error.contains("HTTP 401"), "{name}: {error}");
                for history in [agent.messages(), agent.working_messages()] {
                    assert_eq!(
                        serde_json::to_value(history).unwrap(),
                        serde_json::to_value(&original).unwrap(),
                        "{name}"
                    );
                }
                assert_eq!(provider.calls.load(Ordering::Relaxed), fail_at, "{name}");
                provider.assert_exhausted();
                assert!(provider.requests.lock().unwrap().is_empty());
                assert!(!events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event, AgentEvent::Compaction { .. })));
            }
        }

        #[tokio::test]
        async fn cancellation_during_manual_compaction_is_atomic_and_retryable() {
            let provider = Arc::new(ScriptProvider::new(vec![]).with_summary(Summary::Pending));
            let mut agent = agent(provider.clone());
            agent.load_history(vec![Message {
                role: Role::Assistant,
                content: vec![text(&"old work ".repeat(2000))],
            }]);
            let original = serde_json::to_value(agent.messages()).unwrap();
            let events = capture(&agent);
            let cancel = agent.cancel_handle();
            let (result, ()) = tokio::time::timeout(BOUND, async {
                tokio::join!(agent.compact(), async {
                    provider.started.notified().await;
                    cancel.store(true, Ordering::Relaxed);
                })
            })
            .await
            .unwrap();
            assert!(result.unwrap_err().to_string().contains("cancelled"));
            assert_eq!(
                serde_json::to_value(agent.working_messages()).unwrap(),
                original
            );
            assert_eq!(serde_json::to_value(agent.messages()).unwrap(), original);
            assert!(events.lock().unwrap().is_empty());
            agent.set_provider(Arc::new(
                ScriptProvider::new(vec![]).with_summary(Summary::Complete),
            ));
            assert!(agent.compact().await.unwrap().0);
        }

        #[tokio::test]
        async fn overflow_recovery_compacts_once_and_retries_smaller_context() {
            for streaming_error in [false, true] {
                let first = if streaming_error {
                    Step::Events(vec![StreamEvent::Error {
                        message: "Your input exceeds the context window of this model.".into(),
                    }])
                } else {
                    Step::Overflow
                };
                let provider = Arc::new(
                    ScriptProvider::new(vec![first, Step::Complete(vec![text("done")])])
                        .with_summary(Summary::Complete),
                );
                let mut agent = agent(provider.clone());
                agent.load_history(vec![Message {
                    role: Role::Assistant,
                    content: vec![text(&"old work ".repeat(2000))],
                }]);
                let events = capture(&agent);
                assert_eq!(agent.run("continue").await.unwrap(), "done");
                let requests = provider.requests.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert!(
                    crate::agent::compaction::estimate_history_tokens(&requests[1])
                        < crate::agent::compaction::estimate_history_tokens(&requests[0])
                );
                assert_eq!(requests[1].last().unwrap().text(), "continue");
                assert_eq!(agent.messages().len(), 3);
                assert_eq!(
                    events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|event| matches!(event, AgentEvent::Compaction { .. }))
                        .count(),
                    1
                );
            }
        }

        #[tokio::test]
        async fn repeated_overflow_is_bounded_without_retrying_unchanged_context() {
            let provider = Arc::new(
                ScriptProvider::new(vec![Step::Overflow, Step::Overflow])
                    .with_summary(Summary::Complete),
            );
            let mut agent = agent(provider.clone());
            agent.load_history(vec![Message {
                role: Role::Assistant,
                content: vec![text(&"old work ".repeat(2000))],
            }]);
            assert!(agent
                .run("continue")
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds the context window"));
            assert_eq!(provider.requests.lock().unwrap().len(), 2);

            let provider = Arc::new(ScriptProvider::new(vec![Step::Overflow]));
            let mut short = self::agent(provider.clone());
            assert!(short
                .run("hello")
                .await
                .unwrap_err()
                .to_string()
                .contains("no history could be compacted"));
            assert_eq!(provider.requests.lock().unwrap().len(), 1);
        }

        #[test]
        fn context_warning_does_not_wrap_above_255_percent() {
            let mut agent = agent(Arc::new(ScriptProvider::new(vec![])));
            let events = capture(&agent);
            agent.warn_context(6_400_000);
            assert!(matches!(
                events.lock().unwrap().as_slice(),
                [AgentEvent::ContextWarning {
                    used_tokens: 6_400_000,
                    pct: 95,
                    ..
                }]
            ));
        }

        #[tokio::test]
        async fn cancellation_of_pending_mutation_pairs_all_calls_and_skips_later_tools() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Complete(vec![
                tool("one", "pending"),
                tool("two", "later"),
            ])]));
            let mut agent = agent(provider);
            let (started, _) = add_tool(&mut agent, "pending", false, true);
            let (_, later_calls) = add_tool(&mut agent, "later", false, false);
            let events = capture(&agent);
            let cancel = agent.cancel_handle();
            let (outcome, ()) = tokio::join!(bounded_run(&mut agent, "go", 3), async {
                started.notified().await;
                cancel.store(true, Ordering::Relaxed);
            });
            assert_eq!(outcome.stop_reason, RunStopReason::Cancelled);
            assert_eq!(later_calls.load(Ordering::Relaxed), 0);
            let results = &agent.messages().last().unwrap().content;
            assert_eq!(results.len(), 2);
            assert!(results.iter().all(|r| matches!(
                r,
                ContentBlock::ToolResult {
                    is_error: Some(true),
                    ..
                }
            )));
            let events = events.lock().unwrap();
            for id in ["one", "two"] {
                assert_eq!(
                events
                    .iter()
                    .filter(
                        |e| matches!(e, AgentEvent::ToolCall { tool_use_id, .. } if tool_use_id == id)
                    )
                    .count(),
                1
            );
                assert_eq!(
                events
                    .iter()
                    .filter(
                        |e| matches!(e, AgentEvent::ToolResult { tool_use_id, .. } if tool_use_id == id)
                    )
                    .count(),
                1
            );
            }
        }

        #[tokio::test]
        async fn cancellation_preserves_completed_concurrent_reads_without_duplicate_results() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Complete(vec![
                tool("pending", "pending"),
                tool("done", "done"),
                tool("later", "later"),
            ])]));
            let mut agent = agent(provider);
            let (_, pending_calls) = add_tool(&mut agent, "pending", true, true);
            add_tool(&mut agent, "done", true, false);
            let (_, later_calls) = add_tool(&mut agent, "later", false, false);
            let events = capture(&agent);
            let cancel = agent.cancel_handle();
            agent.bus().on(Arc::new(move |event| {
            if matches!(event, AgentEvent::ToolResult { tool_use_id, .. } if tool_use_id == "done") {
                cancel.store(true, Ordering::Relaxed);
            }
        }));
            let outcome = bounded_run(&mut agent, "go", 3).await;
            assert_eq!(outcome.stop_reason, RunStopReason::Cancelled);
            assert_eq!(pending_calls.load(Ordering::Relaxed), 1);
            assert_eq!(later_calls.load(Ordering::Relaxed), 0);
            let results = &agent.messages().last().unwrap().content;
            assert_eq!(results.len(), 3);
            assert!(
                matches!(&results[1], ContentBlock::ToolResult { tool_use_id, content, is_error: Some(false) } if tool_use_id == "done" && content.as_text() == Some("completed read"))
            );
            assert!(
                matches!(&results[0], ContentBlock::ToolResult { tool_use_id, is_error: Some(true), .. } if tool_use_id == "pending")
            );
            let events = events.lock().unwrap();
            for id in ["pending", "done", "later"] {
                assert_eq!(
                events
                    .iter()
                    .filter(
                        |e| matches!(e, AgentEvent::ToolResult { tool_use_id, .. } if tool_use_id == id)
                    )
                    .count(),
                1
            );
            }
        }

        #[tokio::test]
        async fn cancellation_during_retry_backoff_does_not_start_another_request() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Transient]));
            let mut agent = agent(provider.clone());
            let cancel = agent.cancel_handle();
            agent.bus().on(Arc::new(move |event| {
                if matches!(event, AgentEvent::StreamRetry { .. }) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }));
            let outcome = bounded_run(&mut agent, "go", 3).await;
            assert_eq!(outcome.stop_reason, RunStopReason::Cancelled);
            assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        }

        #[tokio::test]
        async fn cancellation_interrupts_an_already_pending_backoff() {
            let mut agent = agent(Arc::new(ScriptProvider::new(vec![])));
            let cancel = agent.cancel_handle();
            let started = Notify::new();
            tokio::time::timeout(BOUND, async {
                tokio::join!(
                    async {
                        started.notify_one();
                        agent.backoff().await;
                    },
                    async {
                        started.notified().await;
                        cancel.store(true, Ordering::Relaxed);
                    }
                );
            })
            .await
            .expect("cancel must interrupt the ten-second retry delay");
            assert!(agent.is_cancelled());
            assert_eq!(
                bounded_run(&mut agent, "", 0).await.stop_reason,
                RunStopReason::Cancelled
            );
        }

        #[tokio::test]
        async fn cancellation_interrupts_wind_down_and_keeps_completed_tool_results() {
            let provider = Arc::new(
                ScriptProvider::new(vec![Step::Complete(vec![tool("one", "missing")])])
                    .with_summary(Summary::Pending),
            );
            let mut agent = agent(provider.clone());
            let events = capture(&agent);
            let cancel = agent.cancel_handle();
            let (outcome, ()) = tokio::join!(bounded_run(&mut agent, "go", 1), async {
                provider.started.notified().await;
                cancel.store(true, Ordering::Relaxed);
            });
            assert_eq!(outcome.stop_reason, RunStopReason::Cancelled);
            assert_eq!(outcome.turns, 1);
            assert!(agent.turn_limit_summary().is_none());
            assert_eq!(agent.messages().last().unwrap().role, Role::Tool);
            assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| matches!(e, AgentEvent::Completion { .. }))
                    .count(),
                1
            );
        }
    }

    mod structured {
        use super::*;
        use crate::test_support::agent::{config, text, tool, ScriptProvider, Step, Summary};
        use serde_json::json;
        use std::time::Duration;

        #[tokio::test]
        async fn structured_retry_does_not_restart_an_exhausted_budget() {
            let provider = Arc::new(
                ScriptProvider::new(vec![Step::Complete(vec![tool("missing", "missing")])])
                    .with_summary(Summary::Complete),
            );
            let mut cfg = config(provider.clone());
            cfg.max_turns = 1;
            let mut agent = Agent::new(cfg);
            assert!(agent
                .run_structured("keep going", json!({"type":"object"}))
                .await
                .is_err());
            assert!(agent.turn_limit_summary().is_some());
            assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
            provider.assert_exhausted();
            assert!(agent
                .tool_specs()
                .iter()
                .all(|tool| tool.name != "structured_output"));
        }

        #[tokio::test]
        async fn structured_run_captures_the_tool_call() {
            let schema = json!({
                "type": "object",
                "required": ["answer"],
                "properties": { "answer": {"type": "string"} }
            });
            let provider = Arc::new(ScriptProvider::new(vec![
                Step::Complete(vec![ContentBlock::ToolUse {
                    id: "answer".into(),
                    name: "structured_output".into(),
                    input: json!({"answer":"42"}),
                }]),
                Step::Complete(vec![text("done")]),
            ]));
            let mut agent = Agent::new(config(provider.clone()));
            let out = agent
                .run_structured("what is the answer?", schema)
                .await
                .unwrap();
            assert_eq!(out, json!({"answer": "42"}));
            provider.assert_exhausted();
            assert!(agent
                .tool_specs()
                .iter()
                .all(|tool| tool.name != "structured_output"));
        }

        #[tokio::test]
        async fn structured_run_errors_when_never_called() {
            let provider = Arc::new(ScriptProvider::new(vec![
                Step::Complete(vec![text("hi")]),
                Step::Complete(vec![text("still no result")]),
            ]));
            let mut agent = Agent::new(config(provider.clone()));
            assert!(agent
                .run_structured("go", json!({"type":"object", "required":["answer"]}))
                .await
                .is_err());
            provider.assert_exhausted();
            assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
            assert!(agent
                .tool_specs()
                .iter()
                .all(|tool| tool.name != "structured_output"));
        }

        #[tokio::test]
        async fn cancel_is_observed_promptly_while_the_stream_is_parked() {
            let provider = Arc::new(ScriptProvider::new(vec![Step::Parked]));
            let mut agent = Agent::new(config(provider.clone()));
            let cancel = agent.cancel_handle();
            let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(agent.run("go"), async {
                    provider.started.notified().await;
                    cancel.store(true, Ordering::Relaxed);
                })
            })
            .await
            .expect("cancel must break the parked stream promptly");
            assert!(result
                .unwrap_err()
                .downcast_ref::<RunStopped>()
                .is_some_and(|stopped| stopped.0.stop_reason == RunStopReason::Cancelled));
            assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
            provider.assert_exhausted();
        }
    }

    mod reliability {
        use super::run::{agent, bounded_run, capture, BOUND};
        use super::*;
        use crate::agent::team::mailbox;
        use crate::core::types::ToolSpec;
        use crate::test_support::agent::{completion, config, text, tool, ScriptProvider, Step};
        use crate::tools::registry::{Tool, ToolResult};
        use async_trait::async_trait;
        use serde_json::{json, Value};
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use tokio::sync::Notify;

        #[tokio::test]
        async fn read_loader_context_survives_dispatch_and_hook_replacement() {
            struct ReplaceRead;
            #[async_trait]
            impl crate::agent::hooks::PostToolUse for ReplaceRead {
                async fn on_post_tool(
                    &self,
                    _: &str,
                    output: ToolOutput,
                    is_error: bool,
                ) -> ToolOutput {
                    assert!(!is_error);
                    assert!(matches!(
                        output,
                        ToolOutput::Read {
                            loader_context: Some(_),
                            ..
                        }
                    ));
                    "replacement output".into()
                }
            }
            for case in ["valid", "malformed", "oversized", "replaced"] {
                let dir = crate::test_support::TestDir::new();
                std::fs::create_dir(dir.join(".git")).unwrap();
                std::fs::write(dir.join("file.rs"), "source body").unwrap();
                let instructions = match case {
                    "malformed" => vec![0xff],
                    "oversized" => vec![b'x'; 64 * 1024 + 1],
                    _ => b"Scoped project convention".to_vec(),
                };
                std::fs::write(dir.join("AGENTS.md"), instructions).unwrap();
                let provider = Arc::new(ScriptProvider::new(vec![
                    Step::Complete(vec![ContentBlock::ToolUse {
                        id: "read".into(),
                        name: "read_file".into(),
                        input: json!({"path":"file.rs"}),
                    }]),
                    Step::Complete(vec![text("done")]),
                ]));
                let mut cfg = config(provider.clone());
                cfg.cwd = dir.path().to_string_lossy().into_owned();
                cfg.tools.add(Arc::new(crate::tools::builtin::ReadFileTool));
                if case == "replaced" {
                    cfg.hooks.post.push(Arc::new(ReplaceRead));
                }
                let mut agent = Agent::new(cfg);
                let events = capture(&agent);
                assert_eq!(
                    bounded_run(&mut agent, "read source", 3).await.stop_reason,
                    RunStopReason::Completed
                );
                let ContentBlock::ToolResult {
                    content,
                    is_error: Some(false),
                    ..
                } = &agent.messages()[2].content[0]
                else {
                    panic!("successful read result expected")
                };
                if case == "replaced" {
                    assert_eq!(content.model_text(), "replacement output");
                } else {
                    let ToolOutput::Read {
                        body,
                        loader_context: Some(context),
                    } = content
                    else {
                        panic!("typed read expected")
                    };
                    assert_eq!(body, "     1\tsource body");
                    assert!(context.contains(if case == "valid" {
                        "Scoped project convention"
                    } else {
                        "[!] Auxiliary project instructions could not be loaded"
                    }));
                    assert!(content.model_text().contains(context));
                }
                let events = events.lock().unwrap();
                let output = events
                    .iter()
                    .find_map(|event| match event {
                        AgentEvent::ToolResult {
                            output,
                            is_error: false,
                            ..
                        } => Some(output),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(output, content);
                let requests = provider.requests.lock().unwrap();
                assert!(
                    matches!(&requests[1][2].content[0], ContentBlock::ToolResult { content: sent, is_error: Some(false), .. } if sent == content)
                );
                provider.assert_exhausted();
            }
        }

        struct ReliabilityTool {
            name: &'static str,
            read_only: bool,
            panics: bool,
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl Tool for ReliabilityTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.name.into(),
                    description: String::new(),
                    input_schema: json!({"type":"object"}),
                }
            }
            fn is_read_only(&self) -> bool {
                self.read_only
            }
            async fn execute(&self, _: Value, ctx: &ToolContext) -> ToolResult {
                tokio::task::yield_now().await;
                self.calls.fetch_add(1, Ordering::SeqCst);
                if !self.read_only {
                    std::fs::write(std::path::Path::new(&ctx.cwd).join(self.name), "committed")?;
                }
                assert!(!self.panics, "simulated tool panic after work");
                Ok("known success".into())
            }
        }

        #[tokio::test]
        async fn tool_panics_preserve_siblings_and_never_replay_mutations() {
            for read_only in [true, false] {
                let dir = crate::test_support::TestDir::new();
                let calls = Arc::new(AtomicUsize::new(0));
                let skipped = Arc::new(AtomicUsize::new(0));
                let provider = Arc::new(ScriptProvider::new(vec![Step::Complete(vec![
                    tool("ok", "read_ok"),
                    tool("panic", "panic_tool"),
                    tool("skip", "unstarted"),
                ])]));
                let mut cfg = config(provider.clone());
                cfg.cwd = dir.path().to_string_lossy().into_owned();
                for (name, readonly, panics, count) in [
                    ("read_ok", true, false, Arc::new(AtomicUsize::new(0))),
                    ("panic_tool", read_only, true, calls.clone()),
                    ("unstarted", false, false, skipped.clone()),
                ] {
                    cfg.tools.add(Arc::new(ReliabilityTool {
                        name,
                        read_only: readonly,
                        panics,
                        calls: count,
                    }));
                }
                let mut agent = Agent::new(cfg);
                let events = capture(&agent);
                let outcome = bounded_run(&mut agent, "go", 3).await;
                assert_eq!(outcome.stop_reason, RunStopReason::ToolPanic);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(skipped.load(Ordering::SeqCst), 0);
                assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
                if !read_only {
                    assert_eq!(
                        std::fs::read_to_string(dir.join("panic_tool")).unwrap(),
                        "committed"
                    );
                }
                let results = &agent.messages()[2].content;
                assert_eq!(results.len(), 3);
                for (block, id, error) in [
                    (&results[0], "ok", false),
                    (&results[1], "panic", true),
                    (&results[2], "skip", true),
                ] {
                    assert!(
                        matches!(block, ContentBlock::ToolResult { tool_use_id, is_error: Some(actual), .. } if tool_use_id == id && *actual == error)
                    );
                }
                assert!(
                    matches!(&results[0], ContentBlock::ToolResult { content, .. } if content.model_text() == "known success")
                );
                let events = events.lock().unwrap();
                assert_eq!(
                    events
                        .iter()
                        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
                        .count(),
                    3
                );
                assert!(matches!(events.last(), Some(AgentEvent::TurnEnd { .. })));
                provider.assert_exhausted();
            }
        }

        struct HangingPostHook(Arc<Notify>);
        #[async_trait]
        impl crate::agent::hooks::PostToolUse for HangingPostHook {
            async fn on_post_tool(&self, _: &str, _: ToolOutput, _: bool) -> ToolOutput {
                self.0.notify_one();
                std::future::pending().await
            }
        }

        #[tokio::test]
        async fn cancellation_during_posthook_preserves_committed_result_in_root_and_child() {
            for child_stop in [false, true] {
                let dir = crate::test_support::TestDir::new();
                let entered = Arc::new(Notify::new());
                let provider = Arc::new(ScriptProvider::new(vec![Step::Complete(vec![tool(
                    "write",
                    "committed_file",
                )])]));
                let mut cfg = config(provider.clone());
                cfg.cwd = dir.path().to_string_lossy().into_owned();
                cfg.tools.add(Arc::new(ReliabilityTool {
                    name: "committed_file",
                    read_only: false,
                    panics: false,
                    calls: Arc::new(AtomicUsize::new(0)),
                }));
                cfg.hooks
                    .post
                    .push(Arc::new(HangingPostHook(entered.clone())));
                let mut agent = Agent::new(cfg);
                let cancel = agent.cancel_handle();
                let events = capture(&agent);
                let team = crate::agent::team::AgentRegistry::new();
                let (_, tx) = mailbox();
                let handle = team.register("child".into(), 1, "root".into(), tx);
                let work = async {
                    if child_stop {
                        handle
                            .run_until_stopped(
                                Arc::new(AtomicBool::new(false)),
                                cancel.clone(),
                                agent.run_with_budget("go", 3),
                            )
                            .await
                            .unwrap()
                    } else {
                        agent.run_with_budget("go", 3).await
                    }
                };
                let (result, ()) = tokio::time::timeout(BOUND, async {
                    tokio::join!(work, async {
                        entered.notified().await;
                        if child_stop {
                            team.stop("root", "child").unwrap();
                        } else {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    })
                })
                .await
                .unwrap();
                assert_eq!(result.unwrap().stop_reason, RunStopReason::Cancelled);
                assert_eq!(
                    std::fs::read_to_string(dir.join("committed_file")).unwrap(),
                    "committed"
                );
                assert!(
                    matches!(&agent.messages()[2].content[0], ContentBlock::ToolResult { content, is_error: Some(false), .. } if content.model_text() == "known success")
                );
                assert!(events.lock().unwrap().iter().any(|e| matches!(e, AgentEvent::ToolResult { output, is_error: false, .. } if output.model_text() == "known success")));
                provider.assert_exhausted();
            }
        }

        #[tokio::test]
        async fn inline_task_preserves_child_commit_on_cancel_and_propagates_panic() {
            for panics in [false, true] {
                let dir = crate::test_support::TestDir::new();
                let cancel = Arc::new(AtomicBool::new(false));
                let entered = Arc::new(Notify::new());
                let child_provider = Arc::new(ScriptProvider::new(vec![
                    Step::Complete(vec![tool("commit", "child_commit")]),
                    Step::Complete(vec![text("successful sibling")]),
                ]));
                let provider = Arc::new(ScriptProvider::new(vec![Step::Complete(vec![
                    ContentBlock::ToolUse {
                        id: "task-call".into(),
                        name: "task".into(),
                        input: json!({"tasks":[
                            {"description":"first", "prompt":"commit"},
                            {"description":"second", "prompt":"answer"},
                        ]}),
                    },
                ])]));
                let mut cfg = config(provider.clone());
                cfg.cancel = Some(cancel.clone());
                cfg.cwd = dir.path().to_string_lossy().into_owned();
                let mut tools = ToolRegistry::new(None);
                tools.add(Arc::new(ReliabilityTool {
                    name: "child_commit",
                    read_only: false,
                    panics,
                    calls: Arc::new(AtomicUsize::new(0)),
                }));
                let mut hooks = crate::agent::hooks::Hooks::default();
                if !panics {
                    hooks.post.push(Arc::new(HangingPostHook(entered.clone())));
                }
                cfg.tools.add(Arc::new(crate::tools::task::TaskTool {
                    env: crate::agent::env::AgentEnv {
                        provider: child_provider.clone(),
                        subagent_tools: tools,
                        bus: cfg.bus.clone(),
                        cwd: cfg.cwd.clone(),
                        subagent_system: None,
                        jobs: cfg.jobs.clone(),
                        team: crate::agent::team::AgentRegistry::new(),
                        lsp: None,
                        parent_cancel: cancel.clone(),
                        definitions: Default::default(),
                        hooks,
                        memory: None,
                    },
                }));
                let mut agent = Agent::new(cfg);
                let events = capture(&agent);
                let (outcome, ()) =
                    tokio::time::timeout(std::time::Duration::from_secs(3), async {
                        tokio::join!(agent.run_with_budget("delegate", 3), async {
                            if !panics {
                                entered.notified().await;
                                cancel.store(true, Ordering::Relaxed);
                            }
                        })
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    outcome.unwrap().stop_reason,
                    if panics {
                        RunStopReason::ToolPanic
                    } else {
                        RunStopReason::Cancelled
                    }
                );
                assert_eq!(
                    std::fs::read_to_string(dir.join("child_commit")).unwrap(),
                    "committed"
                );
                if panics {
                    assert!(
                        matches!(&agent.messages()[2].content[0], ContentBlock::ToolResult { content, .. } if content.model_text().contains("successful sibling"))
                    );
                } else {
                    assert!(events.lock().unwrap().iter().any(|e| matches!(e, AgentEvent::ToolResult { tool_use_id, output, is_error: false, .. } if tool_use_id == "commit" && output.model_text() == "known success")));
                }
                provider.assert_exhausted();
                child_provider.assert_exhausted();
            }
        }

        fn truncated(id: &str, produced: u64) -> Step {
            let mut reply = completion(vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "some_tool".into(),
                input: json!({ "__bob_tool_args_parse_error__": { "raw": "{", "error": "eof" } }),
            }]);
            reply.stop_reason = StopReason::MaxTokens;
            reply.usage.output_tokens = produced;
            Step::Events(vec![StreamEvent::MessageStop { completion: reply }])
        }

        #[tokio::test]
        async fn truncated_response_recovers_a_bounded_number_of_times_then_stops() {
            for finishes in [false, true] {
                let provider = Arc::new(ScriptProvider::new(vec![
                    truncated("discarded-1", 4096),
                    truncated("discarded-2", 6000),
                    if finishes {
                        Step::Complete(vec![text("done")])
                    } else {
                        truncated("discarded-3", 7000)
                    },
                ]));
                let mut agent = agent(provider.clone());
                let outcome = bounded_run(&mut agent, "do something big", 3).await;
                assert_eq!(outcome.turns, 3);
                assert_eq!(
                    outcome.stop_reason,
                    if finishes {
                        RunStopReason::Completed
                    } else {
                        RunStopReason::OutputLimit
                    }
                );
                assert_eq!(
                    *provider.caps.lock().unwrap(),
                    vec![None, Some(8192), Some(16000)]
                );
                for request in provider.requests.lock().unwrap().iter() {
                    assert_eq!(request.len(), 1);
                }
                for history in [agent.messages(), agent.working_messages()] {
                    assert!(!history.iter().flat_map(|m| &m.content).any(|b| matches!(
                        b,
                        ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
                    )));
                }
                assert_eq!(provider.calls.load(Ordering::Relaxed), 3);
                provider.assert_exhausted();
            }
        }

        #[tokio::test]
        async fn provider_shaped_text_only_max_tokens_is_not_success_and_keeps_usage() {
            for budget in [1, 3] {
                let mut reply = completion(vec![text("partial answer")]);
                reply.stop_reason = StopReason::MaxTokens;
                reply.usage.output_tokens = 4096;
                let provider = Arc::new(ScriptProvider::new(
                    (0..budget)
                        .map(|_| {
                            Step::Events(vec![StreamEvent::MessageStop {
                                completion: reply.clone(),
                            }])
                        })
                        .collect(),
                ));
                let mut agent = agent(provider.clone());
                let events = capture(&agent);
                let outcome = bounded_run(&mut agent, "go", budget).await;
                assert_eq!(outcome.stop_reason, RunStopReason::OutputLimit);
                assert!(outcome.text.contains("partial answer"));
                assert_eq!(outcome.turns, budget);
                assert!(outcome.into_completed_text().is_err());
                let events = events.lock().unwrap();
                let usage = events
                    .iter()
                    .find_map(|event| match event {
                        AgentEvent::TurnEnd { usage, .. } => Some(*usage),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(usage.output_tokens, 4096 * u64::from(budget));
                assert_eq!(usage.cache_creation_input_tokens, 2 * u64::from(budget));
                assert_eq!(usage.cache_read_input_tokens, 3 * u64::from(budget));
                assert_eq!(usage.input_tokens, 10 * u64::from(budget));
                provider.assert_exhausted();
            }
        }
    }

    mod e2e {
        use super::Agent;
        use crate::core::events::{AgentEvent, EventBus};
        use crate::core::types::{ContentBlock, Role, ToolOutput, ToolSpec};
        use crate::test_support::agent::{config, text, ScriptProvider, Step};
        use crate::tools::registry::{Tool, ToolContext, ToolResult};
        use async_trait::async_trait;
        use serde_json::{json, Value};
        use std::sync::{Arc, Mutex};

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
                Ok(format!("echoed: {}", input["msg"].as_str().unwrap_or("")).into())
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
            let mut cfg = config(provider);
            cfg.tools.add(Arc::new(EchoTool { seen: seen.clone() }));
            cfg.bus = bus;
            let agent = Agent::new(cfg);
            (agent, seen)
        }

        #[tokio::test]
        async fn full_turn_calls_a_tool_then_finishes() {
            let provider = Arc::new(echo_script("hello", "all done"));

            let bus = EventBus::new();
            let kinds = Arc::new(Mutex::new(Vec::<String>::new()));
            {
                let kinds = kinds.clone();
                bus.on(Arc::new(move |e: &AgentEvent| {
                    kinds.lock().unwrap().push(e.kind().to_string());
                }));
            }

            let (mut agent, seen) = agent_with_echo(provider.clone(), bus);
            let out = agent.run("please echo").await.unwrap();

            assert_eq!(out, "all done");
            provider.assert_exhausted();
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0]["msg"], "hello");

            let msgs = agent.messages();
            assert_eq!(msgs.len(), 4);
            assert_eq!(msgs.first().unwrap().role, Role::User);
            assert!(
                matches!(&msgs[1].content[0], ContentBlock::ToolUse { id, .. } if id == "echo-call")
            );
            assert!(
                matches!(&msgs[2].content[0], ContentBlock::ToolResult { tool_use_id, content, is_error: Some(false) } if tool_use_id == "echo-call" && content.as_text() == Some("echoed: hello"))
            );
            assert_eq!(msgs.last().unwrap().role, Role::Assistant);
            assert_eq!(msgs.last().unwrap().text(), "all done");

            let kinds = kinds.lock().unwrap();
            assert!(kinds.iter().any(|k| k == "ToolCall"), "kinds: {kinds:?}");
            assert!(kinds.iter().any(|k| k == "ToolResult"), "kinds: {kinds:?}");
            assert!(kinds.iter().any(|k| k == "TurnEnd"), "kinds: {kinds:?}");
        }

        use crate::agent::hooks::{Hooks, PostToolUse, PreToolDecision, PreToolUse};

        fn agent_with_hooks(
            provider: Arc<dyn crate::providers::provider::Provider>,
            hooks: Hooks,
        ) -> (Agent, Arc<Mutex<Vec<Value>>>) {
            let (mut agent, seen) = agent_with_echo(provider, EventBus::new());
            agent.cfg.hooks = hooks;
            (agent, seen)
        }

        fn echo_provider() -> ScriptProvider {
            echo_script("hi", "done")
        }

        fn echo_script(message: &str, reply: &str) -> ScriptProvider {
            ScriptProvider::new(vec![
                Step::Complete(vec![ContentBlock::ToolUse {
                    id: "echo-call".into(),
                    name: "echo".into(),
                    input: json!({"msg":message}),
                }]),
                Step::Complete(vec![text(reply)]),
            ])
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
            assert!(seen.lock().unwrap().is_empty(), "denied tool must not run");
            assert!(
                matches!(&agent.messages()[2].content[0], ContentBlock::ToolResult { content, is_error: Some(true), .. } if content.as_text().unwrap().contains("blocked by policy"))
            );
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
            assert_eq!(seen.lock().unwrap()[0]["msg"], "rewritten");
        }

        #[tokio::test]
        async fn shell_hook_rewrites_reach_history_events_and_retained_pages() {
            use crate::core::types::{ShellOutput, ShellStream};
            use crate::tools::registry::ToolError;

            struct ShellFixture;
            #[async_trait]
            impl Tool for ShellFixture {
                fn spec(&self) -> ToolSpec {
                    ToolSpec {
                        name: "shell_fixture".into(),
                        description: String::new(),
                        input_schema: json!({"type":"object"}),
                    }
                }
                async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
                    let text = format!("private-token {} end", "🦀".repeat(4000));
                    Err(
                        ToolError::failed("command failed").with_output(ShellOutput {
                            exit_code: Some(7),
                            stdout: ShellStream {
                                total_bytes: text.len() as u64,
                                text,
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                    )
                }
            }
            struct Redact(bool);
            #[async_trait]
            impl PostToolUse for Redact {
                async fn on_post_tool(
                    &self,
                    _: &str,
                    output: ToolOutput,
                    is_error: bool,
                ) -> ToolOutput {
                    assert!(is_error);
                    let ToolOutput::Shell(mut shell) = output else {
                        panic!("hook must receive structured data")
                    };
                    assert_eq!(shell.exit_code, Some(7));
                    if self.0 {
                        "entire result withheld".into()
                    } else {
                        shell.stdout.text =
                            shell.stdout.text.replace("private-token", "[redacted]");
                        ToolOutput::Shell(shell)
                    }
                }
            }
            for replace in [false, true] {
                let provider = Arc::new(ScriptProvider::new(vec![
                    Step::Complete(vec![ContentBlock::ToolUse {
                        id: "shell".into(),
                        name: "shell_fixture".into(),
                        input: json!({}),
                    }]),
                    Step::Complete(vec![text("done")]),
                ]));
                let mut cfg = config(provider.clone());
                cfg.tools.add(Arc::new(ShellFixture));
                cfg.hooks.post.push(Arc::new(Redact(replace)));
                let recorded = Arc::new(Mutex::new(None));
                let events = recorded.clone();
                cfg.bus.on(Arc::new(move |event| {
                    if let AgentEvent::ToolResult {
                        output, is_error, ..
                    } = event
                    {
                        assert!(*is_error);
                        *events.lock().unwrap() = Some(output.clone());
                    }
                }));
                let jobs = cfg.jobs.clone();
                let mut agent = Agent::new(cfg);
                agent.run("run the check").await.unwrap();
                let ContentBlock::ToolResult {
                    content,
                    is_error: Some(true),
                    ..
                } = &agent.messages()[2].content[0]
                else {
                    panic!("failed tool result expected")
                };
                assert_eq!(recorded.lock().unwrap().as_ref(), Some(content));
                let requests = provider.requests.lock().unwrap();
                let sent = requests
                    .last()
                    .unwrap()
                    .iter()
                    .flat_map(|message| &message.content)
                    .find_map(|block| {
                        if let ContentBlock::ToolResult { content, .. } = block {
                            Some(content)
                        } else {
                            None
                        }
                    })
                    .unwrap();
                assert_eq!(sent, content);
                match content {
                    ToolOutput::Read { .. } => panic!("expected shell output"),
                    ToolOutput::Text(text) => assert_eq!(text, "entire result withheld"),
                    ToolOutput::Shell(shell) => {
                        assert_eq!(shell.exit_code, Some(7));
                        assert!(shell.stdout.truncated);
                        let page = jobs
                            .output_page(shell.stdout.output_id.as_ref().unwrap(), 0, 24_000)
                            .unwrap();
                        assert!(page.text.starts_with("[redacted]"));
                        assert!(!page.text.contains("private-token"));
                        assert!(page.text.ends_with(" end"));
                    }
                }
                assert!(!content.model_text().contains("private-token"));
            }
        }

        struct ShoutOutput;
        #[async_trait]
        impl PostToolUse for ShoutOutput {
            async fn on_post_tool(
                &self,
                _tool: &str,
                output: ToolOutput,
                _is_error: bool,
            ) -> ToolOutput {
                output.as_text().unwrap().to_uppercase().into()
            }
        }

        #[tokio::test]
        async fn post_tool_hook_rewrites_the_output() {
            let hooks = Hooks {
                pre: vec![],
                post: vec![Arc::new(ShoutOutput)],
            };
            let (agent_provider, recorded) =
                (Arc::new(echo_provider()), Arc::new(Mutex::new(None)));
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
            assert_eq!(
                recorded
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(ToolOutput::as_text),
                Some("ECHOED: HI"),
            );
        }
    }

    mod classify {
        use super::{classify_error, has_status_code, ProviderErrorKind};

        #[test]
        fn status_matching_skips_embedded_numbers_and_finds_later_standalone_codes() {
            for code in ["429", "500", "502", "503", "529"] {
                for message in [
                    code.to_owned(),
                    format!("HTTP ({code}): failure"),
                    format!("1{code} then HTTP {code}"),
                ] {
                    assert!(has_status_code(&message, code), "{message}");
                    assert_eq!(
                        classify_error(&message),
                        ProviderErrorKind::Transient,
                        "{message}"
                    );
                }
                for message in [
                    format!("1{code}"),
                    format!("{code}0"),
                    format!("1{code}0"),
                    format!("1{code} and {code}0"),
                    format!("é{code}🦀"),
                    format!("EOF while parsing an object at line 1 column {code}"),
                ] {
                    assert!(!has_status_code(&message, code), "{message}");
                    assert_eq!(
                        classify_error(&message),
                        ProviderErrorKind::Fatal,
                        "{message}"
                    );
                }
            }
        }

        #[test]
        fn overflow_takes_precedence_over_retryable_statuses_and_case() {
            for message in [
                "HTTP 503: CONTEXT_LENGTH_EXCEEDED",
                "429: PROMPT IS TOO LONG",
                "Connection reset: input TOKENS EXCEED the limit",
            ] {
                assert_eq!(
                    classify_error(message),
                    ProviderErrorKind::Overflow,
                    "{message}"
                );
            }
            for message in [
                "HTTP 401: invalid API key",
                "HTTP 403: forbidden",
                "HTTP 400: invalid schema",
                "HTTP 404: model not found",
            ] {
                assert_eq!(
                    classify_error(message),
                    ProviderErrorKind::Fatal,
                    "{message}"
                );
            }
        }

        #[test]
        fn overflow_phrases_classify_as_overflow() {
            for m in [
                "This model's maximum context length is 200000 tokens",
                "prompt is too long",
                "reduce the length of the messages",
                "input tokens exceed the limit",
                "responses 400 Bad Request: Your input exceeds the context window of this model.",
                "context_window_exceeded",
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
        fn bare_connection_word_is_not_transient() {
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
}
