//! Background jobs: long-running work the model can start and later collect,
//! WITHOUT blocking the current turn. This is deliberately separate from the
//! `task` tool (which spawns short-lived subagents inline) and from the `todo`
//! store (which is just a plan checklist) — a "job" is a detached process whose
//! result is polled back via the `job_status` / `job_output` tools.

use futures::FutureExt;
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

    /// Reserve a restored job id so new work cannot overwrite its sidebar history.
    pub fn reserve_id(&self, id: &str) {
        if let Some(n) = id.strip_prefix("job_").and_then(|n| n.parse::<u64>().ok()) {
            self.counter.fetch_max(n, Ordering::Relaxed);
        }
    }

    /// Register and spawn work, publishing its result when it finishes.
    pub fn spawn<F>(&self, id: String, kind: &str, description: String, work: F)
    where
        F: std::future::Future<Output = (JobStatus, String)> + Send + 'static,
    {
        // Keep registration locked until the handle is stored: even an immediate
        // completion must find its job before publishing a result.
        let mut jobs = self.jobs.lock().unwrap();
        let registry = self.clone();
        let job_id = id.clone();
        let handle = tokio::spawn(async move {
            let (status, result) = std::panic::AssertUnwindSafe(work)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| (JobStatus::Failed, "background job panicked".into()));
            registry.finish(&job_id, status, result);
        });
        let job = BgJob {
            id: id.clone(),
            kind: kind.to_string(),
            description,
            status: JobStatus::Running,
            output: String::new(),
            result: None,
            handle: Some(handle),
        };
        jobs.insert(id.clone(), job);
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
        // Publish state and its wake under the same lock used by output_of, so
        // collecting the result cannot race with a later notification enqueue.
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(id) {
            if j.status != JobStatus::Running {
                return;
            }
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
                self.finished
                    .lock()
                    .unwrap()
                    .push((j.id.clone(), j.description.clone()));
            }
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

    /// Read accumulated output and acknowledge a terminal job's pending wake.
    pub fn output_of(&self, id: &str) -> Option<(JobStatus, String)> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(id)?;
        if job.status != JobStatus::Running {
            self.finished
                .lock()
                .unwrap()
                .retain(|(job_id, _)| job_id != id);
        }
        Some((job.status, job.output.clone()))
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
            description: "List detached jobs or inspect one job's state. Omit id to discover \
                jobs; supply a known ID to check its status. This does not wait for completion. \
                If completion is already known, collect with job_output directly instead of \
                making a redundant status call. Avoid repeated polls of unchanged work."
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
            description: "Read a detached job's available output and current state by ID. \
                Use the ID returned when it started; no preceding job_status call is required. \
                This does not wait: a running job may have no output yet. Collect a finished \
                result when needed, rather than polling unchanged output in a loop."
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

    #[tokio::test]
    async fn output_can_be_collected_directly_without_a_status_call() {
        let jobs = JobRegistry::new();
        jobs.register_tracking("job_1".into(), "bash", "check".into());
        jobs.finish("job_1", JobStatus::Done, "verified result".into());
        let ctx = ToolContext {
            cwd: ".".into(),
            files: Arc::new(crate::tools::file_tracker::FileTracker::new()),
            todos: Arc::new(crate::tools::todo::TodoStore::new()),
            jobs: jobs.clone(),
            user_asker: None,
            lsp: None,
            coord: None,
            permissions: None,
        };
        let result = JobOutputTool
            .execute(json!({"id": "job_1"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result, "job_1 [done]:\nverified result");
        assert!(jobs.take_finished().is_empty());
    }

    #[test]
    fn restored_job_ids_are_not_reused() {
        let jobs = JobRegistry::new();
        jobs.reserve_id("job_12");
        jobs.reserve_id("job_3");
        jobs.reserve_id("reviewer");
        assert_eq!(jobs.next_id(), "job_13");
        assert!(jobs.list().is_empty());
    }

    #[tokio::test]
    async fn panicking_work_finishes_as_failed() {
        let reg = JobRegistry::new();
        reg.spawn("job_1".into(), "task", "panicking task".into(), async {
            panic!("fixture panic");
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while reg.status_of("job_1") == Some(JobStatus::Running) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            reg.output_of("job_1"),
            Some((JobStatus::Failed, "background job panicked".into()))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn immediate_jobs_are_registered_before_completion() {
        let reg = JobRegistry::new();
        let mut ids = Vec::new();
        for _ in 0..64 {
            let id = reg.next_id();
            reg.spawn(id.clone(), "task", "instant".into(), async {
                (JobStatus::Done, "result".into())
            });
            ids.push(id);
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while ids
                .iter()
                .any(|id| reg.status_of(id) == Some(JobStatus::Running))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed jobs must not remain running");
        assert_eq!(reg.take_finished().len(), ids.len());
        for id in ids {
            assert_eq!(reg.output_of(&id), Some((JobStatus::Done, "result".into())));
        }
    }

    #[test]
    fn collecting_terminal_output_acknowledges_only_its_wake() {
        for status in [JobStatus::Done, JobStatus::Failed, JobStatus::Cancelled] {
            let reg = JobRegistry::new();
            reg.register_tracking("job_1".into(), "task", "first".into());
            reg.register_tracking("job_2".into(), "task", "second".into());
            reg.finish("job_1", status, "first result".into());
            reg.finish("job_2", JobStatus::Done, "second result".into());
            assert_eq!(
                reg.output_of("job_1"),
                Some((status, "first result".into()))
            );
            assert_eq!(reg.take_finished(), vec![("job_2".into(), "second".into())]);
            assert_eq!(
                reg.output_of("job_1"),
                Some((status, "first result".into()))
            );
        }
    }

    #[test]
    fn polling_running_output_does_not_acknowledge_future_completion() {
        let reg = JobRegistry::new();
        reg.register_tracking("job_1".into(), "task", "first".into());
        assert_eq!(
            reg.output_of("job_1"),
            Some((JobStatus::Running, String::new()))
        );
        reg.finish("job_1", JobStatus::Done, "result".into());
        assert_eq!(reg.status_of("job_1"), Some(JobStatus::Done));
        assert_eq!(reg.take_finished(), vec![("job_1".into(), "first".into())]);
    }

    #[test]
    fn repeated_finish_preserves_the_first_result_and_wake() {
        let reg = JobRegistry::new();
        reg.register_tracking("job_1".into(), "task", "first".into());
        reg.finish("job_1", JobStatus::Done, "result".into());
        reg.finish("job_1", JobStatus::Failed, "late failure".into());
        assert_eq!(reg.take_finished(), vec![("job_1".into(), "first".into())]);
        assert_eq!(
            reg.output_of("job_1"),
            Some((JobStatus::Done, "result".into()))
        );
        reg.finish("job_1", JobStatus::Done, "duplicate".into());
        assert!(reg.take_finished().is_empty());
    }

    #[test]
    fn late_completion_does_not_resurrect_cancelled_job() {
        let reg = JobRegistry::new();
        reg.register_tracking("job_1".into(), "task", "first".into());
        assert!(reg.cancel("job_1"));
        reg.finish("job_1", JobStatus::Done, "late result".into());
        assert_eq!(reg.status_of("job_1"), Some(JobStatus::Cancelled));
        assert!(reg.take_finished().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_collection_does_not_leave_stale_wakes() {
        for _ in 0..64 {
            let reg = JobRegistry::new();
            reg.register_tracking("job_1".into(), "task", "first".into());
            let writer = reg.clone();
            let finish = tokio::spawn(async move {
                writer.finish("job_1", JobStatus::Done, "result".into());
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if reg.output_of("job_1").unwrap().0 == JobStatus::Done {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("job should finish");
            finish.await.unwrap();
            assert!(reg.take_finished().is_empty());
        }
    }

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
