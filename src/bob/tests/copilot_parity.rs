use anyhow::{ensure, Context, Result};
use bob::{adapters, native_worker};
use bob_kernel::{
    ArtifactId, Command, CommandId, ContextSnapshot, EntryKind, Handle, JobKind, JobState,
    LiveSnapshot, OperationId, OperationState, PermissionPolicy, PermissionRule, Policy, Profile,
    Receipt, RequestSpec, RuleDecision, Runtime, ThreadState, View,
};
use bob_types::ReasoningEffort;
use serde_json::{json, Value};
use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::watch, time::Instant};

const SHELL: &str = "printf 'background-start\\n'; sleep 2; printf 'background-done\\n'";
const PAGE: usize = 256 * 1024;

fn copilot_model(text: &str) -> Result<String> {
    let text = text.trim();
    let id = text
        .strip_prefix("copilot:")
        .or_else(|| text.strip_prefix("copilot/"))
        .unwrap_or(text);
    ensure!(
        !id.is_empty()
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "use an advertised Copilot model ID, optionally prefixed with copilot: or copilot/; other providers are forbidden"
    );
    Ok(format!("copilot:{id}"))
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    database: PathBuf,
}

impl Fixture {
    fn create() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("bob-parity-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root)?;
        let workspace = root.join("workspace");
        let fixture = Self {
            database: root.join("lab.db"),
            workspace,
            root,
        };
        std::fs::create_dir(&fixture.workspace)?;
        std::fs::write(fixture.workspace.join("sample.txt"), "17\n")?;
        Ok(fixture)
    }

    fn unchanged(&self) -> Result<()> {
        ensure!(
            std::fs::read_to_string(self.workspace.join("sample.txt"))? == "17\n",
            "sample.txt was mutated"
        );
        ensure!(
            std::fs::read_dir(&self.workspace)?.count() == 1,
            "unexpected workspace files were created"
        );
        Ok(())
    }
}

fn permissions() -> PermissionPolicy {
    let mut rules: Vec<_> = [
        "read_file",
        "project_instructions",
        "task",
        "workflow",
        "job_status",
        "job_output",
        "list_agents",
    ]
    .into_iter()
    .map(|tool| PermissionRule {
        tool: tool.into(),
        decision: RuleDecision::Allow,
    })
    .collect();
    rules.extend(
        [
            "bash",
            "write_file",
            "create_file",
            "edit_file",
            "multi_edit",
        ]
        .into_iter()
        .map(|tool| PermissionRule {
            tool: tool.into(),
            decision: RuleDecision::RequireApproval,
        }),
    );
    PermissionPolicy {
        default: RuleDecision::Deny,
        rules,
        ..Default::default()
    }
}

fn task_input(worker: &str) -> Value {
    json!({
        "read_only": true,
        "background": false,
        "tasks": [
            {"description": "parity-reader", "model": worker,
             "prompt": "Use read_file on sample.txt and report the integer actually read. Do not use any other tool."},
            {"description": "parity-sibling",
             "prompt": "Use read_file on sample.txt and report sibling-complete followed by the integer actually read. Do not use any other tool."}
        ]
    })
}

fn workflow_input() -> Value {
    json!({
        "title": "parity-double",
        "read_only": true,
        "steps": [
            {"id": "read", "agent": {
                "prompt": "Use read_file on sample.txt. Return only a JSON object with the integer read in the value property.",
                "schema": {"type": "object", "properties": {"value": {"type": "integer"}},
                           "required": ["value"], "additionalProperties": false}
            }},
            {"id": "double", "agent": {
                "prompt": "The previous step read {$read.value}. Multiply that integer by two. Do not call tools. Return only a JSON object with the result in the doubled property.",
                "schema": {"type": "object", "properties": {"doubled": {"type": "integer"}},
                           "required": ["doubled"], "additionalProperties": false}
            }}
        ]
    })
}

async fn tick(changes: &mut watch::Receiver<Arc<LiveSnapshot>>) -> Result<()> {
    tokio::select! {
        changed = changes.changed() => changed.context("runtime watch closed")?,
        _ = tokio::time::sleep(Duration::from_millis(25)) => {}
    }
    Ok(())
}

