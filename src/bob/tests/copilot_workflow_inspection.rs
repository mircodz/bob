use anyhow::{ensure, Context, Result};
use bob::{adapters, native_worker};
use bob_kernel::{
    ArtifactId, Command, CommandId, EntryKind, Handle, JobKind, JobState, OperationId,
    OperationState, PermissionPolicy, PermissionRule, Policy, Profile, Receipt, RequestSpec,
    RuleDecision, Runtime, ThreadState, View, WorkflowNode, WorkflowPageRequest, WorkflowQuery,
    WorkflowQueryResult, WorkflowState, WorkflowSummary, WorkflowTextSource,
};
use bob_types::{ContentBlock, ReasoningEffort};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    time::Duration,
};
use tokio::time::Instant;

const MANIFEST_BYTES: usize = 256 * 1024;
const TEXT_BYTES: usize = 1024;
const FORBIDDEN: &str = "workflow-unselected-must-never-launch";

fn copilot_model(value: &str) -> Result<String> {
    let id = value.trim().strip_prefix("copilot:").context(
        "set BOB_TEST_MODEL=copilot:gpt-6-astra (or another advertised Copilot ID); no other billing backend is allowed",
    )?;
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "invalid explicit Copilot model ID"
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
        let root = std::env::temp_dir().join(format!("bob-workflow-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root)?;
        let fixture = Self {
            workspace: root.join("workspace"),
            database: root.join("lab.db"),
            root,
        };
        std::fs::create_dir(&fixture.workspace)?;
        std::fs::write(fixture.workspace.join("sample.txt"), "17\n")?;
        Ok(fixture)
    }

    fn unchanged(&self) -> Result<()> {
        ensure!(
            std::fs::read_to_string(self.workspace.join("sample.txt"))? == "17\n"
                && std::fs::read_dir(&self.workspace)?.count() == 1,
            "read-only fixture was mutated"
        );
        Ok(())
    }

    fn database(&self) -> Result<Connection> {
        Ok(Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?)
    }
}

fn permissions() -> PermissionPolicy {
    PermissionPolicy {
        default: RuleDecision::Deny,
        rules: ["read_file", "project_instructions", "workflow"]
            .into_iter()
            .map(|tool| PermissionRule {
                tool: tool.into(),
                decision: RuleDecision::Allow,
            })
            .collect(),
        ..Default::default()
    }
}

fn workflow_input() -> Value {
    json!({
        "title": "inspectable-bounded-workflow",
        "read_only": true,
        "max_concurrency": 2,
        "steps": [
            {"id": "read", "agent": {
                "prompt": "Use read_file on sample.txt. Return only JSON containing the integer actually read as value, needs_work true, and skip false. Do not use any other tool.",
                "schema": {"type": "object", "properties": {
                    "value": {"type": "integer"}, "needs_work": {"type": "boolean"},
                    "skip": {"type": "boolean"}
                }, "required": ["value", "needs_work", "skip"], "additionalProperties": false}
            }},
            {"id": "repair", "conditional": {
                "if": "$read.needs_work",
                "then": [{"id": "multiply", "fan_out": {
                    "over": [{"factor": 2}, {"factor": 3}, {"factor": 4}, {"factor": 5}],
                    "prompt": "Read-only arithmetic repair: source={$read.value}; factor={item.factor}. Multiply source by factor. Do not call tools. Return only JSON with integer source, factor, and product properties.",
                    "schema": {"type": "object", "properties": {
                        "source": {"type": "integer"}, "factor": {"type": "integer"},
                        "product": {"type": "integer"}
                    }, "required": ["source", "factor", "product"], "additionalProperties": false}
                }}]
            }},
            {"id": "skipped", "conditional": {
                "if": "$read.skip",
                "then": [{"id": "forbidden", "agent": {
                    "prompt": format!("Return only the marker {FORBIDDEN}. This branch must never be selected.")
                }}]
            }}
        ]
    })
}

fn expected_output() -> Value {
    json!({
        "read": {"value": 17, "needs_work": true, "skip": false},
        "repair": {"branch": "then", "output": {"multiply": [
            {"source": 17, "factor": 2, "product": 34},
            {"source": 17, "factor": 3, "product": 51},
            {"source": 17, "factor": 4, "product": 68},
            {"source": 17, "factor": 5, "product": 85}
        ]}},
        "skipped": {"branch": "else", "output": {}}
    })
}

fn page(cursor: Option<String>) -> WorkflowPageRequest {
    WorkflowPageRequest {
        cursor,
        limit: 1,
        max_bytes: 4096,
    }
}

async fn summaries(handle: &Handle, receipt: &Receipt) -> Result<Vec<WorkflowSummary>> {
    let mut rows = Vec::new();
    let mut cursor = None;
    let mut seen = HashSet::new();
    loop {
        let WorkflowQueryResult::List(result) = handle
            .workflow_query(
                receipt.session.clone(),
                WorkflowQuery::List { page: page(cursor) },
            )
            .await?
        else {
            anyhow::bail!("wrong workflow list response")
        };
        ensure!(result.rows.len() <= 1, "list row bound ignored");
        rows.extend(result.rows);
        cursor = result.next_cursor;
        let Some(next) = &cursor else { break };
        ensure!(seen.insert(next.clone()), "workflow list cursor repeated");
    }
    Ok(rows)
}

async fn text(
    handle: &Handle,
    receipt: &Receipt,
    operation: &OperationId,
    source: WorkflowTextSource,
) -> Result<String> {
    let mut output = String::new();
    let mut offset = 0;
    let mut total = None;
    loop {
        let WorkflowQueryResult::Text(result) = handle
            .workflow_query(
                receipt.session.clone(),
                WorkflowQuery::Text {
                    operation: operation.clone(),
                    source: source.clone(),
                    offset,
                    max_bytes: TEXT_BYTES,
                },
            )
            .await?
        else {
            anyhow::bail!("wrong workflow text response")
        };
        ensure!(
            result.session == receipt.session
                && result.operation == *operation
                && result.source == source
                && result.offset == offset
                && serde_json::to_vec(&result)?.len() <= TEXT_BYTES,
            "workflow text scope or page bound mismatch"
        );
        ensure!(
            total.is_none_or(|value| value == result.total_bytes),
            "artifact changed while paging"
        );
        total = Some(result.total_bytes);
        output.push_str(&result.text);
        match result.next_offset {
            Some(next) => {
                ensure!(
                    next == offset + result.text.len() && next > offset,
                    "text cursor did not advance"
                );
                offset = next;
            }
            None => {
                ensure!(
                    output.len() == result.total_bytes,
                    "paged artifact lost its tail"
                );
                break;
            }
        }
    }
    Ok(output)
}

#[derive(Default)]
struct Inspection {
    active: bool,
    nodes: BTreeMap<String, WorkflowNode>,
    specifications: BTreeMap<String, String>,
    artifacts: BTreeMap<String, String>,
    agents: HashSet<String>,
}

async fn inspect_graph(
    handle: &Handle,
    receipt: &Receipt,
    operation: &OperationId,
    completed: bool,
) -> Result<Inspection> {
    let mut inspection = Inspection::default();
    let mut containers = vec![None];
    let mut visited = HashSet::new();
    while let Some(container) = containers.pop() {
        ensure!(
            visited.insert(container.clone()),
            "container appeared twice"
        );
        let mut cursor = None;
        let mut seen = HashSet::new();
        loop {
            let WorkflowQueryResult::Overview(overview) = handle
                .workflow_query(
                    receipt.session.clone(),
                    WorkflowQuery::Overview {
                        operation: operation.clone(),
                        container: container.clone(),
                        page: page(cursor),
                    },
                )
                .await?
            else {
                anyhow::bail!("wrong workflow overview response")
            };
            ensure!(
                overview.workflow.operation == *operation
                    && overview.container == container
                    && overview.definition_available
                    && overview.definition_error.is_none()
                    && overview.nodes.rows.len() <= 1
                    && serde_json::to_vec(&overview)?.len() <= 4096,
                "invalid bounded workflow overview"
            );
            inspection.active |= overview.workflow.state == WorkflowState::Running
                && overview.workflow.active_agents > 0;
            for node in overview.nodes.rows {
                let WorkflowQueryResult::Node(details) = handle
                    .workflow_query(
                        receipt.session.clone(),
                        WorkflowQuery::Node {
                            operation: operation.clone(),
                            path: node.path.clone(),
                        },
                    )
                    .await?
                else {
                    anyhow::bail!("wrong workflow node response")
                };
                ensure!(
                    details.operation == *operation && details.node.path == node.path,
                    "node scope mismatch"
                );
                if completed {
                    let specification = text(
                        handle,
                        receipt,
                        operation,
                        WorkflowTextSource::NodeSpecification {
                            path: node.path.clone(),
                        },
                    )
                    .await?;
                    ensure!(!specification.is_empty(), "node specification missing");
                    inspection
                        .specifications
                        .insert(node.path.clone(), specification);
                }
                for link in details.artifacts {
                    let contents = text(
                        handle,
                        receipt,
                        operation,
                        WorkflowTextSource::Artifact {
                            artifact: link.artifact.clone(),
                        },
                    )
                    .await?;
                    ensure!(!contents.is_empty(), "linked node artifact missing");
                    inspection.artifacts.insert(link.artifact.0, contents);
                }
                if node.expandable {
                    containers.push(Some(node.path.clone()));
                }
                ensure!(
                    inspection.nodes.insert(node.path.clone(), node).is_none(),
                    "duplicate node across pages"
                );
            }
            cursor = overview.nodes.next_cursor;
            let Some(next) = &cursor else { break };
            ensure!(seen.insert(next.clone()), "overview cursor repeated");
        }
    }
    let mut cursor = None;
    let mut seen = HashSet::new();
    loop {
        let WorkflowQueryResult::Agents(agents) = handle
            .workflow_query(
                receipt.session.clone(),
                WorkflowQuery::Agents {
                    operation: operation.clone(),
                    page: page(cursor),
                },
            )
            .await?
        else {
            anyhow::bail!("wrong workflow agents response")
        };
        ensure!(agents.rows.len() <= 1, "agent row bound ignored");
        for agent in agents.rows {
            ensure!(
                inspection.agents.insert(agent.thread.0),
                "duplicate agent across pages"
            );
            if let Some(artifact) = agent.result {
                let contents = text(
                    handle,
                    receipt,
                    operation,
                    WorkflowTextSource::Artifact {
                        artifact: artifact.clone(),
                    },
                )
                .await?;
                inspection.artifacts.insert(artifact.0, contents);
            }
        }
        cursor = agents.next_cursor;
        let Some(next) = &cursor else { break };
        ensure!(seen.insert(next.clone()), "agent cursor repeated");
    }
    Ok(inspection)
}

fn occupancy(database: &Connection, operation: &OperationId) -> Result<usize> {
    Ok(database.query_row(
        "SELECT COUNT(*) FROM agents a WHERE a.operation=?1 AND (a.state IN ('queued','running','stopping') OR EXISTS(SELECT 1 FROM runs r WHERE r.thread=a.thread AND r.state='stopping'))",
        [&operation.0], |row| row.get(0),
    )?)
}

fn durable_counts(database: &Connection) -> Result<(u64, u64, u64, u64)> {
    Ok(database.query_row(
        "SELECT (SELECT COUNT(*) FROM operations), (SELECT COUNT(*) FROM agents), (SELECT COUNT(*) FROM workflow_nodes), (SELECT COUNT(*) FROM artifacts)",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?)
}

async fn request(handle: &Handle, operation: &bob_kernel::OperationSummary) -> Result<RequestSpec> {
    let artifact = operation.specification["manifest"]
        .as_str()
        .context("provider manifest missing")?;
    Ok(serde_json::from_str(
        &handle
            .artifact(ArtifactId(artifact.into()), 0, MANIFEST_BYTES)
            .await?,
    )?)
}

async fn healthy(handle: &Handle, view: &View) -> Result<()> {
    ensure!(
        view.error.is_none(),
        "kernel error; inspect private database"
    );
    for thread in &view.threads {
        ensure!(
            !matches!(
                thread.state,
                ThreadState::Failed | ThreadState::Cancelled | ThreadState::Interrupted
            ),
            "thread failed; inspect private database"
        );
        let child = handle.view(Some(thread.id.clone()), None, 200).await?;
        ensure!(child.decisions.is_empty(), "unexpected permission prompt");
        let controls = handle.controls(thread.id.clone()).await?;
        ensure!(
            controls.questions.is_empty() && controls.plans.is_empty(),
            "unexpected human control"
        );
        for operation in &child.operations {
            ensure!(
                matches!(
                    operation.name.as_str(),
                    "context" | "model" | "read_file" | "project_instructions" | "workflow"
                ),
                "unexpected operation requested; inspect private database"
            );
            ensure!(
                !matches!(operation.state, OperationState::Failed),
                "operation failed"
            );
        }
    }
    Ok(())
}

async fn check_completed(
    handle: &Handle,
    receipt: &Receipt,
    model: &str,
    marker: &str,
) -> Result<Value> {
    let view = handle.view(Some(receipt.thread.clone()), None, 200).await?;
    healthy(handle, &view).await?;
    let operations: Vec<_> = view
        .operations
        .iter()
        .filter(|op| op.name == "workflow")
        .collect();
    ensure!(
        operations.len() == 1 && operations[0].state == OperationState::Done,
        "expected one completed workflow call"
    );
    let operation = operations[0];
    ensure!(
        handle.tool_input(operation.id.clone()).await? == workflow_input(),
        "model changed predefined workflow arguments"
    );
    let workflows = summaries(handle, receipt).await?;
    ensure!(
        workflows.len() == 1,
        "expected exactly one inspectable workflow"
    );
    let summary = &workflows[0];
    ensure!(
        summary.operation == operation.id
            && summary.owner == receipt.thread
            && summary.state == WorkflowState::Completed
            && summary.admitted
            && summary.max_concurrency == 2
            && summary.active_agents == 0
            && summary.executions == 5,
        "completed workflow summary is incorrect"
    );
    let definition = text(
        handle,
        receipt,
        &operation.id,
        WorkflowTextSource::Definition,
    )
    .await?;
    ensure!(
        serde_json::from_str::<Value>(&definition)? == workflow_input(),
        "original definition was not retained"
    );
    let aggregate_id = summary
        .result
        .clone()
        .context("workflow aggregate missing")?;
    let aggregate = text(
        handle,
        receipt,
        &operation.id,
        WorkflowTextSource::Artifact {
            artifact: aggregate_id.clone(),
        },
    )
    .await?;
    let report: Value = serde_json::from_str(&aggregate)?;
    ensure!(
        report["status"] == "success" && report["output"] == expected_output(),
        "full typed workflow output mismatch"
    );
    let tool_result = handle
        .artifact(
            operation.result.clone().context("tool aggregate missing")?,
            0,
            MANIFEST_BYTES,
        )
        .await?;
    ensure!(
        serde_json::from_str::<Value>(&tool_result)? == report,
        "root received a different or incomplete aggregate"
    );
    let inspection = inspect_graph(handle, receipt, &operation.id, true).await?;
    let node = |path: &str| {
        inspection
            .nodes
            .get(path)
            .with_context(|| format!("missing node {path}"))
    };
    ensure!(
        node("root/repair")?.selected_branch.as_deref() == Some("then")
            && node("root/skipped")?.selected_branch.as_deref() == Some("else"),
        "conditional decision was not retained"
    );
    ensure!(
        node("root/repair/else")?.state == WorkflowState::NotSelected
            && node("root/skipped/then")?.state == WorkflowState::NotSelected
            && node("root/skipped/then/forbidden")?.state == WorkflowState::NotSelected,
        "unselected branches were not distinguished from failed/future work"
    );
    ensure!(
        node("root/skipped/else")?.state == WorkflowState::Completed
            && node("root/skipped/else")?.child.is_none(),
        "omitted else was not a completed no-op"
    );
    for (path, node) in &inspection.nodes {
        if path.starts_with("root/skipped/then") || path.starts_with("root/repair/else") {
            ensure!(
                node.child.is_none() && !node.instantiated && node.result.is_none(),
                "unselected branch instantiated work"
            );
        }
    }
    let mut expected_nodes = vec!["root/read".to_owned()];
    expected_nodes.extend((0..4).map(|index| format!("root/repair/then/multiply/{index}/0")));
    let mut expected_children = HashSet::new();
    for (index, path) in expected_nodes.iter().enumerate() {
        let leaf = node(path)?;
        ensure!(
            leaf.state == WorkflowState::Completed && leaf.instantiated,
            "selected leaf did not complete"
        );
        ensure!(
            expected_children.insert(
                leaf.child
                    .as_ref()
                    .context("selected child missing")?
                    .0
                    .clone()
            ),
            "duplicate selected child"
        );
        let result = leaf
            .result
            .as_ref()
            .context("selected leaf result missing")?;
        let retained: Value = serde_json::from_str(
            inspection
                .artifacts
                .get(&result.0)
                .context("selected leaf result was not linked for inspection")?,
        )?;
        let expected = if index == 0 {
            expected_output()["read"].clone()
        } else {
            expected_output()["repair"]["output"]["multiply"][index - 1].clone()
        };
        ensure!(
            retained["output"] == expected && retained["error"].is_null(),
            "inspected leaf artifact lost its full typed result"
        );
    }
    let actual_children: HashSet<_> = view
        .threads
        .iter()
        .filter(|thread| thread.parent_thread.is_some())
        .map(|thread| thread.id.0.clone())
        .collect();
    ensure!(
        expected_children == actual_children && inspection.agents == expected_children,
        "workflow child membership mismatch"
    );
    let reports = report["agents"]
        .as_array()
        .context("aggregate child reports missing")?;
    ensure!(reports.len() == 5, "aggregate child report count mismatch");
    let report_children: HashSet<_> = reports
        .iter()
        .map(|agent| agent["thread_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    ensure!(
        report_children == expected_children
            && reports.iter().all(|agent| agent["status"] == "completed"
                && agent["output_omitted"] == false
                && agent["output"]
                    .as_str()
                    .is_some_and(|output| !output.is_empty() && !output.contains(FORBIDDEN))),
        "full selected child reports were not retained"
    );
    let mut consumed_aggregate = false;
    let mut model_requests = 0;
    for thread in &view.threads {
        ensure!(
            thread.state == ThreadState::Completed && thread.model == model,
            "thread did not complete with selected Copilot model"
        );
        if thread.id == receipt.thread {
            ensure!(
                view.operations.iter().all(|op| matches!(
                    op.name.as_str(),
                    "context" | "model" | "workflow" | "project_instructions"
                )),
                "root called an unrequested tool instead of collecting its workflow"
            );
        } else {
            ensure!(
                thread.parent_thread.as_ref() == Some(&receipt.thread)
                    && handle.context(thread.id.clone()).await?.profile.read_only,
                "child ownership/read-only mismatch"
            );
        }
        let child = handle.view(Some(thread.id.clone()), None, 200).await?;
        let mut requests = 0;
        for operation in child.operations.iter().filter(|op| op.name == "model") {
            let manifest = request(handle, operation).await?;
            ensure!(
                manifest.model.id == model && manifest.model.id.starts_with("copilot:"),
                "request used a different billing provider/model"
            );
            requests += 1;
            if thread.id == receipt.thread {
                let blocks: Vec<_> = manifest
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .collect();
                let calls: Vec<_> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, name, input }
                            if name == "workflow" && *input == workflow_input() =>
                        {
                            Some(id)
                        }
                        _ => None,
                    })
                    .collect();
                let results = blocks
                    .iter()
                    .filter(|block| match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            calls.contains(&tool_use_id)
                                && *is_error != Some(true)
                                && content.model_text() == tool_result
                        }
                        _ => false,
                    })
                    .count();
                if results > 0 {
                    ensure!(
                        calls.len() == 1 && results == 1,
                        "root consumed duplicate aggregates"
                    );
                    consumed_aggregate = true;
                }
            }
        }
        ensure!(requests > 0, "thread has no real provider requests");
        model_requests += requests;
        if thread.id.0
            == node("root/read")?
                .child
                .as_ref()
                .context("read child missing")?
                .0
        {
            let mut reads = 0;
            for read in child.operations.iter().filter(|op| op.name == "read_file") {
                ensure!(
                    read.state == OperationState::Done
                        && handle.tool_input(read.id.clone()).await?["path"] == "sample.txt",
                    "reader did not read actual fixture"
                );
                reads += 1;
            }
            ensure!(
                reads > 0,
                "reader fabricated the fixture value without a read"
            );
        } else if thread.id != receipt.thread {
            ensure!(
                child.operations.iter().all(|op| matches!(
                    op.name.as_str(),
                    "model" | "context" | "project_instructions"
                )),
                "arithmetic worker called an unrequested tool"
            );
            ensure!(
                child
                    .entries
                    .iter()
                    .any(|entry| entry.kind == EntryKind::Coordination
                        && entry.text.contains("source=17")
                        && !entry.text.contains("{$read.value}")),
                "typed input reference did not reach worker"
            );
        }
    }
    ensure!(
        consumed_aggregate,
        "root finished without consuming the single full workflow aggregate"
    );
    let final_answer = view
        .entries
        .iter()
        .rev()
        .find(|entry| entry.kind == EntryKind::Assistant && !entry.is_preview)
        .context("final answer missing")?;
    ensure!(
        final_answer.text.trim() == format!("{marker}:34,51,68,85"),
        "root final answer does not reflect collected workflow output"
    );
    let jobs = handle.jobs(receipt.session.clone()).await?;
    ensure!(
        jobs.len() == 1
            && jobs[0].kind == JobKind::Workflow
            && jobs[0].state == JobState::Completed
            && summary.job.as_ref() == Some(&jobs[0].id),
        "unexpected jobs or incomplete workflow job"
    );
    let mut agents: Vec<_> = inspection.agents.into_iter().collect();
    agents.sort();
    Ok(json!({
        "summary": summary, "definition": definition, "aggregate": aggregate,
        "nodes": inspection.nodes, "specifications": inspection.specifications,
        "artifacts": inspection.artifacts, "agents": agents,
        "model_requests": model_requests,
        "operations": view.operations.iter().map(|op| &op.id).collect::<Vec<_>>()
    }))
}

