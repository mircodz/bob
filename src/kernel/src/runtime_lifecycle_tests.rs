use super::*;

#[path = "lifecycle_tests.rs"]
mod owned;
#[path = "lifecycle_regression_tests.rs"]
mod regressions;
use crate::jobs::{JobKind, JobState};
use crate::presentation::{OperationState, ThreadState};
use crate::store::tests::fixture;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

type Generate = dyn Fn(RequestSpec) -> anyhow::Result<Completion> + Send + Sync;

struct LifecycleExecutor {
    models: Vec<ModelInfo>,
    generate: Box<Generate>,
    failure: ModelFailureKind,
    generated: AtomicUsize,
    executed: Mutex<Vec<Invocation>>,
}

impl LifecycleExecutor {
    fn new(
        models: Vec<ModelInfo>,
        generate: impl Fn(RequestSpec) -> anyhow::Result<Completion> + Send + Sync + 'static,
    ) -> Self {
        Self {
            models,
            generate: Box::new(generate),
            failure: ModelFailureKind::Fatal,
            generated: AtomicUsize::new(0),
            executed: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Executor for LifecycleExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
    fn model_capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities {
            max_output_tokens: 4096,
            preferred_output_tokens: 4096,
            ..Default::default()
        }
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            spec: ToolSpec {
                name: "edit_file".into(),
                description: "Fixture workspace mutation".into(),
                input_schema: json!({"type":"object"}),
            },
            effect: EffectClass::Write,
        }]
    }
    fn classify_error(&self, _: &anyhow::Error) -> ModelFailureKind {
        self.failure
    }
    async fn generate(
        &self,
        request: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        self.generated.fetch_add(1, Ordering::SeqCst);
        (self.generate)(request)
    }
    async fn prepare(&self, invocation: Invocation) -> anyhow::Result<PreparedTool> {
        Ok(PreparedTool {
            invocation,
            specification: json!({}),
            preview: "fixture mutation".into(),
            presentation: None,
        })
    }
    async fn execute(
        &self,
        prepared: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        self.executed.lock().unwrap().push(prepared.invocation);
        Ok(ToolOutcome {
            text: "mutation completed".into(),
            is_error: false,
            interrupted: false,
            presentation: None,
        })
    }
}

fn answer(text: &str) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        stop_reason: StopReason::EndTurn,
        usage: Default::default(),
    }
}

fn tool_call(name: &str, input: Value) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: format!("call-{name}"),
                name: name.into(),
                input,
            }],
        },
        stop_reason: StopReason::ToolUse,
        usage: Default::default(),
    }
}

fn insert_run(store: &Store, thread: &ThreadId) -> RunId {
    let run = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, thread],
        )
        .unwrap();
    run
}

fn insert_tool(
    store: &Store,
    thread: &ThreadId,
    run: &RunId,
    name: &str,
    input: Value,
) -> OperationId {
    let operation = OperationId::new();
    store.db.execute(
        "INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,?4,'tool',?1,?5,'pending')",
        params![operation, run, thread, name, encode(&input).unwrap()],
    ).unwrap();
    operation
}

fn resume(thread: &ThreadId) -> Command {
    Command::Resume {
        id: CommandId::new(),
        thread: thread.clone(),
    }
}

async fn wait_view(handle: &Handle, thread: &ThreadId, predicate: impl Fn(&View) -> bool) -> View {
    let mut last = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = handle.view(Some(thread.clone()), None, 100).await.unwrap();
            assert!(view.error.is_none(), "kernel halted: {:?}", view.error);
            if predicate(&view) {
                return view;
            }
            last = format!(
                "threads: {:?}; operations: {:?}",
                view.threads, view.operations
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!("runtime did not reach the expected lifecycle checkpoint: {error}; {last}")
    })
}

fn operation_state(view: &View, operation: &OperationId) -> Option<OperationState> {
    view.operations
        .iter()
        .find(|op| &op.id == operation)
        .map(|op| op.state)
}

