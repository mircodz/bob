use super::*;
use crate::store::tests::fixture;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "runtime_path_authorization_tests.rs"]
mod path_authorization;

struct CheckpointExecutor {
    models: Mutex<Vec<ModelInfo>>,
    capabilities: Mutex<ModelCapabilities>,
    requirements: Mutex<Vec<PreparedAuthorizationRequirement>>,
    target: Mutex<Option<std::path::PathBuf>>,
    prepared: AtomicUsize,
    executed: AtomicUsize,
}
impl CheckpointExecutor {
    fn new(models: Vec<ModelInfo>) -> Self {
        Self {
            models: Mutex::new(models),
            capabilities: Mutex::new(ModelCapabilities::default()),
            requirements: Mutex::new(Vec::new()),
            target: Mutex::new(None),
            prepared: AtomicUsize::new(0),
            executed: AtomicUsize::new(0),
        }
    }
}
#[async_trait]
impl Executor for CheckpointExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.lock().unwrap().clone()
    }
    fn model_capabilities(&self, _: &str) -> ModelCapabilities {
        self.capabilities.lock().unwrap().clone()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        ["code_action", "bash", "read_file"]
            .into_iter()
            .map(|name| ToolDescriptor {
                spec: ToolSpec {
                    name: name.into(),
                    description: String::new(),
                    input_schema: json!({"type":"object"}),
                },
                effect: if name == "read_file" {
                    EffectClass::Read
                } else {
                    EffectClass::Write
                },
            })
            .collect()
    }
    fn prepared_authorization_target(
        &self,
        _: &PreparedTool,
    ) -> anyhow::Result<Option<std::path::PathBuf>> {
        Ok(self.target.lock().unwrap().clone())
    }
    fn prepared_authorization_requirements(
        &self,
        _: &PreparedTool,
    ) -> anyhow::Result<Vec<PreparedAuthorizationRequirement>> {
        Ok(self.requirements.lock().unwrap().clone())
    }
    fn prepared_policy(
        &self,
        tool: &PreparedTool,
        _: &PermissionPolicy,
    ) -> anyhow::Result<Option<RuleDecision>> {
        Ok((tool.invocation.name == "bash").then_some(RuleDecision::Allow))
    }
    async fn generate(
        &self,
        _: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        anyhow::bail!("unexpected generation")
    }
    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        anyhow::bail!("cancel-aware preparation override was not called")
    }
    async fn prepare_with_cancel(
        &self,
        _: Invocation,
        mut cancel: watch::Receiver<bool>,
    ) -> anyhow::Result<PreparedTool> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        crate::cancelled(&mut cancel).await;
        anyhow::bail!("preparation observed cancellation")
    }
    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        self.executed.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("unexpected execution")
    }
}
fn prepared(actor: &Actor, root: &Receipt, name: &str) -> PreparedTool {
    PreparedTool {
        invocation: Invocation {
            operation_id: OperationId::new(),
            run_id: RunId::new(),
            thread_id: root.thread.clone(),
            workspace: crate::sessions::session(&actor.store.db, &root.session)
                .unwrap()
                .workspace,
            name: name.into(),
            input: json!({}),
        },
        specification: json!({}),
        preview: "prepared action".into(),
        presentation: None,
    }
}
fn insert_preparing(actor: &Actor, tool: &PreparedTool) {
    let inv = &tool.invocation;
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![inv.run_id, inv.thread_id],
        )
        .unwrap();
    actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,policy_revision) SELECT ?1,?2,?3,?4,'tool','call',?5,'preparing',s.revision FROM threads t JOIN sessions s ON s.id=t.session WHERE t.id=?3", params![inv.operation_id,inv.run_id,inv.thread_id,inv.name,encode(&inv.input).unwrap()]).unwrap();
}