#[tokio::test]
#[ignore = "live Copilot only; set BOB_TEST_MODEL=copilot:gpt-6-astra and prebuild bob-exec"]
async fn copilot_workflow_inspection_survives_reopen() -> Result<()> {
    let model = copilot_model(
        &std::env::var("BOB_TEST_MODEL").context("explicit BOB_TEST_MODEL is required")?,
    )?;
    native_worker::helper_path().context("parent must build bob-exec before this test")?;
    let fixture = Fixture::create()?;
    let started = Instant::now();
    let mut active: Option<Handle> = None;
    let mut phase = "provider setup";
    let outcome = tokio::time::timeout(Duration::from_secs(480), async {
        let executor = adapters::create_executor(std::slice::from_ref(&model)).await?;
        ensure!(!executor.models().is_empty() && executor.models().iter().all(|entry| entry.id.starts_with("copilot:")),
            "executor advertised a non-Copilot provider");
        phase = "open runtime";
        active = Some(Runtime::open(&fixture.database, executor.clone()).await?);
        let handle = active.as_ref().context("runtime missing")?;
        let receipt = handle.command(Command::CreateSessionConfigured {
            id: CommandId::new(), workspace: fixture.workspace.to_string_lossy().into_owned(),
            profile: Profile {
                model: model.clone(), read_only: true, max_requests: 16,
                reasoning: ReasoningEffort::Low,
                instructions: "Follow this bounded acceptance fixture exactly. Preserve supplied workflow JSON unchanged. Use no unrequested tools and do not ask questions. Children must follow their own assigned step, not repeat the root workflow.".into(),
            }, policy: Policy::default(), permissions: permissions(),
        }).await?;
        let marker = format!("workflow-collected-{}", uuid::Uuid::new_v4());
        handle.command(Command::Submit {
            id: CommandId::new(), thread: receipt.thread.clone(),
            text: format!(
                "Call workflow exactly once with the following exact JSON, without adding, removing, or changing any argument: {}\n\
                 Do not call other tools or create any additional agents. Wait for this foreground workflow's single aggregate result. \
                 Only after collecting that successful result, finish with exactly {marker}: followed by the four computed products in order, comma-separated without spaces. \
                 Do not claim completion early; do not retry or replace the workflow. The skipped branch must never launch.",
                workflow_input(),
            ),
        }).await?;
        let database = fixture.database()?;
        let mut changes = handle.subscribe();
        let mut maximum = 0;
        let mut saw_active = false;
        phase = "live workflow and occupancy inspection";
        loop {
            let view = handle.view(Some(receipt.thread.clone()), None, 200).await?;
            healthy(handle, &view).await?;
            let version_before: u64 = database.query_row("PRAGMA data_version", [], |row| row.get(0))?;
            let workflows = summaries(handle, &receipt).await?;
            ensure!(workflows.len() <= 1, "root called additional workflows");
            if let Some(workflow) = workflows.first() {
                let count = occupancy(&database, &workflow.operation)?;
                let version_after: u64 = database.query_row("PRAGMA data_version", [], |row| row.get(0))?;
                if version_before == version_after {
                    ensure!(workflow.active_agents == count, "inspection occupancy disagrees with durable owned-child state");
                }
                maximum = maximum.max(count);
                ensure!(count <= 2 && workflow.active_agents <= 2 && workflow.max_concurrency == 2,
                    "workflow exceeded its durable concurrency budget");
                if !saw_active && workflow.state == WorkflowState::Running && count > 0 {
                    let inspection = inspect_graph(handle, &receipt, &workflow.operation, false).await?;
                    let definition = text(handle, &receipt, &workflow.operation, WorkflowTextSource::Definition).await?;
                    ensure!(serde_json::from_str::<Value>(&definition)? == workflow_input(), "active original input mismatch");
                    ensure!(inspection.active && !inspection.nodes.is_empty() && !inspection.agents.is_empty(),
                        "active workflow not inspectable");
                    saw_active = true;
                }
            }
            if view.threads.iter().any(|thread| thread.id == receipt.thread && thread.state == ThreadState::Completed)
                && handle.queue(receipt.thread.clone()).await?.is_empty() {
                break;
            }
            tokio::select! {
                changed = changes.changed() => changed.context("runtime watch closed")?,
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        ensure!(saw_active && maximum == 2, "live inspection or two-slot concurrent execution was not observed");
        phase = "completed inspection";
        let before = check_completed(handle, &receipt, &model, &marker).await?;
        let context = handle.context(receipt.thread.clone()).await?;
        let usage = handle.usage(receipt.session.clone()).await?;
        let counts = durable_counts(&database)?;
        drop(database);
        fixture.unchanged()?;
        phase = "shutdown before reopen";
        handle.shutdown().await?;
        active = None;
        phase = "reopen without generation";
        active = Some(Runtime::open(&fixture.database, executor).await?);
        let handle = active.as_ref().context("reopened runtime missing")?;
        let after = check_completed(handle, &receipt, &model, &marker).await?;
        ensure!(before == after, "reopen changed inspectable workflow identities or retained outputs");
        let restored = handle.context(receipt.thread.clone()).await?;
        ensure!(restored.revision == context.revision && restored.epoch == context.epoch
            && restored.message_count == context.message_count && restored.checkpoint == context.checkpoint,
            "inspection changed durable context");
        ensure!(handle.usage(receipt.session.clone()).await?.requests == usage.requests
            && durable_counts(&fixture.database()?)? == counts
            && handle.queue(receipt.thread.clone()).await?.is_empty(),
            "reopening/inspection generated requests or replayed work");
        Ok::<_, anyhow::Error>(())
    }).await.context("480-second live acceptance budget exhausted").and_then(|result| result);
    let shutdown = if let Some(handle) = active.take() {
        handle.shutdown().await
    } else {
        Ok(())
    };
    let outcome = match (outcome, shutdown) {
        (Ok(()), Ok(())) => fixture.unchanged(),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(shutdown)) => {
            Err(error.context(format!("shutdown also failed: {shutdown:#}")))
        }
    };
    match outcome {
        Ok(()) => {
            std::fs::remove_dir_all(&fixture.root)
                .with_context(|| format!("fixture cleanup failed at {}", fixture.root.display()))?;
            eprintln!(
                "Copilot workflow inspection passed; model={model}; elapsed={:?}",
                started.elapsed()
            );
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::write(
                fixture.root.join("failure.txt"),
                format!(
                    "phase={phase}\nelapsed={:?}\n{error:#}\n",
                    started.elapsed()
                ),
            );
            anyhow::bail!("Copilot workflow inspection failed during {phase}; private database retained at {}", fixture.database.display())
        }
    }
}

#[test]
fn model_selection_requires_explicit_copilot_provider() -> Result<()> {
    ensure!(
        copilot_model("copilot:gpt-6-astra")? == "copilot:gpt-6-astra",
        "Copilot model was not preserved"
    );
    for value in [
        "",
        "gpt-6-astra",
        "copilot:",
        "copilot/gpt-6-astra",
        "openai:gpt-6-astra",
        "anthropic:claude",
        "copilot:openai:gpt-6-astra",
        "copilot:gpt 6",
        "copilot:../model",
    ] {
        ensure!(
            copilot_model(value).is_err(),
            "nonexplicit/invalid model was accepted"
        );
    }
    Ok(())
}
