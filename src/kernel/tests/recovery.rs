use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use bob_kernel::*;
use bob_types::{
    Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason, ToolSpec, Usage,
};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{watch, Semaphore};
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(10);
const CRASH_EXIT: i32 = 73;
const FIXTURE_DIR: &str = "BOB_KERNEL_RECOVERY_FIXTURE_DIR";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("bob-kernel-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("runtime.sqlite")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    timeout(WAIT, future)
        .await
        .expect("bounded kernel operation timed out")
}

async fn open(path: &Path, executor: Arc<FakeExecutor>) -> Handle {
    bounded(Runtime::open(path, executor)).await.unwrap()
}

async fn reopen(path: &Path, executor: Arc<FakeExecutor>, thread: &ThreadId) -> Handle {
    let handle = open(path, executor).await;
    let snapshot = view(&handle, thread).await;
    let session = snapshot.threads.iter().find(|t| &t.id == thread).unwrap();
    bounded(handle.activate_session(session.session.clone(), vec![]))
        .await
        .unwrap();
    handle
}

async fn command(handle: &Handle, command: Command) -> Receipt {
    bounded(handle.command(command)).await.unwrap()
}

async fn view(handle: &Handle, thread: &ThreadId) -> View {
    let view = bounded(handle.view(Some(thread.clone()), None, 100))
        .await
        .unwrap();
    assert!(view.error.is_none(), "kernel halted: {:?}", view.error);
    view
}

async fn until(handle: &Handle, thread: &ThreadId, predicate: impl Fn(&View) -> bool) -> View {
    bounded(async {
        loop {
            let snapshot = view(handle, thread).await;
            if predicate(&snapshot) {
                return snapshot;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
}

fn state(view: &View, thread: &ThreadId) -> ThreadState {
    view.threads.iter().find(|t| &t.id == thread).unwrap().state
}

async fn completed(handle: &Handle, thread: &ThreadId) -> View {
    until(handle, thread, |v| {
        state(v, thread) == ThreadState::Completed
    })
    .await
}

fn profile(model: &str, read_only: bool) -> Profile {
    Profile {
        model: model.into(),
        instructions: "Deterministic recovery integration test.".into(),
        read_only,
        max_requests: 32,
        reasoning: ReasoningEffort::Off,
    }
}

async fn session(handle: &Handle, workspace: &Path, policy: Policy, read_only: bool) -> Receipt {
    command(
        handle,
        Command::CreateSession {
            id: CommandId::new(),
            workspace: workspace.to_string_lossy().into_owned(),
            profile: profile("root-model", read_only),
            policy,
        },
    )
    .await
}

async fn submit(handle: &Handle, thread: &ThreadId, text: &str) -> Receipt {
    command(
        handle,
        Command::Submit {
            id: CommandId::new(),
            thread: thread.clone(),
            text: text.into(),
        },
    )
    .await
}

fn readonly(path: &Path) -> Connection {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(2)).unwrap();
    db
}

fn count(path: &Path, sql: &str) -> i64 {
    readonly(path).query_row(sql, [], |row| row.get(0)).unwrap()
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

fn launch(read_only: bool) -> Completion {
    call(
        "launch_agent",
        json!({
            "name": "worker", "prompt": "Produce the worker report.",
            "model": "worker-model", "read_only": read_only,
        }),
    )
}

enum ModelStep {
    Reply(Completion),
    WaitForChild,
    SendToChild(Arc<Semaphore>),
    UntilCancelled(Completion),
}

#[derive(Default)]
struct Observed {
    scripts: HashMap<String, VecDeque<ModelStep>>,
    requests: Vec<RequestSpec>,
    preparation_attempts: usize,
    prepared: Vec<PreparedTool>,
    executed: Vec<PreparedTool>,
}

#[derive(Default)]
struct FakeExecutor {
    observed: Mutex<Observed>,
    prepare_gate: Option<Arc<Semaphore>>,
    execute_gate: Option<Arc<Semaphore>>,
    effect_file: Option<PathBuf>,
    crash_after_effect: bool,
    interrupt_first_execution: bool,
}

impl FakeExecutor {
    fn scripted(root: Vec<ModelStep>, worker: Vec<ModelStep>) -> Arc<Self> {
        let executor = Arc::new(Self::default());
        executor.push("root-model", root);
        executor.push("worker-model", worker);
        executor
    }

    fn push(&self, model: &str, steps: Vec<ModelStep>) {
        self.observed
            .lock()
            .unwrap()
            .scripts
            .entry(model.into())
            .or_default()
            .extend(steps);
    }

    fn calls(&self, model: &str) -> usize {
        self.observed
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.model.id == model)
            .count()
    }

    fn preparations(&self) -> usize {
        self.observed.lock().unwrap().preparation_attempts
    }

    fn executions(&self) -> usize {
        self.observed.lock().unwrap().executed.len()
    }

    fn requests(&self) -> Vec<RequestSpec> {
        self.observed.lock().unwrap().requests.clone()
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        ["root-model", "worker-model"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                context_window: 1_000_000,
            })
            .collect()
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        [
            ("write_file", EffectClass::Write),
            ("read_file", EffectClass::Read),
        ]
        .into_iter()
        .map(|(name, effect)| ToolDescriptor {
            spec: ToolSpec {
                name: name.into(),
                description: "Deterministic test tool.".into(),
                input_schema: json!({
                    "type": "object", "properties": {"text": {"type": "string"}},
                    "required": ["text"], "additionalProperties": false,
                }),
            },
            effect,
        })
        .collect()
    }

    async fn generate(
        &self,
        request: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _progress: ModelProgress,
    ) -> Result<Completion> {
        let step = {
            let mut observed = self.observed.lock().unwrap();
            observed.requests.push(request.clone());
            observed
                .scripts
                .get_mut(&request.model.id)
                .and_then(VecDeque::pop_front)
                .with_context(|| format!("unexpected model request for {}", request.model.id))?
        };
        match step {
            ModelStep::Reply(completion) => Ok(completion),
            ModelStep::UntilCancelled(completion) => {
                timeout(WAIT, cancelled(&mut cancel))
                    .await
                    .context("model cancellation timed out")?;
                Ok(completion)
            }
            ModelStep::WaitForChild => Ok(call(
                "wait_agent",
                json!({"thread_id": launched_child(&request)?}),
            )),
            ModelStep::SendToChild(gate) => {
                timeout(WAIT, gate.acquire())
                    .await
                    .context("message gate timed out")??
                    .forget();
                Ok(call(
                    "send_message",
                    json!({
                        "thread_id": launched_child(&request)?, "text": "coordination queued after cancellation",
                    }),
                ))
            }
        }
    }

    async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        self.observed.lock().unwrap().preparation_attempts += 1;
        ensure!(
            matches!(invocation.name.as_str(), "write_file" | "read_file"),
            "control tool reached external preparation"
        );
        let object = invocation
            .input
            .as_object()
            .context("input is not an object")?;
        ensure!(
            object.len() == 1 && object.get("text").is_some_and(Value::is_string),
            "schema-invalid input reached preparation"
        );
        let prepared = PreparedTool {
            presentation: None,
            specification: json!({"validated_text": invocation.input["text"], "tool": invocation.name}),
            preview: format!("{}: {}", invocation.name, invocation.input["text"]),
            invocation,
        };
        self.observed
            .lock()
            .unwrap()
            .prepared
            .push(prepared.clone());
        if let Some(gate) = &self.prepare_gate {
            timeout(WAIT, gate.acquire())
                .await
                .context("preparation gate timed out")??
                .forget();
        }
        Ok(prepared)
    }

    async fn execute(
        &self,
        tool: PreparedTool,
        _cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        let interrupted = {
            let mut observed = self.observed.lock().unwrap();
            observed.executed.push(tool.clone());
            ensure!(
                observed.prepared.iter().any(|p| {
                    p.invocation.operation_id == tool.invocation.operation_id
                        && serde_json::to_value(p).unwrap() == serde_json::to_value(&tool).unwrap()
                }),
                "execute bypassed preparation or changed the prepared tool"
            );
            self.interrupt_first_execution && observed.executed.len() == 1
        };
        if let Some(gate) = &self.execute_gate {
            timeout(WAIT, gate.acquire())
                .await
                .context("execution gate timed out")??
                .forget();
        }
        if let Some(path) = &self.effect_file {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(file, "{}", tool.invocation.operation_id)?;
            file.sync_all()?;
            if self.crash_after_effect {
                std::process::exit(CRASH_EXIT);
            }
        }
        Ok(ToolOutcome {
            presentation: None,
            text: tool.specification["validated_text"]
                .as_str()
                .unwrap()
                .into(),
            is_error: false,
            interrupted,
        })
    }
}

fn launched_child(request: &RequestSpec) -> Result<&str> {
    let child = request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|block| {
            if let ContentBlock::ToolResult { content, .. } = block {
                content
                    .as_text()
                    .and_then(|text| text.strip_prefix("Started agent "))
                    .and_then(|tail| tail.split('.').next())
            } else {
                None
            }
        })
        .context("launch_agent result missing from production model context")?;
    ensure!(
        uuid::Uuid::parse_str(child).is_ok(),
        "launch result did not identify a child thread"
    );
    Ok(child)
}