#[test]
fn bootstrap_failure_keeps_prior_receipts_but_dispatch_paused() {
    let (_dir, store, root, models) = fixture();
    let mut actor = Actor::new(store, Arc::new(CheckpointExecutor::new(models))).unwrap();
    actor.paused = true;
    let submit = Command::Submit {
        id: CommandId::new(),
        thread: root.thread.clone(),
        text: "durable pending input".into(),
    };
    let invalid = Command::SetPolicy {
        id: CommandId::new(),
        session: root.session.clone(),
        policy: Policy::default(),
        expected_revision: 99,
    };
    assert!(actor
        .activate_session(root.session.clone(), vec![submit.clone(), invalid])
        .is_err());
    assert!(actor.paused);
    assert_eq!(actor.selected_session.as_ref(), Some(&root.session));
    let generation = actor.store.claim.as_ref().unwrap().generation.clone();
    assert_eq!(
        crate::jobs::queue(&actor.store.db, &root.thread)
            .unwrap()
            .len(),
        1
    );
    assert!(actor.workers.is_empty());
    let first = actor.store.command(&submit, &actor.models).unwrap();
    let receipts = actor
        .activate_session(root.session.clone(), vec![submit])
        .unwrap();
    assert_eq!(receipts[0].sequence, first.sequence);
    assert_eq!(actor.store.claim.as_ref().unwrap().generation, generation);
    assert!(!actor.paused);
    assert_eq!(actor.selected_session.as_ref(), Some(&root.session));
}

#[tokio::test]
async fn paused_runtime_never_dispatches_queued_input() {
    let (dir, mut store, root, models) = fixture();
    store
        .command(
            &Command::Submit {
                id: CommandId::new(),
                thread: root.thread.clone(),
                text: "must remain queued".into(),
            },
            &models,
        )
        .unwrap();
    drop(store);
    let executor = Arc::new(CheckpointExecutor::new(models));
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor)
        .await
        .unwrap();
    for _ in 0..3 {
        assert!(handle
            .view(Some(root.thread.clone()), None, 10)
            .await
            .unwrap()
            .operations
            .is_empty());
    }
    assert_eq!(handle.queue(root.thread).await.unwrap().len(), 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn activation_only_schedules_selected_session_and_rejects_busy_switch() {
    let (_dir, mut store, root, models) = fixture();
    let snapshot = crate::sessions::session(&store.db, &root.session).unwrap();
    let other = store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace: snapshot.workspace,
                profile: snapshot.profile,
                policy: Policy::default(),
            },
            &models,
        )
        .unwrap();
    store
        .command(
            &Command::Submit {
                id: CommandId::new(),
                thread: root.thread.clone(),
                text: "inactive work".into(),
            },
            &models,
        )
        .unwrap();
    let mut actor = Actor::new(store, Arc::new(CheckpointExecutor::new(models))).unwrap();
    actor
        .activate_session(other.session.clone(), Vec::new())
        .unwrap();
    let (done, _) = mpsc::channel(64);
    actor.schedule(&done).unwrap();
    assert!(actor.workers.is_empty());
    assert_eq!(
        crate::jobs::queue(&actor.store.db, &root.thread)
            .unwrap()
            .len(),
        1
    );
    actor
        .accept_command(&Command::Submit {
            id: CommandId::new(),
            thread: other.thread,
            text: "selected work".into(),
        })
        .unwrap();
    assert!(actor.activate_session(root.session, Vec::new()).is_err());
    assert_eq!(actor.selected_session, Some(other.session));
}

