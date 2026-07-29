//! The `task` tool lets the MODEL spawn subagents — parallelism becomes a
//! decision the model can make. By default subagents run inline (blocking) and
//! their answers are returned together. With `background: true` each task is
//! detached as a background *job*; the tool returns immediately with job ids the
//! model later polls via job_status / job_output.

use crate::agent::agent::{Agent, AgentConfig};
use crate::core::events::EventBus;
use crate::core::types::ToolSpec;
use crate::providers::provider::Provider;
use crate::tools::jobs::{JobRegistry, JobStatus};
use crate::tools::registry::{Tool, ToolContext, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

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
}

impl TaskTool {
    /// Build a subagent for one task and return the future that runs it.
    fn make_child(&self, id: String, cwd: String) -> Agent {
        Agent::new(AgentConfig {
            provider: self.provider.clone(),
            tools: self.subagent_tools.clone(),
            bus: self.bus.clone(),
            system: self.subagent_system.clone(),
            cwd,
            max_turns: 20,
            id: Some(id),
            context_window: 200_000,
            compact_threshold: 0.8,
            keep_recent: 6,
            jobs: self.jobs.clone(),
            user_asker: None,
            lsp: self.lsp.clone(),
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
                to block on). Subagents cannot spawn further subagents."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "description": { "type": "string", "description": "Short label for the sub-task." },
                                "prompt": { "type": "string", "description": "Full instructions for the subagent." }
                            },
                            "required": ["description", "prompt"]
                        }
                    },
                    "background": { "type": "boolean", "description": "Run detached as background jobs (default false)." }
                },
                "required": ["tasks"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let tasks = match input["tasks"].as_array() {
            Some(t) if !t.is_empty() => t.clone(),
            _ => return "error: no tasks provided".to_string(),
        };
        let background = input["background"].as_bool().unwrap_or(false);
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
                let child = self.make_child(job_id.clone(), base_cwd.clone());
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
            return format!(
                "started {} background job(s): {}\nUse job_status / job_output to check on them.",
                ids.len(),
                ids.join(", ")
            );
        }

        // Inline (blocking) — run all subagents concurrently and join.
        let mut handles = Vec::new();
        for (i, t) in tasks.into_iter().enumerate() {
            let description = t["description"].as_str().unwrap_or("").to_string();
            let prompt = t["prompt"].as_str().unwrap_or("").to_string();
            let id = format!("task_{}", i + 1);
            self.bus
                .emit(crate::core::events::AgentEvent::SubagentSpawn {
                    parent_id: "root".to_string(),
                    agent_id: id.clone(),
                    task: description.clone(),
                });
            let child = self.make_child(id, base_cwd.clone());
            handles.push(tokio::spawn(async move {
                let mut child = child;
                match child.run(&prompt).await {
                    Ok(out) => format!("### {}\n{}", description, out),
                    Err(e) => format!("### {}\nerror: {}", description, e),
                }
            }));
        }

        let mut results = Vec::new();
        for h in handles {
            match h.await {
                Ok(r) => results.push(r),
                Err(e) => results.push(format!("error: subagent join failed: {}", e)),
            }
        }
        results.join("\n\n")
    }
}
