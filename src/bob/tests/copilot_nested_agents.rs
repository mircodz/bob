use anyhow::{ensure, Context, Result};
use bob::{adapters, native_worker};
use bob_core::{agent::prompt::build_system_prompt, core::shell::CommandShell};
use bob_kernel::{
    ArtifactId, Command, CommandId, EntryKind, Handle, OperationState, PermissionPolicy,
    PermissionRule, Policy, Profile, Receipt, RequestSpec, RuleDecision, Runtime, ThreadId,
    ThreadState, ThreadSummary, View,
};
use bob_types::{ContentBlock, ReasoningEffort};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tokio::time::Instant;

const PAGE: usize = 256 * 1024;
const FILES: [(&str, &str); 2] = [
    ("samples.txt", "primary=17\ncanary=5\n"),
    (
        "policy.txt",
        "Use primary only; exclude canary. Multiply the selected sample by 2.\n",
    ),
];

fn model_from_env(name: &str, default: &str) -> Result<String> {
    let value = match std::env::var(name) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => default.into(),
        Err(error) => return Err(error).with_context(|| format!("invalid {name}")),
    };
    let id = value.trim().strip_prefix("copilot:").context(
        "live acceptance models must use explicit copilot: IDs; no other billing backend is allowed",
    )?;
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "invalid Copilot model ID in {name}"
    );
    Ok(format!("copilot:{id}"))
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn create() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("bob-nested-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root)?;
        let fixture = Self {
            workspace: root.join("workspace"),
            root,
        };
        std::fs::create_dir(&fixture.workspace)?;
        for (name, content) in FILES {
            std::fs::write(fixture.workspace.join(name), content)?;
        }
        Ok(fixture)
    }

    fn unchanged(&self) -> Result<()> {
        for (name, content) in FILES {
            ensure!(
                std::fs::read_to_string(self.workspace.join(name))? == content,
                "fixture file {name} changed"
            );
        }
        ensure!(
            std::fs::read_dir(&self.workspace)?.count() == FILES.len(),
            "unexpected workspace mutation"
        );
        Ok(())
    }
}

fn permissions() -> PermissionPolicy {
    PermissionPolicy {
        default: RuleDecision::Deny,
        rules: [
            "read_file",
            "project_instructions",
            "spawn_agent",
            "list_agents",
            "send_message",
            "wait_agent",
            "wait_message",
            "read_history",
            "todo_write",
        ]
        .into_iter()
        .map(|tool| PermissionRule {
            tool: tool.into(),
            decision: RuleDecision::Allow,
        })
        .collect(),
        ..Default::default()
    }
}

fn prompt(root: &ThreadId, worker: &str, marker: &str) -> String {
    format!(
        "Research the adjusted primary sample in this temporary workspace. This is a bounded live \
         collaboration acceptance exercise, not a benchmark of autonomous task selection. Do not \
         edit files, run commands, access the network, or read outside this workspace.\n\
         Root reference: {root}. Report prefix: {marker}. The final answer must be exactly the \
         report prefix, a colon, and the computed integer on one line.\n\
         Use exactly three read-only collaborators with these responsibilities:\n\
         - First spawn coordinator (general-purpose, omit model to inherit yours). Give it a \
         self-contained task and this root thread reference rather than copying the whole conversation. \
         It must spawn samples-reader (general-purpose, explicit model {worker}) to read samples.txt \
         and calculate the answer using policy guidance. Immediately after launching its child it \
         should wait_agent for that child, responding to guidance requests when the wait yields. \
         It must collect its child's completed report before its own final answer.\n\
         - Spawn policy-peer (reviewer, omit model to inherit yours) to read policy.txt and send \
         the policy to coordinator using send_message. This is a peer, not coordinator's child. \
         It can finish after sending the policy.\n\
         - samples-reader must actually read samples.txt, ask coordinator via send_message which \
         sample and multiplier to use, and obtain its guidance before calculating. While awaiting \
         guidance, it must explicitly read the root transcript via read_history to recover the report \
         prefix; pass only the root reference, not the prefix, in its task. If the reply is not yet \
         in context, samples-reader must call wait_message with from set to coordinator's ID, not \
         finalize with 'I am waiting'. Coordinator must reply \
         to samples-reader via send_message using policy-peer's findings, not guess the policy. \
         Keep replies concise and avoid repeated polling.\n\
         Use list_agents to discover same-session IDs and relationships. After collecting coordinator \
         and policy-peer, independently inspect samples-reader's retained transcript with read_history \
         and retrieve the actual read_file result using read_history's tool_output view (use the \
         transcript's operation_id, following cursors if needed). Do not reread samples.txt yourself. \
         Collect all owned work before returning the final answer. No additional agents."
    )
}

