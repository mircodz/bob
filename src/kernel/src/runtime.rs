use crate::contracts::*;
use crate::jobs::{JobSummary, ProgressUpdate, QueueSnapshot, QueuedInput, ToolProgress};
use crate::live::{LiveProjection, LiveSnapshot};
use crate::model::*;
use crate::store::*;
use crate::workflow_contracts::{WorkflowQuery, WorkflowQueryResult};
use anyhow::{ensure, Context};
use bob_types::{Completion, ContentBlock, Message, Role, StopReason, ToolSpec};
use futures::FutureExt;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

#[path = "runtime_activation.rs"]
mod activation;
#[path = "runtime_policy.rs"]
mod authorization;
#[cfg(test)]
#[path = "runtime_checkpoint_tests.rs"]
mod checkpoint_tests;
#[cfg(test)]
#[path = "runtime_contention_settlement_tests.rs"]
mod contention_settlement_tests;
#[cfg(test)]
#[path = "conversation_runtime_tests.rs"]
mod conversation_tests;
#[path = "runtime_model.rs"]
mod model_runtime;
#[path = "runtime_scheduling.rs"]
mod scheduling;
#[cfg(test)]
#[path = "runtime_shell_handoff_tests.rs"]
mod shell_handoff_tests;
#[path = "runtime_titles.rs"]
mod title_runtime;

#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeOptions {
    pub auto_titles: bool,
}

const WAIT_NOTICE_AFTER: Duration = Duration::from_secs(5);

pub struct Runtime;
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::Sender<Input>,
    live: watch::Receiver<Arc<LiveSnapshot>>,
    reader: crate::read_worker::Reader,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<anyhow::Result<()>>>>>,
}

enum Input {
    Activate(
        SessionId,
        Vec<Command>,
        oneshot::Sender<anyhow::Result<Vec<Receipt>>>,
    ),
    WorkflowQuery(
        SessionId,
        WorkflowQuery,
        oneshot::Sender<anyhow::Result<WorkflowQueryResult>>,
    ),
    Jobs(SessionId, oneshot::Sender<anyhow::Result<Vec<JobSummary>>>),
    Queue(ThreadId, oneshot::Sender<anyhow::Result<Vec<QueuedInput>>>),
    QueueSnapshot(
        ThreadId,
        Vec<CommandId>,
        oneshot::Sender<anyhow::Result<QueueSnapshot>>,
    ),
    ToolArguments(OperationId, oneshot::Sender<anyhow::Result<Value>>),
    JobOutput(
        JobId,
        usize,
        usize,
        oneshot::Sender<anyhow::Result<TextPage>>,
    ),
    Command(Command, oneshot::Sender<anyhow::Result<Receipt>>),
    View(
        Option<ThreadId>,
        Option<i64>,
        usize,
        oneshot::Sender<anyhow::Result<View>>,
    ),
    Transcript(
        Option<ThreadId>,
        Option<i64>,
        usize,
        oneshot::Sender<anyhow::Result<View>>,
    ),
    ApprovalOptions(
        OperationId,
        i64,
        oneshot::Sender<anyhow::Result<ApprovalOptions>>,
    ),
    TranscriptAfter(
        Option<ThreadId>,
        i64,
        usize,
        oneshot::Sender<anyhow::Result<View>>,
    ),
    Artifact(
        ArtifactId,
        usize,
        usize,
        oneshot::Sender<anyhow::Result<String>>,
    ),
    ToolOutput(
        OperationId,
        usize,
        usize,
        oneshot::Sender<anyhow::Result<TextPage>>,
    ),
    Catalog(oneshot::Sender<anyhow::Result<Vec<ModelCatalogEntry>>>),
    RefreshModels(oneshot::Sender<anyhow::Result<Vec<ModelCatalogEntry>>>),
    Sessions(SessionQuery, oneshot::Sender<anyhow::Result<SessionPage>>),
    Session(SessionId, oneshot::Sender<anyhow::Result<SessionSnapshot>>),
    Context(ThreadId, oneshot::Sender<anyhow::Result<ContextSnapshot>>),
    Usage(
        Option<SessionId>,
        oneshot::Sender<anyhow::Result<UsageSnapshot>>,
    ),
    ThreadDetails(
        SessionId,
        oneshot::Sender<anyhow::Result<Vec<ThreadDetails>>>,
    ),
    Controls(ThreadId, oneshot::Sender<anyhow::Result<ControlSnapshot>>),
    Shutdown(oneshot::Sender<anyhow::Result<()>>),
}

impl Runtime {
    pub async fn open(path: &Path, executor: Arc<dyn Executor>) -> anyhow::Result<Handle> {
        Self::open_with_options(path, executor, RuntimeOptions::default()).await
    }
    pub async fn open_paused(path: &Path, executor: Arc<dyn Executor>) -> anyhow::Result<Handle> {
        Self::open_paused_with_options(path, executor, RuntimeOptions::default()).await
    }
    pub async fn open_with_options(
        path: &Path,
        executor: Arc<dyn Executor>,
        options: RuntimeOptions,
    ) -> anyhow::Result<Handle> {
        Self::open_mode(path, executor, false, options, None).await
    }
    pub async fn open_paused_with_options(
        path: &Path,
        executor: Arc<dyn Executor>,
        options: RuntimeOptions,
    ) -> anyhow::Result<Handle> {
        Self::open_mode(path, executor, true, options, None).await
    }
    pub async fn open_claimed_with_options(
        path: &Path,
        executor: Arc<dyn Executor>,
        options: RuntimeOptions,
        claim: crate::SessionClaim,
    ) -> anyhow::Result<Handle> {
        ensure!(
            crate::canonical_database_path(path)? == claim.database_path(),
            "session claim belongs to another database"
        );
        Self::open_mode(path, executor, true, options, Some(claim)).await
    }

    async fn open_mode(
        path: &Path,
        executor: Arc<dyn Executor>,
        paused: bool,
        options: RuntimeOptions,
        claim: Option<crate::SessionClaim>,
    ) -> anyhow::Result<Handle> {
        let reader_path = path.to_owned();
        let path = path.to_owned();
        let (tx, rx) = mpsc::channel(128);
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("bob-kernel".into())
            .spawn(move || {
                let mut store = match Store::open(&path) {
                    Ok(store) => store,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return Ok(());
                    }
                };
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                store.db.busy_timeout(Duration::ZERO)?;
                if let Some(claim) = claim {
                    let adoption = runtime.block_on(async {
                        let start = Instant::now();
                        loop {
                            match store.adopt_claim_ref(&claim, true) {
                                Err(error)
                                    if crate::ownership::busy(&error)
                                        && start.elapsed() < Duration::from_secs(2) =>
                                {
                                    tokio::time::sleep(Duration::from_millis(25)).await;
                                }
                                result => break result,
                            }
                        }
                    });
                    if let Err(error) = adoption {
                        let _ = ready_tx.send(Err(error));
                        return Ok(());
                    }
                    store.claim = Some(claim);
                }
                let mut actor = Actor::new(store, executor)?;
                actor.paused = paused;
                actor.options = options;
                let _ = ready_tx.send(Ok(actor.live.subscribe()));
                runtime.block_on(actor.run(rx))
            })?;
        let live = ready_rx.await.context("kernel initialization stopped")??;
        let reader = crate::read_worker::Reader::open(&reader_path).await?;
        Ok(Handle {
            tx,
            live,
            reader,
            thread: Arc::new(Mutex::new(Some(thread))),
        })
    }
}

impl Handle {
    pub async fn activate_session(
        &self,
        session: SessionId,
        bootstrap: Vec<Command>,
    ) -> anyhow::Result<Vec<Receipt>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::Activate(session, bootstrap, tx))
            .await?;
        rx.await?
    }
    pub async fn workflow_query(
        &self,
        session: SessionId,
        query: WorkflowQuery,
    ) -> anyhow::Result<WorkflowQueryResult> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::WorkflowQuery(session, query, tx))
            .await?;
        rx.await?
    }

    pub async fn jobs(&self, session: SessionId) -> anyhow::Result<Vec<JobSummary>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Jobs(session, tx)).await?;
        rx.await?
    }
    pub async fn queue(&self, thread: ThreadId) -> anyhow::Result<Vec<QueuedInput>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Queue(thread, tx)).await?;
        rx.await?
    }
    pub async fn queue_snapshot(
        &self,
        thread: ThreadId,
        tracked: Vec<CommandId>,
    ) -> anyhow::Result<QueueSnapshot> {
        ensure!(tracked.len() <= 1024, "too many tracked submissions");
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::QueueSnapshot(thread, tracked, tx))
            .await?;
        rx.await?
    }
    pub async fn tool_input(&self, operation: OperationId) -> anyhow::Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::ToolArguments(operation, tx)).await?;
        rx.await?
    }
    pub async fn job_output(
        &self,
        job: JobId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::JobOutput(job, offset, limit, tx))
            .await?;
        rx.await?
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<LiveSnapshot>> {
        self.live.clone()
    }

    pub async fn command(&self, command: Command) -> anyhow::Result<Receipt> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Command(command, tx)).await?;
        rx.await?
    }
    pub async fn view(
        &self,
        thread: Option<ThreadId>,
        before: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<View> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::View(thread, before, limit, tx)).await?;
        rx.await?
    }
    pub async fn transcript(
        &self,
        thread: Option<ThreadId>,
        before: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<View> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::Transcript(thread, before, limit, tx))
            .await?;
        rx.await?
    }
    pub async fn approval_options(
        &self,
        operation: OperationId,
        revision: i64,
    ) -> anyhow::Result<ApprovalOptions> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::ApprovalOptions(operation, revision, tx))
            .await?;
        rx.await?
    }
    pub async fn transcript_after(
        &self,
        thread: Option<ThreadId>,
        after: i64,
        limit: usize,
    ) -> anyhow::Result<View> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::TranscriptAfter(thread, after, limit, tx))
            .await?;
        rx.await?
    }
    pub async fn entry_text(
        &self,
        thread: ThreadId,
        sequence: i64,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        self.reader
            .entry_text(thread, sequence, offset, limit)
            .await
    }
    pub async fn artifact(
        &self,
        id: ArtifactId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<String> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Artifact(id, offset, limit, tx)).await?;
        rx.await?
    }
    pub async fn tool_output(
        &self,
        operation: OperationId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::ToolOutput(operation, offset, limit, tx))
            .await?;
        rx.await?
    }
    pub async fn refresh_models(&self) -> anyhow::Result<Vec<ModelCatalogEntry>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::RefreshModels(tx)).await?;
        rx.await?
    }
    pub async fn catalog(&self) -> anyhow::Result<Vec<ModelCatalogEntry>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Catalog(tx)).await?;
        rx.await?
    }
    pub async fn sessions(&self, query: SessionQuery) -> anyhow::Result<SessionPage> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Sessions(query, tx)).await?;
        rx.await?
    }
    pub async fn session(&self, session: SessionId) -> anyhow::Result<SessionSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Session(session, tx)).await?;
        rx.await?
    }
    pub async fn context(&self, thread: ThreadId) -> anyhow::Result<ContextSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Context(thread, tx)).await?;
        rx.await?
    }
    pub async fn usage(&self, session: SessionId) -> anyhow::Result<UsageSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Usage(Some(session), tx)).await?;
        rx.await?
    }
    pub async fn usage_all(&self) -> anyhow::Result<UsageSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Usage(None, tx)).await?;
        rx.await?
    }
    pub async fn thread_details(&self, session: SessionId) -> anyhow::Result<Vec<ThreadDetails>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::ThreadDetails(session, tx)).await?;
        rx.await?
    }
    pub async fn controls(&self, thread: ThreadId) -> anyhow::Result<ControlSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Input::Controls(thread, tx)).await?;
        rx.await?
    }
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Input::Shutdown(tx)).await.is_ok() {
            rx.await.unwrap_or(Ok(()))?;
        }
        self.reader.close().await?;
        let thread = self.thread.lock().unwrap().take();
        if let Some(thread) = thread {
            tokio::task::spawn_blocking(move || {
                thread
                    .join()
                    .map_err(|_| anyhow::anyhow!("kernel thread panicked"))?
            })
            .await??;
        }
        Ok(())
    }
}

