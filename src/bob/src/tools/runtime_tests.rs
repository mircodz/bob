use super::ToolService;
use anyhow::{Context, Result};
use async_trait::async_trait;
use bob_kernel::{
    ArtifactId, Command, CommandId, Decision, EntryKind, Executor, Handle, Invocation, ModelInfo,
    ModelProgress, OperationId, OperationState, Policy, PreparedTool, Profile, Receipt,
    RequestSpec, Runtime, ThreadId, ThreadState, ToolBody, ToolDescriptor, ToolOutcome, View,
};
use bob_types::{Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason, Usage};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{watch, Semaphore};

const WAIT: Duration = Duration::from_secs(10);

#[cfg(unix)]
#[path = "foreground_handoff_tests.rs"]
mod foreground_handoff_tests;
#[path = "instruction_permission_tests.rs"]
mod instruction_permission_tests;
#[path = "shell_grant_tests.rs"]
mod shell_grant_tests;
const TARGET: &str = "created.txt";
const CONTENT: &str = "exact UTF-8: 日本語\r\nsecond line\n";

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-runtime-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        let dir = Self(fs::canonicalize(path).unwrap());
        fs::create_dir(dir.workspace()).unwrap();
        dir
    }

    fn db(&self) -> PathBuf {
        self.0.join("runtime.sqlite")
    }

    fn workspace(&self) -> PathBuf {
        self.0.join("workspace")
    }

    fn target(&self) -> PathBuf {
        self.workspace().join(TARGET)
    }

    fn assert_empty(&self) {
        assert_eq!(fs::read_dir(self.workspace()).unwrap().count(), 0);
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("runtime tool fixture timed out")
}

fn text(value: &str) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: value.into() }],
        },
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

fn call(name: &str, input: Value) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.into(),
                input,
            }],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

fn create_call() -> Completion {
    call("create_file", json!({"path": TARGET, "content": CONTENT}))
}

enum Step {
    Reply(Completion),
    WaitForChild,
}

#[derive(Default)]
struct Observed {
    scripts: HashMap<String, VecDeque<Step>>,
    requests: Vec<RequestSpec>,
    preparations: usize,
    executions: usize,
    outcomes: Vec<ToolOutcome>,
}

struct ExecutionGate {
    entered: Semaphore,
    release: Semaphore,
}

impl ExecutionGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
}

#[derive(Default)]
struct ScriptedExecutor {
    tools: ToolService,
    observed: Mutex<Observed>,
    execute_gate: Option<Arc<ExecutionGate>>,
}

impl ScriptedExecutor {
    fn new(
        root: Vec<Step>,
        worker: Vec<Step>,
        execute_gate: Option<Arc<ExecutionGate>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            observed: Mutex::new(Observed {
                scripts: HashMap::from([
                    ("root".into(), root.into()),
                    ("worker".into(), worker.into()),
                ]),
                ..Observed::default()
            }),
            execute_gate,
            ..Self::default()
        })
    }

    fn creator(execute_gate: Option<Arc<ExecutionGate>>) -> Arc<Self> {
        Self::new(
            vec![Step::Reply(create_call()), Step::Reply(text("finished"))],
            vec![],
            execute_gate,
        )
    }

    fn assert_calls(&self, preparations: usize, executions: usize) {
        let observed = self.observed.lock().unwrap();
        assert_eq!(observed.preparations, preparations);
        assert_eq!(observed.executions, executions);
    }
}

