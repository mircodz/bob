//! bob-sdk — the ergonomic, human-writable door to bob's agent engine.
//!
//! `bob-core` has all the power but a wide surface: to stand up an agent you wire
//! a provider, a permission engine, MCP + LSP tools, a system prompt, a session
//! store, and an 18-field `AgentConfig` by hand (which is exactly what the TUI and
//! remote host each do). This crate collapses that into a builder with sane
//! defaults:
//!
//! ```no_run
//! use bob_sdk::prelude::*;
//!
//! # async fn demo() -> anyhow::Result<()> {
//! let mut agent = Agent::builder()
//!     .model("anthropic/claude-opus-4.8") // provider/model; resolved via the registry
//!     .cwd(".")
//!     .resume_latest() // pick up the newest session in this cwd (optional)
//!     .build()
//!     .await?;
//!
//! let reply = agent.run("summarize the repo").await?;
//! println!("{reply}");
//! # Ok(())
//! # }
//! ```
//!
//! Everything the builder omits has a default; drop to `bob_core` directly for
//! anything the builder doesn't expose (no capability is hidden).

use std::sync::Arc;

use bob_core::agent::agent::Agent as CoreAgent;
use bob_core::agent::assembly::{build_root_agent, RootAgentParams};
use bob_core::agent::prompt::build_system_prompt;
use bob_core::agent::team::AgentRegistry;
use bob_core::auth::ProviderAuth;
use bob_core::core::events::EventBus;
use bob_core::core::permissions::{Asker, Decision, Mode, PermissionEngine};
use bob_core::core::store::{MemoryStore, SessionStore};
use bob_core::providers::create_provider_with_auth;
use bob_core::tools::jobs::JobRegistry;
use bob_core::tools::registry::{UserAsker, UserQuery};

/// A curated set of the types most consumers need, re-exported so a single
/// `use bob_sdk::prelude::*;` is enough to get going.
pub mod prelude {
    pub use crate::{Agent, AgentBuilder};
    pub use bob_core::auth::ProviderAuth;
    pub use bob_core::core::permissions::{Asker, Decision, Mode};
    pub use bob_core::core::store::{MemoryStore, SessionStore, SqliteStore};
    pub use bob_types::{ContentBlock, Message, ReasoningEffort, Role};
}

/// A no-op user-asker for headless use: every `ask_user` / `exit_plan` query is
/// declined. The TUI supplies a real one; a library embedder can too.
struct SilentAsker;

#[async_trait::async_trait]
impl UserAsker for SilentAsker {
    async fn ask(&self, _query: &UserQuery) -> Option<String> {
        None
    }
}

/// Where the agent's conversation starts from.
enum Resume {
    /// A brand-new, empty conversation.
    Fresh,
    /// The most-recently-updated session in the builder's cwd.
    Latest,
    /// A specific session id.
    Id(String),
}

/// Fluent builder for an [`Agent`]. Construct with [`Agent::builder`].
pub struct AgentBuilder {
    model: Option<String>,
    cwd: String,
    system_override: Option<String>,
    permission_default: Decision,
    asker: Option<Arc<dyn Asker>>,
    user_asker: Option<Arc<dyn UserAsker>>,
    credential: Option<ProviderAuth>,
    provider: Option<Arc<dyn bob_core::providers::provider::Provider>>,
    store: Option<Arc<dyn SessionStore>>,
    mode: Option<Mode>,
    allow_tools: Vec<String>,
    deny_tools: Vec<String>,
    resume: Resume,
    max_turns: Option<u32>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        AgentBuilder {
            // No default model — the caller MUST pick one (`.model(...)`), so bob
            // never silently talks to a model the user didn't choose.
            model: None,
            cwd: ".".to_string(),
            system_override: None,
            // Fail-closed-ish default: without an asker, `Ask`/`Deny` decline.
            permission_default: Decision::Ask,
            asker: None,
            user_asker: None,
            credential: None,
            provider: None,
            store: None,
            mode: None,
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
            resume: Resume::Fresh,
            max_turns: None,
        }
    }
}

