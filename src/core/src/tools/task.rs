//! The `task` tool lets the MODEL spawn subagents — parallelism becomes a
//! decision the model can make. By default subagents run inline (blocking) and
//! their answers are returned together. With `background: true` each task is
//! detached as a background *job*; the tool returns immediately with job ids the
//! model later polls via job_status / job_output.

use crate::agent::agent::{
    build_subagent, Agent, SubagentSpec, EXPLORE_MAX_TURNS, ROOT_AGENT_ID, SUBAGENT_MAX_TURNS,
};
use crate::core::events::EventBus;
use crate::core::types::ToolSpec;
use crate::providers::provider::Provider;
use crate::tools::jobs::{JobRegistry, JobStatus};
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Process-wide monotonic counter for subagent ids. Per-batch `task_{i+1}` ids
/// collided across successive `task` calls — the team drawer keys threads by id,
/// so batch B's `task_1` silently overwrote batch A's finished `task_1` thread
/// (spawned agents "didn't show up"). A global counter makes every spawn unique.
static SUBAGENT_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_subagent_id() -> String {
    format!("task_{}", SUBAGENT_SEQ.fetch_add(1, Ordering::Relaxed))
}

pub struct TaskTool {
    pub provider: Arc<dyn Provider>,
    pub subagent_tools: ToolRegistry,
    pub bus: EventBus,
    pub cwd: String,
    pub subagent_system: Option<String>,
    /// Shared job registry (same instance the root agent + UI use).
    pub jobs: JobRegistry,
    /// Shared language servers, so subagents get diagnostics/nav too. None if
    /// no lsp_servers are configured.
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    /// The root's cancel flag, so a Cancel cascades into `task` children.
    pub parent_cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl TaskTool {
    /// Build a subagent for one task and return the future that runs it.
    /// `read_only` confines the child to the read-only tool subset (no
    /// write/edit/bash) — for audit/analysis delegation that must not mutate.
    fn make_child(&self, id: String, cwd: String, read_only: bool) -> Agent {
        // The simple `task` tool stays fire-and-forget: its children are not team
        // members (no inbox/registry). Coordinated agents come from `spawn_agent`.
        let tools = if read_only {
            self.subagent_tools.read_only_subset()
        } else {
            self.subagent_tools.clone()
        };
        build_subagent(SubagentSpec {
            provider: self.provider.clone(),
            tools,
            bus: self.bus.clone(),
            system: self.subagent_system.clone(),
            cwd,
            jobs: self.jobs.clone(),
            lsp: self.lsp.clone(),
            name: id,
            max_turns: SUBAGENT_MAX_TURNS,
            depth: 1,
            inbox: None,
            team: None,
            parent_cancel: Some(self.parent_cancel.clone()),
        })
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".to_string(),
            description:
                "Delegate one or more independent sub-tasks to fresh subagents, each with \
                its own isolated context and tools. By default they run inline and this returns \
                every subagent's final result. Set `background: true` to detach them as background \
                jobs instead — this returns immediately with job ids you can later inspect with \
                job_status and collect with job_output (use for long-running work you don't want \
                to block on). Subagents cannot spawn further subagents. \
                Delegate when work is parallelizable or context-heavy (searching a large codebase, \
                researching several areas at once) — launch multiple in ONE call so they run \
                concurrently. Do NOT delegate a quick read/grep/edit you can do directly, or work \
                that needs your accumulated context: a subagent starts blank and sees only its \
                prompt, so give it exact scope, the concrete deliverable, and the specific findings \
                to report back — then verify its result rather than trusting it blindly. \
                For read/analysis/audit work that must not modify anything, set \
                `read_only: true` so the subagent gets only read/search tools (no write/edit/bash)."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "description": { "type": "string", "description": "A 3-5 word label for the sub-task (e.g. \"review src/core\"), shown in the UI. NOT the full instructions." },
                                "prompt": { "type": "string", "description": "Full instructions for the subagent." }
                            },
                            "required": ["description", "prompt"]
                        }
                    },
                    "background": { "type": "boolean", "description": "Run detached as background jobs (default false)." },
                    "read_only": { "type": "boolean", "description": "Confine every subagent to read-only tools (reads/searches, no write/edit/bash). Use for audits/analysis that must not mutate files (default false)." }
                },
                "required": ["tasks"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let tasks = match input["tasks"].as_array() {
            Some(t) if !t.is_empty() => t.clone(),
            _ => return Err(ToolError::invalid_input("no tasks provided")),
        };
        let background = input["background"].as_bool().unwrap_or(false);
        let read_only = input["read_only"].as_bool().unwrap_or(false);
        let base_cwd = if self.cwd.is_empty() {
            ctx.cwd.clone()
        } else {
            self.cwd.clone()
        };

        if background {
            // Detach each task as a background job; return the ids immediately.
            let mut ids = Vec::new();
            for t in tasks {
                let description = t["description"].as_str().unwrap_or("").to_string();
                let prompt = t["prompt"].as_str().unwrap_or("").to_string();
                let job_id = ctx.jobs.next_id();
                let child = self.make_child(job_id.clone(), base_cwd.clone(), read_only);
                let jobs = ctx.jobs.clone();
                let jid = job_id.clone();
                let handle = tokio::spawn(async move {
                    let mut child = child;
                    match child.run(&prompt).await {
                        Ok(out) => jobs.finish(&jid, JobStatus::Done, out),
                        Err(e) => jobs.finish(&jid, JobStatus::Failed, format!("error: {}", e)),
                    }
                });
                ctx.jobs
                    .register(job_id.clone(), "task", description.clone(), handle);
                ids.push(format!("{} ({})", job_id, description));
            }
            return Ok(format!(
                "started {} background job(s): {}\nUse job_status / job_output to check on them.",
                ids.len(),
                ids.join(", ")
            ));
        }

        // Inline (blocking) — run all subagents concurrently and join.
        let mut handles = Vec::new();
        for t in tasks.into_iter() {
            let description = t["description"].as_str().unwrap_or("").to_string();
            let prompt = t["prompt"].as_str().unwrap_or("").to_string();
            let id = next_subagent_id();
            self.bus
                .emit(crate::core::events::AgentEvent::SubagentSpawn {
                    parent_id: ROOT_AGENT_ID.to_string(),
                    agent_id: id.clone(),
                    task: description.clone(),
                    prompt: prompt.clone(),
                });
            let child = self.make_child(id.clone(), base_cwd.clone(), read_only);
            let bus = self.bus.clone();
            handles.push(tokio::spawn(async move {
                let mut child = child;
                let (result, failed) = match child.run(&prompt).await {
                    Ok(out) => (format!("### {}\n{}", description, out), false),
                    Err(e) => (format!("### {}\nerror: {}", description, e), true),
                };
                // Signal completion so the team drawer marks this thread done/failed
                // (mirrors spawn_agent). Without this the thread stays "running".
                bus.emit(crate::core::events::AgentEvent::SubagentDone {
                    agent_id: id,
                    failed,
                });
                result
            }));
        }

        let mut results = Vec::new();
        for h in handles {
            match h.await {
                Ok(r) => results.push(r),
                Err(e) => results.push(format!("error: subagent join failed: {}", e)),
            }
        }
        Ok(results.join("\n\n"))
    }
}