#[derive(Clone)]
enum Output {
    Context(String),
    Prepared(Box<PreparedTool>),
    Model(Completion),
    Tool(ToolOutcome, Option<String>),
}
struct Finished {
    id: OperationId,
    result: anyhow::Result<Output>,
}
struct Worker {
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    cancelled_at: Option<Instant>,
}
struct Actor {
    store: Store,
    executor: Arc<dyn Executor>,
    models: Vec<ModelInfo>,
    model_catalog: Vec<ModelCatalogEntry>,
    tools: HashMap<String, ToolDescriptor>,
    validators: HashMap<String, jsonschema::Validator>,
    workers: HashMap<OperationId, Worker>,
    waiting: HashMap<OperationId, (Instant, bool)>,
    options: RuntimeOptions,
    title_worker: Option<title_runtime::TitleWorker>,
    title_tx: mpsc::Sender<title_runtime::TitleFinished>,
    title_rx: mpsc::Receiver<title_runtime::TitleFinished>,
    progress_tx: mpsc::Sender<ProgressUpdate>,
    progress_rx: mpsc::Receiver<ProgressUpdate>,
    live: LiveProjection,
    archive_token: String,
    run_cursor: i64,
    paused: bool,
    selected_session: Option<SessionId>,
    published_session_sequence: i64,
    error: Option<String>,
}
struct Op {
    id: OperationId,
    run: RunId,
    thread: ThreadId,
    name: String,
    kind: String,
    input: Value,
    state: String,
    prepared: Option<String>,
    revision: i64,
}

impl Actor {
    fn refresh_models(&mut self) -> anyhow::Result<Vec<ModelCatalogEntry>> {
        let models = self.executor.models();
        ensure!(models.len() <= 4096, "model catalog exceeds 4096 entries");
        let mut ids = std::collections::HashSet::new();
        let mut catalog = Vec::with_capacity(models.len());
        for model in &models {
            ensure!(
                !model.id.trim().is_empty()
                    && model.id.len() <= 1024
                    && model.context_window > 0
                    && ids.insert(model.id.clone()),
                "invalid or duplicate model catalog entry"
            );
            let capabilities = self.executor.model_capabilities(&model.id);
            ensure!(
                capabilities.max_output_tokens > 0
                    && capabilities.preferred_output_tokens > 0
                    && capabilities.preferred_output_tokens <= capabilities.max_output_tokens
                    && capabilities.max_input_tokens != Some(0),
                "invalid model output capability"
            );
            catalog.push(ModelCatalogEntry {
                model: model.clone(),
                capabilities,
            });
        }
        self.models = models;
        self.model_catalog = catalog.clone();
        Ok(catalog)
    }
    fn model_capabilities(&self, model: &str) -> ModelCapabilities {
        self.model_catalog
            .iter()
            .find(|entry| entry.model.id == model)
            .map(|entry| entry.capabilities.clone())
            .unwrap_or_default()
    }
    #[cfg(test)]
    fn new_owned(mut store: Store, executor: Arc<dyn Executor>) -> anyhow::Result<Self> {
        if store.claim.is_none() {
            let session = SessionId(store.db.query_row(
                "SELECT id FROM sessions ORDER BY rowid LIMIT 1",
                [],
                |r| r.get(0),
            )?);
            let path = store
                .path
                .as_deref()
                .context("test store has no filesystem identity")?;
            let claim = crate::SessionClaim::acquire(path, session)?;
            store.adopt_claim(claim, false)?;
        }
        Self::new(store, executor)
    }

    fn new(store: Store, executor: Arc<dyn Executor>) -> anyhow::Result<Self> {
        let models = Vec::new();
        let mut tools = HashMap::new();
        let mut validators = HashMap::new();
        for tool in executor
            .tools()
            .into_iter()
            .chain(control_tools())
            .chain(crate::controls::tools())
            .chain(crate::jobs::tools())
            .chain(crate::delegation::tools())
            .chain(crate::coordination::tools())
            .chain(crate::history::tools())
            .chain(crate::conversations::tools())
            .chain(crate::workflow::tools())
        {
            let name = tool.spec.name.clone();
            ensure!(
                !tools.contains_key(&name),
                "duplicate tool registration {name}"
            );
            validators.insert(
                name.clone(),
                jsonschema::validator_for(&tool.spec.input_schema)
                    .map_err(|e| anyhow::anyhow!("invalid tool schema: {e}"))?,
            );
            tools.insert(name, tool);
        }
        let selected_session = store.claim.as_ref().map(|claim| claim.session().clone());
        let sequence = durable_sequence(&store.db)?;
        let (progress_tx, progress_rx) = mpsc::channel(64);
        let (title_tx, title_rx) = mpsc::channel(1);
        let mut actor = Self {
            live: LiveProjection::new(sequence),
            archive_token: uuid::Uuid::new_v4().to_string(),
            store,
            executor,
            models,
            model_catalog: Vec::new(),
            tools,
            validators,
            workers: HashMap::new(),
            waiting: HashMap::new(),
            options: RuntimeOptions::default(),
            title_worker: None,
            title_tx,
            title_rx,
            progress_tx,
            progress_rx,
            run_cursor: 0,
            paused: false,
            selected_session,
            published_session_sequence: -1,
            error: None,
        };
        actor.refresh_models()?;
        Ok(actor)
    }

