//! Validate the entire plan before execution; retain failed agents in the report.

use crate::agent::env::AgentEnv;
use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use crate::workflow::params::{self, WorkflowParams};
use crate::workflow::{dsl, input, validation, WorkflowContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static WORKFLOW_RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn reserve_workflow_id(id: &str) {
    if let Some(number) = id
        .strip_prefix("wf-")
        .and_then(|id| id.rsplit_once('-'))
        .and_then(|(_, number)| number.split('.').next())
        .and_then(|number| number.parse::<u64>().ok())
    {
        WORKFLOW_RUN_COUNTER.fetch_max(number, Ordering::Relaxed);
    }
}

pub struct WorkflowTool {
    pub env: AgentEnv,
}

#[async_trait]
impl Tool for WorkflowTool {
    fn spec(&self) -> ToolSpec {
        let examples = json!([
            {
                "title": "review-crates", "read_only": true,
                "shape": "map_reduce", "items": ["src/cli", "src/core"],
                "map_prompt": "Review only {item} for error-handling bugs. Report concrete evidence.",
                "reduce_prompt": "Summarize the verified findings without overstating coverage."
            },
            {
                "title": "fix-error-handling",
                "steps": [
                    {"id": "files", "agent": {
                        "prompt": "Find Rust files with swallowed I/O errors. Return their paths.",
                        "schema": {"type": "object", "required": ["paths"], "properties": {
                            "paths": {"type": "array", "items": {"type": "string"}}
                        }}
                    }},
                    {"id": "fixes", "fan_out": {
                        "over": "$files.paths",
                        "prompt": "Fix swallowed I/O errors in {item}. Edit only that file."
                    }},
                    {"id": "summary", "agent": {"prompt": "Summarize these changes: {$fixes}"}}
                ]
            },
            {
                "title": "repair-tests",
                "steps": [{"id": "repair", "loop": {
                    "max": 3, "until": "$check.pass",
                    "steps": [
                        {"id": "fix", "agent": {"prompt": "Fix failing unit tests in src/cli, editing only src/cli."}},
                        {"id": "check", "agent": {
                            "prompt": "Run cargo test -p bob-cli. Return whether the command passed, not a prediction.",
                            "schema": {"type": "object", "required": ["pass"], "properties": {"pass": {"type": "boolean"}}}
                        }}
                    ]
                }}]
            }
        ]);
        ToolSpec {
            name: "workflow".into(),
            description: format!(
                "Run repeated structured work or an agent pipeline with explicit data dependencies. \
                Prefer direct tools for ordinary coding, explore for investigation, and task for \
                independent deliverables; complexity alone does not require a workflow. \
                Supply exactly one form: shape (fan_out/map_reduce/loop) or steps. \
                The input schema defines every operation and nesting rule.\n\
                Step data stays unwrapped: reference a prior output with $id or $id.field, \
                or interpolate {{$id.field}} in prompts. Fan-out over must resolve to an array; \
                use {{item}} or {{item.field.nested}} in its prompt. Array indexing is not supported. \
                Loop until must reference a boolean. Inner results are available afterward as \
                $loop_id.inner_id.field; parallel branches cannot reference one another. \
                Step names must be unique within their scope and cannot shadow enclosing names. \
                Missing references and incorrect runtime types fail explicitly. Loops default to \
                5 rounds and accept 1..20; fan-out repeat defaults to 1 and accepts 1..20. \
                Each fan-out accepts at most 256 items; plans have at most 256 operations \
                and 16 nested levels, with at most 1024 agent executions per run.\n\
                Workflows retain normal edit and shell tools, subject to session permissions. \
                Set read_only:true only when the entire run should use read-only tools; default false. \
                Children do not inherit your conversation: supply facts, boundaries, and deliverables. \
                Assign non-overlapping write ownership and one owner for workspace-wide verification.\n\
                Results contain status, output, agents, and error. Agent records include input, \
                status (success/failed/cancelled), output, and error; no failed item is dropped. \
                Failed batches skip dependent steps and synthesis. Incomplete runs return a tool \
                error retaining their partial output and agent reports. A failed finder is not an \
                empty finding set. Loop termination distinguishes the condition being met from \
                reaching the limit; reaching the limit does not mean the goal passed.\nExamples:\n{examples}"
            ),
            input_schema: input::schema(),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        input::validate(&input).map_err(ToolError::invalid_input)?;
        enum Plan {
            Steps(dsl::Spec),
            Shape(WorkflowParams),
        }
        let plan = if input.get("steps").is_some() {
            let spec: dsl::Spec = serde_json::from_value(input.clone())
                .map_err(|error| ToolError::invalid_input(format!("invalid steps: {error}")))?;
            validation::validate(&spec).map_err(ToolError::invalid_input)?;
            Plan::Steps(spec)
        } else {
            let params = serde_json::from_value(input.clone())
                .map_err(|error| ToolError::invalid_input(format!("invalid shape: {error}")))?;
            params::validate(&params).map_err(ToolError::invalid_input)?;
            Plan::Shape(params)
        };
        let read_only = input["read_only"].as_bool().unwrap_or(false);
        let (system, tools) = self
            .env
            .resolve_child(None, read_only)
            .map_err(ToolError::invalid_input)?;
        let cwd = if self.env.cwd.is_empty() {
            ctx.cwd.clone()
        } else {
            self.env.cwd.clone()
        };
        let slug = input["title"]
            .as_str()
            .filter(|title| !title.is_empty())
            .unwrap_or("workflow");
        let n = WORKFLOW_RUN_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        let workflow = WorkflowContext::new(
            format!("wf-{slug}-{n}"),
            self.env.provider.clone(),
            self.env.bus.clone(),
            cwd,
            tools,
            system,
            self.env.lsp.clone(),
            self.env.jobs.clone(),
            self.env.parent_cancel.clone(),
            0,
        )
        .with_hooks(self.env.hooks.clone())
        .with_memory(self.env.memory.clone())
        .with_parent(
            ctx.coord
                .as_ref()
                .map_or("root", |coord| coord.name.as_str())
                .to_owned(),
            ctx.coord.as_ref().map_or(1, |coord| coord.depth + 1),
        )
        .with_team(self.env.team.clone());
        let mut run = match plan {
            Plan::Steps(spec) => dsl::run(workflow.clone(), spec).await,
            Plan::Shape(params) => params::run(workflow.clone(), params).await,
        };
        let status = if workflow.is_cancelled() {
            run.error.get_or_insert_with(|| "workflow cancelled".into());
            "cancelled"
        } else if run.error.is_some() {
            "failed"
        } else {
            "completed"
        };
        let report = json!({
            "status": status,
            "output": run.output,
            "agents": workflow.outcomes(),
            "error": run.error,
        });
        let text = serde_json::to_string_pretty(&report).expect("workflow report is JSON");
        if status == "completed" {
            Ok(text.into())
        } else {
            Err(ToolError::failed(format!("workflow incomplete: {text}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::team::AgentRegistry;
    use crate::core::events::EventBus;
    use crate::core::permissions::{Decision, PermissionEngine};
    use crate::core::types::{Completion, GenerateOptions, StreamEvent};
    use crate::providers::mock::{MockProvider, MockReply, MockRule};
    use crate::providers::provider::Provider;
    use crate::tools::registry::{ToolErrorKind, ToolRegistry};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    struct RecordingProvider {
        mock: MockProvider,
        tools: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        fn name(&self) -> &str {
            "workflow-test"
        }
        fn model(&self) -> &str {
            "workflow-test"
        }
        async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
            self.mock.generate(opts).await
        }
        async fn stream(
            &self,
            opts: GenerateOptions,
        ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
            self.tools
                .lock()
                .unwrap()
                .push(opts.tools.iter().map(|tool| tool.name.clone()).collect());
            self.mock.stream(opts).await
        }
    }

    fn fixture(provider: Arc<dyn Provider>) -> (WorkflowTool, ToolContext) {
        let context = crate::test_support::files::tool_context(std::path::Path::new("."));
        let jobs = context.jobs.clone();
        let env = AgentEnv {
            provider,
            subagent_tools: ToolRegistry::new(None),
            bus: EventBus::new(),
            cwd: String::new(),
            subagent_system: None,
            jobs: jobs.clone(),
            team: AgentRegistry::new(),
            lsp: None,
            parent_cancel: Arc::new(AtomicBool::new(false)),
            definitions: Default::default(),
            hooks: Default::default(),
            memory: None,
        };
        (WorkflowTool { env }, context)
    }

    #[test]
    fn restored_workflow_ids_reserve_the_counter() {
        reserve_workflow_id("wf-title-with-dashes-100000");
        reserve_workflow_id("wf-short-2");
        assert!(WORKFLOW_RUN_COUNTER.load(Ordering::Relaxed) >= 100000);
    }

    #[test]
    fn tool_exposes_recursive_schema_and_valid_complete_examples() {
        let (tool, _) = fixture(Arc::new(MockProvider::new(vec![])));
        let spec = tool.spec();
        assert_eq!(
            spec.input_schema["properties"]["steps"]["$ref"],
            "#/$defs/steps"
        );
        assert_eq!(
            spec.input_schema["$defs"]["steps"]["items"]["$ref"],
            "#/$defs/step"
        );
        assert_eq!(
            spec.input_schema["properties"]["read_only"]["default"],
            false
        );
        assert_eq!(
            spec.input_schema["$defs"]["step"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        let examples: Vec<Value> =
            serde_json::from_str(spec.description.split_once("Examples:\n").unwrap().1).unwrap();
        assert_eq!(examples.len(), 3);
        for example in examples {
            input::validate(&example).unwrap();
            if example.get("steps").is_some() {
                validation::validate(&serde_json::from_value(example).unwrap()).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn invalid_plans_never_start_agents_or_emit_progress() {
        let provider = MockProvider::new(vec![]);
        let (tool, context) = fixture(Arc::new(provider.clone()));
        let events = Arc::new(AtomicUsize::new(0));
        let captured = events.clone();
        tool.env.bus.on(Arc::new(move |_| {
            captured.fetch_add(1, Ordering::Relaxed);
        }));
        for input in [
            json!({"shape":"fan_out", "steps":[]}),
            json!({"shape":"fan_out", "items":["x"], "map_prompt":"do", "read_only":"false"}),
            json!({"shape":"loop", "max_rounds":0}),
            json!({"steps":[{"id":"first","agent":{"prompt":"do"}}, {"id":"later","parallel":{"branches":{"bad":{"unknown":{}}}}}]}),
            json!({"steps":[{"id":"first","agent":{"prompt":"{$later}"}}, {"id":"later","agent":{"prompt":"do"}}]}),
            json!({"steps":[{"id":"same","agent":{"prompt":"do"}}, {"id":"same","agent":{"prompt":"do"}}]}),
            json!({"steps":[{"id":"batch","fan_out":{"over":[], "prompt":"{$missing}"}}]}),
        ] {
            let error = tool.execute(input.clone(), &context).await.unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidInput, "{input}: {error}");
            assert_eq!(provider.call_count(), 0, "{input}");
            assert_eq!(events.load(Ordering::Relaxed), 0, "{input}");
            assert!(tool.env.team.roster().is_empty(), "{input}");
        }
    }

    #[tokio::test]
    async fn valid_pipeline_preserves_raw_references_and_reports_agents() {
        let provider = MockProvider::new(vec![MockRule {
            needle: "find targets".into(),
            reply: MockReply::ToolCall {
                name: "structured_output".into(),
                input: json!({"paths":["cli","core"]}),
            },
        }]);
        let (tool, context) = fixture(Arc::new(provider));
        let result = tool.execute(json!({"steps":[
            {"id":"files", "agent":{"prompt":"find targets", "schema":{"type":"object","required":["paths"]}}},
            {"id":"reviews", "fan_out":{"over":"$files.paths", "prompt":"review {item}"}},
            {"id":"summary", "agent":{"prompt":"summarize {$reviews}"}}
        ]}), &context).await.unwrap();
        let report: Value = serde_json::from_str(result.as_text().unwrap()).unwrap();
        assert_eq!(report["status"], "completed");
        assert_eq!(report["output"]["files"]["paths"], json!(["cli", "core"]));
        assert_eq!(report["output"]["reviews"].as_array().unwrap().len(), 2);
        assert_eq!(report["agents"].as_array().unwrap().len(), 4);
        assert_eq!(report["agents"][1]["input"], "cli");
        assert_eq!(report["agents"][2]["input"], "core");
        assert!(report["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent["status"] == "success"));
    }

    #[tokio::test]
    async fn incomplete_tool_result_retains_partial_outputs_and_failed_inputs() {
        let provider = MockProvider::new(vec![MockRule {
            needle: "good".into(),
            reply: MockReply::ToolCall {
                name: "structured_output".into(),
                input: json!({"ok":true}),
            },
        }]);
        let (tool, context) = fixture(Arc::new(provider));
        let error = tool
            .execute(
                json!({
                    "shape":"map_reduce", "items":["good", "bad", "good"], "map_prompt":"{item}",
                    "map_schema":{"type":"object", "required":["ok"]}, "reduce_prompt":"combine"
                }),
                &context,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::Failed);
        let report: Value =
            serde_json::from_str(error.message.strip_prefix("workflow incomplete: ").unwrap())
                .unwrap();
        assert_eq!(report["status"], "failed");
        assert_eq!(
            report["output"]["results"],
            json!([{"ok":true}, null, {"ok":true}])
        );
        assert_eq!(report["agents"].as_array().unwrap().len(), 3);
        assert_eq!(report["agents"][1]["status"], "failed");
        assert_eq!(report["agents"][1]["input"], "bad");
        assert!(report["agents"][1]["error"]
            .as_str()
            .unwrap()
            .contains("structured"));
        assert!(report["output"].get("reduced").is_none());
    }

    #[tokio::test]
    async fn turn_limit_is_incomplete_and_skips_synthesis() {
        for structured in [false, true] {
            let provider = MockProvider::new(vec![MockRule {
                needle: "You've reached your turn limit".into(),
                reply: MockReply::Text("Partial work; tests remain failing.".into()),
            }])
            .with_default(MockReply::ToolCall {
                name: "missing".into(),
                input: json!({}),
            });
            let (tool, context) = fixture(Arc::new(provider.clone()));
            let mut input = json!({"shape":"map_reduce", "items":["work"], "map_prompt":"keep going", "reduce_prompt":"combine"});
            if structured {
                input["map_schema"] = json!({"type":"object"});
            }
            let error = tool.execute(input, &context).await.unwrap_err();
            let report: Value =
                serde_json::from_str(error.message.strip_prefix("workflow incomplete: ").unwrap())
                    .unwrap();
            assert_eq!(report["status"], "failed");
            assert_eq!(report["output"]["results"], json!([null]));
            assert!(report["output"].get("reduced").is_none());
            assert_eq!(report["agents"].as_array().unwrap().len(), 1);
            assert_eq!(report["agents"][0]["status"], "failed");
            assert!(report["agents"][0]["error"]
                .as_str()
                .unwrap()
                .contains("turn limit"));
            assert_eq!(
                report["agents"][0]["output"]["text"],
                "[turn limit reached] Partial work; tests remain failing."
            );
            assert_eq!(
                provider.call_count(),
                crate::agent::agent::SUBAGENT_MAX_TURNS as usize + 1
            );
        }
    }

    #[tokio::test]
    async fn pre_cancelled_workflow_is_an_incomplete_tool_result() {
        let provider = MockProvider::new(vec![]);
        let (tool, context) = fixture(Arc::new(provider.clone()));
        tool.env.parent_cancel.store(true, Ordering::Relaxed);
        let error = tool
            .execute(
                json!({"shape":"fan_out", "items":["a","b"], "map_prompt":"{item}"}),
                &context,
            )
            .await
            .unwrap_err();
        let report: Value =
            serde_json::from_str(error.message.strip_prefix("workflow incomplete: ").unwrap())
                .unwrap();
        assert_eq!(report["status"], "cancelled");
        assert_eq!(report["agents"].as_array().unwrap().len(), 2);
        assert!(report["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent["status"] == "cancelled"));
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn editing_is_default_and_read_only_is_an_enforced_opt_in() {
        for steps in [false, true] {
            for read_only in [None, Some(false), Some(true)] {
                check_write_capability(steps, read_only, Decision::Allow).await;
            }
        }
    }

    #[tokio::test]
    async fn editing_workflows_still_honor_session_permissions() {
        check_write_capability(false, Some(false), Decision::Deny).await;
    }

    async fn check_write_capability(steps: bool, read_only: Option<bool>, decision: Decision) {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path();
        let provider = Arc::new(RecordingProvider {
            mock: MockProvider::new(vec![MockRule {
                needle: "attempt write".into(),
                reply: MockReply::ToolCall {
                    name: "write_file".into(),
                    input: json!({"path":"result.txt","content":"written"}),
                },
            }]),
            tools: Mutex::new(Vec::new()),
        });
        let (mut tool, mut context) = fixture(provider.clone());
        context.cwd = path.to_string_lossy().into_owned();
        tool.env.subagent_tools =
            ToolRegistry::new(Some(Arc::new(PermissionEngine::new(decision, None))));
        for builtin in crate::tools::builtin_tools(Arc::new(
            crate::core::shell::BashShell::resolve(path, None),
        )) {
            tool.env.subagent_tools.add(builtin);
        }
        let mut input = if steps {
            json!({"steps":[{"id":"write","agent":{"prompt":"attempt write"}}]})
        } else {
            json!({"shape":"fan_out", "items":["file"], "map_prompt":"attempt write"})
        };
        if let Some(read_only) = read_only {
            input["read_only"] = json!(read_only);
        }
        tool.execute(input, &context).await.unwrap();
        let offered = provider.tools.lock().unwrap();
        assert!(!offered.is_empty());
        for tools in offered.iter() {
            assert!(tools.iter().any(|name| name == "read_file"));
            for name in ["write_file", "edit_file", "bash"] {
                assert_eq!(
                    tools.iter().any(|tool| tool == name),
                    read_only != Some(true)
                );
            }
        }
        let wrote = path.join("result.txt").exists();
        assert_eq!(
            wrote,
            read_only != Some(true) && matches!(decision, Decision::Allow)
        );
        if wrote {
            assert_eq!(
                std::fs::read_to_string(path.join("result.txt")).unwrap(),
                "written"
            );
        }
    }
}