#[async_trait]
impl Executor for ScriptedExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        ["root", "worker", "barrier"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                context_window: 128_000,
            })
            .collect()
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        self.tools.tools()
    }

    fn tool_capabilities(&self, name: &str) -> bob_kernel::ToolCapabilities {
        self.tools.capabilities(name)
    }

    fn prepared_capabilities(&self, tool: &PreparedTool) -> Result<bob_kernel::ToolCapabilities> {
        self.tools.prepared_capabilities(tool)
    }

    fn prepared_shell_command(
        &self,
        tool: &PreparedTool,
    ) -> Result<Option<bob_kernel::ShellCommandGrant>> {
        self.tools.prepared_shell_command(tool)
    }

    fn prepared_policy(
        &self,
        tool: &PreparedTool,
        permissions: &bob_kernel::PermissionPolicy,
    ) -> Result<Option<bob_kernel::RuleDecision>> {
        self.tools.prepared_policy(tool, permissions)
    }

    async fn generate(
        &self,
        request: RequestSpec,
        _cancel: watch::Receiver<bool>,
        _progress: ModelProgress,
    ) -> Result<Completion> {
        let step = {
            let mut observed = self.observed.lock().unwrap();
            observed.requests.push(request.clone());
            if request.model.id == "barrier" {
                return Ok(text("scheduling barrier complete"));
            }
            observed
                .scripts
                .get_mut(&request.model.id)
                .and_then(VecDeque::pop_front)
                .with_context(|| format!("unexpected model request for {}", request.model.id))?
        };
        match step {
            Step::Reply(completion) => Ok(completion),
            Step::WaitForChild => {
                let child = request
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .find_map(|block| match block {
                        ContentBlock::ToolResult { content, .. } => content
                            .as_text()
                            .and_then(|text| text.strip_prefix("Started agent "))
                            .and_then(|tail| tail.split('.').next()),
                        _ => None,
                    })
                    .context("launch result missing from model context")?;
                uuid::Uuid::parse_str(child)?;
                Ok(call("wait_agent", json!({"thread_id": child})))
            }
        }
    }

    async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        self.observed.lock().unwrap().preparations += 1;
        self.tools.prepare(invocation).await
    }

    async fn execute(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        self.observed.lock().unwrap().executions += 1;
        if let Some(gate) = &self.execute_gate {
            gate.entered.add_permits(1);
            tokio::time::timeout(WAIT, gate.release.acquire())
                .await
                .context("execution release timed out")??
                .forget();
        }
        let outcome = self.tools.execute(tool, cancel).await?;
        self.observed.lock().unwrap().outcomes.push(outcome.clone());
        Ok(outcome)
    }
}

async fn open(dir: &TestDir, executor: Arc<ScriptedExecutor>) -> Handle {
    bounded(Runtime::open(&dir.db(), executor)).await.unwrap()
}

async fn command(handle: &Handle, command: Command) -> Receipt {
    bounded(handle.command(command)).await.unwrap()
}

async fn session(
    handle: &Handle,
    dir: &TestDir,
    model: &str,
    policy: Policy,
    read_only: bool,
) -> Receipt {
    command(
        handle,
        Command::CreateSession {
            id: CommandId::new(),
            workspace: dir.workspace().to_string_lossy().into_owned(),
            profile: Profile {
                model: model.into(),
                instructions: "Deterministic real-tool runtime fixture.".into(),
                read_only,
                max_requests: 4,
                reasoning: ReasoningEffort::Off,
            },
            policy,
        },
    )
    .await
}

async fn submit(handle: &Handle, thread: &ThreadId) {
    command(
        handle,
        Command::Submit {
            id: CommandId::new(),
            thread: thread.clone(),
            text: "Run the scripted task.".into(),
        },
    )
    .await;
}

async fn view(handle: &Handle, thread: &ThreadId) -> View {
    let snapshot = bounded(handle.view(Some(thread.clone()), None, 100))
        .await
        .unwrap();
    assert!(
        snapshot.error.is_none(),
        "kernel halted: {:?}",
        snapshot.error
    );
    snapshot
}

