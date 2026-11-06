use super::*;
use crate::jobs::{self, JobState};
use crate::store::tests::{fixture, TempDir};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

struct ShellExecutor {
    calls: AtomicUsize,
    release: Semaphore,
    requests: Mutex<Vec<RequestSpec>>,
    invocations: Mutex<Vec<PreparedTool>>,
}

impl ShellExecutor {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            release: Semaphore::new(0),
            requests: Mutex::new(Vec::new()),
            invocations: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Executor for ShellExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "test".into(),
            context_window: 10_000_000,
        }]
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        ["bash", "edit_file"]
            .into_iter()
            .map(|name| ToolDescriptor {
                spec: ToolSpec {
                    name: name.into(),
                    description: String::new(),
                    input_schema: json!({"type":"object"}),
                },
                effect: EffectClass::Write,
            })
            .collect()
    }
    fn prepared_policy(
        &self,
        _: &PreparedTool,
        _: &PermissionPolicy,
    ) -> anyhow::Result<Option<RuleDecision>> {
        Ok(Some(RuleDecision::Allow))
    }
    async fn generate(
        &self,
        spec: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        self.requests.lock().unwrap().push(spec);
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "New input considered.".into(),
                }],
            },
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        })
    }
    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        anyhow::bail!("fixture supplies the authorized prepared invocation")
    }
    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        anyhow::bail!("foreground Bash must use the progress-enabled path")
    }
    async fn execute_with_progress(
        &self,
        tool: PreparedTool,
        mut cancel: watch::Receiver<bool>,
        progress: ToolProgress,
    ) -> anyhow::Result<ToolOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.invocations.lock().unwrap().push(tool);
        progress.append("before\n".into()).await?;
        tokio::select! {
            permit = self.release.acquire() => { permit?.forget(); }
            _ = cancelled(&mut cancel) => return Ok(outcome("cancelled", false)),
        }
        progress.append("after\n".into()).await?;
        Ok(outcome("before\nafter\n", false))
    }
}

fn outcome(text: &str, interrupted: bool) -> ToolOutcome {
    ToolOutcome {
        text: text.into(),
        is_error: false,
        interrupted,
        presentation: None,
    }
}

fn setup() -> (TempDir, Actor, Receipt, PreparedTool, Arc<ShellExecutor>) {
    let (dir, store, root, _) = fixture();
    let executor = Arc::new(ShellExecutor::new());
    let actor = Actor::new_owned(store, executor.clone()).unwrap();
    let tool = PreparedTool {
        invocation: Invocation {
            operation_id: OperationId::new(),
            run_id: RunId::new(),
            thread_id: root.thread.clone(),
            workspace: crate::sessions::session(&actor.store.db, &root.session)
                .unwrap()
                .workspace,
            name: "bash".into(),
            input: json!({"command":"bounded fixture command", "timeout":37, "run_in_background":false}),
        },
        specification: json!({"immutable":"prepared bytes"}),
        preview: "bounded fixture command".into(),
        presentation: None,
    };
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![tool.invocation.run_id, root.thread],
        )
        .unwrap();
    actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,prepared,effect_write) VALUES(?1,?2,?3,'bash','tool','shell-call',?4,'ready',?5,1)", params![tool.invocation.operation_id,tool.invocation.run_id,root.thread,encode(&tool.invocation.input).unwrap(),encode(&tool).unwrap()]).unwrap();
    let message = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "shell-call".into(),
            name: "bash".into(),
            input: tool.invocation.input.clone(),
        }],
    };
    entry(
        &actor.store.db,
        &root.thread,
        "assistant",
        "",
        EntryData {
            message: Some(&message),
            ..Default::default()
        },
    )
    .unwrap();
    (dir, actor, root, tool, executor)
}

fn started(actor: &mut Actor, tool: &PreparedTool) {
    let tx = actor.store.db.transaction().unwrap();
    tx.execute(
        "UPDATE operations SET state='started' WHERE id=?1",
        [&tool.invocation.operation_id],
    )
    .unwrap();
    jobs::track_foreground(&tx, tool).unwrap();
    tx.commit().unwrap();
}

