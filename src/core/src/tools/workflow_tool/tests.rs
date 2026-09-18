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
    let jobs = crate::tools::jobs::JobRegistry::new();
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
    };
    let context = ToolContext {
        cwd: ".".into(),
        files: Arc::new(crate::tools::file_tracker::FileTracker::new()),
        todos: Arc::new(crate::tools::todo::TodoStore::new()),
        jobs,
        user_asker: None,
        lsp: None,
        coord: None,
        permissions: None,
    };
    (WorkflowTool { env }, context)
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
    }
    assert_eq!(provider.call_count(), 0);
    assert_eq!(events.load(Ordering::Relaxed), 0);
    assert!(tool.env.team.roster().is_empty());
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
    let report: Value = serde_json::from_str(&result).unwrap();
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
        serde_json::from_str(error.message.strip_prefix("workflow incomplete: ").unwrap()).unwrap();
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
        serde_json::from_str(error.message.strip_prefix("workflow incomplete: ").unwrap()).unwrap();
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
    let path = std::env::temp_dir().join(format!(
        "bob-workflow-tools-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&path).unwrap();
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
    for builtin in crate::tools::builtin_tools() {
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
    std::fs::remove_dir_all(path).unwrap();
}