/// A focused system prompt for an `explore` agent — read-only investigation that
/// returns a synthesized answer, not a file dump.
const EXPLORE_SYSTEM: &str = "You are a fast, read-only code explorer. You have ONLY \
    read/search tools (read_file, glob, grep, list_dir, lsp) — you cannot edit, run \
    commands, or change anything. Answer the given question about the codebase by \
    searching and reading. Be thorough but efficient: use glob/grep to locate, read the \
    relevant spans, and follow references. Return a DIRECT, synthesized answer — the \
    specific files, symbols, and line numbers that matter, with a one-line explanation of \
    each — not a transcript of everything you looked at. If you can't find something, say \
    so plainly.";

/// The `explore` tool: a read-only search subagent. Cheaper and safer than `task`
/// for "where is X / how does Y work / which files touch Z" questions — it has a
/// curated read-only toolset and returns a synthesized answer.
pub struct ExploreTool {
    pub provider: Arc<dyn Provider>,
    /// The full subagent toolset; explore uses only its read-only subset.
    pub subagent_tools: ToolRegistry,
    pub bus: EventBus,
    pub cwd: String,
    pub jobs: JobRegistry,
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    /// The root's cancel flag, so a Cancel cascades into the explore child.
    pub parent_cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Tool for ExploreTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "explore".to_string(),
            description: "Explore the codebase with a fast, READ-ONLY search agent and get back a \
                synthesized answer. Best for open-ended \"where is X defined / how does Y work / \
                which files reference Z\" questions that would otherwise take several \
                grep/read round-trips — the agent does the searching and returns just the \
                relevant files, symbols, and line numbers with a short explanation. It cannot \
                edit files or run commands. For a single known lookup you can do directly, just \
                use read_file/grep yourself; for work that must change files, use `task`. Give a \
                specific, self-contained question — the agent starts with none of your context."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The question to investigate, self-contained (name the scope + what to find)." },
                    "description": { "type": "string", "description": "A 3-5 word label shown in the UI (e.g. \"trace auth flow\")." }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let query = input["query"].as_str().unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Err(ToolError::invalid_input("query is required"));
        }
        let label = input["description"]
            .as_str()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("explore")
            .to_string();
        let cwd = if self.cwd.is_empty() {
            ctx.cwd.clone()
        } else {
            self.cwd.clone()
        };

        // Announce as a subagent so it appears in the transcript + team drawer.
        let id = "explore".to_string();
        self.bus
            .emit(crate::core::events::AgentEvent::SubagentSpawn {
                parent_id: ROOT_AGENT_ID.to_string(),
                agent_id: id.clone(),
                task: label,
                prompt: query.clone(),
            });

        // A read-only child: only reads/searches, its own focused prompt.
        let mut child = build_subagent(SubagentSpec {
            provider: self.provider.clone(),
            tools: self.subagent_tools.read_only_subset(),
            bus: self.bus.clone(),
            system: Some(EXPLORE_SYSTEM.to_string()),
            cwd,
            jobs: self.jobs.clone(),
            lsp: self.lsp.clone(),
            name: id.clone(),
            max_turns: EXPLORE_MAX_TURNS,
            depth: 1,
            inbox: None,
            team: None,
            parent_cancel: Some(self.parent_cancel.clone()),
        });
        let (out, failed) = match child.run(&query).await {
            Ok(out) => (out, false),
            Err(e) => (format!("error: {}", e), true),
        };
        self.bus
            .emit(crate::core::events::AgentEvent::SubagentDone {
                agent_id: id,
                failed,
            });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::next_subagent_id;
    use std::collections::HashSet;

    // Regression: subagent ids used to be `task_{i+1}` numbered per-batch, so a
    // second `task` call reused `task_1..` and the team drawer (which keys threads
    // by id) silently overwrote the first batch's finished threads — spawned
    // agents "didn't show up". Ids must be globally unique across calls.
    #[test]
    fn subagent_ids_are_unique_across_batches() {
        let mut seen = HashSet::new();
        // Two "batches" of four, as two separate `task` calls would produce.
        for _ in 0..8 {
            let id = next_subagent_id();
            assert!(seen.insert(id.clone()), "duplicate subagent id: {id}");
        }
    }
}
