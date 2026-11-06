use super::*;
use bob_kernel::{JobId, JobKind, JobState, JobSummary, ToolProgress};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

const HUMAN: &str = "Answer this new message while the approved shell keeps running.";
const SHELL: &str = r#"printf '%s\n' "$$" >> executions
printf 'prefix\n'
printf 'before-error\n' >&2
printf ready > ready
while [ ! -e release ]; do sleep 0.02; done
printf '%s\n' "$$" >> effects
printf 'suffix\n'
printf 'after-error\n' >&2
"#;

#[derive(Default)]
struct HandoffObserved {
    requests: Vec<RequestSpec>,
    prepared: Vec<PreparedTool>,
    executed: Vec<PreparedTool>,
    outcomes: Vec<ToolOutcome>,
}

struct HandoffExecutor {
    tools: ToolService,
    input: Value,
    observed: Mutex<HandoffObserved>,
    resumed: Semaphore,
}

#[async_trait]
impl Executor for HandoffExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "root".into(),
            context_window: 128_000,
        }]
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
        let mut observed = self.observed.lock().unwrap();
        observed.requests.push(request);
        if observed.requests.len() == 1 {
            Ok(call("bash", self.input.clone()))
        } else {
            self.resumed.add_permits(1);
            Ok(text(
                "The new message was consumed without waiting for the shell.",
            ))
        }
    }

    async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        let prepared = self.tools.prepare(invocation).await?;
        self.observed
            .lock()
            .unwrap()
            .prepared
            .push(prepared.clone());
        Ok(prepared)
    }

    async fn execute(
        &self,
        _tool: PreparedTool,
        _cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        anyhow::bail!("handoff fixture requires the real progress-aware execution path")
    }

    async fn execute_with_progress(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
        progress: ToolProgress,
    ) -> Result<ToolOutcome> {
        self.observed.lock().unwrap().executed.push(tool.clone());
        let outcome = self
            .tools
            .execute_observed(tool, cancel, Some(progress))
            .await?;
        self.observed.lock().unwrap().outcomes.push(outcome.clone());
        Ok(outcome)
    }
}

struct Fixture {
    dir: TestDir,
    handle: Handle,
    root: Receipt,
    executor: Arc<HandoffExecutor>,
}

impl Fixture {
    async fn new(timeout: f64) -> Self {
        crate::native_worker::helper_path()
            .expect("build bob-exec first, or set BOB_EXEC_HELPER to its absolute path");
        let dir = TestDir::new();
        let executor = Arc::new(HandoffExecutor {
            tools: ToolService::default(),
            input: json!({"command": SHELL, "timeout": timeout, "run_in_background": false}),
            observed: Mutex::new(HandoffObserved::default()),
            resumed: Semaphore::new(0),
        });
        let handle = bounded(Runtime::open(&dir.db(), executor.clone()))
            .await
            .unwrap();
        let root = session(&handle, &dir, "root", Policy::default(), false).await;
        Self {
            dir,
            handle,
            root,
            executor,
        }
    }