    async fn run(mut self, mut inputs: mpsc::Receiver<Input>) -> anyhow::Result<()> {
        let (done_tx, mut done_rx) = mpsc::channel::<Finished>(64);
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_publish = Instant::now();
        let mut closing: Option<Instant> = None;
        let mut shutdown: Vec<oneshot::Sender<anyhow::Result<()>>> = Vec::new();
        let mut inputs_open = true;
        let mut pending_commands = std::collections::VecDeque::new();
        let mut pending_results = std::collections::VecDeque::new();
        let mut pending_progress = std::collections::VecDeque::new();
        let mut pending_title = None;
        let mut next_retry = Instant::now();
        loop {
            tokio::select! {
                input=inputs.recv(), if inputs_open=>match input {
                    Some(Input::WorkflowQuery(session,query,reply))=>{let _=reply.send(crate::workflow_view::query(&self.store.db,&session,&query));}
                    Some(Input::Jobs(session,reply))=>{let _=reply.send(crate::jobs::list(&self.store.db,&session));}
                    Some(Input::Queue(thread,reply))=>{let _=reply.send(crate::jobs::queue(&self.store.db,&thread));}
                    Some(Input::QueueSnapshot(thread,tracked,reply))=>{let _=reply.send(crate::jobs::queue_snapshot(&self.store.db,&thread,&tracked));}
                    Some(Input::ToolArguments(operation,reply))=>{let _=reply.send(crate::jobs::tool_input(&self.store.db,&operation));}
                    Some(Input::JobOutput(job,offset,limit,reply))=>{let _=reply.send(crate::jobs::page(&self.store.db,&job,offset,limit));}
                    Some(input @ Input::Activate(..)) | Some(input @ Input::Command(..)) => {
                        if closing.is_some() {
                            match input {
                                Input::Activate(_,_,reply) => {let _ = reply.send(Err(anyhow::anyhow!("kernel is closing")));}
                                Input::Command(_,reply) => {let _ = reply.send(Err(anyhow::anyhow!("kernel is closing")));}
                                _ => unreachable!(),
                            }
                        } else if pending_commands.len() < 128 {
                            pending_commands.push_back((input, Instant::now()));
                        } else {
                            match input {
                                Input::Activate(_,_,reply) => {let _ = reply.send(Err(anyhow::anyhow!("kernel command queue is full")));}
                                Input::Command(_,reply) => {let _ = reply.send(Err(anyhow::anyhow!("kernel command queue is full")));}
                                _ => unreachable!(),
                            }
                        }
                    }
                    Some(Input::View(thread,before,limit,reply))=>{let _=reply.send(self.store.view(thread,before,limit,self.error.clone()));}
                    Some(Input::Transcript(thread,before,limit,reply))=>{let _=reply.send(self.store.transcript(thread,before,limit,self.error.clone()));}
                    Some(Input::ApprovalOptions(operation,revision,reply))=>{let _=reply.send(crate::approvals::options(&self.store.db,&operation,revision));}
                    Some(Input::TranscriptAfter(thread,after,limit,reply))=>{let _=reply.send(self.store.transcript_after(thread,after,limit,self.error.clone()));}
                    Some(Input::Artifact(id,offset,limit,reply))=>{let _=reply.send(self.store.artifact_page(&id,offset,limit));}
                    Some(Input::ToolOutput(operation,offset,limit,reply))=>{let _=reply.send(self.store.tool_output(&operation,offset,limit));}
                    Some(Input::RefreshModels(reply))=>{let _=reply.send(self.refresh_models());}
                    Some(Input::Catalog(reply))=>{let _=reply.send(Ok(self.model_catalog.clone()));}
                    Some(Input::Sessions(query,reply))=>{let _=reply.send(crate::sessions::sessions(&self.store.db,query));}
                    Some(Input::Session(id,reply))=>{let _=reply.send(crate::sessions::session(&self.store.db,&id));}
                    Some(Input::Context(thread,reply))=>{let _=reply.send(self.context_snapshot(&thread));}
                    Some(Input::Usage(id,reply))=>{let _=reply.send(crate::sessions::usage(&self.store.db,id.as_ref()));}
                    Some(Input::ThreadDetails(id,reply))=>{let _=reply.send(crate::sessions::thread_details(&self.store.db,&id));}
                    Some(Input::Controls(thread,reply))=>{let _=reply.send(crate::controls::snapshot(&self.store.db,&thread));}
                    Some(Input::Shutdown(reply))=>{
                        shutdown.retain(|reply| !reply.is_closed());
                        let start = closing.get_or_insert_with(Instant::now);
                        if start.elapsed() >= Duration::from_secs(5) {
                            let _ = reply.send(Err(anyhow::anyhow!("kernel shutdown is still settling; session ownership retained")));
                        } else if shutdown.len() < 128 {
                            shutdown.push(reply);
                        } else {
                            let _ = reply.send(Err(anyhow::anyhow!("kernel shutdown reply queue is full; shutdown continues")));
                        }
                    }
                    None=>{
                        inputs_open = false;
                        closing.get_or_insert_with(Instant::now);
                    },
                },
                Some(finished)=done_rx.recv()=>{
                    if self.workers.contains_key(&finished.id) { pending_results.push_back(finished); }
                }
                Some(finished)=self.title_rx.recv()=>{ pending_title=Some(finished); }
                Some(progress)=self.progress_rx.recv(), if pending_progress.len() < 64=>{ pending_progress.push_back(progress); }
                _=tick.tick()=>{}
            }
            if closing.is_some() {
                for (input, _) in pending_commands.drain(..) {
                    match input {
                        Input::Activate(_, _, reply) => {
                            let _ = reply.send(Err(anyhow::anyhow!("kernel is closing")));
                        }
                        Input::Command(_, reply) => {
                            let _ = reply.send(Err(anyhow::anyhow!("kernel is closing")));
                        }
                        _ => unreachable!(),
                    }
                }
            }
            if Instant::now() >= next_retry {
                next_retry = Instant::now() + Duration::from_millis(25);
                if let Some((input, since)) = pending_commands.pop_front() {
                    match input {
                        Input::Command(command, reply) => {
                            let result = if let Some(error) = &self.error {
                                Err(anyhow::anyhow!(
                                    "kernel halted after storage/runtime failure: {error}"
                                ))
                            } else {
                                self.accept_command(&command)
                            };
                            if result.as_ref().err().is_some_and(crate::ownership::busy)
                                && since.elapsed() < Duration::from_secs(2)
                            {
                                pending_commands
                                    .push_front((Input::Command(command, reply), since));
                            } else {
                                let _ = reply.send(result);
                            }
                        }
                        Input::Activate(session, bootstrap, reply) => {
                            let result = self.activate_session(session.clone(), bootstrap.clone());
                            if result.as_ref().err().is_some_and(crate::ownership::busy)
                                && since.elapsed() < Duration::from_secs(2)
                            {
                                pending_commands.push_front((
                                    Input::Activate(session, bootstrap, reply),
                                    since,
                                ));
                            } else {
                                let _ = reply.send(result);
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                if let Some(finished) = pending_results.front() {
                    match self.finish_ref(finished) {
                        Ok(()) => {
                            let finished = pending_results.pop_front().unwrap();
                            if let Some(worker) = self.workers.remove(&finished.id) {
                                let _ = worker.task.await;
                            }
                        }
                        Err(error) if crate::ownership::busy(&error) => {}
                        Err(error) => {
                            self.error = Some(format!("{error:#}"));
                        }
                    }
                }
                if let Some(finished) = pending_title.as_ref() {
                    match self.finish_title_ref(finished) {
                        Ok(()) => {
                            pending_title = None;
                        }
                        Err(error) if crate::ownership::busy(&error) => {}
                        Err(error) => {
                            self.error = Some(format!("{error:#}"));
                        }
                    }
                }
                if let Some(progress) = pending_progress.front() {
                    let result = self
                        .store
                        .ensure_owner()
                        .and_then(|()| crate::jobs::append_data(&mut self.store.db, progress));
                    if !result.as_ref().err().is_some_and(crate::ownership::busy) {
                        if let Err(error) = &result {
                            self.error = Some(format!("{error:#}"));
                        }
                        let progress = pending_progress.pop_front().unwrap();
                        let _ = progress.reply.send(result);
                    }
                }
            }
            let mut cancellations_settled = true;
            if let Err(error) = self.cancel_stopped(closing.is_some() || self.error.is_some()) {
                cancellations_settled = false;
                if !crate::ownership::busy(&error) {
                    self.error = Some(format!("{error:#}"));
                }
            }
            if let Err(error) = self.cancel_titles(closing.is_some() || self.error.is_some()) {
                cancellations_settled = false;
                if !crate::ownership::busy(&error) {
                    self.error = Some(format!("{error:#}"));
                }
            }
            if let Some(error) = &self.error {
                self.live.fail(error);
            }
            if last_publish.elapsed() >= Duration::from_millis(50) {
                self.live.collect();
                last_publish = Instant::now();
            }
            if let Err(error) = self.publish_live() {
                if !crate::ownership::busy(&error) {
                    self.error = Some(format!("{error:#}"));
                    self.live.fail(&format!("{error:#}"));
                }
            }
            if let Some(start) = closing {
                if cancellations_settled
                    && self.workers.is_empty()
                    && self.title_worker.is_none()
                    && pending_results.is_empty()
                    && pending_progress.is_empty()
                    && pending_title.is_none()
                {
                    break;
                }
                if start.elapsed() >= Duration::from_secs(5) {
                    for reply in shutdown.drain(..) {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "kernel shutdown is still settling; session ownership retained"
                        )));
                    }
                }
            } else if !self.paused && self.error.is_none() {
                if let Err(error) = self.schedule(&done_tx).and_then(|()| self.schedule_title()) {
                    if !crate::ownership::busy(&error) {
                        self.error = Some(format!("{error:#}"));
                    }
                }
            }
        }
        self.publish_live()?;
        // Reopening distinguishes unfinished dispatch from committed outcomes.
        drop(self.store);
        for reply in shutdown {
            let _ = reply.send(Ok(()));
        }
        Ok(())
    }

    fn publish_live(&mut self) -> anyhow::Result<()> {
        let sequence = if let Some(session) = self.selected_session.as_ref() {
            let scoped = crate::ownership::sequence(&self.store.db, session)?;
            if scoped != self.published_session_sequence {
                self.published_session_sequence = scoped;
                durable_sequence(&self.store.db)?
            } else {
                self.live.subscribe().borrow().sequence
            }
        } else {
            self.live.subscribe().borrow().sequence
        };
        self.live.publish(sequence);
        Ok(())
    }

    fn cancel_stopped(&mut self, all: bool) -> anyhow::Result<()> {
        if all {
            for (id, worker) in &mut self.workers {
                self.live.stop(id);
                worker.cancelled_at.get_or_insert_with(Instant::now);
                let _ = worker.cancel.send(true);
            }
        }
        if self.selected_session.is_none() {
            return Ok(());
        }
        self.store.ensure_owner()?;
        ensure!(
            self.store.claim.as_ref().map(|owner| owner.session())
                == self.selected_session.as_ref(),
            "selected session is not owned"
        );
        if all {
            self.store.db.execute(
                "UPDATE jobs SET state='stopping' WHERE state IN ('queued','running') AND session=?1",
                [self.selected_session.as_ref()],
            )?;
        }
        let mut expired = Vec::new();
        for (id, worker) in &mut self.workers {
            let active: bool = self.store.db.query_row(
                "SELECT r.state='active' AND o.epoch=c.epoch AND o.profile_revision=t.profile_revision FROM operations o JOIN runs r ON r.id=o.run JOIN contexts c ON c.thread=o.thread JOIN threads t ON t.id=o.thread WHERE o.id=?1",
                [id],
                |r| r.get(0),
            )?;
            let job_active: Option<bool> = self.store.db.query_row("SELECT j.state='running' AND j.epoch=c.epoch FROM jobs j JOIN contexts c ON c.thread=j.thread WHERE j.operation=?1 AND j.kind='shell' AND j.background=1",[id],|r|r.get(0)).optional()?;
            if all || !job_active.unwrap_or(active) {
                self.live.stop(id);
                worker.cancelled_at.get_or_insert_with(Instant::now);
                let _ = worker.cancel.send(true);
            }
            if worker.task.is_finished() {
                continue;
            }
            if worker
                .cancelled_at
                .is_some_and(|at| at.elapsed() > Duration::from_secs(5))
            {
                expired.push(id.clone());
            }
        }
        for id in expired {
            self.finish_ref(&Finished {
                id: id.clone(),
                result: Err(anyhow::anyhow!("worker did not settle after cancellation")),
            })?;
            if let Some(worker) = self.workers.remove(&id) {
                worker.task.abort();
            }
        }
        Ok(())
    }

    fn schedule(&mut self, done: &mpsc::Sender<Finished>) -> anyhow::Result<()> {
        if self.selected_session.is_none() {
            return Ok(());
        }
        self.store.ensure_owner()?;
        ensure!(
            self.store.claim.as_ref().map(|owner| owner.session())
                == self.selected_session.as_ref(),
            "selected session is not owned"
        );
        let Some(session) = self.selected_session.as_ref() else {
            return Ok(());
        };
        let runnable: bool = self.store.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs r JOIN threads t ON t.id=r.thread WHERE t.session=?1 AND r.state IN ('active','stopping')) OR EXISTS(SELECT 1 FROM submissions s JOIN threads t ON t.id=s.thread WHERE t.session=?1 AND t.held=0 AND s.state='queued') OR EXISTS(SELECT 1 FROM task_groups g JOIN operations o ON o.id=g.operation JOIN threads t ON t.id=o.thread WHERE t.session=?1 AND g.state='running') OR EXISTS(SELECT 1 FROM workflows w JOIN operations o ON o.id=w.operation JOIN threads t ON t.id=o.thread WHERE t.session=?1 AND w.result IS NULL)",
            [session], |r| r.get(0),
        )?;
        if !runnable {
            return Ok(());
        }
        let tx = self
            .store
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::delegation::advance_groups(&tx, session)?;
        crate::workflow::cleanup(&tx, session, &self.models)?;
        let ready: Vec<(String, String)> = {
            let mut stmt=tx.prepare("SELECT t.id,s.id FROM threads t JOIN submissions s ON s.rowid=(SELECT MIN(x.rowid) FROM submissions x WHERE x.thread=t.id AND x.state='queued') WHERE t.held=0 AND (?1 IS NOT NULL AND t.session=?1) AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.thread=t.id AND o.state='unknown') AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.thread=t.id AND r.state IN ('active','interrupted','stopping')) ORDER BY s.rowid LIMIT 16")?;
            let rows = stmt.query_map([self.selected_session.as_ref()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        for (thread, submission) in ready {
            let run = RunId::new();
            let thread = ThreadId(thread);
            tx.execute(
                "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
                params![run, thread],
            )?;
            tx.execute("UPDATE submissions SET state='queued',owner_run=?1 WHERE thread=?2 AND state='retained' AND source='coordination' AND EXISTS(SELECT 1 FROM submissions WHERE id=?3 AND source='human')", params![run,thread,submission])?;
            consume(&tx, &CommandId(submission), &run)?;
            tx.execute(
                "UPDATE agents SET state='running',reported=0,outcome=NULL WHERE thread=?1",
                [&thread],
            )?;
            change(&tx, Some(&thread), "run_started", &run.0)?;
        }
        tx.commit()?;
        let runs: Vec<(i64, String, String)> = {
            let mut stmt=self.store.db.prepare("SELECT ordinal,id,thread FROM runs WHERE state IN ('active','stopping') AND ordinal>?1 AND (?2 IS NOT NULL AND thread IN (SELECT id FROM threads WHERE session=?2)) ORDER BY ordinal LIMIT 32")?;
            let rows = stmt.query_map(params![self.run_cursor, self.selected_session], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        if runs.is_empty() {
            self.run_cursor = 0;
        }
        for (ordinal, run, thread) in runs {
            self.run_cursor = ordinal;
            let run = RunId(run);
            let thread = ThreadId(thread);
            let state: String =
                self.store
                    .db
                    .query_row("SELECT state FROM runs WHERE id=?1", [&run], |row| {
                        row.get(0)
                    })?;
            if !matches!(state.as_str(), "active" | "stopping") {
                continue;
            }
            if state == "stopping" {
                let running:i64=self.store.db.query_row("SELECT COUNT(*) FROM operations WHERE run=?1 AND state IN ('started','preparing')",[&run],|r|r.get(0))?;
                if running == 0 && !crate::lifecycle::pending(&self.store.db, &run)? {
                    let tx = self
                        .store
                        .db
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    let pending: Vec<String> = {
                        let mut stmt=tx.prepare("SELECT id FROM operations WHERE run=?1 AND kind='tool' AND state IN ('pending','ready','approval','waiting')")?;
                        let rows = stmt.query_map([&run], |r| r.get(0))?;
                        rows.collect::<Result<_, _>>()?
                    };
                    for id in pending {
                        let id = OperationId(id);
                        finish_tool_entry(&tx, &id, "Cancelled before execution.", true)?;
                        tx.execute("UPDATE operations SET state='cancelled' WHERE id=?1", [id])?;
                    }
                    tx.execute("UPDATE decisions SET state='cancelled' WHERE operation IN (SELECT id FROM operations WHERE run=?1)",[&run])?;
                    tx.execute("UPDATE operations SET state='cancelled' WHERE run=?1 AND kind='model' AND state='waiting'", [&run])?;
                    let failure: Option<String> =
                        tx.query_row("SELECT failure FROM runs WHERE id=?1", [&run], |r| r.get(0))?;
                    settle_run(
                        &tx,
                        &run,
                        if failure.is_some() {
                            "failed"
                        } else {
                            "cancelled"
                        },
                        failure
                            .as_deref()
                            .unwrap_or("Stopped; outputs and held input remain inspectable."),
                    )?;
                    tx.commit()?;
                }
                continue;
            }
            if self.workers.len() >= 16 {
                break;
            }
            if let Some(op) = self.next_operation(&run)? {
                if self.workers.contains_key(&op.id)
                    || matches!(op.state.as_str(), "started" | "preparing" | "approval")
                {
                    continue;
                }
                if op.kind == "model" && op.state == "waiting" {
                    let tx = self
                        .store
                        .db
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    crate::lifecycle::collect(&tx, &run)?;
                    if !crate::lifecycle::guidance(&tx, &run)?
                        && crate::lifecycle::blocked(&tx, &run)?
                    {
                        crate::lifecycle::hold_join(&tx, &run, &thread)?;
                        tx.commit()?;
                        continue;
                    }
                    if crate::lifecycle::pending(&tx, &run)?
                        && !crate::lifecycle::guidance(&tx, &run)?
                    {
                        tx.commit()?;
                        continue;
                    }
                    tx.execute("UPDATE operations SET state='done' WHERE id=?1", [&op.id])?;
                    change(&tx, Some(&thread), "operation_updated", &op.id.0)?;
                    tx.commit()?;
                } else if op.kind == "tool" {
                    let (operation, owner) = (op.id.clone(), op.thread.clone());
                    if let Err(error) = self.dispatch_tool(op, done) {
                        self.fail_dispatch(&operation, &owner, contained(error)?)?;
                    }
                }
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(i64::MAX as u128) as i64;
                let retry_at: i64 = self.store.db.query_row(
                    "SELECT retry_at FROM runs WHERE id=?1",
                    [&run],
                    |r| r.get(0),
                )?;
                if retry_at > now {
                    continue;
                }
                let configured: String = self.store.db.query_row(
                    "SELECT json_extract(profile,'$.model') FROM threads WHERE id=?1",
                    [&thread],
                    |r| r.get(0),
                )?;
                if !self.models.iter().any(|model| model.id == configured) {
                    let tx = self
                        .store
                        .db
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    tx.execute("UPDATE threads SET held=1 WHERE id=?1", [&thread])?;
                    crate::lifecycle::fail(&tx, &run, "Configured model is unavailable; load or select a model before resuming this thread.")?;
                    tx.commit()?;
                    continue;
                }
                if let Err(error) = self.request(&run, &thread, done) {
                    let text = format!("Turn stopped: {:#}", contained(error)?);
                    let tx = self
                        .store
                        .db
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    entry(&tx, &thread, "recovery", &text, EntryData::default())?;
                    crate::lifecycle::fail(&tx, &run, &text)?;
                    tx.commit()?;
                }
            }
        }
        Ok(())
    }

    fn fail_dispatch(
        &mut self,
        operation: &OperationId,
        thread: &ThreadId,
        error: anyhow::Error,
    ) -> anyhow::Result<()> {
        self.waiting.remove(operation);
        let text = format!("Tool dispatch failed; nothing executed: {error:#}");
        let tx = self
            .store
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE operations SET state='failed' WHERE id=?1 AND state IN ('pending','ready','approval','waiting')", [operation])? == 1 {
            crate::workflow::fail(&tx, operation, &text)?;
            finish_tool_entry(&tx, operation, &text, true)?;
            change(&tx, Some(thread), "tool_failed", &operation.0)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn next_operation(&self, run: &RunId) -> anyhow::Result<Option<Op>> {
        self.scheduled_operation(run)
    }

    fn dispatch_tool(&mut self, op: Op, done: &mpsc::Sender<Finished>) -> anyhow::Result<()> {
        let Some(_descriptor) = self.tools.get(&op.name) else {
            return self.fail_tool(&op, "Unknown tool; nothing executed.");
        };
        if let Err(error) = self.validators[&op.name].validate(&op.input) {
            return self.fail_tool(&op, &format!("Invalid tool input: {error}"));
        }
        let(profile,workspace):(String,String)=self.store.db.query_row("SELECT t.profile,s.workspace FROM threads t JOIN sessions s ON s.id=t.session WHERE t.id=?1",[&op.thread],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let profile: Profile = decode(profile)?;
        let role = crate::delegation::role(&self.store.db, &op.thread)?;
        if !crate::delegation::allowed(&role, &op.name) {
            return self.fail_tool(
                &op,
                "Tool is unavailable to this agent role; nothing executed.",
            );
        }
        let (policy, revision) = policy_for(&self.store.db, &op.thread)?;
        let permissions = crate::policy::permissions(&self.store.db, &op.thread)?;
        let capabilities = crate::policy::internal_capabilities(&op.name)
            .unwrap_or_else(|| self.executor.tool_capabilities(&op.name));
        let decision = crate::policy::evaluate_with_fallback(
            &policy,
            &permissions,
            &profile,
            crate::policy::ToolRequest {
                name: &op.name,
                input: &op.input,
                workspace: &workspace,
            },
            &capabilities,
            Some(RuleDecision::Allow),
        );
        if decision == RuleDecision::Deny {
            return self.fail_tool(
                &op,
                "Explicit denial or capability restriction; no further work authorized.",
            );
        }
        if matches!(
            op.name.as_str(),
            "launch_agent"
                | "send_message"
                | "read_history"
                | "search_conversation"
                | "read_conversation"
                | "wait_agent"
                | "wait_message"
                | "ask_user"
                | "enter_plan"
                | "exit_plan"
                | "todo_write"
                | "job_status"
                | "job_output"
                | "task"
                | "explore"
                | "spawn_agent"
                | "list_agents"
                | "stop_agent"
                | "workflow"
        ) {
            if !self.authorize_control(&op, &workspace, revision, decision)? {
                return Ok(());
            }
            return self.control(op, profile);
        }
        let invocation = Invocation {
            operation_id: op.id.clone(),
            run_id: op.run.clone(),
            thread_id: op.thread.clone(),
            workspace,
            name: op.name.clone(),
            input: op.input.clone(),
        };
        if op.state == "pending" {
            let tx = self
                .store
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE operations SET state='preparing',policy_revision=?1,profile_revision=(SELECT profile_revision FROM threads WHERE id=operations.thread) WHERE id=?2",
                params![revision, op.id],
            )?;
            change(&tx, Some(&op.thread), "tool_preparing", &op.id.0)?;
            tx.commit()?;
            let executor = self.executor.clone();
            self.spawn(op.id, done.clone(), move |cancel| async move {
                let prepared = executor
                    .prepare_with_cancel(invocation.clone(), cancel)
                    .await?;
                ensure!(
                    encode(&prepared.invocation)? == encode(&invocation)?,
                    "adapter changed the invocation during preparation"
                );
                Ok(Output::Prepared(Box::new(prepared)))
            });
        } else if op.state == "ready" {
            if revision != op.revision {
                let tx = self
                    .store
                    .db
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE operations SET state='pending',prepared=NULL WHERE id=?1",
                    [&op.id],
                )?;
                change(&tx, Some(&op.thread), "operation_updated", &op.id.0)?;
                tx.commit()?;
                return Ok(());
            }
            let prepared: PreparedTool =
                decode(op.prepared.clone().context("prepared invocation missing")?)?;
            let capabilities = match self.executor.prepared_capabilities(&prepared) {
                Ok(caps) => caps,
                Err(error) => {
                    return self.fail_tool(
                        &op,
                        &format!("Prepared capability validation failed: {error:#}"),
                    )
                }
            };
            let (decision, requirements, _) = match self.prepared_authorization(
                &profile,
                &policy,
                &permissions,
                &prepared,
                &capabilities,
            ) {
                Ok(value) => value,
                Err(error) => {
                    return self
                        .fail_tool(&op, &format!("Prepared authorization failed: {error:#}"))
                }
            };
            if decision == RuleDecision::Deny {
                return self.fail_tool(&op, "Prepared capability is denied by the current policy.");
            }
            let retained: String = self.store.db.query_row(
                "SELECT authorizations FROM operations WHERE id=?1",
                [&op.id],
                |r| r.get(0),
            )?;
            if retained != encode(&requirements)? {
                return self.fail_tool(
                    &op,
                    "Prepared authorization targets changed; nothing executed.",
                );
            }
            let approved: bool = self.store.db.query_row("SELECT EXISTS(SELECT 1 FROM decisions WHERE operation=?1 AND revision=?2 AND state='approved')", params![op.id, revision], |r| r.get(0))?;
            if decision != RuleDecision::Allow && !approved {
                let tx = self
                    .store
                    .db
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE operations SET state='pending',prepared=NULL WHERE id=?1",
                    [&op.id],
                )?;
                change(&tx, Some(&op.thread), "authorization_changed", &op.id.0)?;
                tx.commit()?;
                return Ok(());
            }
            let writes = capabilities.mutates_workspace
                || capabilities.executes_process
                || requirements.iter().any(|requirement| {
                    matches!(
                        requirement,
                        PreparedAuthorizationRequirement::EditFile { .. }
                    )
                });
            let resources = crate::effect_claims::resources(&prepared, &requirements)?;
            let claim = self
                .store
                .claim
                .as_ref()
                .context("runtime has no claimed session")?;
            let tx = self
                .store
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if scheduling::execution_blocked(
                &tx,
                &op.id,
                &prepared.invocation.workspace,
                writes,
                claim.session(),
            )? || !crate::effect_claims::claim(
                &tx,
                &op.id,
                claim.session(),
                &claim.generation,
                &resources,
                writes,
            )? {
                drop(tx);
                return self.note_waiting(&op);
            }
            self.waiting.remove(&op.id);
            let background =
                op.name == "bash" && op.input["run_in_background"].as_bool() == Some(true);
            if background {
                let result = crate::jobs::start_shell(&tx, &prepared);
                match result {
                    Ok(_) => tx.commit()?,
                    Err(error) => {
                        drop(tx);
                        return self.fail_tool(&op, &format!("{error:#}"));
                    }
                }
                let executor = self.executor.clone();
                let progress = ToolProgress {
                    operation: op.id.clone(),
                    tx: self.progress_tx.clone(),
                };
                self.spawn(op.id, done.clone(), move |cancel| async move {
                    let outcome = executor
                        .execute_with_progress(prepared.clone(), cancel, progress)
                        .await?;
                    let display = executor.display_text(&prepared, &outcome).unwrap_or(None);
                    Ok(Output::Tool(outcome, display))
                });
                return Ok(());
            }
            tx.execute(
                "UPDATE operations SET state='started' WHERE id=?1",
                [&op.id],
            )?;
            let shell = op.name == "bash";
            if shell {
                crate::jobs::track_foreground(&tx, &prepared)?;
            }
            change(&tx, Some(&op.thread), "tool_started", &op.id.0)?;
            tx.commit()?;
            let executor = self.executor.clone();
            let progress = ToolProgress {
                operation: op.id.clone(),
                tx: self.progress_tx.clone(),
            };
            self.spawn(op.id, done.clone(), move |cancel| async move {
                let outcome = if shell {
                    executor
                        .execute_with_progress(prepared.clone(), cancel, progress)
                        .await?
                } else {
                    executor.execute(prepared.clone(), cancel).await?
                };
                let display = executor.display_text(&prepared, &outcome).unwrap_or(None);
                Ok(Output::Tool(outcome, display))
            });
        }
        Ok(())
    }

    fn spawn<F, Fut>(&mut self, id: OperationId, done: mpsc::Sender<Finished>, f: F)
    where
        F: FnOnce(watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<Output>> + Send + 'static,
    {
        let (cancel, signal) = watch::channel(false);
        let worker_id = id.clone();
        let task = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(f(signal))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "executor panicked; external effects may be uncertain"
                    ))
                });
            let _ = done
                .send(Finished {
                    id: worker_id,
                    result,
                })
                .await;
        });
        self.workers.insert(
            id,
            Worker {
                cancel,
                task,
                cancelled_at: None,
            },
        );
    }

    /// Blocked tool calls retry every tick; a lasting wait gets one notice so it never looks hung.
    fn note_waiting(&mut self, op: &Op) -> anyhow::Result<()> {
        let (since, noticed) = *self
            .waiting
            .entry(op.id.clone())
            .or_insert((Instant::now(), false));
        if noticed || since.elapsed() < WAIT_NOTICE_AFTER {
            return Ok(());
        }
        let tx = self
            .store
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let blocker: Option<String> = tx.query_row("SELECT o.name FROM operations o JOIN threads t ON t.id=o.thread JOIN sessions s ON s.id=t.session WHERE o.kind='tool' AND o.state='started' AND o.id!=?1 AND s.workspace=(SELECT s2.workspace FROM threads t2 JOIN sessions s2 ON s2.id=t2.session WHERE t2.id=?2) ORDER BY o.rowid LIMIT 1", params![op.id, op.thread], |r| r.get(0)).optional()?;
        let text = match blocker {
            Some(name) => format!(
                "{} is waiting for a running {name} call in this workspace to finish.",
                op.name
            ),
            None => format!("{} is waiting for an earlier tool call to finish.", op.name),
        };
        entry(&tx, &op.thread, "notice", &text, EntryData::default())?;
        change(&tx, Some(&op.thread), "operation_updated", &op.id.0)?;
        tx.commit()?;
        self.waiting.insert(op.id.clone(), (since, true));
        Ok(())
    }

    fn fail_tool(&mut self, op: &Op, text: &str) -> anyhow::Result<()> {
        self.waiting.remove(&op.id);
        let tx = self
            .store
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::workflow::fail(&tx, &op.id, text)?;
        finish_tool_entry(&tx, &op.id, text, true)?;
        tx.execute("UPDATE operations SET state='failed' WHERE id=?1", [&op.id])?;
        change(&tx, Some(&op.thread), "tool_failed", &op.id.0)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    fn finish(&mut self, finished: Finished) -> anyhow::Result<()> {
        self.finish_ref(&finished)
    }

    fn finish_ref(&mut self, finished: &Finished) -> anyhow::Result<()> {
        self.store.ensure_owner()?;
        let session = self.store.db.query_row(
            "SELECT t.session FROM operations o JOIN threads t ON t.id=o.thread WHERE o.id=?1",
            [&finished.id],
            |r| r.get::<_, String>(0),
        )?;
        ensure!(
            self.store
                .claim
                .as_ref()
                .is_some_and(|owner| owner.session().0 == session),
            "completion targets an unowned session"
        );
        let original_failure = finished
            .result
            .as_ref()
            .err()
            .map(|error| self.executor.classify_error(error));
        let result = finished
            .result
            .as_ref()
            .map(Clone::clone)
            .map_err(|error| anyhow::anyhow!("{error:#}"));
        let shell: bool = self.store.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE operation=?1 AND kind='shell' AND background=1)",
            [&finished.id],
            |r| r.get(0),
        )?;
        if shell {
            let tx = self
                .store
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            crate::jobs::finish_shell(
                &tx,
                &finished.id,
                result.and_then(|output| match output {
                    Output::Tool(outcome, display) => Ok((outcome, display)),
                    _ => anyhow::bail!("invalid job completion"),
                }),
            )?;
            tx.commit()?;
            return Ok(());
        }
        let opid = &finished.id;
        let (run, thread, kind, state, name, revision): (
            String,
            String,
            String,
            String,
            String,
            i64,
        ) = self.store.db.query_row(
            "SELECT run,thread,kind,state,name,policy_revision FROM operations WHERE id=?1",
            [opid],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
        if !matches!(state.as_str(), "preparing" | "started") {
            return Ok(());
        }
        let run = RunId(run);
        let thread = ThreadId(thread);
        let active: bool = self.store.db.query_row(
            "SELECT r.state='active' AND o.epoch=c.epoch AND o.profile_revision=t.profile_revision FROM operations o JOIN runs r ON r.id=o.run JOIN contexts c ON c.thread=o.thread JOIN threads t ON t.id=o.thread WHERE o.id=?1",
            [opid],
            |r| r.get(0),
        )?;
        let active = active && self.error.is_none();
        let mut prepared_caps = None;
        let mut prepared_decision = None;
        let mut prepared_requirements = Vec::new();
        let mut prepared_target = None;
        let mut prepared_shell = None;
        let result = result.and_then(|output| {
            if let Output::Context(system) = &output {
                ensure!(
                    system.len() <= MAX_ARTIFACT / 2,
                    "system context exceeds bounded request retention"
                );
            }
            if let Output::Prepared(prepared) = &output {
                ensure!(
                    encode(prepared)?.len() <= 64 * 1024 * 1024,
                    "prepared tool exceeds 64 MiB retention"
                );
                let caps = self.executor.prepared_capabilities(prepared)?;
                let profile: Profile = decode(self.store.db.query_row(
                    "SELECT profile FROM threads WHERE id=?1",
                    [&thread],
                    |r| r.get(0),
                )?)?;
                let (policy, _) = policy_for(&self.store.db, &thread)?;
                let permissions = crate::policy::permissions(&self.store.db, &thread)?;
                let (decision, requirements, target) =
                    self.prepared_authorization(&profile, &policy, &permissions, prepared, &caps)?;
                prepared_decision = Some(decision);
                prepared_requirements = requirements;
                prepared_target = target;
                prepared_shell = self.executor.prepared_shell_command(prepared)?;
                prepared_caps = Some(caps);
            }
            if let Output::Model(completion) = &output {
                ensure!(
                    encode(&completion.message)?.len() <= MAX_ARTIFACT,
                    "provider response exceeds the retention limit"
                );
                let mut seen = std::collections::HashSet::new();
                for block in &completion.message.content {
                    if let ContentBlock::ToolUse { id, .. } = block {
                        ensure!(
                            !id.is_empty() && seen.insert(id),
                            "provider returned duplicate/empty tool IDs"
                        );
                        ensure!(seen.len() <= 32, "provider returned too many tool calls");
                    }
                }
            }
            Ok(output)
        });
        let live_error = self.error.clone().or_else(|| match &result {
            Err(error) => Some(format!("{error:#}")),
            Ok(Output::Model(completion))
                if matches!(completion.stop_reason, StopReason::MaxTokens)
                    || completion.message.role != Role::Assistant =>
            {
                Some("Incomplete provider answer".into())
            }
            _ => None,
        });
        let retry_kind = original_failure.or_else(|| {
            result
                .as_ref()
                .err()
                .map(|error| self.executor.classify_error(error))
        });
        let selected_model: String = self.store.db.query_row(
            "SELECT json_extract(profile,'$.model') FROM threads WHERE id=?1",
            [&thread],
            |r| r.get(0),
        )?;
        let max_output = self.model_capabilities(&selected_model).max_output_tokens;
        let tx = self
            .store
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::jobs::clear_foreground(&tx, opid)?;
        match result {
            Ok(Output::Context(system)) => {
                tx.execute(
                    "UPDATE operations SET state=?1 WHERE id=?2",
                    params![if active { "done" } else { "cancelled" }, opid],
                )?;
                if active {
                    tx.execute("INSERT INTO model_context(thread,system,epoch,profile_revision) SELECT thread,?2,epoch,profile_revision FROM operations WHERE id=?1 ON CONFLICT(thread) DO UPDATE SET system=excluded.system,epoch=excluded.epoch,profile_revision=excluded.profile_revision",params![opid,system])?;
                }
            }
            Ok(Output::Prepared(prepared)) => {
                tx.execute(
                    "UPDATE operations SET effect_write=?2,authorizations=?3 WHERE id=?1",
                    params![
                        opid,
                        prepared_caps
                            .as_ref()
                            .is_some_and(|caps| caps.mutates_workspace || caps.executes_process)
                            || prepared_requirements.iter().any(|requirement| matches!(
                                requirement,
                                PreparedAuthorizationRequirement::EditFile { .. }
                            )),
                        encode(&prepared_requirements)?
                    ],
                )?;
                let (_policy, current) = policy_for(&tx, &thread)?;
                if !active {
                    finish_tool_entry(&tx, opid, "Cancelled before execution.", true)?;
                    tx.execute(
                        "UPDATE operations SET state='cancelled' WHERE id=?1",
                        [opid],
                    )?;
                } else if revision != current {
                    tx.execute("UPDATE operations SET state='pending' WHERE id=?1", [opid])?;
                } else if prepared_decision == Some(RuleDecision::Deny) {
                    finish_tool_entry(
                        &tx,
                        opid,
                        "Prepared capability denied; nothing executed.",
                        true,
                    )?;
                    tx.execute("UPDATE operations SET state='failed' WHERE id=?1", [opid])?;
                } else {
                    let approval = prepared_decision != Some(RuleDecision::Allow);
                    tx.execute(
                        "UPDATE operations SET state=?1,prepared=?2 WHERE id=?3",
                        params![
                            if approval { "approval" } else { "ready" },
                            encode(&prepared)?,
                            opid
                        ],
                    )?;
                    if approval {
                        let ordinary = prepared_requirements.iter().all(|requirement| {
                            matches!(
                                requirement,
                                PreparedAuthorizationRequirement::ToolPath { .. }
                            )
                        }) && prepared_decision == Some(RuleDecision::Ask)
                            && prepared_caps.as_ref().is_some_and(|caps| {
                                !caps.external_access && !caps.requires_approval
                            });
                        let options = crate::approvals::choices_with_shell(
                            &prepared,
                            ordinary,
                            prepared_target.as_deref(),
                            prepared_shell.as_ref(),
                        );
                        tx.execute("INSERT INTO decisions(operation,revision,preview,state,options) VALUES(?1,?2,?3,'pending',?4) ON CONFLICT(operation) DO UPDATE SET revision=excluded.revision,preview=excluded.preview,state='pending',options=excluded.options",params![opid,revision,preview(&prepared.preview),encode(&options)?])?;
                    }
                }
            }
            Ok(Output::Tool(mut result, display)) => {
                if result.text.len() > MAX_ARTIFACT {
                    let suffix="\n[Output exceeded the retention budget; remaining output was omitted. The operation settled; do not blindly repeat mutations.]";
                    let mut end = MAX_ARTIFACT - suffix.len();
                    while !result.text.is_char_boundary(end) {
                        end -= 1;
                    }
                    result.text.truncate(end);
                    result.text.push_str(suffix);
                    result.is_error = true;
                }
                if let Some(presentation) = &mut result.presentation {
                    for text in [&mut presentation.summary, &mut presentation.body_preview]
                        .into_iter()
                        .flatten()
                    {
                        let mut end = text.len().min(2048);
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        text.truncate(end);
                    }
                }
                if let Some(mut display) = display {
                    if display.len() > MAX_ARTIFACT {
                        let suffix = "\n[Display output truncated at retention limit.]";
                        let mut end = MAX_ARTIFACT - suffix.len();
                        while !display.is_char_boundary(end) {
                            end -= 1;
                        }
                        display.truncate(end);
                        display.push_str(suffix);
                    }
                    let display = artifact(&tx, &display)?;
                    tx.execute(
                        "UPDATE operations SET display_result=?1 WHERE id=?2",
                        params![display, opid],
                    )?;
                }
                let presentation = result.presentation.as_ref().map(encode).transpose()?;
                finish_tool_entry(
                    &tx,
                    opid,
                    &result.text,
                    result.is_error || result.interrupted,
                )?;
                tx.execute(
                    "UPDATE operations SET state=?1,presentation=?3 WHERE id=?2",
                    params![
                        if result.interrupted {
                            "unknown"
                        } else if result.is_error {
                            "failed"
                        } else {
                            "done"
                        },
                        opid,
                        presentation
                    ],
                )?;
                if result.interrupted {
                    if active {
                        tx.execute("UPDATE runs SET state='interrupted' WHERE id=?1", [&run])?;
                    }
                    tx.execute("UPDATE threads SET held=1 WHERE id=?1", [&thread])?;
                    crate::lifecycle::stop_owned(&tx, &run)?;
                }
            }
            Ok(Output::Model(completion)) if name == "compact" => {
                model_runtime::finish_compact(&tx, opid, &thread, completion, active, max_output)?;
            }
            Ok(Output::Model(completion)) => {
                crate::sessions::record_usage(&tx, opid, completion.usage)?;
                if active && name == "model" {
                    if let Some(measured) = self
                        .executor
                        .measured_input_tokens(&selected_model, &completion.usage)
                        .filter(|value| *value > 0)
                    {
                        tx.execute("UPDATE operations SET input=json_set(input,'$.measured_input_tokens',?1) WHERE id=?2",params![measured.min(i64::MAX as u64),opid])?;
                    }
                }
                let valid = active
                    && !matches!(completion.stop_reason, StopReason::MaxTokens)
                    && completion.message.role == Role::Assistant;
                let truncated = completion.stop_reason == StopReason::MaxTokens;
                let retry = active
                    && truncated
                    && model_runtime::retry_model(
                        &tx,
                        opid,
                        ModelFailureKind::Fatal,
                        max_output,
                        true,
                    )?;
                let message = if valid {
                    completion.message
                } else {
                    Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: completion.message.text(),
                        }],
                    }
                };
                let result = artifact(&tx, &encode(&ModelRecord::Message(message.clone()))?)?;
                crate::visible_text::retain(&tx, &result, &message)?;
                tx.execute(
                    "UPDATE operations SET state=?1,result=?2 WHERE id=?3",
                    params![if valid { "done" } else { "interrupted" }, result, opid],
                )?;
                entry(
                    &tx,
                    &thread,
                    "assistant",
                    &message.text(),
                    EntryData {
                        message: valid.then_some(&message),
                        operation: Some(opid),
                        result: Some(&result),
                        ..Default::default()
                    },
                )?;
                if valid {
                    tx.execute(
                        "UPDATE runs SET retry_count=0,retry_at=0,output_override=NULL WHERE id=?1",
                        [&run],
                    )?;
                    let mut calls = Vec::new();
                    for block in &message.content {
                        if let ContentBlock::ToolUse { id, name, input } = block {
                            calls.push((id, name, input));
                        }
                    }
                    for (id, name, input) in &calls {
                        let operation = OperationId::new();
                        tx.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,?4,'tool',?5,?6,'pending')",params![operation,run,thread,name,id,encode(input)?])?;
                        entry(
                            &tx,
                            &thread,
                            "tool_call",
                            &format!("{name} {}", preview(&encode(input)?)),
                            EntryData {
                                operation: Some(&operation),
                                ..Default::default()
                            },
                        )?;
                    }
                    if calls.is_empty() {
                        crate::lifecycle::collect(&tx, &run)?;
                        if crate::lifecycle::pending(&tx, &run)?
                            || crate::lifecycle::blocked(&tx, &run)?
                        {
                            tx.execute(
                                "UPDATE operations SET state='waiting' WHERE id=?1",
                                [opid],
                            )?;
                            change(&tx, Some(&thread), "joining_agents", &run.0)?;
                        } else if !crate::lifecycle::actionable(&tx, &run)? {
                            settle_run(&tx, &run, "completed", &message.text())?;
                        }
                    }
                } else if active && !retry {
                    fail_model_run(
                        &tx,
                        &run,
                        &thread,
                        "Incomplete provider answer; tool calls were not executed.",
                    )?;
                    entry(&tx,&thread,"recovery","Incomplete provider answer; tool calls were not executed. Explicit resume required.",EntryData { operation: Some(opid), ..Default::default() })?;
                }
            }
            Err(error) => {
                let text = format!("{error:#}");
                let uncertain = kind == "tool" && state == "started";
                let cancelled = self
                    .workers
                    .get(opid)
                    .is_some_and(|worker| worker.cancelled_at.is_some());
                if kind == "model" && active && cancelled {
                    tx.execute("UPDATE operations SET state='failed' WHERE id=?1", [opid])?;
                } else {
                    if kind == "tool" {
                        finish_tool_entry(
                            &tx,
                            opid,
                            &format!(
                                "{}: {text}",
                                if uncertain {
                                    "Unknown operation outcome"
                                } else {
                                    "Preparation failed; nothing executed"
                                }
                            ),
                            true,
                        )?;
                    } else {
                        entry(
                            &tx,
                            &thread,
                            "recovery",
                            &format!("Model request failed: {text}"),
                            EntryData {
                                operation: Some(opid),
                                ..Default::default()
                            },
                        )?;
                    }
                    tx.execute(
                        "UPDATE operations SET state=?1 WHERE id=?2",
                        params![if uncertain { "unknown" } else { "failed" }, opid],
                    )?;
                    let retry = active
                        && kind == "model"
                        && model_runtime::retry_model(
                            &tx,
                            opid,
                            retry_kind.unwrap_or(ModelFailureKind::Fatal),
                            max_output,
                            false,
                        )?;
                    if active && kind == "model" && !retry && name == "compact" {
                        tx.execute("UPDATE runs SET state='interrupted' WHERE id=?1", [&run])?;
                        tx.execute("UPDATE threads SET held=1 WHERE id=?1", [&thread])?;
                        crate::lifecycle::stop_owned(&tx, &run)?;
                        entry(&tx,&thread,"recovery","Compaction failed; original context retained. Select a larger model or explicitly request compaction again. Ordinary resume does not reset exhausted attempts.",EntryData::default())?;
                    } else if active && kind == "model" && !retry {
                        fail_model_run(
                            &tx,
                            &run,
                            &thread,
                            &format!("Model request failed: {text}"),
                        )?;
                    } else if uncertain {
                        if active {
                            tx.execute("UPDATE runs SET state='interrupted' WHERE id=?1", [&run])?;
                        }
                        tx.execute("UPDATE threads SET held=1 WHERE id=?1", [&thread])?;
                        crate::lifecycle::stop_owned(&tx, &run)?;
                    }
                }
            }
        }
        crate::effect_claims::release_settled(&tx, opid)?;
        let sequence = change(&tx, Some(&thread), "operation_updated", &opid.0)?;
        tx.commit()?;
        if kind == "model" {
            self.live.settle(opid, sequence, live_error);
        }
        self.live.publish(sequence);
        Ok(())
    }

    fn control(&mut self, op: Op, profile: Profile) -> anyhow::Result<()> {
        if op.state == "waiting" && matches!(op.name.as_str(), "ask_user" | "exit_plan") {
            return Ok(());
        }
        let tx = self
            .store
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch("SAVEPOINT control_effect")?;
        let result = (|| -> anyhow::Result<Option<String>> {
            match op.name.as_str() {
                "workflow" => crate::workflow::execute(
                    &tx,
                    &op.id,
                    &op.run,
                    &op.thread,
                    &profile,
                    &self.models,
                    &op.input,
                ),
                "task" | "explore" => crate::delegation::launch_group(
                    &tx,
                    crate::delegation::GroupRequest {
                        operation: &op.id,
                        run: &op.run,
                        thread: &op.thread,
                        name: &op.name,
                        input: &op.input,
                    },
                    &profile,
                    &self.models,
                ),
                "list_agents" | "stop_agent" | "send_message" => {
                    crate::coordination::execute(&tx, &op.id, &op.thread, &op.name, &op.input)
                        .map(Some)
                }
                "wait_message" => {
                    crate::coordination::wait(&tx, &op.id, &op.run, &op.thread, &op.input)
                }
                "read_history" => crate::history::execute(&tx, &op.thread, &op.input).map(Some),
                "search_conversation" | "read_conversation" => crate::conversations::execute(
                    &tx,
                    &op.thread,
                    &self.archive_token,
                    &op.name,
                    &op.input,
                )
                .map(Some),
                "launch_agent" | "spawn_agent" => {
                    let name = op.input["name"].as_str().unwrap();
                    let prompt = op.input["task"]
                        .as_str()
                        .or(op.input["prompt"].as_str())
                        .context("task required")?;
                    let child = crate::delegation::launch(
                        &tx,
                        &op.id,
                        &op.run,
                        &op.thread,
                        &profile,
                        &self.models,
                        crate::delegation::Child {
                            name,
                            prompt,
                            role: "collaborator",
                            subagent_type: op.input["subagent_type"].as_str(),
                            model: op.input["model"].as_str(),
                            read_only: op.input["read_only"].as_bool().unwrap_or(false)
                                || op.input["subagent_type"] == "reviewer",
                            blocking: op.name == "launch_agent",
                            schema: None,
                        },
                    )?;
                    Ok(Some(format!("Started agent {child}. Completion reports are durable; inspect and message it by thread ID.")))
                }
                "wait_agent" => {
                    let target = ThreadId(op.input["thread_id"].as_str().unwrap().into());
                    let owner: Option<String> = tx.query_row(
                        "SELECT parent_run FROM threads WHERE id=?1",
                        [&target],
                        |r| r.get(0),
                    )?;
                    ensure!(
                        owner.as_deref() == Some(&op.run.0),
                        "can only wait for children owned by this run"
                    );
                    let state: Option<String> = tx
                        .query_row("SELECT state FROM agents WHERE thread=?1", [&target], |r| {
                            r.get(0)
                        })
                        .optional()?;
                    if matches!(
                        state.as_deref(),
                        Some("completed" | "failed" | "cancelled" | "unknown")
                    ) {
                        crate::lifecycle::publish(&tx, &target)?;
                        Ok(Some(format!(
                            "Agent {target} {}. Inspect its retained report with read_history; unknown or held work requires explicit recovery and was not resumed.",
                            state.unwrap()
                        )))
                    } else if crate::lifecycle::guidance(&tx, &op.run)? {
                        Ok(Some(format!("Agent {target} remains active. Wait yielded to accepted coordination; respond to guidance and wait again if needed.")))
                    } else {
                        Ok(None)
                    }
                }
                "job_status" | "job_output" => {
                    crate::jobs::execute(&tx, &op.thread, &op.name, &op.input).map(Some)
                }
                "ask_user" | "enter_plan" | "exit_plan" | "todo_write" => {
                    crate::controls::execute(&tx, &op.id, &op.thread, &op.name, &op.input)
                }
                _ => unreachable!(),
            }
        })();
        if result.is_err() {
            tx.execute_batch("ROLLBACK TO control_effect")?;
        }
        tx.execute_batch("RELEASE control_effect")?;
        match result {
            Ok(Some(text)) => {
                let failed = matches!(op.name.as_str(), "task" | "explore" | "workflow")
                    && serde_json::from_str::<Value>(&text)
                        .ok()
                        .is_some_and(|value| value["status"] == "failed");
                finish_tool_entry(&tx, &op.id, &text, failed)?;
                tx.execute(
                    "UPDATE operations SET state=?2 WHERE id=?1",
                    params![op.id, if failed { "failed" } else { "done" }],
                )?;
            }
            Ok(None)
                if matches!(op.name.as_str(), "task" | "explore" | "workflow")
                    && crate::lifecycle::guidance(&tx, &op.run)? =>
            {
                let job = if op.name == "workflow" {
                    tx.execute("UPDATE jobs SET background=1 WHERE operation=?1", [&op.id])?;
                    JobId(tx.query_row(
                        "SELECT job FROM workflows WHERE operation=?1",
                        [&op.id],
                        |r| r.get(0),
                    )?)
                } else {
                    crate::delegation::yield_group(&tx, &op.id)?
                };
                finish_tool_entry(&tx, &op.id, &json!({"status":"running","job_id":job,"message":"Collection yielded to accepted coordination. Respond to guidance; this run still owns the work and will collect its completion. Use job_output for retained aggregate results."}).to_string(), false)?;
                tx.execute("UPDATE operations SET state='done' WHERE id=?1", [&op.id])?;
            }
            Ok(None) if op.state == "waiting" => {
                tx.commit()?;
                return Ok(());
            }
            Ok(None) => {
                tx.execute(
                    "UPDATE operations SET state='waiting' WHERE id=?1",
                    [&op.id],
                )?;
            }
            Err(error) => {
                let text = format!("{error:#}");
                if op.name == "workflow" && crate::workflow::fail(&tx, &op.id, &text)? {
                    tx.execute(
                        "UPDATE operations SET state='waiting' WHERE id=?1",
                        [&op.id],
                    )?;
                } else {
                    finish_tool_entry(&tx, &op.id, &text, true)?;
                    tx.execute("UPDATE operations SET state='failed' WHERE id=?1", [&op.id])?;
                }
            }
        }
        change(&tx, Some(&op.thread), "control_updated", &op.id.0)?;
        tx.commit()?;
        Ok(())
    }
}