#[test]
fn readonly_catalog_does_not_recover_active_owned_store() {
    let (dir, mut store, root, models) = fixture();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone()).unwrap();
    store.adopt_claim(claim, false).unwrap();
    let actor = Actor::new(store, Arc::new(CheckpointExecutor::new(models))).unwrap();
    let tool = prepared(&actor, &root, "code_action");
    insert_preparing(&actor, &tool);
    let output = artifact(&actor.store.db, &"λ".repeat(200_000)).unwrap();
    let before = durable_sequence(&actor.store.db).unwrap();
    let catalog = crate::SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
    assert_eq!(
        catalog
            .view(Some(root.thread.clone()), None, usize::MAX)
            .unwrap()
            .operations[0]
            .state,
        crate::OperationState::Preparing
    );
    assert_eq!(
        catalog.artifact(output, 0, usize::MAX).unwrap().len(),
        256 * 1024
    );
    assert!(
        catalog
            .transcript(Some(root.thread), None, usize::MAX)
            .unwrap()
            .entries
            .len()
            <= 100
    );
    assert_eq!(durable_sequence(&actor.store.db).unwrap(), before);
    assert_eq!(
        actor
            .store
            .db
            .query_row(
                "SELECT state FROM operations WHERE id=?1",
                [&tool.invocation.operation_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "preparing"
    );
}

#[test]
fn target_policy_merges_with_action_and_fallback_cannot_override_guards() {
    let (_dir, store, root, models) = fixture();
    let executor = Arc::new(CheckpointExecutor::new(models));
    let actor = Actor::new(store, executor.clone()).unwrap();
    let tool = prepared(&actor, &root, "code_action");
    let snapshot = crate::sessions::session(&actor.store.db, &root.session).unwrap();
    let path = Path::new(&snapshot.workspace).join("file.rs");
    *executor.requirements.lock().unwrap() =
        vec![PreparedAuthorizationRequirement::EditFile { path: path.clone() }];
    let caps = executor.tool_capabilities("code_action");
    let mut permissions = PermissionPolicy {
        rules: vec![PermissionRule {
            tool: "code_action".into(),
            decision: RuleDecision::Allow,
        }],
        ..Default::default()
    };
    assert_eq!(
        actor
            .prepared_authorization(
                &snapshot.profile,
                &snapshot.policy,
                &permissions,
                &tool,
                &caps
            )
            .unwrap()
            .0,
        RuleDecision::Ask
    );
    permissions.grants.push(PermissionGrant {
        tool: "edit_file".into(),
        shell_command: None,
        path_prefix: Some(path.to_string_lossy().into_owned()),
        command: None,
    });
    assert_eq!(
        actor
            .prepared_authorization(
                &snapshot.profile,
                &snapshot.policy,
                &permissions,
                &tool,
                &caps
            )
            .unwrap()
            .0,
        RuleDecision::Allow
    );
    permissions.rules.push(PermissionRule {
        tool: "edit_file".into(),
        decision: RuleDecision::Deny,
    });
    assert_eq!(
        actor
            .prepared_authorization(
                &snapshot.profile,
                &Policy {
                    yolo: true,
                    ..Default::default()
                },
                &permissions,
                &tool,
                &caps
            )
            .unwrap()
            .0,
        RuleDecision::Deny
    );
    executor.requirements.lock().unwrap().clear();
    let tool = prepared(&actor, &root, "bash");
    let mut permissions = PermissionPolicy {
        default: RuleDecision::Deny,
        ..Default::default()
    };
    assert_eq!(
        actor
            .prepared_authorization(
                &snapshot.profile,
                &snapshot.policy,
                &permissions,
                &tool,
                &caps
            )
            .unwrap()
            .0,
        RuleDecision::Allow
    );
    for decision in [
        RuleDecision::Deny,
        RuleDecision::RequireApproval,
        RuleDecision::Ask,
    ] {
        permissions.rules = vec![PermissionRule {
            tool: "bash".into(),
            decision,
        }];
        assert_eq!(
            actor
                .prepared_authorization(
                    &snapshot.profile,
                    &snapshot.policy,
                    &permissions,
                    &tool,
                    &caps
                )
                .unwrap()
                .0,
            decision
        );
    }
    permissions.rules.clear();
    assert_eq!(
        actor
            .prepared_authorization(
                &snapshot.profile,
                &Policy {
                    plan: true,
                    ..Default::default()
                },
                &permissions,
                &tool,
                &caps
            )
            .unwrap()
            .0,
        RuleDecision::Deny
    );
}

#[tokio::test]
async fn changed_authorization_targets_do_not_execute_after_approval() {
    let (_dir, store, root, models) = fixture();
    let executor = Arc::new(CheckpointExecutor::new(models));
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    let tool = prepared(&actor, &root, "code_action");
    let path = Path::new(&tool.invocation.workspace).join("first.rs");
    *executor.requirements.lock().unwrap() =
        vec![PreparedAuthorizationRequirement::EditFile { path }];
    insert_preparing(&actor, &tool);
    actor
        .finish(Finished {
            id: tool.invocation.operation_id.clone(),
            result: Ok(Output::Prepared(Box::new(tool.clone()))),
        })
        .unwrap();
    actor
        .accept_command(&Command::Decide {
            id: CommandId::new(),
            operation: tool.invocation.operation_id.clone(),
            revision: 0,
            approve: true,
        })
        .unwrap();
    *executor.requirements.lock().unwrap() = vec![PreparedAuthorizationRequirement::EditFile {
        path: Path::new(&tool.invocation.workspace).join("second.rs"),
    }];
    let (done, _) = mpsc::channel(64);
    let op = actor
        .next_operation(&tool.invocation.run_id)
        .unwrap()
        .unwrap();
    actor.dispatch_tool(op, &done).unwrap();
    assert!(actor.workers.is_empty());
    assert_eq!(executor.executed.load(Ordering::SeqCst), 0);
    assert_eq!(
        actor
            .store
            .db
            .query_row(
                "SELECT state FROM operations WHERE id=?1",
                [&tool.invocation.operation_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "failed"
    );
}

#[tokio::test]
async fn preparation_receives_actual_worker_cancel_signal() {
    let (_dir, store, root, models) = fixture();
    let executor = Arc::new(CheckpointExecutor::new(models));
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    let tool = prepared(&actor, &root, "code_action");
    insert_preparing(&actor, &tool);
    actor
        .store
        .db
        .execute(
            "UPDATE operations SET state='pending' WHERE id=?1",
            [&tool.invocation.operation_id],
        )
        .unwrap();
    let (done, mut completed) = mpsc::channel(64);
    let op = actor
        .next_operation(&tool.invocation.run_id)
        .unwrap()
        .unwrap();
    actor.dispatch_tool(op, &done).unwrap();
    actor.workers[&tool.invocation.operation_id]
        .cancel
        .send(true)
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), completed.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(result
        .result
        .err()
        .unwrap()
        .to_string()
        .contains("observed cancellation"));
    assert_eq!(executor.prepared.load(Ordering::SeqCst), 1);
    assert_eq!(executor.executed.load(Ordering::SeqCst), 0);
}

#[test]
fn invalid_model_refresh_retains_entire_validated_catalog() {
    let (_dir, store, _root, models) = fixture();
    let executor = Arc::new(CheckpointExecutor::new(models));
    let mut actor = Actor::new(store, executor.clone()).unwrap();
    let before = encode(&actor.model_catalog).unwrap();
    executor
        .capabilities
        .lock()
        .unwrap()
        .preferred_output_tokens = 0;
    assert!(actor.refresh_models().is_err());
    assert_eq!(encode(&actor.model_catalog).unwrap(), before);
    assert_eq!(
        actor.model_capabilities("test").preferred_output_tokens,
        4096
    );
    executor
        .capabilities
        .lock()
        .unwrap()
        .preferred_output_tokens = 2048;
    let refreshed = actor.refresh_models().unwrap();
    assert_eq!(refreshed[0].capabilities.preferred_output_tokens, 2048);
    assert_eq!(
        actor.model_capabilities("test").preferred_output_tokens,
        2048
    );
}

#[tokio::test]
async fn idle_scheduler_and_titles_do_not_acquire_database_writer() {
    let (dir, store, _root, models) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CheckpointExecutor::new(models))).unwrap();
    let mut blocker = rusqlite::Connection::open(dir.0.join("lab.db")).unwrap();
    let tx = blocker
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let (done, _) = mpsc::channel(16);
    actor.schedule(&done).unwrap();
    actor.cancel_titles(false).unwrap();
    assert!(actor.workers.is_empty());
    tx.rollback().unwrap();
}

#[tokio::test]
async fn unloaded_thread_model_does_not_halt_actor() {
    let (_dir, store, root, models) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CheckpointExecutor::new(models))).unwrap();
    actor
        .store
        .db
        .execute(
            "UPDATE threads SET profile=json_set(profile,'$.model','unloaded') WHERE id=?1",
            [&root.thread],
        )
        .unwrap();
    actor
        .accept_command(&Command::Submit {
            id: CommandId::new(),
            thread: root.thread.clone(),
            text: "work".into(),
        })
        .unwrap();
    let (done, _) = mpsc::channel(64);
    actor.schedule(&done).unwrap();
    assert!(actor.error.is_none());
    assert!(actor.workers.is_empty());
    assert!(actor
        .store
        .db
        .query_row(
            "SELECT held FROM threads WHERE id=?1",
            [&root.thread],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
}