fn reply(completion: Completion) -> ModelStep {
    ModelStep::Reply(completion)
}

// A completed independent root proves scheduling progressed while the tested thread stayed held.
async fn scheduling_barrier(executor: &Arc<FakeExecutor>, workspace: &Path) {
    let handle = open(&workspace.join("runtime.sqlite"), executor.clone()).await;
    executor.push("worker-model", vec![reply(text("barrier complete"))]);
    let receipt = command(
        &handle,
        Command::CreateSession {
            id: CommandId::new(),
            workspace: workspace.to_string_lossy().into_owned(),
            profile: profile("worker-model", false),
            policy: Policy::default(),
        },
    )
    .await;
    submit(&handle, &receipt.thread, "scheduling barrier").await;
    completed(&handle, &receipt.thread).await;
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn admission_and_command_identity_survive_reopen() {
    let dir = TempDir::new();
    let executor =
        FakeExecutor::scripted(vec![ModelStep::UntilCancelled(text("interrupted"))], vec![]);
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(&handle, &dir.0, Policy::default(), false).await;
    let input = Command::Submit {
        id: CommandId::new(),
        thread: created.thread.clone(),
        text: "durable instruction".into(),
    };
    let accepted = command(&handle, input.clone()).await;
    let stored: String = readonly(&dir.db())
        .query_row(
            "SELECT receipt FROM commands WHERE id=?1",
            [input.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stored).unwrap(),
        serde_json::to_value(&accepted).unwrap()
    );
    let duplicate = command(&handle, input.clone()).await;
    assert_eq!(
        serde_json::to_value(&accepted).unwrap(),
        serde_json::to_value(&duplicate).unwrap()
    );
    let conflict = Command::Submit {
        id: input.id().clone(),
        thread: created.thread.clone(),
        text: "different payload".into(),
    };
    assert!(bounded(handle.command(conflict.clone())).await.is_err());
    until(&handle, &created.thread, |_| {
        executor.calls("root-model") == 1
    })
    .await;
    bounded(handle.shutdown()).await.unwrap();

    let executor = FakeExecutor::scripted(vec![], vec![]);
    let handle = reopen(&dir.db(), executor.clone(), &created.thread).await;
    let replay = command(&handle, input.clone()).await;
    assert_eq!(
        serde_json::to_value(&accepted).unwrap(),
        serde_json::to_value(&replay).unwrap()
    );
    assert!(bounded(handle.command(conflict)).await.is_err());
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.calls("root-model"), 0);
    let db = readonly(&dir.db());
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM submissions WHERE id=?1",
            [input.id()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT text FROM submissions WHERE id=?1",
            [input.id()],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "durable instruction"
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM runs WHERE thread=?1",
            [&created.thread],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    drop(db);
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn writes_wait_for_current_decision_and_denial_never_executes() {
    let dir = TempDir::new();
    let executor = FakeExecutor::scripted(
        vec![
            reply(call("write_file", json!({"text": "approved output"}))),
            reply(text("done")),
        ],
        vec![],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(&handle, &dir.0, Policy::default(), false).await;
    submit(&handle, &created.thread, "write after approval").await;
    let pending = until(&handle, &created.thread, |v| v.decisions.len() == 1).await;
    let old = pending.decisions[0].clone();
    assert_eq!(executor.preparations(), 1);
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.executions(), 0);
    command(
        &handle,
        Command::SetPolicy {
            id: CommandId::new(),
            session: created.session.clone(),
            expected_revision: old.revision,
            policy: Policy::default(),
        },
    )
    .await;
    assert!(bounded(handle.command(Command::Decide {
        id: CommandId::new(),
        operation: old.operation.clone(),
        revision: old.revision,
        approve: true,
    }))
    .await
    .is_err());
    assert!(bounded(handle.command(Command::SetPolicy {
        id: CommandId::new(),
        session: created.session.clone(),
        expected_revision: old.revision,
        policy: Policy {
            yolo: true,
            ..Policy::default()
        },
    }))
    .await
    .is_err());
    let fresh = until(&handle, &created.thread, |v| {
        v.decisions.iter().any(|d| d.revision > old.revision)
    })
    .await;
    let decision = fresh.decisions[0].clone();
    assert_eq!(decision.operation, old.operation);
    assert_eq!(executor.preparations(), 2);
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.executions(), 0);
    command(
        &handle,
        Command::Decide {
            id: CommandId::new(),
            operation: decision.operation,
            revision: decision.revision,
            approve: true,
        },
    )
    .await;
    completed(&handle, &created.thread).await;
    assert_eq!(executor.executions(), 1);

    executor.push(
        "root-model",
        vec![
            reply(call("write_file", json!({"text": "must not write"}))),
            reply(text("denied")),
        ],
    );
    submit(&handle, &created.thread, "deny this write").await;
    let pending = until(&handle, &created.thread, |v| v.decisions.len() == 1).await;
    let denied = pending.decisions[0].clone();
    command(
        &handle,
        Command::Decide {
            id: CommandId::new(),
            operation: denied.operation.clone(),
            revision: denied.revision,
            approve: false,
        },
    )
    .await;
    let settled = completed(&handle, &created.thread).await;
    assert!(settled
        .operations
        .iter()
        .any(|o| o.id == denied.operation && o.state == OperationState::Failed));
    assert_eq!(executor.executions(), 1);
    assert!(bounded(handle.command(Command::Decide {
        id: CommandId::new(),
        operation: denied.operation,
        revision: denied.revision,
        approve: true,
    }))
    .await
    .is_err());
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn policy_change_during_preparation_prevents_execution() {
    let dir = TempDir::new();
    let gate = Arc::new(Semaphore::new(0));
    let executor = Arc::new(FakeExecutor {
        prepare_gate: Some(gate.clone()),
        ..FakeExecutor::default()
    });
    executor.push(
        "root-model",
        vec![
            reply(call("write_file", json!({"text": "forbidden"}))),
            reply(text("restricted")),
        ],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(
        &handle,
        &dir.0,
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(&handle, &created.thread, "prepare then change policy").await;
    let preparing = until(&handle, &created.thread, |_| executor.preparations() == 1).await;
    let revision = preparing
        .policies
        .iter()
        .find(|p| p.session == created.session)
        .unwrap()
        .revision;
    command(
        &handle,
        Command::SetPolicy {
            id: CommandId::new(),
            session: created.session,
            expected_revision: revision,
            policy: Policy {
                yolo: true,
                plan: true,
                ..Policy::default()
            },
        },
    )
    .await;
    gate.add_permits(1);
    let settled = completed(&handle, &created.thread).await;
    assert!(settled
        .operations
        .iter()
        .any(|o| o.name == "write_file" && o.state == OperationState::Failed));
    assert!(settled.decisions.is_empty());
    assert_eq!(executor.executions(), 0);
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn explicit_denial_plan_and_read_only_override_yolo() {
    for restriction in ["denied", "plan", "read_only"] {
        let dir = TempDir::new();
        let executor = FakeExecutor::scripted(
            vec![
                reply(call("write_file", json!({"text": "forbidden"}))),
                reply(text("restricted")),
            ],
            vec![],
        );
        let handle = open(&dir.db(), executor.clone()).await;
        let mut policy = Policy {
            yolo: true,
            ..Policy::default()
        };
        if restriction == "denied" {
            policy.denied_tools.insert("write_file".into());
        }
        policy.plan = restriction == "plan";
        let created = session(&handle, &dir.0, policy, restriction == "read_only").await;
        submit(&handle, &created.thread, "attempt restricted write").await;
        let settled = completed(&handle, &created.thread).await;
        assert!(
            settled
                .operations
                .iter()
                .any(|o| o.name == "write_file" && o.state == OperationState::Failed),
            "{restriction}"
        );
        assert_eq!(executor.preparations(), 0, "{restriction}");
        assert_eq!(executor.executions(), 0, "{restriction}");
        assert!(executor
            .requests()
            .iter()
            .all(|r| r.tools.iter().all(|t| t.name != "write_file")));
        bounded(handle.shutdown()).await.unwrap();
    }
}

#[tokio::test]
async fn launch_agent_uses_worker_model_and_enforces_child_read_only() {
    let dir = TempDir::new();
    let executor = FakeExecutor::scripted(
        vec![
            reply(launch(true)),
            ModelStep::WaitForChild,
            reply(text("root finished")),
        ],
        vec![
            reply(call("write_file", json!({"text": "forbidden child write"}))),
            reply(text("worker report")),
        ],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(
        &handle,
        &dir.0,
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(&handle, &created.thread, "delegate using worker-model").await;
    let settled = completed(&handle, &created.thread).await;
    let children: Vec<_> = settled
        .threads
        .iter()
        .filter(|t| t.parent_run.is_some())
        .collect();
    assert_eq!(children.len(), 1);
    let child = children[0];
    assert_eq!(child.model, "worker-model");
    assert_eq!(
        child.parent_run,
        settled
            .threads
            .iter()
            .find(|t| t.id == created.thread)
            .unwrap()
            .run
    );
    assert_eq!(executor.calls("root-model"), 3);
    assert_eq!(executor.calls("worker-model"), 2);
    assert_eq!(executor.preparations(), 0);
    assert_eq!(executor.executions(), 0);
    let child_view = view(&handle, &child.id).await;
    assert!(child_view
        .operations
        .iter()
        .any(|o| o.name == "write_file" && o.state == OperationState::Failed));
    let requests = executor.requests();
    assert!(requests
        .iter()
        .filter(|r| r.model.id == "worker-model")
        .all(|r| r.tools.iter().all(|t| t.name != "write_file")));
    let root_final = requests
        .iter()
        .rfind(|r| r.model.id == "root-model")
        .unwrap();
    assert!(root_final.messages.iter().any(
        |m| m.text().contains("worker report") && m.text().contains("not a human instruction")
    ));
    assert!(settled
        .operations
        .iter()
        .any(|o| o.name == "launch_agent" && o.state == OperationState::Done));
    assert!(settled
        .operations
        .iter()
        .any(|o| o.name == "wait_agent" && o.state == OperationState::Done));
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn cancellation_drops_late_tools_and_a_message_after_stop_runs_the_queue_in_order() {
    let dir = TempDir::new();
    let executor = FakeExecutor::scripted(
        vec![
            ModelStep::UntilCancelled(call("write_file", json!({"text": "late forbidden tool"}))),
            reply(text("first queued response")),
            reply(text("second queued response")),
        ],
        vec![],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(
        &handle,
        &dir.0,
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(&handle, &created.thread, "block until cancelled").await;
    let active = until(&handle, &created.thread, |_| {
        executor.calls("root-model") == 1
    })
    .await;
    submit(&handle, &created.thread, "queued before stop").await;
    command(
        &handle,
        Command::Stop {
            id: CommandId::new(),
            run: active
                .threads
                .iter()
                .find(|t| t.id == created.thread)
                .unwrap()
                .run
                .clone()
                .unwrap(),
        },
    )
    .await;
    submit(&handle, &created.thread, "queued after stop").await;
    until(&handle, &created.thread, |v| {
        state(v, &created.thread) == ThreadState::Completed && executor.calls("root-model") == 3
    })
    .await;
    assert_eq!(executor.preparations(), 0);
    assert_eq!(executor.executions(), 0);
    let requests = executor.requests();
    let roots: Vec<_> = requests
        .iter()
        .filter(|r| r.model.id == "root-model")
        .collect();
    assert!(roots[1]
        .messages
        .iter()
        .any(|m| m.text() == "queued before stop"));
    assert!(!roots[1]
        .messages
        .iter()
        .any(|m| m.text() == "queued after stop"));
    assert!(roots[2]
        .messages
        .iter()
        .any(|m| m.text() == "queued after stop"));
    assert_eq!(
        readonly(&dir.db())
            .query_row(
                "SELECT COUNT(*) FROM submissions WHERE thread=?1 AND state='queued'",
                [&created.thread],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    bounded(handle.shutdown()).await.unwrap();

    let executor = FakeExecutor::scripted(vec![], vec![]);
    let handle = reopen(&dir.db(), executor.clone(), &created.thread).await;
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.calls("root-model"), 0);
    assert_eq!(executor.executions(), 0);
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn stopping_write_rejects_resume_and_holds_input_until_unknown_is_resolved() {
    let dir = TempDir::new();
    let gate = Arc::new(Semaphore::new(0));
    let executor = Arc::new(FakeExecutor {
        execute_gate: Some(gate.clone()),
        interrupt_first_execution: true,
        ..FakeExecutor::default()
    });
    executor.push(
        "root-model",
        vec![
            reply(call("write_file", json!({"text": "uncertain write"}))),
            reply(text("queued instruction complete")),
        ],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(
        &handle,
        &dir.0,
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(&handle, &created.thread, "start write").await;
    let active = until(&handle, &created.thread, |_| executor.executions() == 1).await;
    let queued = Command::Submit {
        id: CommandId::new(),
        thread: created.thread.clone(),
        text: "held instruction".into(),
    };
    command(&handle, queued.clone()).await;
    command(
        &handle,
        Command::Stop {
            id: CommandId::new(),
            run: active
                .threads
                .iter()
                .find(|t| t.id == created.thread)
                .unwrap()
                .run
                .clone()
                .unwrap(),
        },
    )
    .await;
    let error = bounded(handle.command(Command::Resume {
        id: CommandId::new(),
        thread: created.thread.clone(),
    }))
    .await
    .unwrap_err();
    assert!(error.to_string().contains("cancellation"), "{error:#}");
    assert_eq!(
        state(&view(&handle, &created.thread).await, &created.thread),
        ThreadState::Stopping
    );
    gate.add_permits(1);
    let settled = until(&handle, &created.thread, |v| {
        state(v, &created.thread) == ThreadState::Cancelled
            && v.operations
                .iter()
                .any(|o| o.state == OperationState::Unknown)
    })
    .await;
    let unknown = settled
        .operations
        .iter()
        .find(|o| o.state == OperationState::Unknown)
        .unwrap();
    let error = bounded(handle.command(Command::Resume {
        id: CommandId::new(),
        thread: created.thread.clone(),
    }))
    .await
    .unwrap_err();
    assert!(error.to_string().contains("unknown"), "{error:#}");
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.calls("root-model"), 1);
    assert_eq!(executor.executions(), 1);
    assert_eq!(
        readonly(&dir.db())
            .query_row(
                "SELECT state FROM submissions WHERE id=?1",
                [queued.id()],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
        "queued"
    );
    command(
        &handle,
        Command::ResolveUnknown {
            id: CommandId::new(),
            operation: unknown.id.clone(),
            evidence: "Inspected the write; do not repeat it".into(),
        },
    )
    .await;
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(
        executor.calls("root-model"),
        1,
        "resolution alone must not resume"
    );
    command(
        &handle,
        Command::Resume {
            id: CommandId::new(),
            thread: created.thread.clone(),
        },
    )
    .await;
    completed(&handle, &created.thread).await;
    assert_eq!(executor.calls("root-model"), 2);
    let requests = executor.requests();
    let resumed = requests
        .iter()
        .rfind(|r| r.model.id == "root-model")
        .unwrap();
    assert!(resumed
        .messages
        .iter()
        .any(|m| m.text() == "held instruction"));
    assert!(resumed
        .messages
        .iter()
        .any(|m| m.text().contains("Inspected the write")));
    assert_eq!(executor.executions(), 1, "unknown write must not be rerun");
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn approval_blocked_runs_do_not_starve_later_sessions() {
    let dir = TempDir::new();
    let executor = FakeExecutor::scripted(
        (0..40)
            .map(|i| {
                reply(call(
                    "write_file",
                    json!({"text": format!("blocked write {i}")}),
                ))
            })
            .collect(),
        vec![],
    );
    let mut blocked = Vec::new();
    for i in 0..40 {
        let handle = open(&dir.db(), executor.clone()).await;
        let created = session(&handle, &dir.0, Policy::default(), false).await;
        submit(&handle, &created.thread, &format!("wait for approval {i}")).await;
        until(&handle, &created.thread, |v| {
            v.decisions.iter().any(|d| d.thread == created.thread)
        })
        .await;
        blocked.push((handle, created.thread));
    }
    assert_eq!(
        count(&dir.db(), "SELECT COUNT(*) FROM runs WHERE state='active'"),
        40
    );
    assert_eq!(
        count(
            &dir.db(),
            "SELECT COUNT(*) FROM decisions WHERE state='pending'"
        ),
        40
    );
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.calls("root-model"), 40);
    assert_eq!(executor.preparations(), 40);
    assert_eq!(executor.executions(), 0);
    for (handle, thread) in &blocked {
        let snapshot = view(handle, thread).await;
        assert!(snapshot.decisions.iter().any(|d| &d.thread == thread));
        assert_eq!(state(&snapshot, thread), ThreadState::Active);
    }
    for (handle, _) in blocked {
        bounded(handle.shutdown()).await.unwrap();
    }
}

#[tokio::test]
async fn recovery_evidence_waits_for_entire_tool_batch_live_and_after_reopen() {
    let dir = TempDir::new();
    let gate = Arc::new(Semaphore::new(1));
    let executor = Arc::new(FakeExecutor {
        execute_gate: Some(gate.clone()),
        interrupt_first_execution: true,
        ..FakeExecutor::default()
    });
    let mut batch = call("write_file", json!({"text": "A outcome unknown"}));
    batch.message.content = vec![
        ContentBlock::ToolUse {
            id: "A".into(),
            name: "write_file".into(),
            input: json!({"text": "A outcome unknown"}),
        },
        ContentBlock::ToolUse {
            id: "B".into(),
            name: "read_file".into(),
            input: json!({"text": "B read complete"}),
        },
    ];
    executor.push(
        "root-model",
        vec![reply(batch), reply(text("batch recovered"))],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(
        &handle,
        &dir.0,
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(&handle, &created.thread, "perform A then B").await;
    let interrupted = until(&handle, &created.thread, |v| {
        state(v, &created.thread) == ThreadState::Interrupted
            && v.operations
                .iter()
                .any(|o| o.name == "write_file" && o.state == OperationState::Unknown)
    })
    .await;
    assert_eq!(executor.executions(), 1);
    assert!(interrupted
        .operations
        .iter()
        .any(|o| o.name == "read_file" && o.state == OperationState::Pending));
    let operation = interrupted
        .operations
        .iter()
        .find(|o| o.name == "write_file")
        .unwrap();
    let resolution = Command::ResolveUnknown {
        id: CommandId::new(),
        operation: operation.id.clone(),
        evidence: "Checked A on disk; retain this evidence after both results".into(),
    };
    command(&handle, resolution.clone()).await;
    assert_recovery_evidence_queued(&dir.db(), resolution.id());
    bounded(handle.shutdown()).await.unwrap();

    let handle = reopen(&dir.db(), executor.clone(), &created.thread).await;
    assert_recovery_evidence_queued(&dir.db(), resolution.id());
    assert_eq!(executor.calls("root-model"), 1);
    command(
        &handle,
        Command::Resume {
            id: CommandId::new(),
            thread: created.thread.clone(),
        },
    )
    .await;
    until(&handle, &created.thread, |_| executor.executions() == 2).await;
    assert_recovery_evidence_queued(&dir.db(), resolution.id());
    assert_eq!(
        executor.calls("root-model"),
        1,
        "B result must precede the next request"
    );
    gate.add_permits(1);
    let settled = completed(&handle, &created.thread).await;
    assert!(settled
        .operations
        .iter()
        .any(|o| o.name == "write_file" && o.state == OperationState::Resolved));
    assert!(settled
        .operations
        .iter()
        .any(|o| o.name == "read_file" && o.state == OperationState::Done));
    assert_eq!(executor.executions(), 2);
    assert_eq!(executor.calls("root-model"), 2);
    let requests = executor.requests();
    let messages = &requests[1].messages;
    let start = messages
        .iter()
        .position(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "A"))
        })
        .unwrap();
    assert_eq!(messages[start].role, Role::Assistant);
    let calls: Vec<_> = messages[start]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["A", "B"]);
    for (offset, id, text) in [(1, "A", "A outcome unknown"), (2, "B", "B read complete")] {
        let result = &messages[start + offset];
        assert_eq!(result.role, Role::Tool);
        assert_eq!(result.content.len(), 1);
        assert!(
            matches!(&result.content[0], ContentBlock::ToolResult { tool_use_id, content, .. }
            if tool_use_id == id && content.as_text() == Some(text))
        );
    }
    assert_eq!(messages[start + 3].role, Role::User);
    assert!(messages[start + 3].text().contains("Checked A on disk"));
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.text().contains("Checked A on disk"))
            .count(),
        1
    );
    assert_eq!(
        readonly(&dir.db())
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE submission=?1 AND model_order IS NOT NULL",
                [resolution.id()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    bounded(handle.shutdown()).await.unwrap();
}

fn assert_recovery_evidence_queued(path: &Path, id: &CommandId) {
    let db = readonly(path);
    let (source, state, text): (String, String, String) = db
        .query_row(
            "SELECT source,state,text FROM submissions WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(source, "recovery");
    assert_eq!(state, "queued");
    assert!(text.contains("Checked A on disk"));
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM entries WHERE submission=?1 AND model_order IS NOT NULL",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap(),
        0,
        "durable evidence must not enter context before the tool batch finishes"
    );
}

#[tokio::test]
async fn waiting_for_approval_blocked_child_does_not_repeat_durable_changes() {
    let dir = TempDir::new();
    let executor = FakeExecutor::scripted(
        vec![
            reply(launch(false)),
            ModelStep::WaitForChild,
            reply(text("parent complete")),
        ],
        vec![
            reply(call("write_file", json!({"text": "approved child write"}))),
            reply(text("child report after approval")),
        ],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(&handle, &dir.0, Policy::default(), false).await;
    submit(&handle, &created.thread, "wait for child approval").await;
    let waiting = until(&handle, &created.thread, |v| {
        v.operations
            .iter()
            .any(|o| o.name == "wait_agent" && o.state == OperationState::Waiting)
    })
    .await;
    let child = waiting
        .threads
        .iter()
        .find(|t| t.parent_run.is_some())
        .unwrap();
    let pending = until(&handle, &child.id, |v| v.decisions.len() == 1).await;
    let decision = pending.decisions[0].clone();
    let waiting = view(&handle, &created.thread).await;
    let changes = count(&dir.db(), "SELECT COUNT(*) FROM changes");
    bounded(async {
        for _ in 0..64 {
            let snapshot = view(&handle, &created.thread).await;
            assert_eq!(snapshot.sequence, waiting.sequence);
            assert!(snapshot
                .operations
                .iter()
                .any(|o| o.name == "wait_agent" && o.state == OperationState::Waiting));
            assert_eq!(count(&dir.db(), "SELECT COUNT(*) FROM changes"), changes);
        }
    })
    .await;
    assert_eq!(executor.calls("root-model"), 2);
    assert_eq!(executor.calls("worker-model"), 1);
    assert_eq!(executor.executions(), 0);
    command(
        &handle,
        Command::Decide {
            id: CommandId::new(),
            operation: decision.operation,
            revision: decision.revision,
            approve: true,
        },
    )
    .await;
    let settled = completed(&handle, &created.thread).await;
    assert!(settled
        .operations
        .iter()
        .any(|o| o.name == "wait_agent" && o.state == OperationState::Done));
    assert_eq!(state(&settled, &decision.thread), ThreadState::Completed);
    assert_eq!(executor.executions(), 1);
    assert_eq!(executor.calls("root-model"), 3);
    assert_eq!(executor.calls("worker-model"), 2);
    assert!(count(&dir.db(), "SELECT COUNT(*) FROM changes") > changes);
    let requests = executor.requests();
    let final_request = requests
        .iter()
        .rfind(|r| r.model.id == "root-model")
        .unwrap();
    assert!(final_request
        .messages
        .iter()
        .any(|m| m.text().contains("child report after approval")));
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn send_message_does_not_restart_a_cancelled_child() {
    let dir = TempDir::new();
    let gate = Arc::new(Semaphore::new(0));
    let executor = FakeExecutor::scripted(
        vec![
            reply(launch(false)),
            ModelStep::SendToChild(gate.clone()),
            ModelStep::UntilCancelled(text("root interrupted")),
        ],
        vec![ModelStep::UntilCancelled(text("cancelled worker"))],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(&handle, &dir.0, Policy::default(), false).await;
    submit(
        &handle,
        &created.thread,
        "launch a child and message it after cancellation",
    )
    .await;
    let active = until(&handle, &created.thread, |_| {
        executor.calls("worker-model") == 1 && executor.calls("root-model") == 2
    })
    .await;
    let child = active
        .threads
        .iter()
        .find(|t| t.parent_run.is_some())
        .unwrap()
        .clone();
    command(
        &handle,
        Command::Stop {
            id: CommandId::new(),
            run: child.run.unwrap(),
        },
    )
    .await;
    until(&handle, &child.id, |v| {
        state(v, &child.id) == ThreadState::Cancelled
    })
    .await;
    gate.add_permits(1);
    until(&handle, &created.thread, |v| {
        v.operations
            .iter()
            .any(|o| o.name == "send_message" && o.state == OperationState::Failed)
    })
    .await;
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(
        executor.calls("worker-model"),
        2,
        "only the child launch and the independent scheduling barrier may run"
    );
    assert_eq!(
        state(&view(&handle, &child.id).await, &child.id),
        ThreadState::Cancelled
    );
    assert_eq!(readonly(&dir.db()).query_row(
        "SELECT COUNT(*) FROM submissions WHERE thread=?1 AND source='coordination' AND state='queued' AND text=?2",
        params![child.id, "coordination queued after cancellation"], |r| r.get::<_, i64>(0),
    ).unwrap(), 0);
    bounded(handle.shutdown()).await.unwrap();

    let executor = FakeExecutor::scripted(vec![], vec![]);
    let handle = reopen(&dir.db(), executor.clone(), &created.thread).await;
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(
        executor.calls("worker-model"),
        1,
        "reopen must not restart the child after its rejected message"
    );
    assert_eq!(executor.calls("root-model"), 0);
    assert_eq!(
        state(&view(&handle, &child.id).await, &child.id),
        ThreadState::Cancelled
    );
    assert_eq!(readonly(&dir.db()).query_row(
        "SELECT COUNT(*) FROM submissions WHERE thread=?1 AND source='coordination' AND state='queued' AND text=?2",
        params![child.id, "coordination queued after cancellation"], |r| r.get::<_, i64>(0),
    ).unwrap(), 0);
    bounded(handle.shutdown()).await.unwrap();
}

fn assert_followup_after_tool_result(request: &RequestSpec) {
    let (call_index, call_id) = request
        .messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::ToolUse { id, name, .. } if name == "read_file" => Some((index, id)),
                _ => None,
            })
        })
        .expect("read_file call missing from model history");
    let result_index = request.messages.iter().position(|message| message.content.iter().any(|block| {
        matches!(block, ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == call_id && content.as_text() == Some("ordered tool output"))
    })).expect("matching read_file result missing from model history");
    let followup_index = request
        .messages
        .iter()
        .position(|m| m.text() == "follow-up accepted during tool execution")
        .expect("accepted follow-up missing from next run");
    assert!(call_index < result_index && result_index < followup_index,
        "model context must follow consumption order, not UI admission order: call={call_index}, result={result_index}, followup={followup_index}");
}

#[tokio::test]
async fn queued_followup_preserves_tool_result_order_live_and_after_reopen() {
    let dir = TempDir::new();
    let gate = Arc::new(Semaphore::new(0));
    let executor = Arc::new(FakeExecutor {
        execute_gate: Some(gate.clone()),
        ..FakeExecutor::default()
    });
    executor.push(
        "root-model",
        vec![
            reply(call("read_file", json!({"text": "ordered tool output"}))),
            reply(text("first run complete")),
            reply(text("follow-up complete")),
        ],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(&handle, &dir.0, Policy::default(), false).await;
    submit(
        &handle,
        &created.thread,
        "read before handling the next instruction",
    )
    .await;
    until(&handle, &created.thread, |_| executor.executions() == 1).await;
    submit(
        &handle,
        &created.thread,
        "follow-up accepted during tool execution",
    )
    .await;
    gate.add_permits(1);
    let settled = until(&handle, &created.thread, |v| {
        state(v, &created.thread) == ThreadState::Completed && executor.calls("root-model") == 3
    })
    .await;
    let requests = executor.requests();
    assert!(!requests[1]
        .messages
        .iter()
        .any(|m| m.text() == "follow-up accepted during tool execution"));
    assert_followup_after_tool_result(&requests[2]);
    let admitted = settled
        .entries
        .iter()
        .find(|e| {
            e.kind == EntryKind::Submission && e.text == "follow-up accepted during tool execution"
        })
        .unwrap();
    let result = settled
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::ToolResult && e.text == "ordered tool output")
        .unwrap();
    assert!(
        admitted.sequence > result.sequence,
        "the follow-up must be shown where it was delivered, after the tool result"
    );
    bounded(handle.shutdown()).await.unwrap();

    let executor = FakeExecutor::scripted(
        vec![reply(text("history restored in consumption order"))],
        vec![],
    );
    let handle = reopen(&dir.db(), executor.clone(), &created.thread).await;
    submit(&handle, &created.thread, "inspect restored history").await;
    completed(&handle, &created.thread).await;
    assert_eq!(executor.calls("root-model"), 1);
    assert_followup_after_tool_result(&executor.requests()[0]);
    assert_eq!(executor.executions(), 0);
    bounded(handle.shutdown()).await.unwrap();
}

async fn artifact_text(handle: &Handle, id: &ArtifactId) -> String {
    bounded(async {
        let mut result = String::new();
        loop {
            let page = bounded(handle.artifact(id.clone(), result.len(), 17_003))
                .await
                .unwrap();
            if page.is_empty() {
                return result;
            }
            result.push_str(&page);
        }
    })
    .await
}

#[tokio::test]
async fn complete_tool_output_and_artifact_pages_survive_reopen() {
    let dir = TempDir::new();
    let output = format!("{}END", "abcdefé\n".repeat(24_000));
    let executor = FakeExecutor::scripted(
        vec![
            reply(call("read_file", json!({"text": output}))),
            reply(text("retained output")),
        ],
        vec![],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(&handle, &dir.0, Policy::default(), false).await;
    submit(&handle, &created.thread, "retain the complete output").await;
    let settled = completed(&handle, &created.thread).await;
    let operation = settled
        .operations
        .iter()
        .find(|o| o.name == "read_file")
        .unwrap();
    let artifact = operation.result.clone().unwrap();
    assert_eq!(operation.state, OperationState::Done);
    assert_eq!(artifact_text(&handle, &artifact).await, output);
    assert!(settled
        .entries
        .iter()
        .any(|e| e.kind == EntryKind::ToolResult && e.artifact.as_ref() == Some(&artifact)));
    assert_eq!(executor.preparations(), 1);
    assert_eq!(executor.executions(), 1);
    bounded(handle.shutdown()).await.unwrap();

    let executor = FakeExecutor::scripted(vec![], vec![]);
    let handle = reopen(&dir.db(), executor.clone(), &created.thread).await;
    let reopened = view(&handle, &created.thread).await;
    assert_eq!(state(&reopened, &created.thread), ThreadState::Completed);
    assert_eq!(artifact_text(&handle, &artifact).await, output);
    let final_entry = reopened
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::Assistant && e.text == "retained output")
        .unwrap();
    let message: Message =
        serde_json::from_str(&artifact_text(&handle, final_entry.artifact.as_ref().unwrap()).await)
            .unwrap();
    assert_eq!(message.text(), "retained output");
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.calls("root-model"), 0);
    assert_eq!(executor.executions(), 0);
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn artifact_page_rejects_limit_smaller_than_next_utf8_character() {
    let dir = TempDir::new();
    let executor = FakeExecutor::scripted(
        vec![
            reply(call("read_file", json!({"text": "é"}))),
            reply(text("output retained")),
        ],
        vec![],
    );
    let handle = open(&dir.db(), executor).await;
    let created = session(&handle, &dir.0, Policy::default(), false).await;
    submit(&handle, &created.thread, "retain a multibyte character").await;
    let settled = completed(&handle, &created.thread).await;
    let artifact = settled
        .operations
        .iter()
        .find(|o| o.name == "read_file")
        .unwrap()
        .result
        .clone()
        .unwrap();
    let error = bounded(handle.artifact(artifact.clone(), 0, 1))
        .await
        .expect_err("a page unable to hold the next character must not report EOF");
    let message = error.to_string();
    assert!(
        message.contains("limit") && message.contains("too small"),
        "{message}"
    );
    assert_eq!(
        bounded(handle.artifact(artifact.clone(), 0, 2))
            .await
            .unwrap(),
        "é"
    );
    assert_eq!(bounded(handle.artifact(artifact, 2, 1)).await.unwrap(), "");
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn incomplete_max_tokens_response_never_admits_tools() {
    let dir = TempDir::new();
    let mut incomplete = call(
        "write_file",
        json!({"text": "complete-looking but truncated response"}),
    );
    incomplete.stop_reason = StopReason::MaxTokens;
    incomplete.message.content.insert(
        0,
        ContentBlock::Text {
            text: "partial answer".into(),
        },
    );
    let executor = FakeExecutor::scripted(vec![reply(incomplete)], vec![]);
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(
        &handle,
        &dir.0,
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(&handle, &created.thread, "receive incomplete response").await;
    let interrupted = until(&handle, &created.thread, |v| {
        state(v, &created.thread) == ThreadState::Failed
    })
    .await;
    assert!(interrupted
        .entries
        .iter()
        .any(|e| e.kind == EntryKind::Assistant && e.text == "partial answer"));
    assert!(interrupted
        .operations
        .iter()
        .all(|o| matches!(o.name.as_str(), "model" | "context")));
    assert!(!interrupted
        .entries
        .iter()
        .any(|e| e.kind == EntryKind::ToolCall));
    assert_eq!(executor.preparations(), 0);
    assert_eq!(executor.executions(), 0);
    bounded(handle.shutdown()).await.unwrap();

    let executor = FakeExecutor::scripted(vec![reply(text("explicitly resumed"))], vec![]);
    let handle = reopen(&dir.db(), executor.clone(), &created.thread).await;
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.calls("root-model"), 0);
    assert_eq!(
        state(&view(&handle, &created.thread).await, &created.thread),
        ThreadState::Failed
    );
    submit(
        &handle,
        &created.thread,
        "Continue after inspecting the incomplete answer.",
    )
    .await;
    completed(&handle, &created.thread).await;
    assert!(executor
        .requests()
        .iter()
        .flat_map(|r| &r.messages)
        .flat_map(|m| &m.content)
        .all(|b| !matches!(b, ContentBlock::ToolUse { .. })));
    assert_eq!(executor.executions(), 0);
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn strict_schema_rejects_invalid_tool_input_before_preparation() {
    let dir = TempDir::new();
    let mut invalid = text("");
    invalid.stop_reason = StopReason::ToolUse;
    invalid.message.content = [
        json!({}),
        json!({"text": 7}),
        json!({"text": "valid field", "extra": true}),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, input)| ContentBlock::ToolUse {
        id: format!("invalid-{i}"),
        name: "write_file".into(),
        input,
    })
    .collect();
    let executor = FakeExecutor::scripted(
        vec![reply(invalid), reply(text("invalid calls rejected"))],
        vec![],
    );
    let handle = open(&dir.db(), executor.clone()).await;
    let created = session(
        &handle,
        &dir.0,
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(&handle, &created.thread, "reject invalid inputs").await;
    let settled = completed(&handle, &created.thread).await;
    let tools: Vec<_> = settled
        .operations
        .iter()
        .filter(|o| o.name == "write_file")
        .collect();
    assert_eq!(tools.len(), 3);
    assert!(tools.iter().all(|o| o.state == OperationState::Failed));
    assert_eq!(executor.preparations(), 0);
    assert_eq!(executor.executions(), 0);
    bounded(handle.shutdown()).await.unwrap();
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match self.0.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

#[tokio::test]
#[ignore = "subprocess fixture; invoked only by crashed_mutation_is_not_reexecuted_and_child_report_is_not_duplicated"]
async fn crash_after_effect_fixture() {
    let dir = PathBuf::from(
        std::env::var_os(FIXTURE_DIR).expect("fixture needs isolated parent-supplied directory"),
    );
    assert!(dir.is_dir());
    let executor = Arc::new(FakeExecutor {
        effect_file: Some(dir.join("effects.log")),
        crash_after_effect: true,
        ..FakeExecutor::default()
    });
    executor.push(
        "root-model",
        vec![
            reply(launch(false)),
            ModelStep::WaitForChild,
            reply(call(
                "write_file",
                json!({"text": "effect committed before process exit"}),
            )),
        ],
    );
    executor.push("worker-model", vec![reply(text("durable worker report"))]);
    let handle = open(&dir.join("runtime.sqlite"), executor).await;
    let created = session(
        &handle,
        &dir,
        Policy {
            yolo: true,
            ..Policy::default()
        },
        false,
    )
    .await;
    submit(
        &handle,
        &created.thread,
        "launch child, retain report, then crash in mutation",
    )
    .await;
    until(&handle, &created.thread, |_| false).await;
    panic!("fixture did not exit from execute");
}

#[tokio::test]
async fn crashed_mutation_is_not_reexecuted_and_child_report_is_not_duplicated() {
    let dir = TempDir::new();
    let mut child = ChildGuard(
        ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "crash_after_effect_fixture",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FIXTURE_DIR, &dir.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let status = bounded(async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_eq!(
        status.code(),
        Some(CRASH_EXIT),
        "fixture failed before effect-before-result window"
    );
    let marker = std::fs::read_to_string(dir.0.join("effects.log")).unwrap();
    assert_eq!(marker.lines().count(), 1);
    let db = readonly(&dir.db());
    let root = ThreadId(
        db.query_row("SELECT id FROM threads WHERE parent_run IS NULL", [], |r| {
            r.get(0)
        })
        .unwrap(),
    );
    let child_thread = ThreadId(
        db.query_row(
            "SELECT id FROM threads WHERE parent_run IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    );
    let operation = OperationId(marker.trim().into());
    let (op_state, result): (String, Option<String>) = db
        .query_row(
            "SELECT state,result FROM operations WHERE id=?1",
            [&operation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(op_state, "started");
    assert!(result.is_none());
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM submissions WHERE sender=?1 AND thread=?2",
            params![child_thread, root],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    drop(db);

    let executor = Arc::new(FakeExecutor {
        effect_file: Some(dir.0.join("effects.log")),
        ..FakeExecutor::default()
    });
    executor.push("root-model", vec![reply(text("recovered with evidence"))]);
    let handle = reopen(&dir.db(), executor.clone(), &root).await;
    let recovered = view(&handle, &root).await;
    let unknown = recovered
        .operations
        .iter()
        .find(|o| o.id == operation)
        .unwrap();
    assert_eq!(unknown.state, OperationState::Unknown);
    assert_eq!(state(&recovered, &root), ThreadState::Interrupted);
    let recovery_artifact = unknown.result.clone().unwrap();
    let recovery_text = artifact_text(&handle, &recovery_artifact).await;
    assert!(recovery_text.contains("unknown"));
    assert!(bounded(handle.command(Command::Resume {
        id: CommandId::new(),
        thread: root.clone()
    }))
    .await
    .is_err());
    scheduling_barrier(&executor, &dir.0).await;
    assert_eq!(executor.calls("root-model"), 0);
    assert_eq!(executor.executions(), 0);
    command(
        &handle,
        Command::ResolveUnknown {
            id: CommandId::new(),
            operation: operation.clone(),
            evidence: format!("Inspected effects.log; operation {operation} was applied once."),
        },
    )
    .await;
    command(
        &handle,
        Command::Resume {
            id: CommandId::new(),
            thread: root.clone(),
        },
    )
    .await;
    let settled = completed(&handle, &root).await;
    assert!(settled
        .operations
        .iter()
        .any(|o| o.id == operation && o.state == OperationState::Resolved));
    assert_eq!(executor.preparations(), 0);
    assert_eq!(executor.executions(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.0.join("effects.log")).unwrap(),
        marker
    );
    assert_eq!(
        count(
            &dir.db(),
            "SELECT COUNT(*) FROM threads WHERE parent_run IS NOT NULL"
        ),
        1
    );
    assert_eq!(
        count(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE name='launch_agent'"
        ),
        1
    );
    assert_eq!(
        readonly(&dir.db())
            .query_row(
                "SELECT COUNT(*) FROM submissions WHERE sender=?1 AND thread=?2",
                params![child_thread, root],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    bounded(handle.shutdown()).await.unwrap();

    let executor = FakeExecutor::scripted(vec![], vec![]);
    let handle = reopen(&dir.db(), executor.clone(), &root).await;
    scheduling_barrier(&executor, &dir.0).await;
    let reopened = view(&handle, &root).await;
    assert_eq!(state(&reopened, &root), ThreadState::Completed);
    assert_eq!(executor.calls("root-model"), 0);
    assert_eq!(executor.executions(), 0);
    assert_eq!(
        artifact_text(&handle, &recovery_artifact).await,
        recovery_text
    );
    assert_eq!(
        count(
            &dir.db(),
            "SELECT COUNT(*) FROM threads WHERE parent_run IS NOT NULL"
        ),
        1
    );
    assert_eq!(
        readonly(&dir.db())
            .query_row(
                "SELECT COUNT(*) FROM submissions WHERE sender=?1 AND thread=?2",
                params![child_thread, root],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("effects.log")).unwrap(),
        marker
    );
    bounded(handle.shutdown()).await.unwrap();
}