    async fn check(&self, checks: impl Future<Output = ()>) {
        // Catch assertion panics and timer expiry before shutting down the owned worker runtime.
        let result = AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(30), checks))
            .catch_unwind()
            .await;
        let shutdown = tokio::time::timeout(WAIT, self.handle.shutdown()).await;
        let stopped = tokio::time::timeout(WAIT, async {
            if let Ok(pids) = fs::read_to_string(self.dir.workspace().join("executions")) {
                for pid in pids.lines().filter_map(|line| line.parse::<i32>().ok()) {
                    while process_exists(pid) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        })
        .await;
        shutdown
            .expect("fixture runtime shutdown timed out")
            .unwrap();
        stopped.expect("owned shell remained alive after runtime shutdown");
        match result {
            Ok(result) => result.expect("foreground handoff fixture timed out"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    async fn start(&self) -> (Decision, String) {
        submit(&self.handle, &self.root.thread).await;
        let pending = until(&self.handle, &self.root.thread, |v| v.decisions.len() == 1).await;
        let decision = pending.decisions[0].clone();
        assert_eq!(decision.name, "bash");
        assert!(self.executor.observed.lock().unwrap().executed.is_empty());
        self.dir.assert_empty();
        decide(&self.handle, &decision, true).await;
        bounded(async {
            while !self.dir.workspace().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let pid = fs::read_to_string(self.dir.workspace().join("executions")).unwrap();
        assert_eq!(pid.lines().count(), 1);
        assert!(process_exists(pid.trim().parse().unwrap()));
        let running = view(&self.handle, &self.root.thread).await;
        assert_eq!(
            running
                .operations
                .iter()
                .find(|op| op.id == decision.operation)
                .unwrap()
                .state,
            OperationState::Started,
        );
        assert!(bounded(self.handle.jobs(self.root.session.clone()))
            .await
            .unwrap()
            .is_empty());
        (decision, pid)
    }

    async fn promote(&self, decision: &Decision, steer: bool) -> JobSummary {
        let id = CommandId::new();
        let thread = self.root.thread.clone();
        let text = HUMAN.into();
        let input = if steer {
            Command::Steer { id, thread, text }
        } else {
            Command::Submit { id, thread, text }
        };
        command(&self.handle, input).await;
        bounded(self.executor.resumed.acquire())
            .await
            .unwrap()
            .forget();
        let jobs = bounded(self.handle.jobs(self.root.session.clone()))
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        let job = jobs[0].clone();
        assert_eq!(job.kind, JobKind::Shell);
        assert_eq!(job.state, JobState::Running);
        assert!(job.background);
        assert_eq!(job.operation.as_ref(), Some(&decision.operation));
        assert_eq!(job.thread, self.root.thread);
        let snapshot = view(&self.handle, &self.root.thread).await;
        assert!(
            snapshot.decisions.is_empty(),
            "promotion must not request approval again",
        );
        let operation = snapshot
            .operations
            .iter()
            .find(|op| op.id == decision.operation)
            .unwrap();
        assert_eq!(operation.state, OperationState::Done);
        assert_eq!(job.run.as_ref(), Some(&operation.run));
        self.assert_unchanged(decision).await;
        {
            let observed = self.executor.observed.lock().unwrap();
            assert_eq!(observed.requests.len(), 2);
            let request = &observed.requests[1];
            assert!(request.messages.iter().any(|message| {
                message.role == Role::User
                    && message.content.iter().any(|block| {
                        matches!(block, ContentBlock::Text { text } if text.contains(HUMAN))
                    })
            }));
            let handoffs: Vec<Value> = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { content, .. } => content
                        .as_text()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok())
                        .filter(|value| value["job_id"] == json!(job.id)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                handoffs.len(),
                1,
                "next model request must contain the handoff"
            );
            assert_eq!(handoffs[0]["state"], "running");
            assert!(
                observed.outcomes.is_empty(),
                "the shell must still be running"
            );
        }
        assert!(!self.dir.workspace().join("release").exists());
        assert!(!self.dir.workspace().join("effects").exists());
        bounded(async {
            loop {
                let output = self.output(&job.id).await;
                if output.contains("prefix") && output.contains("before-error") {
                    assert!(!output.contains("suffix"));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        job
    }

    async fn assert_unchanged(&self, decision: &Decision) {
        assert_eq!(
            bounded(self.handle.tool_input(decision.operation.clone()))
                .await
                .unwrap(),
            self.executor.input,
        );
        let observed = self.executor.observed.lock().unwrap();
        assert_eq!(observed.prepared.len(), 1);
        assert_eq!(observed.executed.len(), 1);
        assert_eq!(observed.executed[0].invocation.input, self.executor.input);
        assert_eq!(
            serde_json::to_value(&observed.prepared[0]).unwrap(),
            serde_json::to_value(&observed.executed[0]).unwrap(),
        );
    }

    async fn assert_single_handoff(&self, decision: &Decision, job: &JobSummary) {
        let snapshot = view(&self.handle, &self.root.thread).await;
        let results: Vec<_> = snapshot
            .entries
            .iter()
            .filter(|entry| {
                entry.operation.as_ref() == Some(&decision.operation)
                    && matches!(entry.kind, EntryKind::ToolResult | EntryKind::ToolError)
            })
            .collect();
        assert_eq!(
            results.len(),
            1,
            "completion must not duplicate the original tool result",
        );
        assert_eq!(results[0].kind, EntryKind::ToolResult);
        let result = artifact_text(&self.handle, results[0].artifact.as_ref().unwrap()).await;
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["job_id"], json!(job.id));
    }

    async fn output(&self, job: &JobId) -> String {
        let page = bounded(self.handle.job_output(job.clone(), 0, 16 * 1024))
            .await
            .unwrap();
        assert!(page.next_offset.is_none());
        page.text
    }

    async fn terminal(&self, job: &JobSummary, expected: JobState) -> JobSummary {
        bounded(async {
            loop {
                let jobs = self.handle.jobs(self.root.session.clone()).await.unwrap();
                assert_eq!(jobs.len(), 1);
                assert_eq!(jobs[0].id, job.id);
                if jobs[0].state == expected {
                    return jobs[0].clone();
                }
                assert!(matches!(
                    jobs[0].state,
                    JobState::Running | JobState::Stopping
                ));
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }
}

fn process_exists(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test]
#[ignore = "requires parent-built bob-exec (or BOB_EXEC_HELPER) and Bash; executes gated local shell fixtures"]
async fn approved_foreground_handoff_submit_and_steer_preserve_process_and_output() {
    for steer in [false, true] {
        let fixture = Fixture::new(20.0).await;
        fixture
            .check(async {
                let (decision, pid) = fixture.start().await;
                let job = fixture.promote(&decision, steer).await;
                assert!(process_exists(pid.trim().parse().unwrap()));
                assert_eq!(
                    fs::read_to_string(fixture.dir.workspace().join("executions")).unwrap(),
                    pid,
                );
                fs::write(fixture.dir.workspace().join("release"), "go").unwrap();
                let finished = fixture.terminal(&job, JobState::Completed).await;
                let output: Value = serde_json::from_str(&fixture.output(&job.id).await).unwrap();
                assert_eq!(output["outcome"], "exited");
                assert_eq!(output["exit_code"], 0);
                assert_eq!(output["stdout"]["text"], "prefix\nsuffix\n");
                assert_eq!(output["stderr"]["text"], "before-error\nafter-error\n");
                assert_eq!(
                    artifact_text(&fixture.handle, finished.result.as_ref().unwrap()).await,
                    fixture.output(&job.id).await,
                );
                assert_eq!(
                    fs::read_to_string(fixture.dir.workspace().join("executions")).unwrap(),
                    pid,
                );
                assert_eq!(
                    fs::read_to_string(fixture.dir.workspace().join("effects")).unwrap(),
                    pid,
                );
                fixture.assert_unchanged(&decision).await;
                fixture.assert_single_handoff(&decision, &job).await;
                let observed = fixture.executor.observed.lock().unwrap();
                assert_eq!(observed.outcomes.len(), 1);
                assert!(!observed.outcomes[0].is_error && !observed.outcomes[0].interrupted);
            })
            .await;
    }
}

async fn interrupted_handoff(fixture: &Fixture, stop: bool) {
    let (decision, pid) = fixture.start().await;
    let job = fixture.promote(&decision, false).await;
    if stop {
        command(
            &fixture.handle,
            Command::StopJob {
                id: CommandId::new(),
                job: job.id.clone(),
            },
        )
        .await;
    }
    fixture.terminal(&job, JobState::Unknown).await;
    let held = until(&fixture.handle, &fixture.root.thread, |v| {
        v.operations.iter().any(|operation| {
            operation.id == decision.operation && operation.state == OperationState::Unknown
        })
    })
    .await;
    let error = bounded(fixture.handle.command(Command::Resume {
        id: CommandId::new(),
        thread: fixture.root.thread.clone(),
    }))
    .await
    .unwrap_err();
    assert!(error.to_string().contains("resolve unknown operations"));
    assert_eq!(
        held.operations
            .iter()
            .find(|op| op.id == decision.operation)
            .unwrap()
            .state,
        OperationState::Unknown,
    );
    assert!(held.decisions.is_empty());
    let output: Value = serde_json::from_str(&fixture.output(&job.id).await).unwrap();
    assert_eq!(
        output["outcome"],
        if stop { "cancelled" } else { "timed_out" },
    );
    assert_eq!(output["stdout"]["text"], "prefix\n");
    assert_eq!(output["stderr"]["text"], "before-error\n");
    assert!(!fixture.dir.workspace().join("release").exists());
    assert!(!fixture.dir.workspace().join("effects").exists());
    assert_eq!(
        fs::read_to_string(fixture.dir.workspace().join("executions")).unwrap(),
        pid,
    );
    fixture.assert_unchanged(&decision).await;
    fixture.assert_single_handoff(&decision, &job).await;
    let observed = fixture.executor.observed.lock().unwrap();
    assert_eq!(observed.outcomes.len(), 1);
    assert!(observed.outcomes[0].is_error && observed.outcomes[0].interrupted);
}

#[tokio::test]
#[ignore = "requires parent-built bob-exec (or BOB_EXEC_HELPER) and Bash; waits for original foreground timeout"]
async fn approved_foreground_handoff_preserves_timeout_and_holds_unknown_outcome() {
    let fixture = Fixture::new(4.0).await;
    fixture.check(interrupted_handoff(&fixture, false)).await;
}

#[tokio::test]
#[ignore = "requires parent-built bob-exec (or BOB_EXEC_HELPER) and Bash; cancels a gated local shell"]
async fn approved_foreground_handoff_stop_job_cancels_original_process() {
    let fixture = Fixture::new(20.0).await;
    fixture.check(interrupted_handoff(&fixture, true)).await;
}