fn named<'a>(view: &'a View, name: &str) -> Result<&'a ThreadSummary> {
    let matching: Vec<_> = view
        .threads
        .iter()
        .filter(|thread| thread.name == name)
        .collect();
    ensure!(matching.len() == 1, "expected exactly one {name} thread");
    Ok(matching[0])
}

async fn inputs(handle: &Handle, view: &View, name: &str) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for operation in view.operations.iter().filter(|op| op.name == name) {
        ensure!(
            operation.state == OperationState::Done,
            "{name} did not succeed"
        );
        values.push(handle.tool_input(operation.id.clone()).await?);
    }
    Ok(values)
}

async fn requests(handle: &Handle, view: &View, model: &str) -> Result<Vec<RequestSpec>> {
    let mut requests = Vec::new();
    for operation in view
        .operations
        .iter()
        .filter(|op| matches!(op.name.as_str(), "model" | "compact"))
    {
        let id = operation.specification["manifest"]
            .as_str()
            .context("missing request manifest")?;
        let request: RequestSpec =
            serde_json::from_str(&handle.artifact(ArtifactId(id.into()), 0, PAGE).await?)?;
        ensure!(
            request.model.id == model && request.model.id.starts_with("copilot:"),
            "unexpected request model/provider: {}",
            request.model.id
        );
        requests.push(request);
    }
    ensure!(!requests.is_empty(), "no retained model requests");
    Ok(requests)
}

fn contains_tool_result(request: &RequestSpec, name: &str, input: &Value, text: &str) -> bool {
    let blocks: Vec<_> = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .collect();
    blocks.iter().any(|block| match block {
        ContentBlock::ToolUse {
            id,
            name: called,
            input: arguments,
        } if called == name && arguments == input => blocks.iter().any(|block| {
            matches!(block,
                ContentBlock::ToolResult { tool_use_id, content, is_error }
                    if tool_use_id == id && *is_error != Some(true) && content.model_text() == text)
        }),
        _ => false,
    })
}

async fn message(
    handle: &Handle,
    source: &View,
    target: &ThreadSummary,
    received: &View,
) -> Result<()> {
    let calls = inputs(handle, source, "send_message").await?;
    let source_id = &source
        .entries
        .first()
        .context("sender transcript missing")?
        .thread;
    let final_entry = received
        .entries
        .iter()
        .rev()
        .find(|entry| entry.kind == EntryKind::Assistant && !entry.is_preview)
        .context("recipient final missing")?;
    ensure!(
        calls.iter().any(|input| {
            (input["to"] == target.id.0 || input["to"] == target.name)
                && input["message"].as_str().is_some_and(|text| {
                    !text.trim().is_empty()
                        && received.entries.iter().any(|entry| {
                            entry.kind == EntryKind::Coordination
                                && entry
                                    .sender
                                    .as_ref()
                                    .is_some_and(|sender| &sender.thread == source_id)
                                && entry.text.contains(text)
                                && entry.sequence < final_entry.sequence
                        })
                })
        }),
        "guidance to {} was not sent and retained before its final answer",
        target.name
    );
    Ok(())
}