fn input(root: &Receipt, text: &str, steer: bool) -> Command {
    if steer {
        Command::Steer {
            id: CommandId::new(),
            thread: root.thread.clone(),
            text: text.into(),
        }
    } else {
        Command::Submit {
            id: CommandId::new(),
            thread: root.thread.clone(),
            text: text.into(),
        }
    }
}

fn result_count(actor: &Actor, operation: &OperationId) -> usize {
    actor.store.db.query_row("SELECT COUNT(*) FROM entries WHERE operation=?1 AND kind IN ('tool_result','tool_error')", [operation], |r| r.get(0)).unwrap()
}

async fn progress(actor: &mut Actor) {
    let update = tokio::time::timeout(Duration::from_secs(2), actor.progress_rx.recv())
        .await
        .unwrap()
        .unwrap();
    jobs::append(&mut actor.store.db, update).unwrap();
}

async fn finish_next(actor: &mut Actor, rx: &mut mpsc::Receiver<Finished>) -> OperationId {
    let finished = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let id = finished.id.clone();
    actor.workers.remove(&id).unwrap().task.await.unwrap();
    actor.finish(finished).unwrap();
    id
}

#[tokio::test]
async fn submit_and_steer_deliver_before_shell_release_without_restarting() {
    for steer in [false, true] {
        let (_dir, mut actor, root, tool, executor) = setup();
        let (done, mut completed) = mpsc::channel(16);
        let op = actor
            .next_operation(&tool.invocation.run_id)
            .unwrap()
            .unwrap();
        actor.dispatch_tool(op, &done).unwrap();
        progress(&mut actor).await;
        assert!(jobs::list(&actor.store.db, &root.session)
            .unwrap()
            .is_empty());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        let worker_id = actor.workers[&tool.invocation.operation_id].task.id();
        let command = input(&root, "change direction now", steer);
        let receipt = actor.accept_command(&command).unwrap();
        assert_eq!(
            actor.accept_command(&command).unwrap().sequence,
            receipt.sequence
        );
        assert_eq!(
            actor.workers[&tool.invocation.operation_id].task.id(),
            worker_id
        );
        let job = jobs::list(&actor.store.db, &root.session)
            .unwrap()
            .remove(0);
        assert!(job.background);
        assert_eq!(job.state, JobState::Running);
        assert_eq!(
            jobs::page(&actor.store.db, &job.id, 0, 100).unwrap().text,
            "before\n"
        );
        assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
        actor.store.db.execute("INSERT INTO model_context(thread,system,epoch,profile_revision) SELECT id,'fixture',0,profile_revision FROM threads WHERE id=?1", [&root.thread]).unwrap();
        actor.run_cursor = 0;
        actor.schedule(&done).unwrap();
        let followup = input(&root, "and preserve this follow-up", false);
        actor.accept_command(&followup).unwrap();
        assert!(crate::lifecycle::actionable(&actor.store.db, &tool.invocation.run_id).unwrap());
        assert!(crate::lifecycle::guidance(&actor.store.db, &tool.invocation.run_id).unwrap());
        assert_ne!(
            finish_next(&mut actor, &mut completed).await,
            tool.invocation.operation_id
        );
        assert_eq!(
            actor
                .store
                .db
                .query_row(
                    "SELECT state FROM runs WHERE id=?1",
                    [&tool.invocation.run_id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "active"
        );
        assert!(executor.requests.lock().unwrap()[0]
            .messages
            .iter()
            .any(|m| m.text() == "change direction now"));
        actor.store.db.execute("INSERT INTO model_context(thread,system,epoch,profile_revision) SELECT id,'fixture',0,profile_revision FROM threads WHERE id=?1", [&root.thread]).unwrap();
        actor.run_cursor = 0;
        actor.schedule(&done).unwrap();
        assert_ne!(
            finish_next(&mut actor, &mut completed).await,
            tool.invocation.operation_id
        );
        let messages: Vec<_> = executor.requests.lock().unwrap()[1]
            .messages
            .iter()
            .map(Message::text)
            .collect();
        assert_eq!(
            messages
                .iter()
                .filter(|text| text.as_str() == "change direction now")
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|text| text.as_str() == "and preserve this follow-up")
                .count(),
            1
        );
        assert!(
            messages
                .iter()
                .position(|text| text == "change direction now")
                .unwrap()
                < messages
                    .iter()
                    .position(|text| text == "and preserve this follow-up")
                    .unwrap()
        );
        assert!(jobs::queue(&actor.store.db, &root.thread)
            .unwrap()
            .is_empty());
        assert!(actor.workers.contains_key(&tool.invocation.operation_id));
        assert_eq!(
            encode(&executor.invocations.lock().unwrap()[0]).unwrap(),
            encode(&tool).unwrap()
        );
        let source: String = actor
            .store
            .db
            .query_row(
                "SELECT source FROM submissions WHERE id=?1",
                [command.id()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(source, if steer { "steering" } else { "human" });
        executor.release.add_permits(1);
        progress(&mut actor).await;
        assert_eq!(
            jobs::page(&actor.store.db, &job.id, 0, 100).unwrap().text,
            "before\nafter\n"
        );
        assert_eq!(
            finish_next(&mut actor, &mut completed).await,
            tool.invocation.operation_id
        );
        assert_eq!(
            jobs::list(&actor.store.db, &root.session).unwrap()[0].state,
            JobState::Completed
        );
        assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn promotion_closes_unstarted_batch_and_preserves_fifo_and_idempotency() {
    let (_dir, mut actor, root, tool, _) = setup();
    let earlier = input(&root, "earlier queued input", false);
    actor.accept_command(&earlier).unwrap();
    started(&mut actor, &tool);
    let mut pending = Vec::new();
    for state in ["pending", "ready", "approval", "waiting"] {
        let id = OperationId::new();
        actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'edit_file','tool',?1,'{}',?4)", params![id,tool.invocation.run_id,root.thread,state]).unwrap();
        if state == "approval" {
            actor.store.db.execute("INSERT INTO decisions(operation,revision,preview,state) VALUES(?1,0,'fixture','pending')", [&id]).unwrap();
        }
        pending.push(id);
    }
    let command = input(&root, "latest input", false);
    let first = actor.accept_command(&command).unwrap();
    assert_eq!(
        first.sequence,
        actor.accept_command(&command).unwrap().sequence
    );
    for id in &pending[..3] {
        assert_eq!(result_count(&actor, id), 1);
        assert!(actor
            .store
            .tool_output(id, 0, 1000)
            .unwrap()
            .text
            .contains("Not executed; new human input"));
    }
    assert_eq!(result_count(&actor, &pending[3]), 0);
    let decision: String = actor
        .store
        .db
        .query_row(
            "SELECT state FROM decisions WHERE operation=?1",
            [&pending[2]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(decision, "stale");
    let ids = actor
        .store
        .db
        .prepare("SELECT id FROM submissions WHERE source='human' AND owner_run=?1 ORDER BY rowid")
        .unwrap()
        .query_map([&tool.invocation.run_id], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(ids, vec![earlier.id().0.clone(), command.id().0.clone()]);
    assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
    assert_eq!(
        actor
            .store
            .db
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE kind='notice'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn promotion_and_submission_roll_back_together_on_receipt_failure() {
    let (_dir, mut actor, root, tool, _) = setup();
    started(&mut actor, &tool);
    actor.store.db.execute_batch("CREATE TRIGGER reject_receipt BEFORE INSERT ON commands BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;").unwrap();
    let command = input(&root, "atomic", false);
    assert!(actor.accept_command(&command).is_err());
    assert!(jobs::list(&actor.store.db, &root.session)
        .unwrap()
        .is_empty());
    assert!(jobs::queue(&actor.store.db, &root.thread)
        .unwrap()
        .is_empty());
    assert_eq!(result_count(&actor, &tool.invocation.operation_id), 0);
    actor
        .store
        .db
        .execute_batch("DROP TRIGGER reject_receipt")
        .unwrap();
    actor.accept_command(&command).unwrap();
    assert_eq!(jobs::list(&actor.store.db, &root.session).unwrap().len(), 1);
}

#[test]
fn only_current_running_authorized_foreground_bash_is_promoted() {
    for mutation in [
        "UPDATE operations SET state='pending'",
        "UPDATE operations SET state='preparing'",
        "UPDATE operations SET state='approval'",
        "UPDATE operations SET state='ready'",
        "UPDATE operations SET state='cancelled'",
        "UPDATE operations SET state='done'",
        "UPDATE operations SET name='edit_file'",
        "UPDATE threads SET held=1",
        "UPDATE runs SET state='stopping'",
        "UPDATE runs SET state='interrupted'",
        "UPDATE runs SET state='completed'",
        "UPDATE jobs SET state='stopping'",
        "UPDATE jobs SET state='completed'",
        "UPDATE threads SET profile_revision=profile_revision+1",
        "UPDATE contexts SET epoch=epoch+1",
        "UPDATE operations SET input=json_set(input,'$.run_in_background',json('true'))",
    ] {
        let (_dir, mut actor, root, tool, _) = setup();
        started(&mut actor, &tool);
        actor.store.db.execute_batch(mutation).unwrap();
        actor
            .accept_command(&input(&root, "retain my input", false))
            .unwrap();
        assert!(
            jobs::list(&actor.store.db, &root.session)
                .unwrap()
                .is_empty(),
            "{mutation}"
        );
        assert_eq!(
            jobs::queue(&actor.store.db, &root.thread).unwrap().len(),
            1,
            "{mutation}"
        );
        assert_eq!(
            result_count(&actor, &tool.invocation.operation_id),
            0,
            "{mutation}"
        );
    }
}

#[test]
fn unknown_or_concurrent_preparation_does_not_get_bypassed() {
    for state in ["unknown", "preparing", "started"] {
        let (_dir, mut actor, root, tool, _) = setup();
        started(&mut actor, &tool);
        actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'edit_file','tool','other','{}',?4)", params![OperationId::new(),tool.invocation.run_id,root.thread,state]).unwrap();
        actor
            .accept_command(&input(&root, "do not bypass", false))
            .unwrap();
        assert!(jobs::list(&actor.store.db, &root.session)
            .unwrap()
            .is_empty());
        assert_eq!(jobs::queue(&actor.store.db, &root.thread).unwrap().len(), 1);
    }
}

#[test]
fn job_cap_keeps_input_and_foreground_execution_unchanged() {
    let (_dir, mut actor, root, tool, _) = setup();
    started(&mut actor, &tool);
    for _ in 0..64 {
        actor.store.db.execute("INSERT INTO jobs(id,session,thread,kind,state,title,background,epoch) VALUES(?1,?2,?3,'shell','running','explicit',1,0)", params![JobId::new(),root.session,root.thread]).unwrap();
    }
    let receipt = actor
        .accept_command(&input(&root, "keep queued", false))
        .unwrap();
    assert!(receipt.detail.contains("active job limit"));
    assert_eq!(jobs::queue(&actor.store.db, &root.thread).unwrap().len(), 1);
    assert_eq!(result_count(&actor, &tool.invocation.operation_id), 0);
}

#[test]
fn promoted_shell_runs_as_a_background_job_without_blocking_later_tools() {
    let (_dir, mut actor, root, tool, _) = setup();
    started(&mut actor, &tool);
    actor
        .accept_command(&input(&root, "respond", false))
        .unwrap();
    let next = OperationId::new();
    actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,effect_write) VALUES(?1,?2,?3,'edit_file','tool','write','{}','ready',1)", params![next,tool.invocation.run_id,root.thread]).unwrap();
    for state in ["running", "stopping"] {
        actor
            .store
            .db
            .execute("UPDATE jobs SET state=?1", [state])
            .unwrap();
        assert!(!actor
            .execution_blocked(&next, &tool.invocation.workspace, true)
            .unwrap());
        assert!(!actor
            .execution_blocked(&next, &tool.invocation.workspace, false)
            .unwrap());
    }
    assert!(jobs::execute(
        &actor.store.db,
        &root.thread,
        "job_output",
        &json!({"id":jobs::list(&actor.store.db, &root.session).unwrap()[0].id})
    )
    .is_ok());
}

#[tokio::test]
async fn blocked_tool_call_posts_one_waiting_notice_after_a_lasting_wait() {
    let (_dir, mut actor, root, tool, _) = setup();
    let running = OperationId::new();
    actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,effect_write) VALUES(?1,?2,?3,'edit_file','tool','other','{}','started',1)", params![running,tool.invocation.run_id,root.thread]).unwrap();
    let (done, _completed) = mpsc::channel(16);
    let notices = |actor: &Actor| -> Vec<String> {
        actor
            .store
            .db
            .prepare("SELECT text FROM entries WHERE kind='notice'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let dispatch = |actor: &mut Actor| {
        let op = actor
            .next_operation(&tool.invocation.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(op.id, tool.invocation.operation_id);
        actor.dispatch_tool(op, &done).unwrap();
    };
    dispatch(&mut actor);
    assert!(notices(&actor).is_empty(), "short waits stay silent");
    actor.waiting.insert(
        tool.invocation.operation_id.clone(),
        (Instant::now() - WAIT_NOTICE_AFTER, false),
    );
    dispatch(&mut actor);
    dispatch(&mut actor);
    assert_eq!(
        notices(&actor),
        vec!["bash is waiting for a running edit_file call in this workspace to finish."]
    );
    actor
        .store
        .db
        .execute("UPDATE operations SET state='done' WHERE id=?1", [&running])
        .unwrap();
    dispatch(&mut actor);
    assert!(actor.workers.contains_key(&tool.invocation.operation_id));
    assert!(actor.waiting.is_empty());
}

#[tokio::test]
async fn root_stop_before_promotion_cancels_but_after_promotion_only_stop_job_does() {
    for promote in [false, true] {
        let (_dir, mut actor, root, tool, _) = setup();
        let (done, mut completed) = mpsc::channel(16);
        let op = actor
            .next_operation(&tool.invocation.run_id)
            .unwrap()
            .unwrap();
        actor.dispatch_tool(op, &done).unwrap();
        progress(&mut actor).await;
        if promote {
            actor
                .accept_command(&input(&root, "handoff", false))
                .unwrap();
        }
        actor
            .accept_command(&Command::Stop {
                id: CommandId::new(),
                run: tool.invocation.run_id.clone(),
            })
            .unwrap();
        actor.cancel_stopped(false).unwrap();
        assert_eq!(
            *actor.workers[&tool.invocation.operation_id].cancel.borrow(),
            !promote
        );
        if promote {
            let job = jobs::list(&actor.store.db, &root.session)
                .unwrap()
                .remove(0);
            actor
                .accept_command(&Command::StopJob {
                    id: CommandId::new(),
                    job: job.id,
                })
                .unwrap();
            actor.cancel_stopped(false).unwrap();
            assert!(*actor.workers[&tool.invocation.operation_id].cancel.borrow());
        }
        assert_eq!(
            finish_next(&mut actor, &mut completed).await,
            tool.invocation.operation_id
        );
        assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
        if promote {
            assert_eq!(
                jobs::list(&actor.store.db, &root.session).unwrap()[0].state,
                JobState::Cancelled
            );
        } else {
            assert!(jobs::list(&actor.store.db, &root.session)
                .unwrap()
                .is_empty());
        }
    }
}

#[test]
fn completion_order_produces_normal_result_or_handoff_never_both() {
    for promote_first in [false, true] {
        let (_dir, mut actor, root, tool, _) = setup();
        started(&mut actor, &tool);
        let command = input(&root, "new input", false);
        if promote_first {
            actor.accept_command(&command).unwrap();
        }
        actor
            .finish(Finished {
                id: tool.invocation.operation_id.clone(),
                result: Ok(Output::Tool(outcome("completed once", false), None)),
            })
            .unwrap();
        if !promote_first {
            actor.accept_command(&command).unwrap();
        }
        assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
        let output = actor
            .store
            .tool_output(&tool.invocation.operation_id, 0, 1000)
            .unwrap()
            .text;
        assert_eq!(output.contains("job_id"), promote_first);
        assert_eq!(
            jobs::list(&actor.store.db, &root.session).unwrap().len(),
            usize::from(promote_first)
        );
        assert_eq!(
            actor
                .store
                .db
                .query_row("SELECT COUNT(*) FROM jobs WHERE background=0", [], |r| {
                    r.get::<_, usize>(0)
                })
                .unwrap(),
            0
        );
    }
}

#[test]
fn reopen_requires_unknown_resolution_without_duplicate_result_or_replay() {
    for promoted in [false, true] {
        let (dir, mut actor, root, tool, executor) = setup();
        started(&mut actor, &tool);
        if promoted {
            actor
                .accept_command(&input(&root, "promote", false))
                .unwrap();
        }
        let job = JobId(
            actor
                .store
                .db
                .query_row(
                    "SELECT id FROM jobs WHERE operation=?1",
                    [&tool.invocation.operation_id],
                    |r| r.get(0),
                )
                .unwrap(),
        );
        actor
            .store
            .db
            .execute(
                "INSERT INTO job_spool(job,ordinal,text) VALUES(?1,0,'retained before crash')",
                [&job],
            )
            .unwrap();
        actor
            .store
            .db
            .execute("UPDATE jobs SET spool_bytes=21 WHERE id=?1", [&job])
            .unwrap();
        drop(actor);
        let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
        let claim =
            crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone())
                .unwrap();
        store.adopt_claim(claim, true).unwrap();
        let mut actor = Actor::new(store, executor.clone()).unwrap();
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
            "unknown"
        );
        assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
        if promoted {
            assert_eq!(
                jobs::list(&actor.store.db, &root.session).unwrap()[0].state,
                JobState::Unknown
            );
            assert_eq!(
                jobs::page(&actor.store.db, &job, 0, 100).unwrap().text,
                "retained before crash"
            );
        } else {
            assert!(jobs::list(&actor.store.db, &root.session)
                .unwrap()
                .is_empty());
            assert_eq!(
                actor
                    .store
                    .db
                    .query_row(
                        "SELECT COUNT(*) FROM entries WHERE kind='coordination'",
                        [],
                        |r| r.get::<_, usize>(0)
                    )
                    .unwrap(),
                0
            );
        }
        assert!(actor
            .accept_command(&Command::Resume {
                id: CommandId::new(),
                thread: root.thread.clone()
            })
            .is_err());
        actor
            .accept_command(&Command::ResolveUnknown {
                id: CommandId::new(),
                operation: tool.invocation.operation_id.clone(),
                evidence: "inspected effects".into(),
            })
            .unwrap();
        actor
            .accept_command(&Command::Resume {
                id: CommandId::new(),
                thread: root.thread.clone(),
            })
            .unwrap();
        assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
        assert!(actor
            .next_operation(&tool.invocation.run_id)
            .unwrap()
            .is_none());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn promoted_unknown_holds_current_run_but_not_new_context_epoch() {
    for new_epoch in [false, true] {
        let (_dir, mut actor, root, tool, _) = setup();
        started(&mut actor, &tool);
        actor
            .accept_command(&input(&root, "promote", false))
            .unwrap();
        if new_epoch {
            actor
                .store
                .db
                .execute("UPDATE contexts SET epoch=epoch+1", [])
                .unwrap();
        }
        actor
            .finish(Finished {
                id: tool.invocation.operation_id.clone(),
                result: Err(anyhow::anyhow!("executor lost completion")),
            })
            .unwrap();
        assert_eq!(
            jobs::list(&actor.store.db, &root.session).unwrap()[0].state,
            JobState::Unknown
        );
        assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
        let held: bool = actor
            .store
            .db
            .query_row(
                "SELECT held FROM threads WHERE id=?1",
                [&root.thread],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(held, !new_epoch);
    }
}

#[test]
fn stopped_run_preserves_unconsumed_promoted_human_input() {
    let (_dir, mut actor, root, tool, _) = setup();
    started(&mut actor, &tool);
    let command = input(&root, "do not discard", false);
    actor.accept_command(&command).unwrap();
    actor
        .accept_command(&Command::Stop {
            id: CommandId::new(),
            run: tool.invocation.run_id.clone(),
        })
        .unwrap();
    let tx = actor.store.db.transaction().unwrap();
    settle_run(&tx, &tool.invocation.run_id, "cancelled", "stopped").unwrap();
    tx.commit().unwrap();
    let queue = jobs::queue(&actor.store.db, &root.thread).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(&queue[0].id, command.id());
    let owner: Option<String> = actor
        .store
        .db
        .query_row(
            "SELECT owner_run FROM submissions WHERE id=?1",
            [command.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(owner.is_none());
}

#[test]
fn hidden_foreground_job_is_not_publicly_addressable() {
    let (_dir, mut actor, root, tool, _) = setup();
    started(&mut actor, &tool);
    let job = JobId(
        actor
            .store
            .db
            .query_row(
                "SELECT id FROM jobs WHERE operation=?1",
                [&tool.invocation.operation_id],
                |r| r.get(0),
            )
            .unwrap(),
    );
    assert!(jobs::page(&actor.store.db, &job, 0, 100).is_err());
    assert!(jobs::execute(
        &actor.store.db,
        &root.thread,
        "job_status",
        &json!({"id":job})
    )
    .is_err());
    assert!(actor
        .accept_command(&Command::StopJob {
            id: CommandId::new(),
            job
        })
        .is_err());
}

#[test]
fn explicit_background_unknown_does_not_acquire_foreground_hold_semantics() {
    let (_dir, mut actor, root, mut tool, _) = setup();
    tool.invocation.input["run_in_background"] = json!(true);
    actor
        .store
        .db
        .execute(
            "UPDATE operations SET input=?1 WHERE id=?2",
            params![
                encode(&tool.invocation.input).unwrap(),
                tool.invocation.operation_id
            ],
        )
        .unwrap();
    let tx = actor.store.db.transaction().unwrap();
    jobs::start_shell(&tx, &tool).unwrap();
    tx.commit().unwrap();
    actor
        .finish(Finished {
            id: tool.invocation.operation_id.clone(),
            result: Ok(Output::Tool(outcome("explicit interrupted", true), None)),
        })
        .unwrap();
    assert_eq!(
        jobs::list(&actor.store.db, &root.session).unwrap()[0].state,
        JobState::Unknown
    );
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
        "done"
    );
    assert!(!actor
        .store
        .db
        .query_row(
            "SELECT held FROM threads WHERE id=?1",
            [&root.thread],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
    assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
}

#[tokio::test]
async fn cancellation_timeout_retries_unknown_publication_after_database_busy() {
    let (dir, mut actor, _root, tool, _) = setup();
    let operation = &tool.invocation.operation_id;
    actor
        .store
        .db
        .execute(
            "UPDATE operations SET state='started' WHERE id=?1",
            [operation],
        )
        .unwrap();
    let (done, _completed) = mpsc::channel(16);
    actor.spawn(operation.clone(), done, |_| {
        std::future::pending::<anyhow::Result<Output>>()
    });
    actor.workers.get_mut(operation).unwrap().cancelled_at =
        Some(Instant::now() - Duration::from_secs(6));
    let mut blocker = rusqlite::Connection::open(dir.0.join("lab.db")).unwrap();
    let tx = blocker
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let error = actor.cancel_stopped(false).unwrap_err();
    assert!(crate::ownership::busy(&error));
    tokio::task::yield_now().await;
    assert!(!actor.workers[operation].task.is_finished());
    tx.rollback().unwrap();
    actor.cancel_stopped(false).unwrap();
    assert!(!actor.workers.contains_key(operation));
    assert_eq!(
        actor
            .store
            .db
            .query_row(
                "SELECT state FROM operations WHERE id=?1",
                [operation],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "unknown"
    );
}

#[ignore = "request budget removed; turns are no longer stopped by max_requests"]
#[tokio::test]
async fn exhausted_budget_preserves_unseen_human_input_for_explicit_resume_without_shell_replay() {
    let (_dir, mut actor, root, tool, executor) = setup();
    let (done, mut completed) = mpsc::channel(16);
    let op = actor
        .next_operation(&tool.invocation.run_id)
        .unwrap()
        .unwrap();
    actor.dispatch_tool(op, &done).unwrap();
    progress(&mut actor).await;
    actor
        .store
        .db
        .execute(
            "UPDATE runs SET requests=32 WHERE id=?1",
            [&tool.invocation.run_id],
        )
        .unwrap();
    let command = input(&root, "do not lose this unseen instruction", false);
    actor.accept_command(&command).unwrap();
    actor.store.db.execute("INSERT INTO model_context(thread,system,epoch,profile_revision) SELECT id,'fixture',0,profile_revision FROM threads WHERE id=?1", [&root.thread]).unwrap();
    actor
        .request(&tool.invocation.run_id, &root.thread, &done)
        .unwrap();
    assert!(executor.requests.lock().unwrap().is_empty());
    assert_eq!(
        jobs::queue(&actor.store.db, &root.thread).unwrap()[0].id,
        *command.id()
    );
    assert!(actor
        .store
        .db
        .query_row(
            "SELECT model IS NULL FROM entries WHERE submission=?1",
            [command.id()],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
    actor.run_cursor = 0;
    actor.schedule(&done).unwrap();
    actor.cancel_stopped(false).unwrap();
    assert!(!*actor.workers[&tool.invocation.operation_id].cancel.borrow());
    assert_eq!(
        actor
            .store
            .db
            .query_row(
                "SELECT state FROM runs WHERE id=?1",
                [&tool.invocation.run_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "failed"
    );
    assert_eq!(jobs::queue(&actor.store.db, &root.thread).unwrap().len(), 1);
    assert!(actor
        .store
        .db
        .query_row(
            "SELECT owner_run FROM submissions WHERE id=?1",
            [command.id()],
            |r| r.get::<_, Option<String>>(0)
        )
        .unwrap()
        .is_none());
    actor
        .accept_command(&Command::Resume {
            id: CommandId::new(),
            thread: root.thread.clone(),
        })
        .unwrap();
    actor.run_cursor = 0;
    actor.schedule(&done).unwrap();
    assert_ne!(
        finish_next(&mut actor, &mut completed).await,
        tool.invocation.operation_id
    );
    assert!(executor.requests.lock().unwrap()[0]
        .messages
        .iter()
        .any(|message| message.text() == "do not lose this unseen instruction"));
    let consumed_run: String = actor
        .store
        .db
        .query_row(
            "SELECT run FROM submissions WHERE id=?1",
            [command.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(consumed_run, tool.invocation.run_id.0);
    assert_eq!(
        actor
            .store
            .db
            .query_row(
                "SELECT requests FROM runs WHERE id=?1",
                [&tool.invocation.run_id],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        32
    );
    assert!(jobs::queue(&actor.store.db, &root.thread)
        .unwrap()
        .is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    executor.release.add_permits(1);
    progress(&mut actor).await;
    assert_eq!(
        finish_next(&mut actor, &mut completed).await,
        tool.invocation.operation_id
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(result_count(&actor, &tool.invocation.operation_id), 1);
}