fn healthy(view: &View) -> Result<()> {
    ensure!(
        view.error.is_none(),
        "kernel reported an error; inspect retained database"
    );
    ensure!(
        view.threads.iter().all(|t| !matches!(
            t.state,
            ThreadState::Interrupted | ThreadState::Failed | ThreadState::Cancelled
        )),
        "a thread failed, was interrupted or cancelled; inspect retained database"
    );
    Ok(())
}

#[derive(Default)]
struct Observed {
    approved: HashSet<OperationId>,
    running_output: bool,
    completed_output: bool,
}

async fn inspect(
    handle: &Handle,
    receipt: &Receipt,
    fixture: &Fixture,
    observed: &mut Observed,
    allow_shell: bool,
) -> Result<View> {
    let view = handle.view(Some(receipt.thread.clone()), None, 200).await?;
    healthy(&view)?;
    for thread in &view.threads {
        let child;
        let selected = if thread.id == receipt.thread {
            &view
        } else {
            child = handle.view(Some(thread.id.clone()), None, 200).await?;
            &child
        };
        for decision in &selected.decisions {
            let input = handle.tool_input(decision.operation.clone()).await?;
            let allowed = allow_shell
                && decision.thread == receipt.thread
                && decision.name == "bash"
                && input["command"] == SHELL
                && input["run_in_background"] == true
                && input.as_object().is_some_and(|o| o.len() == 2)
                && observed.approved.is_empty();
            handle
                .command(Command::Decide {
                    id: CommandId::new(),
                    operation: decision.operation.clone(),
                    revision: decision.revision,
                    approve: allowed,
                })
                .await?;
            ensure!(
                allowed,
                "unexpected effect was denied; inspect retained database"
            );
            observed.approved.insert(decision.operation.clone());
        }
        for operation in &selected.operations {
            ensure!(
                matches!(
                    operation.name.as_str(),
                    "context"
                        | "model"
                        | "compact"
                        | "read_file"
                        | "project_instructions"
                        | "task"
                        | "workflow"
                        | "bash"
                        | "job_output"
                        | "job_status"
                        | "list_agents"
                ),
                "unexpected operation requested; inspect retained database"
            );
            if operation.name == "bash" {
                let input = handle.tool_input(operation.id.clone()).await?;
                ensure!(
                    input["command"] == SHELL && input["run_in_background"] == true,
                    "unexpected shell invocation"
                );
            }
        }
        let controls = handle.controls(thread.id.clone()).await?;
        ensure!(
            controls.questions.is_empty() && controls.plans.is_empty(),
            "unexpected human question or plan approval"
        );
    }
    for job in handle.jobs(receipt.session.clone()).await? {
        ensure!(
            !matches!(
                job.state,
                JobState::Failed | JobState::Unknown | JobState::Cancelled
            ),
            "a job failed or has an uncertain outcome"
        );
        if job.kind != JobKind::Shell {
            continue;
        }
        ensure!(job.background, "shell was not a real background job");
        let page = handle.job_output(job.id.clone(), 0, 24_000).await?;
        ensure!(
            page.next_offset.is_none(),
            "fixture output was unexpectedly truncated"
        );
        if job.state == JobState::Running && page.text.contains("background-start") {
            let still_running = handle
                .jobs(receipt.session.clone())
                .await?
                .iter()
                .any(|j| j.id == job.id && j.state == JobState::Running);
            if still_running {
                ensure!(
                    !page.text.contains("background-done"),
                    "live output arrived only after completion"
                );
                observed.running_output = true;
            }
        }
        if job.state == JobState::Completed {
            ensure!(
                page.text.contains("background-start") && page.text.contains("background-done"),
                "completed shell output lost a chunk"
            );
            observed.completed_output = true;
        }
    }
    fixture.unchanged()?;
    Ok(view)
}