fn collected(view: &View, requests: &[RequestSpec], child: &ThreadSummary) -> Result<()> {
    let prefix = format!("Agent {} completed:", child.id);
    let reports: Vec<_> = view
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == EntryKind::Coordination
                && entry
                    .sender
                    .as_ref()
                    .is_some_and(|sender| sender.thread == child.id)
                && entry.text.starts_with(&prefix)
        })
        .collect();
    ensure!(
        reports.len() == 1,
        "expected one completion report from {}",
        child.name
    );
    let final_entry = view
        .entries
        .iter()
        .rev()
        .find(|entry| entry.kind == EntryKind::Assistant && !entry.is_preview)
        .context("parent final answer missing")?;
    ensure!(
        reports[0].sequence < final_entry.sequence,
        "parent answered before collecting {}",
        child.name
    );
    ensure!(
        requests.iter().any(|request| request
            .messages
            .iter()
            .any(|message| message.text().contains(&prefix))),
        "{} report never reached a parent model request",
        child.name
    );
    Ok(())
}

async fn verify(
    handle: &Handle,
    receipt: &Receipt,
    root: &View,
    model: &str,
    worker: &str,
    marker: &str,
    fixture: &Fixture,
) -> Result<()> {
    ensure!(
        root.threads.len() == 4,
        "expected root, coordinator, peer, and grandchild only"
    );
    let coordinator = named(root, "coordinator")?;
    let peer = named(root, "policy-peer")?;
    let reader = named(root, "samples-reader")?;
    ensure!(
        coordinator.parent_thread.as_ref() == Some(&receipt.thread)
            && peer.parent_thread.as_ref() == Some(&receipt.thread)
            && reader.parent_thread.as_ref() == Some(&coordinator.id),
        "wrong ownership graph"
    );
    let root_thread = root
        .threads
        .iter()
        .find(|thread| thread.id == receipt.thread)
        .context("root missing")?;
    let mut views = Vec::new();
    for thread in [root_thread, coordinator, peer, reader] {
        ensure!(
            thread.session == receipt.session && thread.state == ThreadState::Completed,
            "{} not completed in the fixture session",
            thread.name
        );
        ensure!(
            handle.queue(thread.id.clone()).await?.is_empty(),
            "uncollected coordination remains"
        );
        ensure!(
            handle.context(thread.id.clone()).await?.profile.read_only,
            "thread lost read-only policy"
        );
        let view = handle.view(Some(thread.id.clone()), None, 100).await?;
        ensure!(
            view.entries.len() < 100 && view.operations.len() < 100,
            "fixture exceeded inspection page; inspect retained database"
        );
        ensure!(
            view.operations
                .iter()
                .all(|op| op.state == OperationState::Done),
            "unsettled or failed operation"
        );
        let expected = if thread.id == reader.id {
            worker
        } else {
            model
        };
        ensure!(
            thread.model == expected,
            "incorrect inherited/explicit thread model"
        );
        let manifests = requests(handle, &view, expected).await?;
        views.push((view, manifests));
    }
    let (root_view, root_requests) = &views[0];
    let (coordinator_view, coordinator_requests) = &views[1];
    let (peer_view, _) = &views[2];
    let (reader_view, _) = &views[3];
    collected(root_view, root_requests, coordinator)?;
    collected(root_view, root_requests, peer)?;
    collected(coordinator_view, coordinator_requests, reader)?;
    message(handle, peer_view, coordinator, coordinator_view).await?;
    message(handle, reader_view, coordinator, coordinator_view).await?;
    message(handle, coordinator_view, reader, reader_view).await?;
    ensure!(
        inputs(handle, coordinator_view, "wait_agent")
            .await?
            .iter()
            .any(|input| input["thread_id"] == reader.id.0),
        "coordinator never waited for owned child"
    );
    for (view, file) in [(peer_view, "policy.txt"), (reader_view, "samples.txt")] {
        ensure!(
            inputs(handle, view, "read_file")
                .await?
                .iter()
                .any(|input| {
                    input["path"].as_str().is_some_and(|path| {
                        let path = std::path::Path::new(path);
                        path == std::path::Path::new(file) || path == fixture.workspace.join(file)
                    })
                }),
            "assigned reader skipped {file}"
        );
    }
    ensure!(
        inputs(handle, root_view, "read_file").await?.is_empty(),
        "root bypassed delegated research"
    );
    let root_spawns = inputs(handle, root_view, "spawn_agent").await?;
    ensure!(
        root_spawns.len() == 2 && root_spawns.iter().all(|input| input.get("model").is_none()),
        "root collaborators must test default model inheritance"
    );
    ensure!(
        root_spawns
            .iter()
            .any(|input| input["name"] == "policy-peer" && input["subagent_type"] == "reviewer"),
        "peer must exercise reviewer model inheritance"
    );
    let child_spawns = inputs(handle, coordinator_view, "spawn_agent").await?;
    ensure!(child_spawns.len() == 1 && child_spawns[0]["model"] == worker
        && !child_spawns[0]["task"].as_str().context("grandchild task missing")?.contains(marker),
        "grandchild must explicitly select the worker model and retrieve the report prefix from history");
    let mut root_history = false;
    let mut root_prefix = false;
    let mut retained_output = false;
    let mut discovery = false;
    for (view, manifests) in &views {
        for operation in view
            .operations
            .iter()
            .filter(|op| matches!(op.name.as_str(), "read_history" | "list_agents"))
        {
            let input = handle.tool_input(operation.id.clone()).await?;
            let page = handle.tool_output(operation.id.clone(), 0, PAGE).await?;
            ensure!(
                page.next_offset.is_none(),
                "bounded history/discovery result unexpectedly truncated"
            );
            let output: Value = serde_json::from_str(&page.text)?;
            ensure!(manifests.iter().any(|request| contains_tool_result(request, &operation.name, &input, &page.text)),
                "history/discovery result never reached a model request with its matching tool call ID");
            if operation.name == "list_agents" {
                discovery |= output["agents"].is_array()
                    && output.get("next_cursor").is_some()
                    && output["root_thread_id"] == receipt.thread.0
                    && output["self_thread_id"].is_string();
            } else if input["thread_id"] == receipt.thread.0
                && view
                    .entries
                    .first()
                    .is_some_and(|entry| entry.thread == reader.id)
            {
                root_history |= input["view"] == "transcript"
                    && output["thread_id"] == receipt.thread.0
                    && output["initial_head"].is_number()
                    && output.get("next_cursor").is_some();
                root_prefix |=
                    output["thread_id"] == receipt.thread.0 && page.text.contains(marker);
            } else if input["view"] == "tool_output"
                && input["thread_id"] == reader.id.0
                && view
                    .entries
                    .first()
                    .is_some_and(|entry| entry.thread == receipt.thread)
            {
                let read = reader_view
                    .operations
                    .iter()
                    .find(|op| op.name == "read_file" && input["operation_id"] == op.id.0)
                    .context("history did not target child's read_file")?;
                ensure!(
                    read.state == OperationState::Done,
                    "history targeted unfinished file read"
                );
                retained_output |= output["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("primary=17") && text.contains("canary=5"))
                    && output["next_offset"].is_null();
            }
        }
    }
    ensure!(discovery && root_history && root_prefix && retained_output,
        "missing successful discovery, grandchild root-history read, or root retained child-output read");
    let answer = root_view
        .entries
        .iter()
        .rev()
        .find(|entry| entry.kind == EntryKind::Assistant && !entry.is_preview)
        .context("root final missing")?;
    ensure!(
        answer.text.trim() == format!("{marker}:34"),
        "incorrect final marker/result"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live Copilot acceptance; requires authenticated Copilot and parent-built bob-exec"]
async fn copilot_nested_agents_acceptance() -> Result<()> {
    let model = model_from_env("BOB_TEST_MODEL", "copilot:gpt-6-astra")?;
    let worker = model_from_env("BOB_TEST_WORKER_MODEL", "copilot:gpt-5.4-nano")?;
    let seconds = match std::env::var("BOB_TEST_TIMEOUT_SECONDS") {
        Ok(value) => value.parse::<u64>().context("invalid live timeout")?,
        Err(std::env::VarError::NotPresent) => 300,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        (120..=600).contains(&seconds),
        "live timeout must be 120..600 seconds"
    );
    native_worker::helper_path().context("parent must build bob-exec before verification")?;
    let fixture = Fixture::create()?;
    let start = Instant::now();
    let mut active: Option<Handle> = None;
    let result = tokio::time::timeout(Duration::from_secs(seconds), async {
        let executor = adapters::create_executor(&[model.clone(), worker.clone()]).await?;
        ensure!(
            executor
                .models()
                .iter()
                .all(|model| model.id.starts_with("copilot:")),
            "executor advertised a non-Copilot model"
        );
        let (executable, kind) = native_worker::discover_shell(&fixture.workspace, None)?;
        let shell = CommandShell::from_resolved(Ok(executable), kind);
        let instructions = build_system_prompt(None, &fixture.workspace, &shell);
        active = Some(Runtime::open(&fixture.root.join("lab.db"), executor).await?);
        let handle = active.as_ref().context("runtime missing")?;
        let receipt = handle
            .command(Command::CreateSessionConfigured {
                id: CommandId::new(),
                workspace: fixture.workspace.to_string_lossy().into_owned(),
                profile: Profile {
                    model: model.clone(),
                    read_only: true,
                    max_requests: 24,
                    reasoning: ReasoningEffort::Low,
                    instructions,
                },
                policy: Policy::default(),
                permissions: permissions(),
            })
            .await?;
        let marker = format!("nested-{}", uuid::Uuid::new_v4());
        handle
            .command(Command::Submit {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                text: prompt(&receipt.thread, &worker, &marker),
            })
            .await?;
        let mut changes = handle.subscribe();
        let root =
            loop {
                let view = handle.view(Some(receipt.thread.clone()), None, 100).await?;
                ensure!(view.error.is_none(), "kernel error: {:?}", view.error);
                for thread in &view.threads {
                    ensure!(
                        !matches!(
                            thread.state,
                            ThreadState::Failed | ThreadState::Cancelled | ThreadState::Interrupted
                        ),
                        "{} became {:?}",
                        thread.name,
                        thread.state
                    );
                    ensure!(
                        !thread.needs_approval,
                        "unexpected approval; no test grants are issued"
                    );
                }
                if view.threads.iter().any(|thread| {
                    thread.id == receipt.thread && thread.state == ThreadState::Completed
                }) && handle.queue(receipt.thread.clone()).await?.is_empty()
                {
                    break view;
                }
                tokio::select! {
                    changed = changes.changed() => changed.context("runtime watch closed")?,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            };
        verify(handle, &receipt, &root, &model, &worker, &marker, &fixture).await?;
        fixture.unchanged()?;
        handle.usage(receipt.session).await
    })
    .await
    .context("whole live acceptance budget exhausted")
    .and_then(|result| result);
    let shutdown = if let Some(handle) = active.take() {
        tokio::time::timeout(Duration::from_secs(15), handle.shutdown())
            .await
            .context("bounded runtime shutdown timed out")
            .and_then(|result| result)
    } else {
        Ok(())
    };
    let outcome = match (result, shutdown) {
        (Ok(usage), Ok(())) => fixture.unchanged().map(|()| usage),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(shutdown)) => {
            Err(error.context(format!("shutdown also failed: {shutdown:#}")))
        }
    };
    match outcome {
        Ok(usage) => {
            std::fs::remove_dir_all(&fixture.root)
                .with_context(|| format!("fixture cleanup failed at {}", fixture.root.display()))?;
            eprintln!("Copilot nested acceptance root={model}, explicit worker={worker}; usage={usage:?}; elapsed={:?}", start.elapsed());
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::write(
                fixture.root.join("failure.txt"),
                format!(
                    "root={model}\nworker={worker}\nelapsed={:?}\n{error:#}\n",
                    start.elapsed()
                ),
            );
            anyhow::bail!(
                "Copilot nested acceptance failed: {error:#}; private artifacts retained at {}",
                fixture.root.display()
            )
        }
    }
}
