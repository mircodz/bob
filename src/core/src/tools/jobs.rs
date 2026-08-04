//! Background jobs: long-running work the model can start and later collect,
//! WITHOUT blocking the current turn. This is deliberately separate from the
//! `task` tool (which spawns short-lived subagents inline) and from the `todo`
//! store (which is just a plan checklist) — a "job" is a detached process whose
//! result is polled back via the `job_status` / `job_output` tools.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    /// Lowercase display label ("running", "done", "failed", "cancelled").
    pub fn label(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

/// A single background job's live state. Output accumulates as the job streams;
/// `result` is set once when it finishes.
pub struct BgJob {
    pub id: String,
    pub kind: String, // "task", "bash", …
    pub description: String,
    pub status: JobStatus,
    /// Streamed/aggregated output so far.
    pub output: String,
    /// Final result text once done (mirrors the tail of `output`).
    pub result: Option<String>,
    /// Abort handle; dropping/aborting cancels the underlying future.
    pub handle: Option<JoinHandle<()>>,
}

/// Shared registry of background jobs. Cloneable handle over shared state, held
/// on the ToolContext so job tools (and the UI) can inspect/mutate jobs.
#[derive(Clone, Default)]
pub struct JobRegistry {
    jobs: Arc<Mutex<HashMap<String, BgJob>>>,
    order: Arc<Mutex<Vec<String>>>,
    counter: Arc<AtomicU64>,
    /// Real background jobs (not detached root turns) that finished since the last
    /// drain, as (id, description). The frontend drains this while the agent is
    /// idle to wake it so it inspects the result instead of sitting there.
    finished: Arc<Mutex<Vec<(String, String)>>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        JobRegistry::default()
    }

    /// Allocate the next job id ("job_1", "job_2", …).
    pub fn next_id(&self) -> String {
        format!("job_{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Register a new running job. `handle` is the spawned future's join handle.
    pub fn register(&self, id: String, kind: &str, description: String, handle: JoinHandle<()>) {
        let job = BgJob {
            id: id.clone(),
            kind: kind.to_string(),
            description,
            status: JobStatus::Running,
            output: String::new(),
            result: None,
            handle: Some(handle),
        };
        self.jobs.lock().unwrap().insert(id.clone(), job);
        self.order.lock().unwrap().push(id);
    }

    /// Register a running job we can only *track*, not abort (e.g. the detached
    /// root turn, whose completion is signalled elsewhere). No abort handle.
    pub fn register_tracking(&self, id: String, kind: &str, description: String) {
        let job = BgJob {
            id: id.clone(),
            kind: kind.to_string(),
            description,
            status: JobStatus::Running,
            output: String::new(),
            result: None,
            handle: None,
        };
        self.jobs.lock().unwrap().insert(id.clone(), job);
        self.order.lock().unwrap().push(id);
    }

    /// Mark a job finished with its final result (or failure message).
    pub fn finish(&self, id: &str, status: JobStatus, result: String) {
        let mut record: Option<(String, String)> = None;
        if let Some(j) = self.jobs.lock().unwrap().get_mut(id) {
            j.status = status;
            if !result.is_empty() {
                if !j.output.is_empty() && !j.output.ends_with('\n') {
                    j.output.push('\n');
                }
                j.output.push_str(&result);
            }
            j.result = Some(result);
            j.handle = None;
            // Queue a wake for real background work only — a detached root turn
            // ("turn") is closed out by the turn_done channel, and waking on it
            // would re-drive the very turn that just ended.
            if j.kind != "turn" {
                record = Some((j.id.clone(), j.description.clone()));
            }
        }
        if let Some(r) = record {
            self.finished.lock().unwrap().push(r);
        }
    }

    /// Take the set of background jobs that finished since the last call, as
    /// (id, description). Used by the frontend to wake an idle agent so it can
    /// collect and act on the result. Draining clears the queue.
    pub fn take_finished(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.finished.lock().unwrap())
    }

    /// Cancel a running job (aborts the future).
    pub fn cancel(&self, id: &str) -> bool {
        if let Some(j) = self.jobs.lock().unwrap().get_mut(id) {
            if j.status == JobStatus::Running {
                if let Some(h) = j.handle.take() {
                    h.abort();
                }
                j.status = JobStatus::Cancelled;
                return true;
            }
        }
        false
    }

    pub fn status_of(&self, id: &str) -> Option<JobStatus> {
        self.jobs.lock().unwrap().get(id).map(|j| j.status)
    }

    /// Read a job's accumulated output (with its current status).
    pub fn output_of(&self, id: &str) -> Option<(JobStatus, String)> {
        self.jobs
            .lock()
            .unwrap()
            .get(id)
            .map(|j| (j.status, j.output.clone()))
    }

    /// Snapshot of all jobs (id, kind, description, status) in creation order —
    /// for the UI panel and `job_status` with no id.
    pub fn list(&self) -> Vec<(String, String, String, JobStatus)> {
        let jobs = self.jobs.lock().unwrap();
        self.order
            .lock()
            .unwrap()
            .iter()
            .filter_map(|id| jobs.get(id))
            .map(|j| {
                (
                    j.id.clone(),
                    j.kind.clone(),
                    j.description.clone(),
                    j.status,
                )
            })
            .collect()
    }
}

/// `job_status`: list background jobs (or one by id) with their state. This is
/// how the model checks on work it detached, without blocking.
pub struct JobStatusTool;

#[async_trait]
impl Tool for JobStatusTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_status".to_string(),
            description: "List background jobs and their status (running/done/failed/cancelled). \
                Pass an `id` to check one job. Use this to see whether detached/background work \
                has finished, then call job_output to read its result."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Optional job id, e.g. job_1." }
                }
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        if let Some(id) = input["id"].as_str() {
            return match ctx.jobs.status_of(id) {
                Some(s) => Ok(format!("{}: {:?}", id, s)),
                None => Err(ToolError::not_found(format!("no such job {}", id))),
            };
        }
        let jobs = ctx.jobs.list();
        if jobs.is_empty() {
            return Ok("(no background jobs)".to_string());
        }
        Ok(jobs
            .iter()
            .map(|(id, kind, desc, status)| {
                format!("{} [{}] {}: {}", id, status.label(), kind, desc)
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// `job_output`: read a background job's accumulated output / final result. This
/// is the "collect" half of the poll/collect model — the job's result re-enters
/// the conversation as a normal tool result on whatever turn the model asks.
pub struct JobOutputTool;

#[async_trait]
impl Tool for JobOutputTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_output".to_string(),
            description: "Read the output (and final result, if finished) of a background job by \
                id. Call job_status first to see which jobs exist."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The job id, e.g. job_1." }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let id = match input["id"].as_str() {
            Some(i) => i,
            None => return Err(ToolError::invalid_input("id is required")),
        };
        match ctx.jobs.output_of(id) {
            Some((status, output)) => {
                let state = match status {
                    JobStatus::Running => "still running",
                    JobStatus::Done => "done",
                    JobStatus::Failed => "failed",
                    JobStatus::Cancelled => "cancelled",
                };
                let body = if output.is_empty() {
                    "(no output yet)"
                } else {
                    &output
                };
                Ok(format!("{} [{}]:\n{}", id, state, body))
            }
            None => Err(ToolError::not_found(format!("no such job {}", id))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finishing_a_bg_job_queues_a_wake() {
        let reg = JobRegistry::new();
        reg.register_tracking("job_1".into(), "bash", "run benchmarks".into());
        reg.finish("job_1", JobStatus::Done, "ok".into());
        let finished = reg.take_finished();
        assert_eq!(
            finished,
            vec![("job_1".to_string(), "run benchmarks".to_string())]
        );
        // Draining clears the queue.
        assert!(reg.take_finished().is_empty());
    }

    #[test]
    fn finishing_a_detached_turn_does_not_wake() {
        let reg = JobRegistry::new();
        // A detached root turn has kind "turn"; its completion is handled by the
        // turn_done channel, so it must NOT queue a self-retriggering wake.
        reg.register_tracking("job_1".into(), "turn", "some prompt".into());
        reg.finish("job_1", JobStatus::Done, "turn finished".into());
        assert!(reg.take_finished().is_empty());
    }
}