async fn manifest(
    handle: &Handle,
    operation: &bob_kernel::OperationSummary,
) -> Result<RequestSpec> {
    let id = operation.specification["manifest"]
        .as_str()
        .context("request manifest missing")?;
    serde_json::from_str(&handle.artifact(ArtifactId(id.into()), 0, PAGE).await?)
        .context("decode complete request manifest")
}

async fn check_models(handle: &Handle, view: &View, model: &str) -> Result<usize> {
    let mut requests = 0;
    for operation in view
        .operations
        .iter()
        .filter(|op| op.name == "model" || op.name == "compact")
    {
        let request = manifest(handle, operation).await?;
        ensure!(
            request.model.id == model && request.model.id.starts_with("copilot:"),
            "request used an unexpected model/provider"
        );
        requests += 1;
    }
    ensure!(requests > 0, "no model requests retained");
    Ok(requests)
}

async fn check_work(
    handle: &Handle,
    receipt: &Receipt,
    view: &View,
    model: &str,
    worker: &str,
) -> Result<()> {
    check_models(handle, view, model).await?;
    for (name, expected) in [("task", task_input(worker)), ("workflow", workflow_input())] {
        let operations: Vec<_> = view
            .operations
            .iter()
            .filter(|op| op.name == name)
            .collect();
        ensure!(
            operations.len() == 1 && operations[0].state == OperationState::Done,
            "expected exactly one completed {name}"
        );
        ensure!(
            handle.tool_input(operations[0].id.clone()).await? == expected,
            "{name} did not use the requested schema and arguments"
        );
        let result = operations[0]
            .result
            .clone()
            .context("missing aggregate result")?;
        let report: Value = serde_json::from_str(&handle.artifact(result, 0, PAGE).await?)?;
        ensure!(
            report["status"] == "success",
            "{name} aggregate did not succeed"
        );
        if name == "task" {
            let agents = report["agents"]
                .as_array()
                .context("missing task sibling reports")?;
            ensure!(
                agents.len() == 2
                    && agents.iter().all(|a| a["status"] == "completed"
                        && a["output"]
                            .as_str()
                            .is_some_and(|output| output.contains("17"))
                        && a["output_omitted"] == false),
                "full task sibling outcomes were not retained"
            );
            ensure!(
                agents.iter().any(|a| a["name"] == "parity-sibling"
                    && a["output"]
                        .as_str()
                        .is_some_and(|output| output.contains("sibling-complete"))),
                "sibling completion report missing"
            );
        } else {
            ensure!(
                report["output"]["read"]["value"] == 17
                    && report["output"]["double"]["doubled"] == 34,
                "workflow did not resolve the typed data dependency"
            );
        }
    }
    let jobs = handle.jobs(receipt.session.clone()).await?;
    ensure!(
        jobs.iter().all(|job| job.kind != JobKind::Task),
        "foreground task must settle its operation/report without creating a Task job"
    );
    for kind in [JobKind::Workflow, JobKind::Shell] {
        let matching: Vec<_> = jobs.iter().filter(|job| job.kind == kind).collect();
        ensure!(
            matching.len() == 1 && matching[0].state == JobState::Completed,
            "expected one completed {kind:?} job"
        );
    }
    let children: Vec<_> = view
        .threads
        .iter()
        .filter(|t| t.parent_thread.is_some())
        .collect();
    ensure!(
        children.len() == 4,
        "expected two task siblings and two workflow children"
    );
    for thread in children {
        ensure!(
            thread.parent_thread.as_ref() == Some(&receipt.thread)
                && thread.state == ThreadState::Completed,
            "child was not completed under root"
        );
        let expected = if thread.name == "parity-reader" {
            worker
        } else {
            model
        };
        ensure!(thread.model == expected, "child selected the wrong model");
        ensure!(
            handle.context(thread.id.clone()).await?.profile.read_only,
            "child was not read-only"
        );
        let child = handle.view(Some(thread.id.clone()), None, 200).await?;
        check_models(handle, &child, expected).await?;
        if matches!(
            thread.name.as_str(),
            "parity-reader" | "parity-sibling" | "workflow-1"
        ) {
            let mut reads = 0;
            for operation in child.operations.iter().filter(|op| op.name == "read_file") {
                ensure!(
                    operation.state == OperationState::Done
                        && handle.tool_input(operation.id.clone()).await?["path"] == "sample.txt",
                    "task child did not read sample.txt"
                );
                reads += 1;
            }
            ensure!(reads > 0, "child skipped its real file read");
        }
        if thread.name == "workflow-2" {
            ensure!(
                child
                    .entries
                    .iter()
                    .any(|entry| entry.kind == EntryKind::Coordination
                        && entry.text.contains("previous step read 17")
                        && !entry.text.contains("{$read.value}")),
                "workflow child did not receive the resolved reference"
            );
        }
    }
    ensure!(
        view.threads.iter().any(|t| t.name == "parity-reader")
            && view.threads.iter().any(|t| t.name == "parity-sibling"),
        "task siblings missing"
    );
    Ok(())
}