async fn until(handle: &Handle, thread: &ThreadId, predicate: impl Fn(&View) -> bool) -> View {
    let mut changes = handle.subscribe();
    bounded(async {
        loop {
            changes.borrow_and_update();
            let snapshot = view(handle, thread).await;
            if predicate(&snapshot) {
                return snapshot;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
}

fn state(snapshot: &View, thread: &ThreadId) -> ThreadState {
    snapshot
        .threads
        .iter()
        .find(|t| &t.id == thread)
        .unwrap()
        .state
}

async fn completed(handle: &Handle, thread: &ThreadId) -> View {
    until(handle, thread, |snapshot| {
        state(snapshot, thread) == ThreadState::Completed
    })
    .await
}

async fn complete_peer_turn(dir: &TestDir, executor: Arc<ScriptedExecutor>) {
    let handle = open(dir, executor).await;
    let barrier = session(&handle, dir, "barrier", Policy::default(), false).await;
    submit(&handle, &barrier.thread).await;
    completed(&handle, &barrier.thread).await;
    bounded(handle.shutdown()).await.unwrap();
}

async fn pending_create(handle: &Handle, thread: &ThreadId, dir: &TestDir) -> Decision {
    let pending = until(handle, thread, |snapshot| snapshot.decisions.len() == 1).await;
    let decision = pending.decisions[0].clone();
    assert_eq!(&decision.thread, thread);
    assert_eq!(decision.name, "create_file");
    let presentation = decision.presentation.as_ref().unwrap();
    assert_eq!(
        presentation.target,
        Some(dir.target().display().to_string())
    );
    assert_eq!(
        presentation.body,
        ToolBody::Create {
            content: CONTENT.into(),
        }
    );
    assert_eq!(
        pending
            .operations
            .iter()
            .find(|o| o.id == decision.operation)
            .unwrap()
            .state,
        OperationState::Approval
    );
    dir.assert_empty();
    decision
}

async fn decide(handle: &Handle, decision: &Decision, approve: bool) {
    command(
        handle,
        Command::Decide {
            id: CommandId::new(),
            operation: decision.operation.clone(),
            revision: decision.revision,
            approve,
        },
    )
    .await;
}

async fn artifact_text(handle: &Handle, artifact: &ArtifactId) -> String {
    bounded(async {
        let mut text = String::new();
        loop {
            let page = handle
                .artifact(artifact.clone(), text.len(), 4096)
                .await
                .unwrap();
            if page.is_empty() {
                return text;
            }
            text.push_str(&page);
        }
    })
    .await
}

fn assert_created_result(snapshot: &View, operation: &OperationId) -> ArtifactId {
    let op = snapshot
        .operations
        .iter()
        .find(|o| &o.id == operation)
        .unwrap();
    assert_eq!(op.name, "create_file");
    assert_eq!(op.state, OperationState::Done);
    let artifact = op.result.clone().unwrap();
    let entries: Vec<_> = snapshot
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == EntryKind::ToolResult && entry.operation.as_ref() == Some(operation)
        })
        .collect();
    assert_eq!(entries.len(), 1);
    let entry = entries[0];
    assert_eq!(entry.artifact.as_ref(), Some(&artifact));
    let summary = entry.tool.as_ref().unwrap();
    assert_eq!(&summary.id, operation);
    assert_eq!(summary.state, OperationState::Done);
    assert_eq!(summary.result.as_ref(), Some(&artifact));
    assert_eq!(
        summary.presentation.as_ref().unwrap().summary.as_deref(),
        Some("Created file")
    );
    artifact
}

fn assert_blocked(snapshot: &View) {
    let operations: Vec<_> = snapshot
        .operations
        .iter()
        .filter(|o| o.name == "create_file")
        .collect();
    assert_eq!(operations.len(), 1);
    let operation = operations[0];
    assert_eq!(operation.state, OperationState::Failed);
    assert!(operation.result.is_some());
    assert!(snapshot.decisions.is_empty());
    assert!(snapshot.entries.iter().any(|entry| {
        entry.kind == EntryKind::ToolError && entry.operation.as_ref() == Some(&operation.id)
    }));
}

#[tokio::test]
async fn external_read_requires_exact_approval_and_denial_grants_no_evidence() {
    for approve in [false, true] {
        let dir = TestDir::new();
        let outside = dir.0.join("external.txt");
        fs::write(&outside, "external fixture contents").unwrap();
        let executor = ScriptedExecutor::new(
            vec![
                Step::Reply(call("read_file", json!({"path":outside}))),
                Step::Reply(text("done")),
            ],
            vec![],
            None,
        );
        let handle = open(&dir, executor.clone()).await;
        let root = session(&handle, &dir, "root", Policy::default(), false).await;
        submit(&handle, &root.thread).await;
        let pending = until(&handle, &root.thread, |view| view.decisions.len() == 1).await;
        let decision = &pending.decisions[0];
        assert_eq!(decision.name, "read_file");
        assert_eq!(
            decision.presentation.as_ref().unwrap().target,
            Some(outside.display().to_string())
        );
        executor.assert_calls(1, 0);
        assert!(executor.tools.files.lock().await.reads.is_empty());
        decide(&handle, decision, approve).await;
        completed(&handle, &root.thread).await;
        executor.assert_calls(1, usize::from(approve));
        assert_eq!(executor.tools.files.lock().await.reads.is_empty(), !approve);
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "external fixture contents"
        );
        bounded(handle.shutdown()).await.unwrap();
    }
}

#[tokio::test]
async fn approval_creates_exact_file_and_retains_result_without_replay_on_reopen() {
    let dir = TestDir::new();
    let executor = ScriptedExecutor::creator(None);
    let handle = open(&dir, executor.clone()).await;
    let root = session(&handle, &dir, "root", Policy::default(), false).await;
    submit(&handle, &root.thread).await;
    let decision = pending_create(&handle, &root.thread, &dir).await;
    complete_peer_turn(&dir, executor.clone()).await;
    executor.assert_calls(1, 0);
    dir.assert_empty();

    decide(&handle, &decision, true).await;
    let settled = completed(&handle, &root.thread).await;
    assert!(settled.decisions.is_empty());
    assert_eq!(fs::read(dir.target()).unwrap(), CONTENT.as_bytes());
    assert_eq!(fs::read_dir(dir.workspace()).unwrap().count(), 1);
    executor.assert_calls(1, 1);
    let artifact = assert_created_result(&settled, &decision.operation);
    let outcome = executor.observed.lock().unwrap().outcomes[0].clone();
    assert!(!outcome.is_error && !outcome.interrupted);
    assert_eq!(artifact_text(&handle, &artifact).await, outcome.text);
    bounded(handle.shutdown()).await.unwrap();

    let executor = ScriptedExecutor::new(vec![], vec![], None);
    let handle = open(&dir, executor.clone()).await;
    bounded(handle.activate_session(root.session.clone(), Vec::new()))
        .await
        .unwrap();
    complete_peer_turn(&dir, executor.clone()).await;
    let reopened = view(&handle, &root.thread).await;
    assert_eq!(state(&reopened, &root.thread), ThreadState::Completed);
    assert!(reopened.decisions.is_empty());
    assert_eq!(
        assert_created_result(&reopened, &decision.operation),
        artifact
    );
    assert_eq!(artifact_text(&handle, &artifact).await, outcome.text);
    assert_eq!(fs::read(dir.target()).unwrap(), CONTENT.as_bytes());
    bounded(handle.shutdown()).await.unwrap();
    executor.assert_calls(0, 0);
    assert!(executor
        .observed
        .lock()
        .unwrap()
        .requests
        .iter()
        .all(|r| r.model.id == "barrier"));
}

#[tokio::test]
async fn denied_creation_never_reaches_execution_or_creates_staging_files() {
    let dir = TestDir::new();
    let executor = ScriptedExecutor::creator(None);
    let handle = open(&dir, executor.clone()).await;
    let root = session(&handle, &dir, "root", Policy::default(), false).await;
    submit(&handle, &root.thread).await;
    let decision = pending_create(&handle, &root.thread, &dir).await;
    decide(&handle, &decision, false).await;
    assert_blocked(&completed(&handle, &root.thread).await);
    executor.assert_calls(1, 0);
    dir.assert_empty();
    bounded(handle.shutdown()).await.unwrap();

    let executor = ScriptedExecutor::new(vec![], vec![], None);
    let handle = open(&dir, executor.clone()).await;
    bounded(handle.activate_session(root.session.clone(), Vec::new()))
        .await
        .unwrap();
    complete_peer_turn(&dir, executor.clone()).await;
    let reopened = view(&handle, &root.thread).await;
    assert_eq!(state(&reopened, &root.thread), ThreadState::Completed);
    assert_blocked(&reopened);
    bounded(handle.shutdown()).await.unwrap();
    executor.assert_calls(0, 0);
    dir.assert_empty();
}

#[tokio::test]
async fn kernel_restrictions_block_real_creation_before_preparation_even_with_yolo() {
    for (policy, read_only) in [
        (
            Policy {
                yolo: true,
                denied_tools: ["create_file".into()].into(),
                ..Policy::default()
            },
            false,
        ),
        (
            Policy {
                yolo: true,
                plan: true,
                ..Policy::default()
            },
            false,
        ),
        (
            Policy {
                yolo: true,
                ..Policy::default()
            },
            true,
        ),
    ] {
        let dir = TestDir::new();
        let executor = ScriptedExecutor::creator(None);
        let handle = open(&dir, executor.clone()).await;
        let root = session(&handle, &dir, "root", policy, read_only).await;
        submit(&handle, &root.thread).await;
        assert_blocked(&completed(&handle, &root.thread).await);
        executor.assert_calls(0, 0);
        dir.assert_empty();
        {
            let observed = executor.observed.lock().unwrap();
            assert_eq!(observed.requests.len(), 2);
            assert!(observed
                .requests
                .iter()
                .all(|request| { request.tools.iter().all(|tool| tool.name != "create_file") }));
        }
        bounded(handle.shutdown()).await.unwrap();
    }
}

#[tokio::test]
async fn read_only_child_cannot_create_under_writable_yolo_parent() {
    let dir = TestDir::new();
    let executor = ScriptedExecutor::new(
        vec![
            Step::Reply(call(
                "launch_agent",
                json!({
                    "name": "reader", "prompt": "Try the scripted creation.",
                    "model": "worker", "read_only": true,
                }),
            )),
            Step::WaitForChild,
            Step::Reply(text("parent finished")),
        ],
        vec![
            Step::Reply(create_call()),
            Step::Reply(text("child report")),
        ],
        None,
    );
    let handle = open(&dir, executor.clone()).await;
    let root = session(
        &handle,
        &dir,
        "root",
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(&handle, &root.thread).await;
    let settled = completed(&handle, &root.thread).await;
    let children: Vec<_> = settled
        .threads
        .iter()
        .filter(|t| t.parent_thread.as_ref() == Some(&root.thread))
        .collect();
    assert_eq!(children.len(), 1);
    let child = children[0];
    assert_eq!(child.model, "worker");
    assert_eq!(child.state, ThreadState::Completed);
    assert_blocked(&view(&handle, &child.id).await);
    executor.assert_calls(0, 0);
    dir.assert_empty();
    {
        let observed = executor.observed.lock().unwrap();
        let worker: Vec<_> = observed
            .requests
            .iter()
            .filter(|r| r.model.id == "worker")
            .collect();
        assert_eq!(worker.len(), 2);
        assert!(worker
            .iter()
            .all(|r| r.tools.iter().all(|t| t.name != "create_file")));
        assert!(observed
            .requests
            .iter()
            .filter(|r| r.model.id == "root")
            .any(|r| { r.tools.iter().any(|t| t.name == "create_file") }));
    }
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn creator_racing_after_approval_is_never_overwritten() {
    let dir = TestDir::new();
    let gate = ExecutionGate::new();
    let executor = ScriptedExecutor::creator(Some(gate.clone()));
    let handle = open(&dir, executor.clone()).await;
    let root = session(&handle, &dir, "root", Policy::default(), false).await;
    submit(&handle, &root.thread).await;
    let decision = pending_create(&handle, &root.thread, &dir).await;
    decide(&handle, &decision, true).await;
    bounded(gate.entered.acquire()).await.unwrap().forget();
    dir.assert_empty();
    fs::write(dir.target(), "competing creator\r\n").unwrap();
    gate.release.add_permits(1);
    let settled = completed(&handle, &root.thread).await;
    assert_blocked(&settled);
    executor.assert_calls(1, 1);
    let outcome = executor.observed.lock().unwrap().outcomes[0].clone();
    assert!(outcome.is_error && !outcome.interrupted);
    assert!(
        outcome.text.contains("nothing overwritten"),
        "{}",
        outcome.text
    );
    assert_eq!(fs::read(dir.target()).unwrap(), b"competing creator\r\n");
    assert_eq!(fs::read_dir(dir.workspace()).unwrap().count(), 1);
    let operation = settled
        .operations
        .iter()
        .find(|o| o.id == decision.operation)
        .unwrap();
    assert_eq!(
        artifact_text(&handle, operation.result.as_ref().unwrap()).await,
        outcome.text
    );
    bounded(handle.shutdown()).await.unwrap();
}
