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
use bob_core::core::events::EventBus;
use bob_core::core::permissions::{Asker, Decision, PermissionEngine};
use bob_core::core::store::{SessionStore, SqliteStore};
use bob_core::providers::create_provider;
use bob_core::tools::jobs::JobRegistry;
use bob_core::tools::registry::{UserAsker, UserQuery};

/// A curated set of the types most consumers need, re-exported so a single
/// `use bob_sdk::prelude::*;` is enough to get going.
pub mod prelude {
    pub use crate::{Agent, AgentBuilder};
    pub use bob_core::core::permissions::{Asker, Decision};
    pub use bob_core::core::store::{SessionStore, SqliteStore};
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
    model: String,
    cwd: String,
    system_override: Option<String>,
    permission_default: Decision,
    asker: Option<Arc<dyn Asker>>,
    user_asker: Option<Arc<dyn UserAsker>>,
    store: Option<Arc<dyn SessionStore>>,
    resume: Resume,
    max_turns: Option<u32>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        AgentBuilder {
            // A sensible default model; override with `.model(...)`.
            model: "anthropic".to_string(),
            cwd: ".".to_string(),
            system_override: None,
            // Fail-closed-ish default: without an asker, `Ask`/`Deny` decline.
            permission_default: Decision::Ask,
            asker: None,
            user_asker: None,
            store: None,
            resume: Resume::Fresh,
            max_turns: None,
        }
    }
}

impl AgentBuilder {
    /// The `provider/model` (or legacy `provider:model`, or bare `provider`) to
    /// run — resolved through the provider registry.
    pub fn model(mut self, spec: impl Into<String>) -> Self {
        self.model = spec.into();
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

    /// The default permission decision when no rule matches (default `Ask`).
    pub fn permission_default(mut self, decision: Decision) -> Self {
        self.permission_default = decision;
        self
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
        let provider = create_provider(&self.model).await?;
        let bus = EventBus::new();
        let jobs = JobRegistry::new();
        let team = AgentRegistry::new();
        let store: Arc<dyn SessionStore> = self
            .store
            .unwrap_or_else(|| Arc::new(SqliteStore::default()));

        let permissions = Arc::new(PermissionEngine::new(self.permission_default, self.asker));

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

        Ok(Agent { inner: agent })
    }
}

/// A ready-to-run agent. Thin handle over the core agent with the ergonomic
/// entry points; drop to [`Agent::core`] for the full core API.
pub struct Agent {
    inner: CoreAgent,
}

impl Agent {
    /// Start building an agent.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Run one turn to completion and return the assistant's final text.
    pub async fn run(&mut self, prompt: &str) -> anyhow::Result<String> {
        self.inner.run(prompt).await
    }

    /// Borrow the underlying core agent for anything the SDK doesn't surface.
    pub fn core(&mut self) -> &mut CoreAgent {
        &mut self.inner
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
            .permission_default(Decision::Allow)
            .max_turns(10)
            .resume_latest();
        // Defaults are sane without any setters.
        let d = AgentBuilder::default();
        assert_eq!(d.cwd, ".");
        assert!(matches!(d.permission_default, Decision::Ask));
    }
}