async fn assert_control_requires_current_decision(rule: RuleDecision) {
    for name in ["todo_write", "enter_plan"] {
        let (dir, mut store, root, models) = fixture();
        store
            .command(
                &Command::SetPermissions {
                    id: CommandId::new(),
                    session: root.session.clone(),
                    expected_revision: 0,
                    permissions: PermissionPolicy {
                        rules: vec![PermissionRule {
                            tool: name.into(),
                            decision: rule,
                        }],
                        ..Default::default()
                    },
                },
                &models,
            )
            .unwrap();
        let run = insert_run(&store, &root.thread);
        let input = if name == "todo_write" {
            json!({"todos":[{"content":"approved task","status":"in_progress"}]})
        } else {
            json!({})
        };
        let operation = insert_tool(&store, &root.thread, &run, name, input);
        drop(store);
        let executor = Arc::new(LifecycleExecutor::new(models, |_| Ok(answer("finished"))));
        let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
            .await
            .unwrap();
        handle
            .activate_session(root.session.clone(), vec![resume(&root.thread)])
            .await
            .unwrap();
        let first = wait_view(&handle, &root.thread, |v| !v.decisions.is_empty()).await;
        let revision = first.decisions[0].revision;
        assert_eq!(
            operation_state(&first, &operation),
            Some(OperationState::Approval)
        );
        assert!(handle
            .controls(root.thread.clone())
            .await
            .unwrap()
            .todos
            .is_empty());
        assert!(
            !handle
                .session(root.session.clone())
                .await
                .unwrap()
                .policy
                .plan
        );
        assert_eq!(executor.generated.load(Ordering::SeqCst), 0);
        handle
            .command(Command::SetPolicy {
                id: CommandId::new(),
                session: root.session.clone(),
                policy: Policy::default(),
                expected_revision: revision,
            })
            .await
            .unwrap();
        let current = wait_view(&handle, &root.thread, |v| {
            v.decisions
                .iter()
                .any(|d| d.operation == operation && d.revision == revision + 1)
        })
        .await;
        assert!(handle
            .command(Command::Decide {
                id: CommandId::new(),
                operation: operation.clone(),
                revision,
                approve: true,
            })
            .await
            .is_err());
        assert_eq!(
            operation_state(&current, &operation),
            Some(OperationState::Approval)
        );
        assert!(handle
            .controls(root.thread.clone())
            .await
            .unwrap()
            .todos
            .is_empty());
        assert!(
            !handle
                .session(root.session.clone())
                .await
                .unwrap()
                .policy
                .plan
        );
        handle
            .command(Command::Decide {
                id: CommandId::new(),
                operation: operation.clone(),
                revision: revision + 1,
                approve: true,
            })
            .await
            .unwrap();
        wait_view(&handle, &root.thread, |v| {
            operation_state(v, &operation) == Some(OperationState::Done)
        })
        .await;
        let controls = handle.controls(root.thread.clone()).await.unwrap();
        if name == "todo_write" {
            assert_eq!(controls.todos.len(), 1);
            assert_eq!(controls.todos[0].content, "approved task");
        } else {
            assert!(
                handle
                    .session(root.session.clone())
                    .await
                    .unwrap()
                    .policy
                    .plan
            );
        }
        assert!(executor.executed.lock().unwrap().is_empty());
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn internal_controls_honor_require_approval_and_revision_bound_decisions() {
    assert_control_requires_current_decision(RuleDecision::RequireApproval).await;
}

#[tokio::test]
async fn internal_controls_honor_explicit_ask_and_revision_bound_decisions() {
    assert_control_requires_current_decision(RuleDecision::Ask).await;
}

#[tokio::test]
async fn yolo_bypasses_control_permissions_without_answering_questions_or_approving_plans() {
    let (dir, mut store, root, models) = fixture();
    store
        .command(
            &Command::SetPermissions {
                id: CommandId::new(),
                session: root.session.clone(),
                expected_revision: 0,
                permissions: PermissionPolicy {
                    rules: ["todo_write", "enter_plan", "ask_user", "exit_plan"]
                        .into_iter()
                        .map(|name| PermissionRule {
                            tool: name.into(),
                            decision: RuleDecision::RequireApproval,
                        })
                        .collect(),
                    ..Default::default()
                },
            },
            &models,
        )
        .unwrap();
    store
        .command(
            &Command::SetPolicy {
                id: CommandId::new(),
                session: root.session.clone(),
                expected_revision: 1,
                policy: Policy {
                    yolo: true,
                    ..Default::default()
                },
            },
            &models,
        )
        .unwrap();
    let run = insert_run(&store, &root.thread);
    let todo = insert_tool(
        &store,
        &root.thread,
        &run,
        "todo_write",
        json!({"todos":[{"content":"work","status":"in_progress"}]}),
    );
    let enter = insert_tool(&store, &root.thread, &run, "enter_plan", json!({}));
    let ask = insert_tool(
        &store,
        &root.thread,
        &run,
        "ask_user",
        json!({"question":"Which approach?","options":["A","B"]}),
    );
    let exit = insert_tool(
        &store,
        &root.thread,
        &run,
        "exit_plan",
        json!({"plan":"Implement only after approval."}),
    );
    drop(store);
    let executor = Arc::new(LifecycleExecutor::new(models, |_| Ok(answer("finished"))));
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(root.session.clone(), vec![resume(&root.thread)])
        .await
        .unwrap();
    let view = wait_view(&handle, &root.thread, |v| {
        operation_state(v, &ask) == Some(OperationState::Waiting)
    })
    .await;
    assert_eq!(operation_state(&view, &todo), Some(OperationState::Done));
    assert_eq!(operation_state(&view, &enter), Some(OperationState::Done));
    assert!(view.decisions.is_empty());
    let controls = handle.controls(root.thread.clone()).await.unwrap();
    assert_eq!(controls.todos.len(), 1);
    assert_eq!(controls.questions.len(), 1);
    assert!(controls.plans.is_empty());
    for _ in 0..3 {
        let view = handle
            .view(Some(root.thread.clone()), None, 100)
            .await
            .unwrap();
        assert_eq!(operation_state(&view, &ask), Some(OperationState::Waiting));
        assert_eq!(operation_state(&view, &exit), Some(OperationState::Pending));
    }
    assert_eq!(executor.generated.load(Ordering::SeqCst), 0);
    let question = &controls.questions[0];
    handle
        .command(Command::AnswerQuestion {
            id: CommandId::new(),
            operation: ask,
            revision: question.revision,
            answer: "B".into(),
        })
        .await
        .unwrap();
    wait_view(&handle, &root.thread, |v| {
        operation_state(v, &exit) == Some(OperationState::Waiting)
    })
    .await;
    let controls = handle.controls(root.thread.clone()).await.unwrap();
    assert!(controls.questions.is_empty());
    assert_eq!(controls.plans.len(), 1);
    assert!(
        handle
            .session(root.session.clone())
            .await
            .unwrap()
            .policy
            .plan
    );
    assert_eq!(executor.generated.load(Ordering::SeqCst), 0);
    handle
        .command(Command::DecidePlan {
            id: CommandId::new(),
            operation: exit.clone(),
            revision: controls.plans[0].revision,
            approve: true,
            feedback: None,
        })
        .await
        .unwrap();
    wait_view(&handle, &root.thread, |v| {
        operation_state(v, &exit) == Some(OperationState::Done)
    })
    .await;
    let policy = handle.session(root.session).await.unwrap().policy;
    assert!(!policy.plan);
    assert!(policy.yolo);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn inactive_recovered_ready_write_does_not_block_selected_session_in_same_workspace() {
    let (dir, mut store, inactive, models) = fixture();
    let snapshot = crate::sessions::session(&store.db, &inactive.session).unwrap();
    let active = store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace: snapshot.workspace.clone(),
                profile: snapshot.profile,
                policy: Policy {
                    yolo: true,
                    ..Default::default()
                },
            },
            &models,
        )
        .unwrap();
    let old_run = insert_run(&store, &inactive.thread);
    let old = insert_tool(&store, &inactive.thread, &old_run, "edit_file", json!({}));
    let prepared = PreparedTool {
        invocation: Invocation {
            operation_id: old.clone(),
            run_id: old_run,
            thread_id: inactive.thread.clone(),
            workspace: snapshot.workspace,
            name: "edit_file".into(),
            input: json!({}),
        },
        specification: json!({}),
        preview: "inactive write".into(),
        presentation: None,
    };
    store
        .db
        .execute(
            "UPDATE operations SET state='ready',effect_write=1,prepared=?2 WHERE id=?1",
            params![old, encode(&prepared).unwrap()],
        )
        .unwrap();
    let run = insert_run(&store, &active.thread);
    let selected = insert_tool(&store, &active.thread, &run, "edit_file", json!({}));
    drop(store);
    let executor = Arc::new(LifecycleExecutor::new(models, |_| Ok(answer("finished"))));
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(active.session, vec![resume(&active.thread)])
        .await
        .unwrap();
    wait_view(&handle, &active.thread, |v| {
        operation_state(v, &selected) == Some(OperationState::Done)
    })
    .await;
    let view = handle.view(Some(inactive.thread), None, 100).await.unwrap();
    assert_eq!(operation_state(&view, &old), Some(OperationState::Ready));
    let executed = executor.executed.lock().unwrap().clone();
    assert_eq!(executed.len(), 1);
    assert_eq!(executed[0].operation_id, selected);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn paused_same_session_bootstrap_can_retry_after_resume_commits_and_policy_fails() {
    let (dir, store, root, models) = fixture();
    insert_run(&store, &root.thread);
    drop(store);
    let executor = Arc::new(LifecycleExecutor::new(models, |_| {
        Ok(answer("resumed safely"))
    }));
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(root.session.clone(), vec![])
        .await
        .unwrap();
    let resume = resume(&root.thread);
    assert!(handle
        .activate_session(
            root.session.clone(),
            vec![
                resume.clone(),
                Command::SetPolicy {
                    id: CommandId::new(),
                    session: root.session.clone(),
                    policy: Policy::default(),
                    expected_revision: 99,
                }
            ]
        )
        .await
        .is_err());
    let receipt = handle.command(resume.clone()).await.unwrap();
    for _ in 0..3 {
        let view = handle
            .view(Some(root.thread.clone()), None, 100)
            .await
            .unwrap();
        assert!(view.error.is_none());
        assert!(view.operations.is_empty());
    }
    assert_eq!(executor.generated.load(Ordering::SeqCst), 0);
    let retried = handle
        .activate_session(
            root.session.clone(),
            vec![
                resume,
                Command::SetPolicy {
                    id: CommandId::new(),
                    session: root.session,
                    policy: Policy::default(),
                    expected_revision: 0,
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(retried[0].sequence, receipt.sequence);
    wait_view(&handle, &root.thread, |v| {
        v.threads
            .iter()
            .any(|t| t.id == root.thread && t.state == ThreadState::Completed)
    })
    .await;
    assert_eq!(executor.generated.load(Ordering::SeqCst), 1);
    handle.shutdown().await.unwrap();
}

#[derive(Clone, Copy)]
enum Failure {
    Fatal,
    Exhausted,
    Incomplete,
}

fn failed_completion(failure: Failure) -> anyhow::Result<Completion> {
    match failure {
        Failure::Fatal | Failure::Exhausted => anyhow::bail!("fixture provider failure"),
        Failure::Incomplete => {
            let mut completion = tool_call("edit_file", json!({}));
            completion.message.content.insert(
                0,
                ContentBlock::Text {
                    text: "partial child answer".into(),
                },
            );
            completion.stop_reason = StopReason::MaxTokens;
            Ok(completion)
        }
    }
}

fn seed_exhaustion(store: &Store, failure: Failure) {
    if matches!(failure, Failure::Exhausted) {
        store.db.execute_batch("CREATE TRIGGER exhausted_retry_fixture AFTER INSERT ON runs BEGIN UPDATE runs SET retry_count=3 WHERE id=NEW.id; END;").unwrap();
    }
}

async fn assert_failed_children_settle_collectors(failure: Failure) {
    for background in [false, true] {
        let (dir, store, root, models) = fixture();
        seed_exhaustion(&store, failure);
        drop(store);
        let child_attempts = Arc::new(AtomicUsize::new(0));
        let attempts = child_attempts.clone();
        let mut executor = LifecycleExecutor::new(models, move |request| {
            if request.system.contains("# Role: task child") {
                if request
                    .messages
                    .iter()
                    .any(|m| m.text().contains("failing-child-fixture"))
                {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    return failed_completion(failure);
                }
                return Ok(answer("successful sibling evidence"));
            }
            let launched =
                request.messages.iter().flat_map(|m| &m.content).any(
                    |block| matches!(block, ContentBlock::ToolUse { name, .. } if name == "task"),
                );
            if launched {
                Ok(answer("parent finished"))
            } else {
                Ok(tool_call(
                    "task",
                    json!({"background":background,"tasks":[
                        {"description":"successful sibling","prompt":"successful-child-fixture"},
                        {"description":"failed sibling","prompt":"failing-child-fixture"}
                    ]}),
                ))
            }
        });
        if matches!(failure, Failure::Exhausted) {
            executor.failure = ModelFailureKind::Transient;
        }
        let executor = Arc::new(executor);
        let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
            .await
            .unwrap();
        handle
            .activate_session(
                root.session.clone(),
                vec![Command::Submit {
                    id: CommandId::new(),
                    thread: root.thread.clone(),
                    text: "launch batch".into(),
                }],
            )
            .await
            .unwrap();
        let view = wait_view(&handle, &root.thread, |v| {
            v.threads
                .iter()
                .any(|t| t.name == "failed sibling" && t.state == ThreadState::Failed)
                && v.threads
                    .iter()
                    .any(|t| t.name == "successful sibling" && t.state == ThreadState::Completed)
        })
        .await;
        let child = view
            .threads
            .iter()
            .find(|t| t.name == "failed sibling")
            .unwrap()
            .id
            .clone();
        let task = view
            .operations
            .iter()
            .find(|o| o.name == "task")
            .unwrap()
            .id
            .clone();
        let report: Value = if background {
            let job = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(job) = handle
                        .jobs(root.session.clone())
                        .await
                        .unwrap()
                        .into_iter()
                        .find(|j| j.kind == JobKind::Task && j.state == JobState::Failed)
                    {
                        break job;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("background task collector remained pending");
            let page = handle.job_output(job.id, 0, 24000).await.unwrap();
            assert!(page.next_offset.is_none());
            serde_json::from_str(&page.text).unwrap()
        } else {
            wait_view(&handle, &root.thread, |v| {
                operation_state(v, &task) == Some(OperationState::Failed)
            })
            .await;
            serde_json::from_str(&handle.tool_output(task, 0, 64 * 1024).await.unwrap().text)
                .unwrap()
        };
        assert_eq!(report["status"], "failed");
        let agents = report["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2);
        let success = agents
            .iter()
            .find(|a| a["name"] == "successful sibling")
            .unwrap();
        assert_eq!(success["status"], "completed");
        assert_eq!(success["output"], "successful sibling evidence");
        let failed = agents
            .iter()
            .find(|a| a["name"] == "failed sibling")
            .unwrap();
        assert_eq!(failed["status"], "failed");
        assert!(failed["result_artifact"].is_string());
        assert!(!failed["output"].as_str().unwrap().is_empty());
        assert!(handle.queue(child.clone()).await.unwrap().is_empty());
        let child_view = handle.view(Some(child), None, 100).await.unwrap();
        assert!(!child_view.operations.iter().any(|o| o.name == "edit_file"));
        if matches!(failure, Failure::Incomplete) {
            assert!(child_view
                .entries
                .iter()
                .any(|e| e.text.contains("partial child answer")));
        }
        assert_eq!(child_attempts.load(Ordering::SeqCst), 1);
        assert!(executor.executed.lock().unwrap().is_empty());
        wait_view(&handle, &root.thread, |v| {
            v.threads
                .iter()
                .any(|t| t.id == root.thread && t.state == ThreadState::Completed)
        })
        .await;
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn fatal_child_failure_settles_inline_and_background_task_collectors() {
    assert_failed_children_settle_collectors(Failure::Fatal).await;
}

#[tokio::test]
async fn exhausted_child_failure_settles_inline_and_background_task_collectors() {
    assert_failed_children_settle_collectors(Failure::Exhausted).await;
}

#[tokio::test]
async fn incomplete_child_answer_settles_collectors_without_executing_partial_calls() {
    assert_failed_children_settle_collectors(Failure::Incomplete).await;
}

#[tokio::test]
async fn terminal_model_failures_require_explicit_new_root_instruction() {
    for failure in [Failure::Fatal, Failure::Exhausted, Failure::Incomplete] {
        let (dir, store, root, models) = fixture();
        seed_exhaustion(&store, failure);
        drop(store);
        let attempts = AtomicUsize::new(0);
        let mut executor = LifecycleExecutor::new(models, move |_| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                failed_completion(failure)
            } else {
                Ok(answer("root recovered"))
            }
        });
        if matches!(failure, Failure::Exhausted) {
            executor.failure = ModelFailureKind::Transient;
        }
        let executor = Arc::new(executor);
        let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
            .await
            .unwrap();
        handle
            .activate_session(
                root.session,
                vec![Command::Submit {
                    id: CommandId::new(),
                    thread: root.thread.clone(),
                    text: "root work".into(),
                }],
            )
            .await
            .unwrap();
        let interrupted = wait_view(&handle, &root.thread, |v| {
            v.threads
                .iter()
                .any(|t| t.id == root.thread && t.state == ThreadState::Failed)
        })
        .await;
        let run = interrupted
            .threads
            .iter()
            .find(|t| t.id == root.thread)
            .unwrap()
            .run
            .clone();
        assert_eq!(executor.generated.load(Ordering::SeqCst), 1);
        assert!(executor.executed.lock().unwrap().is_empty());
        handle
            .command(Command::Submit {
                id: CommandId::new(),
                thread: root.thread.clone(),
                text: "Continue after inspecting the failure.".into(),
            })
            .await
            .unwrap();
        let completed = wait_view(&handle, &root.thread, |v| {
            v.threads
                .iter()
                .any(|t| t.id == root.thread && t.state == ThreadState::Completed)
        })
        .await;
        assert_ne!(
            completed
                .threads
                .iter()
                .find(|t| t.id == root.thread)
                .unwrap()
                .run,
            run
        );
        assert_eq!(executor.generated.load(Ordering::SeqCst), 2);
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn queued_child_of_completed_parent_remains_unknown_and_held_across_reopens() {
    let (dir, mut store, root, models) = fixture();
    let run = insert_run(&store, &root.thread);
    let input = json!({"background":true,"tasks":[{"description":"queued child","prompt":"must not autoexecute"}]});
    let operation = insert_tool(&store, &root.thread, &run, "task", input.clone());
    let profile = crate::sessions::session(&store.db, &root.session)
        .unwrap()
        .profile;
    let tx = store.db.transaction().unwrap();
    crate::delegation::launch_group(
        &tx,
        crate::delegation::GroupRequest {
            operation: &operation,
            run: &run,
            thread: &root.thread,
            name: "task",
            input: &input,
        },
        &profile,
        &models,
    )
    .unwrap();
    tx.execute(
        "UPDATE operations SET state='done' WHERE id=?1",
        [&operation],
    )
    .unwrap();
    tx.execute("UPDATE runs SET state='completed' WHERE id=?1", [&run])
        .unwrap();
    let child = ThreadId(
        tx.query_row(
            "SELECT thread FROM agents WHERE operation=?1",
            [&operation],
            |r| r.get(0),
        )
        .unwrap(),
    );
    tx.commit().unwrap();
    drop(store);
    let executor = Arc::new(LifecycleExecutor::new(models, |_| {
        anyhow::bail!("recovered queued child must not generate")
    }));
    for _ in 0..2 {
        let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
            .await
            .unwrap();
        handle
            .activate_session(root.session.clone(), vec![])
            .await
            .unwrap();
        for _ in 0..3 {
            let view = handle.view(Some(child.clone()), None, 100).await.unwrap();
            assert!(view.error.is_none());
            let summary = view.threads.iter().find(|t| t.id == child).unwrap();
            assert_eq!(summary.state, ThreadState::Held);
            assert!(summary.run.is_none());
            assert!(view.operations.is_empty());
            assert!(handle.queue(child.clone()).await.unwrap().is_empty());
        }
        assert_eq!(executor.generated.load(Ordering::SeqCst), 0);
        handle.shutdown().await.unwrap();
        let db = rusqlite::Connection::open_with_flags(
            dir.0.join("lab.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            db.query_row(
                "SELECT state,text FROM submissions WHERE thread=?1",
                [&child],
                |r| { Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)) }
            )
            .unwrap(),
            ("queued".into(), "must not autoexecute".into())
        );
        assert_eq!(
            db.query_row("SELECT state FROM agents WHERE thread=?1", [&child], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "unknown"
        );
        assert_eq!(
            db.query_row(
                "SELECT state FROM jobs WHERE operation=?1",
                [&operation],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "unknown"
        );
        assert_eq!(
            db.query_row("SELECT state FROM runs WHERE id=?1", [&run], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "completed"
        );
    }
}
