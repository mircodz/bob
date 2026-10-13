use crate::core::types::{ShellOutcome, ShellOutput, ShellPage, ShellStream, ToolOutput, ToolSpec};
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult, SHELL_STREAM_TEXT_LIMIT};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const JOB_OUTPUT_LIMIT: usize = 256 * 1024;
const MAX_JOBS: usize = 128;
const ARTIFACT_LIMIT: usize = 1024 * 1024;
const ARTIFACT_BUDGET: usize = 8 * ARTIFACT_LIMIT;
const PAGE_LIMIT: usize = 24_000;
const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
pub struct OutputPage {
    pub status: JobStatus,
    pub text: String,
    pub shell: Option<Box<ShellOutput>>,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub total_bytes: usize,
    pub retained_bytes: usize,
    pub truncated: bool,
}

struct RetainedOutput {
    text: String,
    total_bytes: usize,
}

#[derive(Default)]
struct Artifacts {
    entries: VecDeque<(String, RetainedOutput)>,
    bytes: usize,
}

fn retain_text(text: String, limit: usize) -> RetainedOutput {
    let total_bytes = text.len();
    if total_bytes <= limit {
        return RetainedOutput { text, total_bytes };
    }
    let available = limit.saturating_sub(100);
    let mut head = available / 2;
    let mut tail = text.len() - available / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let retained = format!(
        "{}\n... [{} bytes not retained] ...\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    );
    RetainedOutput {
        text: retained,
        total_bytes,
    }
}

fn page(
    status: JobStatus,
    text: &str,
    total_bytes: usize,
    offset: usize,
    limit: usize,
) -> OutputPage {
    let mut start = offset.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = start
        .saturating_add(limit.clamp(4, PAGE_LIMIT))
        .min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    OutputPage {
        status,
        text: text[start..end].to_string(),
        shell: None,
        offset: start,
        next_offset: (end < text.len()).then_some(end),
        total_bytes,
        retained_bytes: text.len(),
        truncated: total_bytes > text.len(),
    }
}

fn retain_job_output(output: ToolOutput) -> (ToolOutput, usize) {
    match output {
        ToolOutput::Text(text) => {
            let retained = retain_text(text, JOB_OUTPUT_LIMIT);
            (retained.text.into(), retained.total_bytes)
        }
        output @ ToolOutput::Read { .. } => {
            let retained = retain_text(output.model_text().into_owned(), JOB_OUTPUT_LIMIT);
            (retained.text.into(), retained.total_bytes)
        }
        ToolOutput::Shell(mut shell) => {
            let stdout_limit = JOB_OUTPUT_LIMIT - shell.stderr.text.len().min(JOB_OUTPUT_LIMIT / 2);
            retain_stream(&mut shell.stdout, stdout_limit);
            retain_stream(
                &mut shell.stderr,
                JOB_OUTPUT_LIMIT - shell.stdout.text.len(),
            );
            let total_bytes = shell
                .stdout
                .total_bytes
                .saturating_add(shell.stderr.total_bytes);
            (
                ToolOutput::Shell(shell),
                total_bytes.min(usize::MAX as u64) as usize,
            )
        }
    }
}

fn retain_stream(stream: &mut ShellStream, limit: usize) {
    if stream.text.len() <= limit {
        return;
    }
    stream.text = retain_text(std::mem::take(&mut stream.text), limit).text;
    stream.truncated = true;
}

fn shell_page(
    id: &str,
    status: JobStatus,
    shell: &ShellOutput,
    offset: usize,
    limit: usize,
) -> OutputPage {
    let stdout_len = shell.stdout.text.len();
    let retained_bytes = stdout_len + shell.stderr.text.len();
    let boundary = |at: usize| {
        if at < stdout_len {
            shell.stdout.text.is_char_boundary(at)
        } else {
            shell.stderr.text.is_char_boundary(at - stdout_len)
        }
    };
    let mut start = offset.min(retained_bytes);
    while !boundary(start) {
        start -= 1;
    }
    let mut end = start
        .saturating_add(limit.clamp(4, PAGE_LIMIT))
        .min(retained_bytes);
    loop {
        while !boundary(end) {
            end -= 1;
        }
        let stdout = &shell.stdout.text[start.min(stdout_len)..end.min(stdout_len)];
        let stderr =
            &shell.stderr.text[start.saturating_sub(stdout_len)..end.saturating_sub(stdout_len)];
        if [stdout, stderr]
            .iter()
            .all(|text| serde_json::to_string(text).unwrap().len() <= SHELL_STREAM_TEXT_LIMIT)
        {
            break;
        }
        end = start + (end - start) / 2;
    }
    let next_offset = (end < retained_bytes).then_some(end);
    let stream_page = |stream: &ShellStream, range: std::ops::Range<usize>| ShellStream {
        text: stream.text[range].to_string(),
        total_bytes: stream.total_bytes,
        truncated: stream.truncated,
        output_id: stream.output_id.clone(),
    };
    let output = ShellOutput {
        exit_code: shell.exit_code,
        signal: shell.signal,
        core_dumped: shell.core_dumped,
        outcome: shell.outcome,
        stdout: stream_page(&shell.stdout, start.min(stdout_len)..end.min(stdout_len)),
        stderr: stream_page(
            &shell.stderr,
            start.saturating_sub(stdout_len)..end.saturating_sub(stdout_len),
        ),
        page: Some(ShellPage {
            job_id: id.to_string(),
            offset: start,
            next_offset,
            retained_bytes,
        }),
    };
    OutputPage {
        status,
        text: String::new(),
        shell: Some(Box::new(output)),
        offset: start,
        next_offset,
        total_bytes: shell
            .stdout
            .total_bytes
            .saturating_add(shell.stderr.total_bytes)
            .min(usize::MAX as u64) as usize,
        retained_bytes,
        truncated: shell.stdout.truncated || shell.stderr.truncated,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

pub struct BgJob {
    pub id: String,
    pub kind: String,
    pub description: String,
    pub status: JobStatus,
    pub output: ToolOutput,
    /// Task handle retained after cancellation so its cleanup can be awaited.
    pub handle: Option<JoinHandle<()>>,
    cancellation: Option<watch::Sender<bool>>,
    total_bytes: usize,
}

#[derive(Clone, Default)]
pub struct JobRegistry {
    jobs: Arc<Mutex<HashMap<String, BgJob>>>,
    order: Arc<Mutex<Vec<String>>>,
    counter: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    draining: Arc<AtomicUsize>,
    artifacts: Arc<Mutex<Artifacts>>,
    cleanup: Arc<tokio::sync::Mutex<()>>,
    mutations: Arc<Mutex<Vec<BlockingMutation>>>,
    mutation_drains: Arc<Mutex<HashMap<String, usize>>>,
    mutation_cleanup: Arc<tokio::sync::Mutex<()>>,
    /// Completion wakes exclude detached root turns to avoid re-driving them.
    finished: Arc<Mutex<Vec<(String, String)>>>,
}

struct PendingJobs {
    jobs: Arc<Mutex<HashMap<String, BgJob>>>,
    handles: Vec<(String, JoinHandle<()>)>,
}

impl PendingJobs {
    async fn settle(&mut self, deadline: tokio::time::Instant) -> bool {
        while let Some((_, handle)) = self.handles.last_mut() {
            if tokio::time::timeout_at(deadline, handle).await.is_err() {
                return false;
            }
            self.handles.pop();
        }
        true
    }
}

impl Drop for PendingJobs {
    fn drop(&mut self) {
        let mut jobs = self.jobs.lock().unwrap();
        for (id, handle) in self.handles.drain(..) {
            handle.abort();
            if let Some(job) = jobs.get_mut(&id) {
                job.handle = Some(handle);
            }
        }
    }
}

struct DrainGuard(Arc<AtomicUsize>);

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct BlockingMutation {
    owner: String,
    handle: JoinHandle<()>,
}

struct MutationDrainGuard {
    owners: Arc<Mutex<HashMap<String, usize>>>,
    owner: String,
}

impl Drop for MutationDrainGuard {
    fn drop(&mut self) {
        let mut owners = self.owners.lock().unwrap();
        let count = owners.get_mut(&self.owner).unwrap();
        *count -= 1;
        if *count == 0 {
            owners.remove(&self.owner);
        }
    }
}

struct PendingMutations {
    registry: Arc<Mutex<Vec<BlockingMutation>>>,
    handles: Vec<BlockingMutation>,
}

impl PendingMutations {
    async fn settle(&mut self, deadline: tokio::time::Instant) -> bool {
        while let Some(mutation) = self.handles.last_mut() {
            if tokio::time::timeout_at(deadline, &mut mutation.handle)
                .await
                .is_err()
            {
                return false;
            }
            self.handles.pop();
        }
        true
    }
}

impl Drop for PendingMutations {
    fn drop(&mut self) {
        self.registry.lock().unwrap().append(&mut self.handles);
    }
}

impl JobRegistry {
    pub fn new() -> Self {
        JobRegistry::default()
    }

    pub fn next_id(&self) -> String {
        format!("job_{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Reserve restored job and artifact ids, which share one counter.
    pub fn reserve_id(&self, id: &str) {
        if let Some(n) = id
            .strip_prefix("job_")
            .or_else(|| id.strip_prefix("output_"))
            .and_then(|n| n.parse::<u64>().ok())
            .filter(|n| *n < u64::MAX)
        {
            self.counter.fetch_max(n, Ordering::Relaxed);
        }
    }

    /// Keep blocking mutations owned even when the calling tool future is dropped.
    pub async fn run_blocking_mutation<F>(&self, owner: &str, work: F) -> ToolResult
    where
        F: FnOnce() -> ToolResult + Send + 'static,
    {
        struct AbortQueued(tokio::task::AbortHandle);
        impl Drop for AbortQueued {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let _abort = {
            let mut mutations = self.mutations.lock().unwrap();
            if self.closed.load(Ordering::Acquire)
                || self.draining.load(Ordering::Acquire) != 0
                || self.mutation_drains.lock().unwrap().contains_key(owner)
            {
                return Err(ToolError::failed("blocking mutation admission is closed"));
            }
            mutations.retain(|mutation| !mutation.handle.is_finished());
            let handle = tokio::task::spawn_blocking(move || {
                let result = crate::core::panic::catch_sync(work)
                    .unwrap_or_else(|_| Err(ToolError::new(
                        crate::tools::registry::ToolErrorKind::Panic,
                        "blocking mutation panicked; outcome unknown, changes may have taken effect; inspect state before retrying",
                    )));
                let _ = sender.send(result);
            });
            let abort = AbortQueued(handle.abort_handle());
            mutations.push(BlockingMutation {
                owner: owner.to_string(),
                handle,
            });
            abort
        };
        receiver
            .await
            .map_err(|_| ToolError::failed("blocking mutation cancelled before starting"))?
    }

    fn pending_mutations(&self, owner: Option<&str>) -> PendingMutations {
        let mut mutations = self.mutations.lock().unwrap();
        let (handles, retained): (Vec<_>, Vec<_>) = std::mem::take(&mut *mutations)
            .into_iter()
            .partition(|mutation| owner.is_none_or(|owner| mutation.owner == owner));
        *mutations = retained;
        for mutation in &handles {
            mutation.handle.abort();
        }
        PendingMutations {
            registry: self.mutations.clone(),
            handles,
        }
    }

    /// Cancel queued mutations and await started mutations for up to five seconds.
    /// False means settlement is incomplete: a started mutation may still commit.
    /// Dropping this wait retains handles for a later drain; no rollback is implied.
    pub async fn cancel_blocking_mutations_and_wait(&self) -> bool {
        let deadline = tokio::time::Instant::now() + CLEANUP_TIMEOUT;
        self.draining.fetch_add(1, Ordering::AcqRel);
        let _guard = DrainGuard(self.draining.clone());
        let Ok(_cleanup) = tokio::time::timeout_at(deadline, self.mutation_cleanup.lock()).await
        else {
            return false;
        };
        let mut pending = self.pending_mutations(None);
        pending.settle(deadline).await
    }

    /// Settle only this agent's mutations after dropping its tool/run future.
    /// False means a started mutation may still commit; unrelated owners keep running.
    pub async fn cancel_blocking_mutations_for_and_wait(&self, owner: &str) -> bool {
        let deadline = tokio::time::Instant::now() + CLEANUP_TIMEOUT;
        *self
            .mutation_drains
            .lock()
            .unwrap()
            .entry(owner.to_string())
            .or_default() += 1;
        let _guard = MutationDrainGuard {
            owners: self.mutation_drains.clone(),
            owner: owner.to_string(),
        };
        let Ok(_cleanup) = tokio::time::timeout_at(deadline, self.mutation_cleanup.lock()).await
        else {
            return false;
        };
        let mut pending = self.pending_mutations(Some(owner));
        pending.settle(deadline).await
    }

    pub fn spawn<F>(&self, id: String, kind: &str, description: String, work: F) -> bool
    where
        F: std::future::Future<Output = (JobStatus, String)> + Send + 'static,
    {
        self.spawn_inner(id, kind, description, work, None)
    }

    pub fn spawn_cancellable<F, Fut>(
        &self,
        id: String,
        kind: &str,
        description: String,
        work: F,
    ) -> bool
    where
        F: FnOnce(watch::Receiver<bool>) -> Fut,
        Fut: std::future::Future<Output = (JobStatus, ToolOutput)> + Send + 'static,
    {
        let (sender, receiver) = watch::channel(false);
        self.spawn_inner(id, kind, description, work(receiver), Some(sender))
    }

    pub fn spawn_batch<F>(&self, work: Vec<(String, String, String, F)>) -> bool
    where
        F: std::future::Future<Output = (JobStatus, String)> + Send + 'static,
    {
        let mut jobs = self.jobs.lock().unwrap();
        let mut ids = std::collections::HashSet::new();
        if self.closed.load(Ordering::Acquire)
            || self.draining.load(Ordering::Acquire) != 0
            || work
                .iter()
                .any(|(id, _, _, _)| jobs.contains_key(id) || !ids.insert(id.clone()))
            || !self.make_room(&mut jobs, work.len())
        {
            return false;
        }
        for (id, kind, description, future) in work {
            self.spawn_locked(&mut jobs, id, kind, description, future, None);
        }
        true
    }

    fn spawn_inner<F, O>(
        &self,
        id: String,
        kind: &str,
        description: String,
        work: F,
        cancellation: Option<watch::Sender<bool>>,
    ) -> bool
    where
        F: std::future::Future<Output = (JobStatus, O)> + Send + 'static,
        O: Into<ToolOutput> + Send + 'static,
    {
        let mut jobs = self.jobs.lock().unwrap();
        if !self.admit(&mut jobs, &id) {
            return false;
        }
        self.spawn_locked(
            &mut jobs,
            id,
            kind.to_string(),
            description,
            work,
            cancellation,
        );
        true
    }

    fn spawn_locked<F, O>(
        &self,
        jobs: &mut HashMap<String, BgJob>,
        id: String,
        kind: String,
        description: String,
        work: F,
        cancellation: Option<watch::Sender<bool>>,
    ) where
        F: std::future::Future<Output = (JobStatus, O)> + Send + 'static,
        O: Into<ToolOutput> + Send + 'static,
    {
        let registry = self.clone();
        let job_id = id.clone();
        let handle = tokio::spawn(async move {
            let (status, result) = crate::core::panic::catch(work)
                .await
                .map(|(status, result)| (status, result.into()))
                .unwrap_or_else(|_| (JobStatus::Failed, "background job panicked".into()));
            registry.finish_output(&job_id, status, result);
        });
        let job = BgJob {
            id: id.clone(),
            kind: kind.to_string(),
            description,
            status: JobStatus::Running,
            output: String::new().into(),
            handle: Some(handle),
            cancellation,
            total_bytes: 0,
        };
        jobs.insert(id.clone(), job);
        self.order.lock().unwrap().push(id);
    }

    fn admit(&self, jobs: &mut HashMap<String, BgJob>, id: &str) -> bool {
        if self.closed.load(Ordering::Acquire)
            || self.draining.load(Ordering::Acquire) != 0
            || jobs.contains_key(id)
        {
            return false;
        }
        self.make_room(jobs, 1)
    }

    fn make_room(&self, jobs: &mut HashMap<String, BgJob>, count: usize) -> bool {
        if count > MAX_JOBS {
            return false;
        }
        let mut order = self.order.lock().unwrap();
        let needed = jobs.len().saturating_add(count).saturating_sub(MAX_JOBS);
        let removable: Vec<_> = order
            .iter()
            .filter(|id| {
                jobs.get(*id).is_some_and(|job| {
                    job.status != JobStatus::Running
                        && job.handle.as_ref().is_none_or(JoinHandle::is_finished)
                })
            })
            .take(needed)
            .cloned()
            .collect();
        if removable.len() < needed {
            return false;
        }
        for removed in removable {
            jobs.remove(&removed);
            order.retain(|id| id != &removed);
            self.finished
                .lock()
                .unwrap()
                .retain(|(id, _)| id != &removed);
        }
        true
    }

    /// Track externally owned work without an abort handle.
    pub fn register_tracking(&self, id: String, kind: &str, description: String) -> bool {
        let mut jobs = self.jobs.lock().unwrap();
        if !self.admit(&mut jobs, &id) {
            return false;
        }
        let job = BgJob {
            id: id.clone(),
            kind: kind.to_string(),
            description,
            status: JobStatus::Running,
            output: String::new().into(),
            handle: None,
            cancellation: None,
            total_bytes: 0,
        };
        jobs.insert(id.clone(), job);
        self.order.lock().unwrap().push(id);
        true
    }

    pub fn finish(&self, id: &str, status: JobStatus, result: String) {
        self.finish_output(id, status, result.into());
    }

    fn finish_output(&self, id: &str, status: JobStatus, mut result: ToolOutput) {
        // Publish state and its wake under the same lock used by output_of, so
        // collecting the result cannot race with a later notification enqueue.
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(id) {
            if j.status == JobStatus::Cancelled && j.cancellation.is_some() {
                if let ToolOutput::Shell(shell) = &mut result {
                    shell.outcome = ShellOutcome::Cancelled;
                }
                (j.output, j.total_bytes) = retain_job_output(result);
                return;
            }
            if j.status != JobStatus::Running {
                return;
            }
            j.status = status;
            (j.output, j.total_bytes) = retain_job_output(result);
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

    pub fn take_finished(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.finished.lock().unwrap())
    }

    /// Cancel a running job, requesting cooperative process cleanup when available.
    pub fn cancel(&self, id: &str) -> bool {
        if let Some(j) = self.jobs.lock().unwrap().get_mut(id) {
            if j.status == JobStatus::Running {
                if let Some(cancel) = &j.cancellation {
                    let _ = cancel.send(true);
                } else if let Some(h) = &j.handle {
                    h.abort();
                }
                j.status = JobStatus::Cancelled;
                return true;
            }
        }
        false
    }

    /// Cancel running work and await its cleanup, including an earlier `cancel`.
    /// Returns whether this call newly cancelled the job. Tracking-only entries
    /// have no task handle, so their underlying work cannot be awaited.
    pub async fn cancel_and_wait(&self, id: &str) -> bool {
        let cancelled = self.cancel(id);
        let deadline = tokio::time::Instant::now() + CLEANUP_TIMEOUT;
        let Ok(_cleanup) = tokio::time::timeout_at(deadline, self.cleanup.lock()).await else {
            return cancelled;
        };
        let handles = self
            .jobs
            .lock()
            .unwrap()
            .get_mut(id)
            .and_then(|job| job.handle.take())
            .map(|handle| (id.to_string(), handle))
            .into_iter()
            .collect();
        let mut pending = PendingJobs {
            jobs: self.jobs.clone(),
            handles,
        };
        pending.settle(deadline).await;
        cancelled
    }

    /// Permanently close admission and drain owned work for up to five seconds.
    /// False means cleanup exceeded the deadline; aborted handles remain retriable.
    pub async fn shutdown(&self) -> bool {
        self.closed.store(true, Ordering::Release);
        self.cancel_all_and_wait().await
    }

    /// Pause admission while cancelling/draining, then allow reuse unless shut down.
    pub async fn cancel_all_and_wait(&self) -> bool {
        let deadline = tokio::time::Instant::now() + CLEANUP_TIMEOUT;
        self.draining.fetch_add(1, Ordering::AcqRel);
        let _guard = DrainGuard(self.draining.clone());
        let Ok(_cleanup) = tokio::time::timeout_at(deadline, self.cleanup.lock()).await else {
            return false;
        };
        let handles = {
            let mut jobs = self.jobs.lock().unwrap();
            let mut handles = Vec::new();
            for job in jobs.values_mut() {
                if job.status == JobStatus::Running {
                    job.status = JobStatus::Cancelled;
                    if let Some(cancel) = &job.cancellation {
                        let _ = cancel.send(true);
                    } else if let Some(handle) = &job.handle {
                        handle.abort();
                    }
                }
                if let Some(handle) = job.handle.take() {
                    handles.push((job.id.clone(), handle));
                }
            }
            handles
        };
        let mut pending = PendingJobs {
            jobs: self.jobs.clone(),
            handles,
        };
        for mutation in self.mutations.lock().unwrap().iter() {
            mutation.handle.abort();
        }
        let settled = pending.settle(deadline).await;
        let Ok(_mutations) = tokio::time::timeout_at(deadline, self.mutation_cleanup.lock()).await
        else {
            return false;
        };
        let mutations_settled = self.pending_mutations(None).settle(deadline).await;
        self.finished.lock().unwrap().clear();
        settled && mutations_settled
    }

    /// Observe settlement without discarding retained output on a failed transition.
    pub fn is_settled(&self) -> bool {
        let Ok(_cleanup) = self.cleanup.try_lock() else {
            return false;
        };
        let Ok(_mutations) = self.mutation_cleanup.try_lock() else {
            return false;
        };
        let jobs = self.jobs.lock().unwrap();
        let mutations = self.mutations.lock().unwrap();
        self.draining.load(Ordering::Acquire) == 0
            && mutations
                .iter()
                .all(|mutation| mutation.handle.is_finished())
            && jobs.values().all(|job| {
                job.status != JobStatus::Running
                    && job.handle.as_ref().is_none_or(JoinHandle::is_finished)
            })
    }

    /// Discard settled context output without reusing IDs or reopening shutdown.
    pub fn clear_settled(&self) -> bool {
        let Ok(_cleanup) = self.cleanup.try_lock() else {
            return false;
        };
        let Ok(_mutations) = self.mutation_cleanup.try_lock() else {
            return false;
        };
        let mut jobs = self.jobs.lock().unwrap();
        let mut mutations = self.mutations.lock().unwrap();
        if self.draining.load(Ordering::Acquire) != 0
            || mutations
                .iter()
                .any(|mutation| !mutation.handle.is_finished())
            || jobs.values().any(|job| {
                job.status == JobStatus::Running
                    || job
                        .handle
                        .as_ref()
                        .is_some_and(|handle| !handle.is_finished())
            })
        {
            return false;
        }
        mutations.clear();
        jobs.clear();
        self.order.lock().unwrap().clear();
        self.finished.lock().unwrap().clear();
        *self.artifacts.lock().unwrap() = Artifacts::default();
        true
    }

    pub fn retain_output(&self, content: String) -> String {
        self.insert_artifact(retain_text(content, ARTIFACT_LIMIT))
    }

    pub(crate) fn retain_complete_output(&self, content: String) -> Option<String> {
        if content.len() > ARTIFACT_BUDGET {
            return None;
        }
        Some(self.insert_artifact(RetainedOutput {
            total_bytes: content.len(),
            text: content,
        }))
    }

    fn insert_artifact(&self, retained: RetainedOutput) -> String {
        let id = format!(
            "output_{}",
            self.counter.fetch_add(1, Ordering::Relaxed) + 1
        );
        let mut artifacts = self.artifacts.lock().unwrap();
        while artifacts.bytes + retained.text.len() > ARTIFACT_BUDGET
            || artifacts.entries.len() >= 128
        {
            if let Some((_, old)) = artifacts.entries.pop_front() {
                artifacts.bytes -= old.text.len();
            }
        }
        artifacts.bytes += retained.text.len();
        artifacts.entries.push_back((id.clone(), retained));
        id
    }

    pub fn output_page(&self, id: &str, offset: usize, limit: usize) -> Option<OutputPage> {
        if id.starts_with("output_") {
            let artifacts = self.artifacts.lock().unwrap();
            let (_, output) = artifacts.entries.iter().find(|(key, _)| key == id)?;
            return Some(page(
                JobStatus::Done,
                &output.text,
                output.total_bytes,
                offset,
                limit,
            ));
        }
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(id)?;
        let output = match &job.output {
            ToolOutput::Text(text) => page(job.status, text, job.total_bytes, offset, limit),
            output @ ToolOutput::Read { .. } => page(
                job.status,
                &output.model_text(),
                job.total_bytes,
                offset,
                limit,
            ),
            ToolOutput::Shell(shell) => shell_page(id, job.status, shell, offset, limit),
        };
        if job.status != JobStatus::Running && output.next_offset.is_none() {
            self.finished
                .lock()
                .unwrap()
                .retain(|(job_id, _)| job_id != id);
        }
        Some(output)
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
        Some((job.status, job.output.model_text().into_owned()))
    }

    /// Snapshot (id, kind, description, status) in creation order.
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
                Some(s) => Ok(format!("{}: {:?}", id, s).into()),
                None => Err(ToolError::not_found(format!("no such job {}", id))),
            };
        }
        let jobs = ctx.jobs.list();
        if jobs.is_empty() {
            return Ok("(no background jobs)".into());
        }
        Ok(jobs
            .iter()
            .map(|(id, kind, desc, status)| {
                format!("{} [{}] {}: {}", id, status.label(), kind, desc)
            })
            .collect::<Vec<_>>()
            .join("\n")
            .into())
    }
}

pub struct JobOutputTool;

#[async_trait]
impl Tool for JobOutputTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_output".to_string(),
            description: "Read a detached job's or retained output artifact's available output by ID. \
                Output pages are bounded; use next_offset with offset/limit to retrieve more. \
                Output is retained in session memory with size/count limits; old entries may be evicted. \
                Use the ID returned when it started; no preceding job_status call is required. \
                This does not wait: a running job may have no output yet. Collect a finished \
                result when needed, rather than polling unchanged output in a loop."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The job or retained output id, e.g. job_1 or output_2." },
                    "offset": { "type": "integer", "minimum": 0, "description": "UTF-8 byte offset in retained output; use next_offset from the previous page." },
                    "limit": { "type": "integer", "minimum": 4, "maximum": 24000, "description": "Maximum page bytes (default 24000)." }
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
        let offset = input["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
        let limit = input["limit"]
            .as_u64()
            .unwrap_or(PAGE_LIMIT as u64)
            .min(PAGE_LIMIT as u64) as usize;
        match ctx.jobs.output_page(id, offset, limit) {
            Some(output) => {
                if let Some(shell) = output.shell {
                    return Ok(ToolOutput::Shell(shell));
                }
                let state = match output.status {
                    JobStatus::Running => "still running",
                    JobStatus::Done => "done",
                    JobStatus::Failed => "failed",
                    JobStatus::Cancelled => "cancelled",
                };
                let body = if output.text.is_empty() {
                    "(no output at this offset)"
                } else {
                    &output.text
                };
                let mut result = format!("{} [{}]:\n{}", id, state, body);
                if output.next_offset.is_some() || output.truncated || offset != 0 {
                    result.push_str(&format!("\n[bytes {}..{} of {} retained; {} original; truncated={}; next_offset={}]",
                        output.offset, output.offset + output.text.len(), output.retained_bytes,
                        output.total_bytes, output.truncated,
                        output.next_offset.map_or_else(|| "none".into(), |n| n.to_string())));
                }
                Ok(result.into())
            }
            None => Err(ToolError::not_found(format!(
                "no retained output for {id}; it may have been evicted"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn capacity_rejection_is_atomic_and_evicts_only_settled_history() {
        let jobs = JobRegistry::new();
        for index in 0..MAX_JOBS {
            assert!(jobs.register_tracking(
                format!("old-{index}"),
                "task",
                format!("work-{index}")
            ));
        }
        jobs.finish("old-0", JobStatus::Done, "first result".into());
        let batch = || {
            (0..2)
                .map(|index| {
                    (
                        format!("new-{index}"),
                        "task".into(),
                        "new work".into(),
                        std::future::pending::<(JobStatus, String)>(),
                    )
                })
                .collect()
        };
        let before = jobs.list();
        assert!(!jobs.spawn_batch(batch()));
        assert_eq!(jobs.list(), before);
        assert_eq!(jobs.finished.lock().unwrap().len(), 1);
        jobs.finish("old-1", JobStatus::Failed, "second result".into());
        assert!(jobs.spawn_batch(batch()));
        assert!(jobs.output_of("old-0").is_none());
        assert!(jobs.output_of("old-1").is_none());
        assert!(jobs.take_finished().is_empty());
        let ids: Vec<_> = jobs.list().into_iter().map(|job| job.0).collect();
        let expected: Vec<_> = (2..MAX_JOBS)
            .map(|index| format!("old-{index}"))
            .chain(["new-0".into(), "new-1".into()])
            .collect();
        assert_eq!(ids, expected);
        assert!(jobs.shutdown().await);
    }

    #[tokio::test]
    async fn cancelled_job_cannot_be_evicted_or_cleared_until_cleanup_finishes() {
        let jobs = JobRegistry::new();
        let (release, gate) = tokio::sync::oneshot::channel();
        assert!(jobs.spawn_cancellable(
            "owned".into(),
            "bash",
            "cleanup".into(),
            move |mut cancel| async move {
                cancel.changed().await.unwrap();
                gate.await.unwrap();
                (JobStatus::Done, "settled output".into())
            }
        ));
        for index in 1..MAX_JOBS {
            assert!(jobs.register_tracking(format!("busy-{index}"), "turn", "busy".into()));
        }
        assert!(jobs.cancel("owned"));
        assert!(!jobs.register_tracking("replacement".into(), "bash", "new".into()));
        assert_eq!(jobs.status_of("owned"), Some(JobStatus::Cancelled));
        assert!(!jobs.is_settled());
        assert!(!jobs.clear_settled());
        release.send(()).unwrap();
        assert!(!jobs.cancel_and_wait("owned").await);
        assert_eq!(
            jobs.output_of("owned"),
            Some((JobStatus::Cancelled, "settled output".into()))
        );
        assert!(jobs.take_finished().is_empty());
        assert!(jobs.register_tracking("replacement".into(), "bash", "new".into()));
        assert!(jobs.status_of("owned").is_none());
        assert_eq!(jobs.list().len(), MAX_JOBS);
        assert!(jobs.shutdown().await);
    }

    #[tokio::test]
    async fn dropping_owner_drain_reopens_only_its_admission_gate() {
        let jobs = JobRegistry::new();
        let cleanup = jobs.mutation_cleanup.lock().await;
        let mut first = Box::pin(jobs.cancel_blocking_mutations_for_and_wait("first"));
        let mut second = Box::pin(jobs.cancel_blocking_mutations_for_and_wait("second"));
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        for owner in ["first", "second"] {
            let error = jobs
                .run_blocking_mutation(owner, || panic!("closed owner must not execute"))
                .await
                .unwrap_err();
            assert_eq!(error.message, "blocking mutation admission is closed");
        }
        assert_eq!(
            jobs.run_blocking_mutation("unrelated", || Ok("allowed".into()))
                .await
                .unwrap()
                .model_text(),
            "allowed"
        );
        drop(first);
        assert_eq!(
            jobs.run_blocking_mutation("first", || Ok("reopened".into()))
                .await
                .unwrap()
                .model_text(),
            "reopened"
        );
        assert!(jobs
            .run_blocking_mutation("second", || panic!("other drain still owns its gate"))
            .await
            .is_err());
        drop(second);
        drop(cleanup);
        assert!(jobs.mutation_drains.lock().unwrap().is_empty());
        assert!(jobs.shutdown().await);
    }

    #[tokio::test]
    async fn global_drain_blocks_every_admission_path_until_dropped() {
        let jobs = JobRegistry::new();
        let cleanup = jobs.cleanup.lock().await;
        let mut drain = Box::pin(jobs.cancel_all_and_wait());
        assert!(futures::poll!(&mut drain).is_pending());
        assert!(!jobs.register_tracking("tracking".into(), "turn", "blocked".into()));
        assert!(
            !jobs.spawn("spawn".into(), "task", "blocked".into(), async {
                panic!("must not run")
            })
        );
        assert!(!jobs.spawn_cancellable(
            "cooperative".into(),
            "bash",
            "blocked".into(),
            |_| async { panic!("must not run") }
        ));
        assert!(!jobs.spawn_batch(vec![(
            "batch".into(),
            "task".into(),
            "blocked".into(),
            async { panic!("must not run") }
        )]));
        assert!(jobs
            .run_blocking_mutation("root", || panic!("must not run"))
            .await
            .is_err());
        assert!(jobs.list().is_empty());
        assert!(!jobs.is_settled());
        assert!(!jobs.clear_settled());
        drop(drain);
        drop(cleanup);
        assert!(jobs.register_tracking("reopened".into(), "turn", "allowed".into()));
        assert!(jobs.shutdown().await);
    }

    struct CleanupGuard {
        jobs: JobRegistry,
        completed: Arc<AtomicBool>,
        notified: Arc<Notify>,
    }

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            assert_eq!(self.jobs.status_of("job_1"), Some(JobStatus::Cancelled));
            self.jobs
                .finish("job_1", JobStatus::Done, "late result".into());
            self.completed.store(true, Ordering::Release);
            self.notified.notify_one();
        }
    }

    async fn assert_cancellation_waits_for_cleanup(cancel_first: bool) {
        let jobs = JobRegistry::new();
        let started = Arc::new(Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let notified = Arc::new(Notify::new());
        let guard = CleanupGuard {
            jobs: jobs.clone(),
            completed: completed.clone(),
            notified: notified.clone(),
        };
        let ready = started.clone();
        jobs.spawn("job_1".into(), "task", "cleanup".into(), async move {
            let _guard = guard;
            ready.notify_one();
            std::future::pending::<(JobStatus, String)>().await
        });
        let timeout = std::time::Duration::from_secs(2);
        tokio::time::timeout(timeout, started.notified())
            .await
            .unwrap();
        if cancel_first {
            assert!(jobs.cancel("job_1"));
            assert!(!jobs.cancel("job_1"));
            assert!(jobs.jobs.lock().unwrap()["job_1"].handle.is_some());
        }
        assert!(!completed.load(Ordering::Acquire));
        assert_eq!(
            tokio::time::timeout(timeout, jobs.cancel_and_wait("job_1"))
                .await
                .unwrap(),
            !cancel_first
        );
        assert!(completed.load(Ordering::Acquire));
        tokio::time::timeout(timeout, notified.notified())
            .await
            .unwrap();
        assert_eq!(
            jobs.output_of("job_1"),
            Some((JobStatus::Cancelled, String::new()))
        );
        assert!(jobs.take_finished().is_empty());
        assert!(!jobs.cancel_and_wait("job_1").await);
    }

    mod lifecycle {
        use super::*;

        #[tokio::test]
        async fn cancel_and_wait_settles_new_and_already_cancelled_jobs() {
            for cancel_first in [false, true] {
                assert_cancellation_waits_for_cleanup(cancel_first).await;
            }
        }

        #[tokio::test]
        async fn cancel_and_wait_preserves_finished_jobs_and_handles_tracking_only_jobs() {
            let jobs = JobRegistry::new();
            assert!(!jobs.cancel_and_wait("missing").await);
            jobs.register_tracking("tracked".into(), "turn", "tracked".into());
            assert!(jobs.cancel_and_wait("tracked").await);
            assert!(!jobs.cancel_and_wait("tracked").await);
            assert_eq!(jobs.status_of("tracked"), Some(JobStatus::Cancelled));
            for status in [JobStatus::Done, JobStatus::Failed] {
                let id = jobs.next_id();
                jobs.register_tracking(id.clone(), "task", "finished".into());
                jobs.finish(&id, status, "original".into());
                assert!(!jobs.cancel_and_wait(&id).await);
                assert_eq!(jobs.take_finished(), vec![(id.clone(), "finished".into())]);
                assert_eq!(jobs.output_of(&id), Some((status, "original".into())));
            }
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

        #[test]
        fn late_completion_does_not_resurrect_cancelled_job() {
            let reg = JobRegistry::new();
            reg.register_tracking("job_1".into(), "task", "first".into());
            assert!(reg.cancel("job_1"));
            reg.finish("job_1", JobStatus::Done, "late result".into());
            assert_eq!(reg.status_of("job_1"), Some(JobStatus::Cancelled));
            assert!(reg.take_finished().is_empty());
        }

        #[tokio::test]
        async fn cancellation_waits_for_cooperative_cleanup_and_keeps_terminal_status() {
            let jobs = JobRegistry::new();
            let started = Arc::new(Notify::new());
            let cleanup = Arc::new(AtomicBool::new(false));
            let ready = started.clone();
            let done = cleanup.clone();
            assert!(jobs.spawn_cancellable(
                "job_1".into(),
                "bash",
                "cleanup".into(),
                move |mut cancel| async move {
                    ready.notify_one();
                    cancel.changed().await.unwrap();
                    tokio::task::yield_now().await;
                    done.store(true, Ordering::Release);
                    (JobStatus::Done, "settled".into())
                }
            ));
            started.notified().await;
            assert!(jobs.cancel_and_wait("job_1").await);
            assert!(cleanup.load(Ordering::Acquire));
            assert_eq!(
                jobs.output_of("job_1"),
                Some((JobStatus::Cancelled, "settled".into()))
            );
            assert!(jobs.jobs.lock().unwrap()["job_1"].handle.is_none());
        }

        #[tokio::test]
        async fn cancelled_drain_preserves_handles_for_later_cleanup() {
            let jobs = JobRegistry::new();
            let started = Arc::new(Notify::new());
            let ready = started.clone();
            jobs.spawn_cancellable(
                "job_1".into(),
                "bash",
                "uncooperative".into(),
                move |mut cancel| async move {
                    cancel.changed().await.unwrap();
                    ready.notify_one();
                    std::future::pending().await
                },
            );
            let registry = jobs.clone();
            let drain = tokio::spawn(async move { registry.cancel_all_and_wait().await });
            started.notified().await;
            drain.abort();
            let _ = drain.await;
            assert!(jobs.jobs.lock().unwrap()["job_1"].handle.is_some());
            assert!(jobs.shutdown().await);
            assert!(jobs.jobs.lock().unwrap()["job_1"].handle.is_none());
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn cooperative_process_cancellation_reaps_the_shell() {
            use crate::core::process::ManagedProcess;
            let jobs = JobRegistry::new();
            let (ready, started) = tokio::sync::oneshot::channel();
            jobs.spawn_cancellable(
                "job_1".into(),
                "bash",
                "process".into(),
                move |mut cancel| async move {
                    let mut process = ManagedProcess::spawn(
                        tokio::process::Command::new("sh").args(["-c", "sleep 60 & wait"]),
                    )
                    .unwrap();
                    ready.send(process.id().unwrap()).unwrap();
                    cancel.changed().await.unwrap();
                    let status = process.terminate_and_wait().await.unwrap();
                    assert!(!status.success());
                    (JobStatus::Cancelled, "reaped".into())
                },
            );
            let pid = tokio::time::timeout(std::time::Duration::from_secs(3), started)
                .await
                .unwrap()
                .unwrap();
            assert!(jobs.cancel_and_wait("job_1").await);
            assert_eq!(
                unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            assert_eq!(
                jobs.output_of("job_1"),
                Some((JobStatus::Cancelled, "reaped".into()))
            );
        }
    }

    mod admission {
        use super::*;

        #[test]
        fn restored_job_ids_are_not_reused() {
            let jobs = JobRegistry::new();
            jobs.reserve_id("job_12");
            jobs.reserve_id("job_3");
            jobs.reserve_id("reviewer");
            assert_eq!(jobs.next_id(), "job_13");
            assert!(jobs.list().is_empty());
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

        #[tokio::test]
        async fn drain_pauses_admission_then_reopens_but_shutdown_stays_closed() {
            let jobs = JobRegistry::new();
            let ready = Arc::new(Notify::new());
            let cleanup_entered = Arc::new(Notify::new());
            let cleanup_release = Arc::new(Notify::new());
            let started = ready.clone();
            let entered = cleanup_entered.clone();
            let release = cleanup_release.clone();
            jobs.spawn_cancellable(
                "job_1".into(),
                "bash",
                "drain".into(),
                move |mut cancel| async move {
                    started.notify_one();
                    cancel.changed().await.unwrap();
                    entered.notify_one();
                    release.notified().await;
                    (JobStatus::Cancelled, "reaped".into())
                },
            );
            ready.notified().await;
            let registry = jobs.clone();
            let drain = tokio::spawn(async move { registry.cancel_all_and_wait().await });
            cleanup_entered.notified().await;
            assert!(!jobs.register_tracking("rejected".into(), "turn", "paused".into()));
            cleanup_release.notify_one();
            assert!(drain.await.unwrap());
            assert!(jobs.register_tracking("accepted".into(), "turn", "resumed".into()));
            assert!(jobs.shutdown().await);
            assert_eq!(jobs.status_of("accepted"), Some(JobStatus::Cancelled));
            assert!(!jobs.register_tracking("closed".into(), "turn", "closed".into()));
            assert!(jobs
                .jobs
                .lock()
                .unwrap()
                .values()
                .all(|job| job.handle.is_none()));
        }

        #[tokio::test]
        async fn batch_rejection_launches_nothing_and_success_registers_everything() {
            let jobs = JobRegistry::new();
            assert!(jobs.register_tracking("existing".into(), "turn", "reserved".into()));
            let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let work = |id: &str| {
                let executed = executed.clone();
                (id.into(), "task".into(), "batch".into(), async move {
                    executed.fetch_add(1, Ordering::SeqCst);
                    (JobStatus::Done, "done".into())
                })
            };
            assert!(!jobs.spawn_batch(vec![work("new"), work("existing")]));
            assert_eq!(jobs.status_of("new"), None);
            assert!(!jobs.spawn_batch(vec![work("dup"), work("dup")]));
            tokio::task::yield_now().await;
            assert_eq!(executed.load(Ordering::SeqCst), 0);
            assert!(jobs.spawn_batch(vec![work("one"), work("two")]));
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while ["one", "two"]
                    .iter()
                    .any(|id| jobs.status_of(id) != Some(JobStatus::Done))
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(executed.load(Ordering::SeqCst), 2);
            assert!(jobs.shutdown().await);
            assert_eq!(executed.load(Ordering::SeqCst), 2);
        }
    }

    mod mutations {
        use super::*;

        #[test]
        fn owner_mutation_cleanup_does_not_abort_another_owners_queued_work() {
            use std::future::Future;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .unwrap();
            runtime.block_on(async {
                let jobs = JobRegistry::new();
                let (release, gate) = std::sync::mpsc::channel::<()>();
                let (started, entered) = tokio::sync::oneshot::channel();
                let mut first = Box::pin(jobs.run_blocking_mutation("first", move || {
                    let _ = started.send(());
                    let _ = gate.recv();
                    Ok("first committed".into())
                }));
                assert!(
                    std::future::poll_fn(|cx| std::task::Poll::Ready(
                        first.as_mut().poll(cx).is_pending()
                    ))
                    .await
                );
                entered.await.unwrap();
                let mut second = Box::pin(
                    jobs.run_blocking_mutation("second", || Ok("second committed".into())),
                );
                assert!(
                    std::future::poll_fn(|cx| std::task::Poll::Ready(
                        second.as_mut().poll(cx).is_pending()
                    ))
                    .await
                );
                drop(first);
                let mut settlement = Box::pin(jobs.cancel_blocking_mutations_for_and_wait("first"));
                assert!(
                    std::future::poll_fn(|cx| std::task::Poll::Ready(
                        settlement.as_mut().poll(cx).is_pending()
                    ))
                    .await
                );
                assert!(!jobs.is_settled());
                assert!(!jobs.clear_settled());
                assert!(jobs.register_tracking(
                    "unrelated".into(),
                    "turn",
                    "still admitted".into()
                ));
                drop(release);
                assert!(settlement.await);
                assert_eq!(second.await.unwrap().model_text(), "second committed");
                assert!(jobs.shutdown().await);
                assert!(jobs.is_settled());
            });
        }

        #[tokio::test]
        async fn timed_out_blocking_mutation_is_still_owned_and_cannot_be_cleared() {
            use std::future::Future;
            let jobs = JobRegistry::new();
            let (release, gate) = std::sync::mpsc::channel::<()>();
            let (started, entered) = tokio::sync::oneshot::channel();
            let committed = Arc::new(AtomicBool::new(false));
            let completed = committed.clone();
            let mut mutation = Box::pin(jobs.run_blocking_mutation("root", move || {
                let _ = started.send(());
                let _ = gate.recv();
                completed.store(true, Ordering::Release);
                Ok("committed".into())
            }));
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    mutation.as_mut().poll(cx).is_pending()
                ))
                .await
            );
            entered.await.unwrap();
            drop(mutation);
            assert!(!jobs.cancel_blocking_mutations_and_wait().await);
            assert!(!committed.load(Ordering::Acquire));
            assert!(!jobs.is_settled());
            assert!(!jobs.clear_settled());
            assert!(jobs.list().is_empty());
            assert!(jobs.take_finished().is_empty());
            drop(release);
            assert!(jobs.shutdown().await);
            assert!(committed.load(Ordering::Acquire));
            assert!(jobs.clear_settled());
        }

        #[tokio::test]
        async fn global_job_drain_allows_job_owned_mutation_cleanup() {
            let jobs = JobRegistry::new();
            let registry = jobs.clone();
            assert!(jobs.spawn_cancellable(
                "job_1".into(),
                "task",
                "child cleanup".into(),
                move |mut cancel| async move {
                    cancel.changed().await.unwrap();
                    assert!(
                        registry
                            .cancel_blocking_mutations_for_and_wait("child")
                            .await
                    );
                    (JobStatus::Cancelled, "settled".into())
                }
            ));
            assert!(tokio::time::timeout(
                std::time::Duration::from_secs(1),
                jobs.cancel_all_and_wait()
            )
            .await
            .unwrap());
        }
    }

    mod output {
        use super::*;

        #[tokio::test]
        async fn cancellation_before_publication_keeps_captured_output_and_cancelled_status() {
            let jobs = JobRegistry::new();
            let (release, ready) = tokio::sync::oneshot::channel();
            assert!(jobs.spawn_cancellable(
                "late".into(),
                "bash",
                "finishing".into(),
                move |_| async move {
                    ready.await.unwrap();
                    (
                        JobStatus::Done,
                        ShellOutput {
                            exit_code: Some(0),
                            stdout: ShellStream {
                                text: "already captured".into(),
                                total_bytes: 16,
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                        .into(),
                    )
                }
            ));
            assert!(jobs.cancel("late"));
            release.send(()).unwrap();
            jobs.cancel_and_wait("late").await;
            let page = jobs.output_page("late", 0, 1024).unwrap();
            assert_eq!(page.status, JobStatus::Cancelled);
            let shell = page.shell.unwrap();
            assert_eq!(shell.outcome, ShellOutcome::Cancelled);
            assert_eq!(shell.exit_code, Some(0));
            assert_eq!(shell.stdout.text, "already captured");
        }

        #[test]
        fn shell_pages_remain_contiguous_after_model_output_bounding() {
            let jobs = JobRegistry::new();
            jobs.register_tracking("shell".into(), "bash", "paging".into());
            let expected = ["\0".repeat(20_000), "日本語".repeat(5_000)];
            let stream = |text: &String| ShellStream {
                text: text.clone(),
                total_bytes: text.len() as u64,
                ..Default::default()
            };
            jobs.finish_output(
                "shell",
                JobStatus::Done,
                ShellOutput {
                    exit_code: Some(0),
                    stdout: stream(&expected[0]),
                    stderr: stream(&expected[1]),
                    ..Default::default()
                }
                .into(),
            );
            let mut recovered = [String::new(), String::new()];
            let mut offset = 0;
            loop {
                let output = ToolOutput::Shell(
                    jobs.output_page("shell", offset, PAGE_LIMIT)
                        .unwrap()
                        .shell
                        .unwrap(),
                );
                let bounded = crate::agent::output::bound_tool_output(output.clone(), &jobs);
                assert_eq!(bounded, output);
                let ToolOutput::Shell(shell) = bounded else {
                    unreachable!()
                };
                recovered[0].push_str(&shell.stdout.text);
                recovered[1].push_str(&shell.stderr.text);
                match shell.page.unwrap().next_offset {
                    Some(next) => {
                        assert!(next > offset);
                        offset = next;
                    }
                    None => break,
                }
            }
            assert_eq!(recovered, expected);
        }

        #[test]
        fn shell_retention_bounds_lossy_text_and_pages_preserve_metadata() {
            use crate::core::types::ShellOutcome;
            let jobs = JobRegistry::new();
            jobs.register_tracking("shell".into(), "bash", "binary".into());
            let stream = ShellStream {
                text: "\u{fffd}".repeat(100_000),
                total_bytes: 100_000,
                truncated: false,
                output_id: None,
            };
            jobs.finish_output(
                "shell",
                JobStatus::Cancelled,
                ShellOutput {
                    signal: Some(9),
                    outcome: ShellOutcome::Cancelled,
                    stdout: stream.clone(),
                    stderr: stream,
                    ..ShellOutput::default()
                }
                .into(),
            );
            let mut offset = 0;
            let mut lengths = [0, 0];
            let mut retained = [String::new(), String::new()];
            loop {
                let page = jobs.output_page("shell", offset, 4093).unwrap();
                assert_eq!(page.status, JobStatus::Cancelled);
                assert!(page.retained_bytes <= JOB_OUTPUT_LIMIT);
                assert_eq!(page.total_bytes, 200_000);
                let shell = page.shell.unwrap();
                assert_eq!(shell.exit_code, None);
                assert_eq!(shell.signal, Some(9));
                assert_eq!(shell.outcome, ShellOutcome::Cancelled);
                assert_eq!(shell.stdout.total_bytes, 100_000);
                assert_eq!(shell.stderr.total_bytes, 100_000);
                assert!(shell.stdout.truncated && shell.stderr.truncated);
                assert!(shell.stdout.text.len() + shell.stderr.text.len() <= 4093);
                lengths[0] += shell.stdout.text.len();
                lengths[1] += shell.stderr.text.len();
                retained[0].push_str(&shell.stdout.text);
                retained[1].push_str(&shell.stderr.text);
                let metadata = shell.page.unwrap();
                assert_eq!(metadata.offset, offset);
                assert_eq!(metadata.next_offset, page.next_offset);
                assert_eq!(metadata.retained_bytes, page.retained_bytes);
                assert_eq!(metadata.job_id, "shell");
                match page.next_offset {
                    Some(next) => {
                        assert_eq!(jobs.finished.lock().unwrap().len(), 1);
                        assert!(next > offset);
                        offset = next;
                    }
                    None => break,
                }
            }
            assert!(lengths.iter().all(|length| *length > 0));
            assert!(retained
                .iter()
                .all(|text| text.contains("bytes not retained")));
            assert!(lengths.iter().sum::<usize>() <= JOB_OUTPUT_LIMIT);
            assert!(jobs.take_finished().is_empty());
        }

        #[test]
        fn shell_page_offsets_span_streams_without_splitting_utf8() {
            let jobs = JobRegistry::new();
            jobs.register_tracking("shell".into(), "bash", "utf8".into());
            jobs.finish_output(
                "shell",
                JobStatus::Done,
                ShellOutput {
                    exit_code: Some(0),
                    stdout: ShellStream {
                        text: "a界".into(),
                        total_bytes: 4,
                        ..ShellStream::default()
                    },
                    stderr: ShellStream {
                        text: "🙂z".into(),
                        total_bytes: 5,
                        ..ShellStream::default()
                    },
                    ..ShellOutput::default()
                }
                .into(),
            );
            let first = jobs.output_page("shell", 0, 7).unwrap().shell.unwrap();
            assert_eq!(first.stdout.text, "a界");
            assert_eq!(first.stderr.text, "");
            assert_eq!(first.page.unwrap().next_offset, Some(4));
            let last = jobs.output_page("shell", 6, 7).unwrap().shell.unwrap();
            assert_eq!(last.stdout.text, "");
            assert_eq!(last.stderr.text, "🙂z");
            assert_eq!(last.page.unwrap().offset, 4);
            assert!(!last.stdout.truncated && !last.stderr.truncated);
            let past = jobs
                .output_page("shell", usize::MAX, 7)
                .unwrap()
                .shell
                .unwrap();
            assert!(past.stdout.text.is_empty() && past.stderr.text.is_empty());
            assert_eq!(past.page.unwrap().offset, 9);
            assert_eq!(past.exit_code, Some(0));
        }

        #[test]
        fn retention_keeps_exact_head_tail_and_counts_original_bytes() {
            let text = format!("head:{}:tail", "x".repeat(200));
            let retained = retain_text(text.clone(), 128);
            assert_eq!(retained.total_bytes, 210);
            assert_eq!(
                retained.text,
                format!(
                    "head:{}\n... [182 bytes not retained] ...\n{}:tail",
                    "x".repeat(9),
                    "x".repeat(9)
                )
            );
            for length in [127, 128] {
                let text = "x".repeat(length);
                let retained = retain_text(text.clone(), 128);
                assert_eq!(retained.text, text);
                assert_eq!(retained.total_bytes, length);
            }
            for limit in [JOB_OUTPUT_LIMIT, ARTIFACT_LIMIT] {
                let input = format!("HEAD{}TAIL", "界🙂".repeat(limit));
                let retained = retain_text(input.clone(), limit);
                assert!(retained.text.starts_with("HEAD"));
                assert!(retained.text.ends_with("TAIL"));
                assert!(retained.text.len() <= limit);
                assert_eq!(retained.total_bytes, input.len());
            }
        }

        #[test]
        fn page_clamps_limits_and_rounds_offsets_without_losing_utf8() {
            let text = "a界🙂z".repeat(PAGE_LIMIT);
            for limit in [0, 1, 3, 4, 7, PAGE_LIMIT, usize::MAX] {
                let bound = limit.clamp(4, PAGE_LIMIT);
                for requested in [0, 1, 2, 3, 4, 5, 7, 8, text.len(), usize::MAX] {
                    let output = page(JobStatus::Failed, &text, text.len() + 10, requested, limit);
                    assert_eq!(output.status, JobStatus::Failed);
                    assert_eq!(output.retained_bytes, text.len());
                    assert_eq!(output.total_bytes, text.len() + 10);
                    assert!(output.truncated);
                    assert!(output.text.len() <= bound);
                    assert!(output.offset <= requested.min(text.len()));
                    assert!(requested.min(text.len()) - output.offset < 4);
                    assert!(text.is_char_boundary(output.offset));
                    let end = output.offset + output.text.len();
                    assert_eq!(output.text, text[output.offset..end]);
                    assert_eq!(output.next_offset, (end < text.len()).then_some(end));
                    if let Some(next) = output.next_offset {
                        assert!(next > output.offset);
                    }
                }
            }
        }

        #[test]
        fn terminal_wake_is_acknowledged_only_when_the_last_page_is_collected() {
            for status in [JobStatus::Done, JobStatus::Failed, JobStatus::Cancelled] {
                let jobs = JobRegistry::new();
                assert!(jobs.register_tracking("paged".into(), "bash", "paged job".into()));
                assert!(jobs.register_tracking("other".into(), "bash", "other job".into()));
                jobs.finish("paged", status, "a界🙂z".into());
                jobs.finish("other", JobStatus::Done, "other result".into());
                assert!(jobs.output_page("missing", 0, 4).is_none());
                let first = jobs.output_page("paged", 0, 4).unwrap();
                assert_eq!(first.text, "a界");
                assert_eq!(first.next_offset, Some(4));
                assert_eq!(jobs.finished.lock().unwrap().len(), 2);
                let second = jobs.output_page("paged", 4, 4).unwrap();
                assert_eq!(second.text, "🙂");
                assert_eq!(second.next_offset, Some(8));
                assert_eq!(jobs.finished.lock().unwrap().len(), 2);
                let last = jobs.output_page("paged", 8, 4).unwrap();
                assert_eq!(last.text, "z");
                assert_eq!(last.status, status);
                assert_eq!(last.next_offset, None);
                assert_eq!(
                    jobs.take_finished(),
                    vec![("other".into(), "other job".into())]
                );
                assert_eq!(jobs.output_of("paged"), Some((status, "a界🙂z".into())));
            }
        }

        #[test]
        fn artifact_budget_evicts_oldest_at_exact_byte_and_count_boundaries() {
            let jobs = JobRegistry::new();
            let ids: Vec<_> = (0..ARTIFACT_BUDGET / ARTIFACT_LIMIT)
                .map(|_| jobs.retain_output("x".repeat(ARTIFACT_LIMIT)))
                .collect();
            assert_eq!(jobs.artifacts.lock().unwrap().bytes, ARTIFACT_BUDGET);
            assert!(jobs.output_page(&ids[0], 0, 4).is_some());
            let newest = jobs.retain_output("y".into());
            assert!(jobs.output_page(&ids[0], 0, 4).is_none());
            for id in &ids[1..] {
                assert!(jobs.output_page(id, 0, 4).is_some());
            }
            assert_eq!(jobs.output_page(&newest, 0, 4).unwrap().text, "y");
            assert_eq!(
                jobs.artifacts.lock().unwrap().bytes,
                ARTIFACT_BUDGET - ARTIFACT_LIMIT + 1
            );
            let jobs = JobRegistry::new();
            let ids: Vec<_> = (0..128)
                .map(|_| jobs.retain_output(String::new()))
                .collect();
            assert!(jobs.output_page(&ids[0], 0, 4).is_some());
            jobs.retain_output(String::new());
            assert!(jobs.output_page(&ids[0], 0, 4).is_none());
            assert!(jobs.output_page(&ids[1], 0, 4).is_some());
            assert_eq!(jobs.artifacts.lock().unwrap().entries.len(), 128);
            assert_eq!(jobs.artifacts.lock().unwrap().bytes, 0);
        }

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
            assert_eq!(result.model_text(), "job_1 [done]:\nverified result");
            assert!(jobs.take_finished().is_empty());
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
            for kind in ["task", "bash"] {
                let reg = JobRegistry::new();
                reg.register_tracking("job_1".into(), kind, "first".into());
                reg.finish("job_1", JobStatus::Done, "result".into());
                reg.finish("job_1", JobStatus::Failed, "late failure".into());
                assert_eq!(reg.take_finished(), vec![("job_1".into(), "first".into())]);
                assert!(reg.take_finished().is_empty());
                assert_eq!(
                    reg.output_of("job_1"),
                    Some((JobStatus::Done, "result".into()))
                );
                reg.finish("job_1", JobStatus::Done, "duplicate".into());
                assert!(reg.take_finished().is_empty());
            }
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
        fn finishing_a_detached_turn_does_not_wake() {
            let reg = JobRegistry::new();
            reg.register_tracking("job_1".into(), "turn", "some prompt".into());
            reg.finish("job_1", JobStatus::Done, "turn finished".into());
            assert!(reg.take_finished().is_empty());
        }

        #[test]
        fn restored_output_ids_do_not_alias_new_artifacts() {
            use crate::core::events::AgentEvent;
            use crate::core::session::new_session;
            use crate::core::session_runtime::reserve_runtime_ids;
            use crate::core::types::Message;
            let jobs = JobRegistry::new();
            let mut session = new_session("mock", "ids".into(), "0".into(), ".".into());
            session
                .messages
                .push(Message::user_text("inspect output_40 and job_12"));
            let mut archived = session.clone();
            archived
                .messages
                .push(Message::user_text("archived output_90"));
            let events = vec![
                AgentEvent::ContextArchive {
                    agent_id: "root".into(),
                    snapshot: Box::new(archived),
                    stored_snapshot: None,
                },
                AgentEvent::ToolResult {
                    agent_id: "root".into(),
                    tool_use_id: "old".into(),
                    output: "output_70".into(),
                    is_error: false,
                },
                AgentEvent::ContextReset {
                    agent_id: "root".into(),
                    epoch: 1,
                },
            ];
            reserve_runtime_ids(&session, &events, &jobs);
            assert_eq!(jobs.retain_output("new".into()), "output_91");
            assert_eq!(jobs.next_id(), "job_92");
            assert!(jobs.output_page("output_40", 0, 100).is_none());
            assert!(jobs.output_page("output_70", 0, 100).is_none());
            assert!(jobs.output_page("output_90", 0, 100).is_none());
            assert!(jobs.is_settled());
            assert!(jobs.output_page("output_91", 0, 100).is_some());
        }

        #[test]
        fn complete_attachments_respect_shared_retention_budget_without_partial_storage() {
            let jobs = JobRegistry::new();
            let small = jobs.retain_output("earlier output".into());
            assert!(jobs
                .retain_complete_output("x".repeat(ARTIFACT_BUDGET + 1))
                .is_none());
            assert_eq!(
                jobs.output_page(&small, 0, 100).unwrap().text,
                "earlier output"
            );
            let complete = jobs
                .retain_complete_output("界".repeat(ARTIFACT_BUDGET / 3))
                .unwrap();
            assert!(jobs.output_page(&small, 0, 100).is_none());
            let page = jobs.output_page(&complete, 0, 100).unwrap();
            assert!(!page.truncated);
            assert_eq!(page.retained_bytes, ARTIFACT_BUDGET / 3 * 3);
            let newest = jobs.retain_output("new output".into());
            assert!(jobs.output_page(&complete, 0, 100).is_none());
            assert_eq!(
                jobs.output_page(&newest, 0, 100).unwrap().text,
                "new output"
            );
            assert!(jobs.artifacts.lock().unwrap().bytes <= ARTIFACT_BUDGET);
        }

        #[test]
        fn artifacts_are_utf8_pageable_and_report_retention_loss() {
            let jobs = JobRegistry::new();
            let text = "a界🙂z".repeat(20);
            let id = jobs.retain_output(text.clone());
            let mut collected = String::new();
            let mut offset = 0;
            loop {
                let output = jobs.output_page(&id, offset, 7).unwrap();
                assert!(output.text.len() <= 7);
                assert!(!output.truncated);
                collected.push_str(&output.text);
                match output.next_offset {
                    Some(next) => {
                        assert!(next > offset);
                        offset = next;
                    }
                    None => break,
                }
            }
            assert_eq!(collected, text);
            assert!(jobs
                .output_page(&id, usize::MAX, usize::MAX)
                .unwrap()
                .text
                .is_empty());
            assert_eq!(jobs.output_page(&id, 3, 4).unwrap().offset, 1);
            let large = jobs.retain_output("界".repeat(ARTIFACT_LIMIT));
            let page = jobs.output_page(&large, 0, PAGE_LIMIT).unwrap();
            assert!(page.truncated);
            assert_eq!(page.total_bytes, ARTIFACT_LIMIT * 3);
            assert!(page.retained_bytes <= ARTIFACT_LIMIT);
        }

        #[tokio::test]
        async fn clearing_settled_context_drops_artifacts_but_preserves_ids_and_shutdown() {
            let jobs = JobRegistry::new();
            let id = jobs.next_id();
            jobs.register_tracking(id.clone(), "bash", "old".into());
            let artifact = jobs.retain_output("old context".into());
            assert!(!jobs.clear_settled());
            jobs.finish(&id, JobStatus::Done, "old job".into());
            assert!(jobs.clear_settled());
            assert!(jobs.output_of(&id).is_none());
            assert!(jobs.output_page(&artifact, 0, 100).is_none());
            assert!(jobs.take_finished().is_empty());
            assert!(jobs.list().is_empty());
            assert_eq!(jobs.next_id(), "job_3");
            assert!(jobs.shutdown().await);
            assert!(jobs.clear_settled());
            assert!(!jobs.register_tracking("closed".into(), "bash", "closed".into()));
        }

        #[test]
        fn job_output_and_history_have_caps() {
            let jobs = JobRegistry::new();
            for _ in 0..MAX_JOBS + 4 {
                let id = jobs.next_id();
                assert!(jobs.register_tracking(id.clone(), "bash", "bounded".into()));
                jobs.finish(&id, JobStatus::Done, "x".repeat(JOB_OUTPUT_LIMIT * 2));
                let page = jobs.output_page(&id, 0, 32).unwrap();
                assert!(page.truncated);
                assert!(page.retained_bytes <= JOB_OUTPUT_LIMIT);
                assert_eq!(page.total_bytes, JOB_OUTPUT_LIMIT * 2);
            }
            assert_eq!(jobs.list().len(), MAX_JOBS);
            assert!(jobs.output_page("job_1", 0, 20).is_none());
        }
    }
}