impl AgentBuilder {
    /// The `provider/model` (or legacy `provider:model`, or bare `provider`) to
    /// run — resolved through the provider registry. Required; `build()` errors if
    /// it's not set.
    pub fn model(mut self, spec: impl Into<String>) -> Self {
        self.model = Some(spec.into());
        self
    }

    /// A programmatic credential for the chosen provider (e.g.
    /// `ProviderAuth::ApiKey("sk-…")`). When unset, the provider resolves auth from
    /// the ambient environment (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) or an
    /// on-disk `bob login`. Each provider honors only the auth schemes it supports.
    pub fn credential(mut self, credential: ProviderAuth) -> Self {
        self.credential = Some(credential);
        self
    }

    /// The working directory the agent operates in (default `.`).
    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// Override the composed system prompt entirely (default: bob's base prompt +
    /// environment + project context).
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_override = Some(prompt.into());
        self
    }

    /// Supply a pre-built provider directly, bypassing model/credential resolution.
    /// The escape hatch for advanced callers (a custom provider impl) and for tests
    /// (a mock). When set, `.model()` and `.credential()` are ignored.
    pub fn provider(mut self, provider: Arc<dyn bob_core::providers::provider::Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// The default permission decision when no rule matches (default `Ask`).
    pub fn permission_default(mut self, decision: Decision) -> Self {
        self.permission_default = decision;
        self
    }

    /// The interaction mode: `Normal` (prompt per rules), `AutoAccept` (auto-allow
    /// edits), or `Plan` (read-only — block all mutating tools). Default `Normal`.
    pub fn permission_mode(mut self, mode: Mode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Auto-allow these tools by name (a permission rule). Names match the tool's
    /// `spec().name` (e.g. `"read_file"`, `"bash"`).
    pub fn allow_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_tools.extend(tools.into_iter().map(Into::into));
        self
    }

    /// Force a prompt (never silently auto-run) for these tools by name — a `Deny`
    /// rule surfaced to the asker, so the human still gets the final say.
    pub fn deny_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny_tools.extend(tools.into_iter().map(Into::into));
        self
    }

    /// Alias for [`asker`]: supply the callback consulted when a tool call needs
    /// approval. Mirrors Claude's `canUseTool`.
    pub fn on_permission(self, asker: Arc<dyn Asker>) -> Self {
        self.asker(asker)
    }

    /// Supply a permission asker (interactive approval). Without one, `Ask`/`Deny`
    /// decisions decline — safe for headless use.
    pub fn asker(mut self, asker: Arc<dyn Asker>) -> Self {
        self.asker = Some(asker);
        self
    }

    /// Supply a user-question asker (`ask_user` / `exit_plan`). Defaults to a
    /// silent asker that declines.
    pub fn user_asker(mut self, asker: Arc<dyn UserAsker>) -> Self {
        self.user_asker = Some(asker);
        self
    }

    /// The session store to persist to (default: [`SqliteStore`] at `~/.bob`).
    pub fn store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Resume the most-recently-updated session in this cwd, if one exists.
    pub fn resume_latest(mut self) -> Self {
        self.resume = Resume::Latest;
        self
    }

    /// Resume a specific session by id.
    pub fn resume(mut self, id: impl Into<String>) -> Self {
        self.resume = Resume::Id(id.into());
        self
    }

    /// Cap the number of turns per `run` (default: the core default).
    pub fn max_turns(mut self, turns: u32) -> Self {
        self.max_turns = Some(turns);
        self
    }

    /// Build the agent: resolve the provider, assemble tools + permissions, compose
    /// the prompt, and seed history from the chosen session.
    pub async fn build(self) -> anyhow::Result<Agent> {
        // A pre-built provider (escape hatch / tests) wins; otherwise resolve one
        // from the required model spec + optional credential.
        let provider = match self.provider {
            Some(p) => p,
            None => {
                let model = self.model.ok_or_else(|| {
                    anyhow::anyhow!(
                        "no model set — call `.model(\"provider/model\")` or `.provider(...)`"
                    )
                })?;
                create_provider_with_auth(&model, self.credential).await?
            }
        };
        let bus = EventBus::new();
        let jobs = JobRegistry::new();
        let team = AgentRegistry::new();
        // Default to a non-persistent in-memory store: an SDK embedder shouldn't
        // silently write to `~/.bob`. Opt into persistence with `.store(...)`.
        let store: Arc<dyn SessionStore> = self
            .store
            .unwrap_or_else(|| Arc::new(MemoryStore::default()));

        // Permission engine: the default decision + asker, plus optional allow/deny
        // tool rules and an interaction mode. Deny rules are added AFTER allow rules
        // so a denied tool still forces a prompt even if broadly allowed.
        let mut engine = PermissionEngine::new(self.permission_default, self.asker);
        if !self.allow_tools.is_empty() {
            engine.add(bob_core::core::policies::allow_tools(self.allow_tools));
        }
        if !self.deny_tools.is_empty() {
            engine.add(bob_core::core::policies::deny_tools(self.deny_tools));
        }
        if let Some(mode) = self.mode {
            engine.set_mode(mode);
        }
        let permissions = Arc::new(engine);

        let cwd_path = std::path::Path::new(&self.cwd);
        let system_prompt = self
            .system_override
            .clone()
            .unwrap_or_else(|| build_system_prompt(None, cwd_path));

        let user_asker: Arc<dyn UserAsker> =
            self.user_asker.unwrap_or_else(|| Arc::new(SilentAsker));

        let mut agent = build_root_agent(RootAgentParams {
            provider,
            permissions,
            bus,
            jobs,
            team,
            cwd: self.cwd.clone(),
            system_prompt,
            mcp_tools: Vec::new(),
            lsp: None,
            user_asker,
            max_turns: self.max_turns,
        });

        // Seed history from the chosen session (if any), reconstructing from the
        // event log with the blob-fallback guard — exactly the frontends' resume.
        let session = match self.resume {
            Resume::Fresh => None,
            Resume::Latest => store.latest_in(&self.cwd).ok().flatten(),
            Resume::Id(id) => store.load(&id).ok().flatten(),
        };
        if let Some(s) = &session {
            let history = store.history_for(s);
            if !history.is_empty() {
                agent.load_history(history);
            }
        }

        let bus = agent.bus();
        let cancel = agent.cancel_handle();
        Ok(Agent {
            inner: Arc::new(tokio::sync::Mutex::new(agent)),
            bus,
            cancel,
        })
    }
}