async fn check_checkpoint(
    handle: &Handle,
    fixture: &Fixture,
    snapshot: &ContextSnapshot,
) -> Result<()> {
    let checkpoint = snapshot
        .checkpoint
        .as_ref()
        .context("checkpoint was not committed")?;
    ensure!(
        !snapshot.compact_pending && snapshot.message_count > 0,
        "partial context checkpoint"
    );
    let db = rusqlite::Connection::open_with_flags(
        &fixture.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (epoch, revision, manifest, partial): (i64, i64, String, usize) = db.query_row(
        "SELECT epoch,revision,manifest,(SELECT COUNT(*) FROM compactions WHERE thread=?2) FROM checkpoints WHERE operation=?1 AND thread=?2",
        rusqlite::params![checkpoint.0, snapshot.thread.0],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    ensure!(
        epoch == snapshot.epoch && revision == snapshot.revision && partial == 0,
        "checkpoint is stale or a partial compaction remains"
    );
    drop(db);
    let ids: Vec<ArtifactId> =
        serde_json::from_str(&handle.artifact(ArtifactId(manifest), 0, PAGE).await?)?;
    ensure!(
        ids.len() == snapshot.message_count,
        "checkpoint manifest is incomplete"
    );
    for id in ids {
        let record: Value = serde_json::from_str(&handle.artifact(id, 0, PAGE).await?)?;
        ensure!(record.is_object(), "checkpoint message artifact missing");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "live Copilot only; requires explicit BOB_TEST_MODEL, BOB_TEST_WORKER_MODEL and a prebuilt bob-exec"]
async fn copilot_task_workflow_background_compact_and_resume() -> Result<()> {
    let model =
        copilot_model(&std::env::var("BOB_TEST_MODEL").context("explicit root model required")?)?;
    let worker = copilot_model(
        &std::env::var("BOB_TEST_WORKER_MODEL").context("explicit worker model required")?,
    )?;
    let seconds = match std::env::var("BOB_TEST_TIMEOUT_SECONDS") {
        Ok(value) => value.parse::<u64>().context("invalid live test timeout")?,
        Err(std::env::VarError::NotPresent) => 300,
        Err(_) => anyhow::bail!("live test timeout is not valid Unicode"),
    };
    ensure!(
        (1..=600).contains(&seconds),
        "live test budget must be 1..600 seconds"
    );
    native_worker::helper_path().context("parent must build the native helper before this test")?;
    let fixture = Fixture::create()?;
    let start = Instant::now();
    let mut active: Option<Handle> = None;
    let mut phase = "provider setup";
    let result = tokio::time::timeout(Duration::from_secs(seconds), async {
        let executor = adapters::create_executor(&[model.clone(), worker.clone()]).await?;
        ensure!(executor.models().iter().all(|m| m.id.starts_with("copilot:")),
            "executor advertised a non-Copilot model");
        phase = "open runtime";
        active = Some(Runtime::open(&fixture.database, executor.clone()).await?);
        let handle = active.as_ref().context("runtime missing")?;
        let receipt = handle.command(Command::CreateSessionConfigured {
            id: CommandId::new(), workspace: fixture.workspace.to_string_lossy().into_owned(),
            profile: Profile {
                model: model.clone(), read_only: false, max_requests: 24,
                reasoning: ReasoningEffort::Low,
                instructions: "Follow the bounded acceptance fixture exactly. Execute supplied tool JSON unchanged. Never explore unrelated files or use unrequested tools. After finishing, remember the computed result and recovery marker for a later question.".into(),
            }, policy: Policy::default(), permissions: permissions(),
        }).await?;
        let final_token = format!("parity-complete-{}", uuid::Uuid::new_v4());
        let resume_token = format!("parity-resume-{}", uuid::Uuid::new_v4());
        let prompt = format!(
            "Execute these steps in order, without changing the supplied tool arguments.\n\
             1. Call task with this exact JSON: {}\n\
             2. After both siblings complete, call workflow with this exact JSON: {}\n\
             3. After the workflow completes, call bash with this exact JSON: {}\n\
             4. Use job_output (and job_status if needed) to collect the shell's completed output; do not run another shell command, create more agents, or mutate any file.\n\
             5. Your final answer must contain {final_token} and the doubled integer computed by the workflow.\n\
             Remember the computed integer and this recovery marker for our later followup: {resume_token}. Preserve these facts through compaction. Do not ask questions.",
            task_input(&worker), workflow_input(),
            json!({"command": SHELL, "run_in_background": true}),
        );
        handle.command(Command::Submit {
            id: CommandId::new(), thread: receipt.thread.clone(), text: prompt,
        }).await?;
        phase = "task/workflow/background";
        let mut observed = Observed::default();
        let mut changes = handle.subscribe();
        let view = loop {
            let view = inspect(handle, &receipt, &fixture, &mut observed, true).await?;
            let root = view.threads.iter().find(|t| t.id == receipt.thread).context("root missing")?;
            if root.state == ThreadState::Completed && handle.queue(receipt.thread.clone()).await?.is_empty() {
                ensure!(observed.approved.len() == 1 && observed.running_output && observed.completed_output,
                    "shell approval, running output or completed output was not observed");
                ensure!(view.entries.iter().any(|e| e.kind == EntryKind::Assistant && !e.is_preview
                    && e.text.contains(&final_token) && e.text.contains("34")), "root final token/result missing");
                break view;
            }
            tick(&mut changes).await?;
        };
        check_work(handle, &receipt, &view, &model, &worker).await?;
        phase = "explicit compaction";
        let before = handle.context(receipt.thread.clone()).await?;
        handle.command(Command::Compact {
            id: CommandId::new(), thread: receipt.thread.clone(), expected_revision: before.revision,
        }).await?;
        let compacted = loop {
            let view = inspect(handle, &receipt, &fixture, &mut observed, false).await?;
            let current = handle.context(receipt.thread.clone()).await?;
            if current.revision > before.revision {
                ensure!(current.revision == before.revision + 1 && current.epoch == before.epoch
                    && current.checkpoint != before.checkpoint && !current.compact_pending,
                    "compaction did not publish one atomic revision");
                if view.threads.iter().any(|t| t.id == receipt.thread && t.state == ThreadState::Completed) {
                    break current;
                }
            } else {
                ensure!(current.checkpoint == before.checkpoint && current.message_count == before.message_count,
                    "compaction published a partial checkpoint");
            }
            tick(&mut changes).await?;
        };
        check_checkpoint(handle, &fixture, &compacted).await?;
        let usage = handle.usage(receipt.session.clone()).await?;
        ensure!(usage.compaction_requests > 0, "compaction did not make a live model request");
        phase = "shutdown before reopen";
        handle.shutdown().await?;
        active = None;
        phase = "reopen and followup";
        active = Some(Runtime::open(&fixture.database, executor).await?);
        let handle = active.as_ref().context("reopened runtime missing")?;
        let restored = handle.context(receipt.thread.clone()).await?;
        ensure!(restored.revision == compacted.revision && restored.epoch == compacted.epoch
            && restored.checkpoint == compacted.checkpoint && restored.message_count == compacted.message_count,
            "reopening lost committed context");
        check_checkpoint(handle, &fixture, &restored).await?;
        let baseline = handle.view(Some(receipt.thread.clone()), None, 200).await?;
        let old_operations: HashSet<_> = baseline.operations.iter().map(|o| o.id.clone()).collect();
        let last_entry = baseline.entries.last().map_or(0, |entry| entry.sequence);
        handle.command(Command::Submit {
            id: CommandId::new(), thread: receipt.thread.clone(),
            text: "Without using any tools, answer with only the previously computed doubled integer and the recovery marker I asked you to remember.".into(),
        }).await?;
        let mut changes = handle.subscribe();
        loop {
            let view = inspect(handle, &receipt, &fixture, &mut observed, false).await?;
            let new_operations: Vec<_> = view.operations.iter().filter(|o| !old_operations.contains(&o.id)).collect();
            ensure!(new_operations.iter().all(|o| matches!(o.name.as_str(), "model" | "context")), "followup used tools instead of restored context");
            if view.threads.iter().any(|t| t.id == receipt.thread && t.state == ThreadState::Completed)
                && handle.queue(receipt.thread.clone()).await?.is_empty() {
                ensure!(new_operations.iter().any(|o| o.name == "model"), "followup never issued a request");
                for operation in new_operations.into_iter().filter(|o| o.name == "model") {
                    let request = manifest(handle, operation).await?;
                    ensure!(request.model.id == model, "followup used wrong provider/model");
                    let context = request.messages.iter().map(|m| m.text()).collect::<Vec<_>>().join("\n");
                    ensure!(context.contains(&resume_token) && context.contains("34"),
                        "followup request omitted compacted recovery facts");
                }
                ensure!(view.entries.iter().any(|e| e.sequence > last_entry
                    && e.kind == EntryKind::Assistant && !e.is_preview
                    && e.text.contains("34") && e.text.contains(&resume_token)),
                    "recovered final answer lost result or resume token");
                break;
            }
            tick(&mut changes).await?;
        }
        check_work(handle, &receipt, &handle.view(Some(receipt.thread.clone()), None, 200).await?, &model, &worker).await?;
        let usage = handle.usage(receipt.session.clone()).await?;
        Ok::<_, anyhow::Error>((usage.requests, usage.compaction_requests))
    }).await.context("whole live acceptance budget exhausted").and_then(|result| result);
    let shutdown = if let Some(handle) = active.take() {
        handle.shutdown().await
    } else {
        Ok(())
    };
    let outcome = match (result, shutdown) {
        (Ok(counts), Ok(())) => Ok(counts),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(shutdown)) => {
            Err(error.context(format!("shutdown also failed: {shutdown:#}")))
        }
    };
    let outcome = outcome.and_then(|counts| fixture.unchanged().map(|()| counts));
    match outcome {
        Ok((requests, compactions)) => {
            std::fs::remove_dir_all(&fixture.root)
                .with_context(|| format!("fixture cleanup failed at {}", fixture.root.display()))?;
            eprintln!("Copilot root={model}, worker={worker}; requests={requests}, compactions={compactions}; elapsed={:?}", start.elapsed());
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::write(
                fixture.root.join("failure.txt"),
                format!("phase={phase}\nelapsed={:?}\n{error:#}\n", start.elapsed()),
            );
            anyhow::bail!(
                "Copilot parity failed during {phase}; private artifacts retained at {}",
                fixture.root.display()
            )
        }
    }
}

#[test]
fn model_selection_is_explicit_and_copilot_only() -> Result<()> {
    for input in ["gpt-5", "copilot:gpt-5", "copilot/gpt-5"] {
        ensure!(
            copilot_model(input)? == "copilot:gpt-5",
            "model normalization failed"
        );
    }
    for input in [
        "",
        "copilot:",
        "copilot/",
        "openai:gpt-5",
        "anthropic:claude",
        "openai/gpt-5",
        "anthropic/claude",
        "gpt 5",
        "copilot:openai:gpt-5",
    ] {
        ensure!(
            copilot_model(input).is_err(),
            "invalid model selection was accepted"
        );
    }
    Ok(())
}