fn fail_model_run(
    tx: &rusqlite::Transaction<'_>,
    run: &RunId,
    _thread: &ThreadId,
    text: &str,
) -> anyhow::Result<()> {
    crate::lifecycle::fail(tx, run, text)
}

/// Only storage errors halt the kernel; other failures stay with the turn or tool that hit them.
fn contained(error: anyhow::Error) -> anyhow::Result<anyhow::Error> {
    if error.chain().any(|cause| cause.is::<rusqlite::Error>()) {
        Err(error)
    } else {
        Ok(error)
    }
}

fn durable_sequence(db: &rusqlite::Connection) -> anyhow::Result<i64> {
    Ok(db.query_row("SELECT COALESCE(MAX(seq),0) FROM changes", [], |r| r.get(0))?)
}

fn policy_for(db: &rusqlite::Connection, thread: &ThreadId) -> anyhow::Result<(Policy, i64)> {
    let (policy, revision): (String, i64) = db.query_row(
        "SELECT s.policy,s.revision FROM sessions s JOIN threads t ON t.session=s.id WHERE t.id=?1",
        [thread],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((decode(policy)?, revision))
}
fn control_tools() -> Vec<ToolDescriptor> {
    [
        ("launch_agent","Launch a child using this same runtime. Model omission inherits this thread's model.",json!({"name":{"type":"string","minLength":1,"maxLength":80},"prompt":{"type":"string","minLength":1,"maxLength":16000},"model":{"type":"string"},"read_only":{"type":"boolean"}}),vec!["name","prompt"]),
        ("wait_agent","Wait for a child owned by this run. Completed reports enter context automatically.",json!({"thread_id":{"type":"string"}}),vec!["thread_id"]),
    ].into_iter().map(|(name,description,properties,required)|ToolDescriptor{spec:ToolSpec{name:name.into(),description:description.into(),input_schema:json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})},effect:EffectClass::Read}).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::ModelProgress;
    use crate::presentation::{OperationState, ThreadState, ToolResultPresentation};
    use crate::store::tests::fixture;
    use async_trait::async_trait;

    pub(super) struct NoDispatch(pub(super) Vec<ModelInfo>);
    #[async_trait]
    impl Executor for NoDispatch {
        fn models(&self) -> Vec<ModelInfo> {
            self.0.clone()
        }
        fn tools(&self) -> Vec<ToolDescriptor> {
            Vec::new()
        }
        async fn generate(
            &self,
            _: RequestSpec,
            _: watch::Receiver<bool>,
            _: ModelProgress,
        ) -> anyhow::Result<Completion> {
            panic!("unexpected generation")
        }
        async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
            panic!("unexpected preparation")
        }
        async fn execute(
            &self,
            _: PreparedTool,
            _: watch::Receiver<bool>,
        ) -> anyhow::Result<ToolOutcome> {
            panic!("unexpected effect")
        }
    }

    pub(super) fn start_run(actor: &Actor, thread: &ThreadId) -> RunId {
        let run = RunId::new();
        actor
            .store
            .db
            .execute(
                "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
                params![run, thread],
            )
            .unwrap();
        run
    }

    #[tokio::test]
    async fn restart_holds_a_committed_child_before_its_first_run() {
        let (dir, store, receipt, models) = fixture();
        let executor = Arc::new(NoDispatch(models));
        let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
        let run = start_run(&actor, &receipt.thread);
        let operation = OperationId::new();
        let input = json!({"name":"queued child","prompt":"inspect only"});
        actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'launch_agent','tool','launch',?4,'pending')", params![operation,run,receipt.thread,encode(&input).unwrap()]).unwrap();
        let (done, _) = mpsc::channel(64);
        let op = actor.next_operation(&run).unwrap().unwrap();
        actor.dispatch_tool(op, &done).unwrap();
        let child: String = actor
            .store
            .db
            .query_row("SELECT id FROM threads WHERE parent_run=?1", [&run], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            actor
                .store
                .db
                .query_row("SELECT COUNT(*) FROM runs WHERE thread=?1", [&child], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(actor);
        let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
        let claim =
            crate::SessionClaim::acquire(&dir.0.join("lab.db"), receipt.session.clone()).unwrap();
        store.adopt_claim(claim, true).unwrap();
        let mut actor = Actor::new(store, executor).unwrap();
        actor.schedule(&done).unwrap();
        assert!(actor.workers.is_empty());
        let view = actor
            .store
            .view(Some(ThreadId(child.clone())), None, 100, None)
            .unwrap();
        assert_eq!(
            view.threads.iter().find(|t| t.id.0 == child).unwrap().state,
            ThreadState::Held
        );
        assert_eq!(view.entries.len(), 1);
        assert_eq!(
            actor
                .store
                .db
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE parent_run=?1",
                    [&run],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn late_completion_cannot_overwrite_an_unknown_outcome() {
        let (_dir, store, receipt, models) = fixture();
        let mut actor = Actor::new_owned(store, Arc::new(NoDispatch(models))).unwrap();
        let run = start_run(&actor, &receipt.thread);
        let id = OperationId::new();
        actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'shell','tool','effect','{}','started')", params![id,run,receipt.thread]).unwrap();
        actor
            .finish(Finished {
                id: id.clone(),
                result: Err(anyhow::anyhow!("cancellation settlement expired")),
            })
            .unwrap();
        let before = actor
            .store
            .view(Some(receipt.thread.clone()), None, 100, None)
            .unwrap();
        actor
            .finish(Finished {
                id,
                result: Ok(Output::Tool(
                    ToolOutcome {
                        text: "late success".into(),
                        presentation: None,
                        is_error: false,
                        interrupted: false,
                    },
                    None,
                )),
            })
            .unwrap();
        let after = actor
            .store
            .view(Some(receipt.thread), None, 100, None)
            .unwrap();
        assert_eq!(before.sequence, after.sequence);
        assert_eq!(after.entries.len(), 1);
        assert_eq!(after.operations[0].state, OperationState::Unknown);
    }

    #[tokio::test]
    async fn failed_model_start_never_creates_a_live_attempt_or_worker() {
        let (_dir, store, receipt, models) = fixture();
        let mut actor = Actor::new_owned(store, Arc::new(NoDispatch(models))).unwrap();
        let run = start_run(&actor, &receipt.thread);
        actor.store.db.execute("INSERT INTO model_context(thread,system,epoch,profile_revision) VALUES(?1,'test',0,0)",[&receipt.thread]).unwrap();
        let sequence = durable_sequence(&actor.store.db).unwrap();
        actor.store.db.execute_batch("CREATE TRIGGER fail_start BEFORE INSERT ON changes WHEN NEW.kind='model_started' BEGIN SELECT RAISE(ABORT,'injected start failure'); END;").unwrap();
        let (done, _) = mpsc::channel(64);
        assert!(actor.request(&run, &receipt.thread, &done).is_err());
        assert!(actor.workers.is_empty());
        actor.publish_live().unwrap();
        let live = actor.live.subscribe();
        assert!(live.borrow().attempts.is_empty());
        assert_eq!(live.borrow().sequence, sequence);
        assert_eq!(
            actor
                .store
                .db
                .query_row("SELECT COUNT(*) FROM operations", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            actor
                .store
                .db
                .query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn result_presentation_is_bounded_and_commits_with_the_output() {
        let (_dir, store, receipt, models) = fixture();
        let mut actor = Actor::new_owned(store, Arc::new(NoDispatch(models))).unwrap();
        let run = start_run(&actor, &receipt.thread);
        let id = OperationId::new();
        actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'shell','tool','effect','{}','started')", params![id,run,receipt.thread]).unwrap();
        let result = ToolOutcome {
            text: "complete process output".into(),
            is_error: true,
            interrupted: false,
            presentation: Some(ToolResultPresentation {
                summary: Some("界".repeat(2048)),
                body_preview: Some("é".repeat(2048)),
                exit_code: Some(-1073741819),
            }),
        };
        actor.store.db.execute_batch("CREATE TRIGGER fail_result BEFORE INSERT ON changes WHEN NEW.kind='operation_updated' BEGIN SELECT RAISE(ABORT,'injected result failure'); END;").unwrap();
        assert!(actor
            .finish(Finished {
                id: id.clone(),
                result: Ok(Output::Tool(result.clone(), None))
            })
            .is_err());
        let pending: (String, Option<String>, Option<String>) = actor
            .store
            .db
            .query_row(
                "SELECT state,result,presentation FROM operations WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(pending, ("started".into(), None, None));
        assert_eq!(
            actor
                .store
                .db
                .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        actor
            .store
            .db
            .execute_batch("DROP TRIGGER fail_result")
            .unwrap();
        actor
            .finish(Finished {
                id: id.clone(),
                result: Ok(Output::Tool(result, None)),
            })
            .unwrap();
        let (artifact, presentation): (String, String) = actor
            .store
            .db
            .query_row(
                "SELECT result,presentation FROM operations WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            full_artifact(&actor.store.db, &ArtifactId(artifact)).unwrap(),
            "complete process output"
        );
        let presentation: ToolResultPresentation = decode(presentation).unwrap();
        assert_eq!(presentation.exit_code, Some(-1073741819));
        let body = presentation.body_preview.unwrap();
        assert_eq!(body.len(), 2048);
        assert!(body.chars().all(|c| c == 'é'));
        let summary = presentation.summary.unwrap();
        assert_eq!(summary.len(), 2046);
        assert!(summary.chars().all(|c| c == '界'));
        let view = actor
            .store
            .view(Some(receipt.thread), None, 100, None)
            .unwrap();
        assert_eq!(view.operations[0].state, OperationState::Failed);
        assert_eq!(actor.live.subscribe().borrow().sequence, view.sequence);
    }

    #[tokio::test]
    async fn manifest_overhead_pauses_only_its_thread() {
        let (_dir, store, receipt, models) = fixture();
        let mut actor = Actor::new_owned(store, Arc::new(NoDispatch(models))).unwrap();
        let run = start_run(&actor, &receipt.thread);
        let mut profile: Profile = decode(
            actor
                .store
                .db
                .query_row(
                    "SELECT profile FROM threads WHERE id=?1",
                    [&receipt.thread],
                    |r| r.get(0),
                )
                .unwrap(),
        )
        .unwrap();
        profile.instructions = "instructions ".repeat(400);
        actor
            .store
            .db
            .execute(
                "UPDATE threads SET profile=?1 WHERE id=?2",
                params![encode(&profile).unwrap(), receipt.thread],
            )
            .unwrap();
        let message = Message::user_text("x".repeat(MAX_ARTIFACT - 1000));
        entry(
            &actor.store.db,
            &receipt.thread,
            "submission",
            "large input",
            EntryData {
                message: Some(&message),
                ..Default::default()
            },
        )
        .unwrap();
        actor.models[0].context_window = 1000;
        actor
            .store
            .db
            .execute(
                "INSERT INTO model_context(thread,system,epoch,profile_revision) VALUES(?1,?2,0,0)",
                params![receipt.thread, profile.instructions],
            )
            .unwrap();
        let (done, _) = mpsc::channel(64);
        actor.request(&run, &receipt.thread, &done).unwrap();
        assert!(actor.workers.is_empty());
        let view = actor
            .store
            .view(Some(receipt.thread), None, 100, actor.error.clone())
            .unwrap();
        assert!(view.error.is_none());
        assert_eq!(view.threads[0].state, ThreadState::Interrupted);
        assert!(view.entries.iter().any(|e| e
            .text
            .contains("System instructions and tool schemas exceed")));
    }
}

#[cfg(test)]
#[path = "runtime_lifecycle_tests.rs"]
mod lifecycle_tests;