/// A ready-to-run agent. Thin handle over the core agent with the ergonomic
/// entry points; drop to [`Agent::with_core`] for the full core API.
///
/// The core agent lives behind an async mutex so a streamed run can execute on a
/// background task while the caller drains its event stream, and so [`interrupt`]
/// can signal cancellation without waiting for the lock.
pub struct Agent {
    inner: Arc<tokio::sync::Mutex<CoreAgent>>,
    bus: bob_core::core::events::EventBus,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// One event from a streamed run — a re-export of the core [`AgentEvent`] so a
/// consumer can pattern-match reasoning deltas, tool calls/results, and turn
/// boundaries as they happen.
pub use bob_core::core::events::AgentEvent;

/// The handle returned by [`Agent::run_streamed`]: an async stream of
/// [`AgentEvent`]s plus the eventual final result. Drain `events` to observe
/// progress, then `await` `finish()` for the assistant's final text.
pub struct RunStream {
    /// Live events as the turn progresses (reasoning, tool calls/results, …).
    pub events: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    handle: tokio::task::JoinHandle<anyhow::Result<String>>,
}

impl RunStream {
    /// Await the run's completion and return the assistant's final text. Call this
    /// after the `events` stream closes (or concurrently — the events channel and
    /// this future are independent).
    pub async fn finish(self) -> anyhow::Result<String> {
        match self.handle.await {
            Ok(r) => r,
            Err(e) => Err(anyhow::anyhow!("streamed run task failed: {e}")),
        }
    }
}

impl Agent {
    /// Start building an agent.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Run one turn to completion and return the assistant's final text.
    pub async fn run(&mut self, prompt: &str) -> anyhow::Result<String> {
        self.inner.lock().await.run(prompt).await
    }

    /// Run one turn, streaming [`AgentEvent`]s as they happen. Returns immediately
    /// with a [`RunStream`]: consume its `events` receiver for live progress and
    /// `finish()` for the final text. The run executes on a background task.
    pub async fn run_streamed(&mut self, prompt: &str) -> RunStream {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Forward every bus event into the channel for the lifetime of this run.
        // The subscription is moved into the task and dropped when the run ends,
        // so the listener never outlives the stream.
        let sub = self.bus.subscribe(Arc::new(move |e: &AgentEvent| {
            let _ = tx.send(e.clone());
        }));
        let inner = self.inner.clone();
        let prompt = prompt.to_string();
        let handle = tokio::spawn(async move {
            let _sub = sub; // held for the duration; dropped (unsubscribes) on exit
            inner.lock().await.run(&prompt).await
        });
        RunStream { events: rx, handle }
    }

    /// Cooperatively interrupt an in-flight run (e.g. from another task). The
    /// current turn finishes its step, leaves history valid, and returns early.
    pub fn interrupt(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Run a closure with mutable access to the underlying core agent, for anything
    /// the SDK doesn't surface (the escape hatch).
    pub async fn with_core<R>(&self, f: impl FnOnce(&mut CoreAgent) -> R) -> R {
        f(&mut *self.inner.lock().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The public builder surface must stay ergonomic and chainable. This doesn't
    /// hit the network (no `.build()`), it guards that the fluent API compiles and
    /// composes — a regression here means we broke the SDK's front door.
    #[test]
    fn builder_is_fluent_and_defaulted() {
        let _b = Agent::builder()
            .model("anthropic/claude-opus-4.8")
            .cwd("/tmp/project")
            .credential(ProviderAuth::ApiKey("sk-ant-test".into()))
            .permission_default(Decision::Allow)
            .max_turns(10)
            .resume_latest();
        // Defaults are sane without any setters.
        let d = AgentBuilder::default();
        assert_eq!(d.cwd, ".");
        assert!(matches!(d.permission_default, Decision::Ask));
        // No default model: the caller must choose one explicitly.
        assert!(d.model.is_none());
        // No default credential: auth resolves from env / login unless set.
        assert!(d.credential.is_none());
    }

    #[tokio::test]
    async fn run_streamed_yields_events_and_final_text() {
        use bob_core::providers::mock::{MockProvider, MockReply};
        // A provider that answers with plain text — one clean turn.
        let provider = MockProvider::new(vec![]).with_default(MockReply::Text("hello sdk".into()));
        let mut agent = Agent::builder()
            .provider(Arc::new(provider))
            .permission_default(Decision::Allow)
            .build()
            .await
            .unwrap();

        let mut stream = agent.run_streamed("hi").await;
        // Drain events while the run proceeds.
        let mut kinds = Vec::new();
        while let Some(ev) = stream.events.recv().await {
            kinds.push(ev.kind().to_string());
        }
        // The turn boundary events are present in the stream.
        assert!(kinds.iter().any(|k| k == "TurnStart"), "{kinds:?}");
        assert!(kinds.iter().any(|k| k == "TurnEnd"), "{kinds:?}");
        // And the final text is available from finish().
        let out = stream.finish().await.unwrap();
        assert_eq!(out, "hello sdk");
    }

    #[tokio::test]
    async fn plain_run_works_through_the_provider_escape_hatch() {
        use bob_core::providers::mock::{MockProvider, MockReply};
        let provider = MockProvider::new(vec![]).with_default(MockReply::Text("ok".into()));
        let mut agent = Agent::builder()
            .provider(Arc::new(provider))
            .permission_default(Decision::Allow)
            .build()
            .await
            .unwrap();
        assert_eq!(agent.run("go").await.unwrap(), "ok");
    }
}
