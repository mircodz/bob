use super::agent::{RunStopReason, ROOT_AGENT_ID};
use super::assembly::{build_root_agent, RootAgentParams};
use super::hooks::{Hooks, PreToolDecision, PreToolUse};
use super::prompt::{build_system_prompt, BASE_PROMPT};
use super::team::AgentRegistry;
use crate::core::config::load_config;
use crate::core::events::{AgentEvent, EventBus};
use crate::core::permissions::{Decision, PermissionEngine};
use crate::core::shell::CommandShell;
use crate::core::types::Usage;
use crate::providers::create_provider_with_options;
use crate::providers::models::ModelSpec;
use crate::providers::provider::StreamTimeouts;
use crate::test_support::TestDir;
use crate::tools::jobs::JobRegistry;
use crate::tools::registry::{UserAsker, UserQuery};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

struct NoQuestions;

#[async_trait::async_trait]
impl UserAsker for NoQuestions {
    async fn ask(&self, _: &UserQuery) -> Option<String> {
        None
    }
}

struct MoveBeforeRead {
    from: PathBuf,
    to: PathBuf,
    moved: AtomicBool,
}

#[async_trait::async_trait]
impl PreToolUse for MoveBeforeRead {
    async fn on_pre_tool(&self, tool: &str, input: &serde_json::Value) -> PreToolDecision {
        if tool == "read_file"
            && input["path"]
                .as_str()
                .is_some_and(|path| Path::new(path).file_name() == self.from.file_name())
            && !self.moved.swap(true, Ordering::Relaxed)
        {
            std::fs::rename(&self.from, &self.to).expect("inject concurrent file move");
        }
        PreToolDecision::Proceed(input.clone())
    }
}

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Lookup,
    Broad,
    Independent,
    StalePath,
}

impl Scenario {
    fn prompt(self) -> &'static str {
        match self {
            Self::Lookup => "What is DEFAULT_PAGE_SIZE in src/settings.py? Cite the source.",
            Self::Broad => "Trace startup authentication across the CLI, runtime, provider selection, token refresh, and UI. Why does an expired token with an offline refresh service produce a sign-in banner? Identify the error code and displayed message with source references. Do not change files.",
            Self::Independent => "Investigate two independent repository questions: (1) How does startup authentication turn an expired token and offline refresh service into a UI error? Identify the error code and message. (2) How does the cache decide whether to reuse or replace a stored response, and what is its lifetime in seconds? Give source references for both. Do not change files.",
            Self::StalePath => "Read src/config.py and tell me DEFAULT_PAGE_SIZE, citing its current source. Do not change files.",
        }
    }

    fn facts(self) -> &'static [&'static str] {
        match self {
            Self::Lookup | Self::StalePath => &["73", "settings.py"],
            Self::Broad => &[
                "auth_renewal_required",
                "sign in again",
                "refresh.py",
                "errors.py",
            ],
            Self::Independent => &[
                "auth_renewal_required",
                "sign in again",
                "317",
                "policy.py",
                "service.py",
            ],
        }
    }
}

fn fixture() -> TestDir {
    let dir = TestDir::new();
    std::fs::create_dir(dir.join(".git")).unwrap();
    for (path, content) in [
        ("src/settings.py", "DEFAULT_PAGE_SIZE = 73\n"),
        ("src/cli/main.py", "from runtime.session import start\nfrom ui.errors import render\n\ndef launch(token, refresh_service):\n    return render(start(token, refresh_service))\n"),
        ("src/runtime/session.py", "from providers.router import select\n\ndef start(token, refresh_service):\n    return select('atlas').authenticate(token, refresh_service)\n"),
        ("src/providers/router.py", "from providers import atlas\n\ndef select(name):\n    if name == 'atlas':\n        return atlas\n    raise ValueError(name)\n"),
        ("src/providers/atlas.py", "from auth.refresh import renew\n\ndef authenticate(token, refresh_service):\n    current = renew(token, refresh_service)\n    return 'AUTH_RENEWAL_REQUIRED' if current is None else 'READY'\n"),
        ("src/auth/refresh.py", "from auth.store import save\n\ndef renew(token, refresh_service):\n    if not token.expired:\n        return token\n    try:\n        current = refresh_service.exchange(token)\n    except ConnectionError:\n        return None\n    save(current)\n    return current\n"),
        ("src/auth/store.py", "saved_token = None\n\ndef save(token):\n    global saved_token\n    saved_token = token\n"),
        ("src/ui/errors.py", "def render(code):\n    return {'AUTH_RENEWAL_REQUIRED': 'Sign in again', 'READY': 'Connected'}[code]\n"),
        ("src/cache/policy.py", "CACHE_TTL_SECONDS = 317\n\ndef usable(entry, now):\n    return now - entry.created_at < CACHE_TTL_SECONDS\n"),
        ("src/cache/service.py", "from cache.policy import usable\n\ndef load(key, now, store, fetch):\n    entry = store.get(key)\n    if entry is not None and usable(entry, now):\n        return entry.value\n    value = fetch(key)\n    store.put(key, value, now)\n    return value\n"),
    ] {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    dir
}

#[tokio::test]
#[ignore = "requires GitHub Copilot authentication and live model requests"]
async fn copilot_prompt_behavior() {
    let model = match std::env::var("BOB_EVAL_MODEL") {
        Ok(model) => model,
        Err(std::env::VarError::NotPresent) => {
            let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let config = load_config(&project)
                .unwrap_or_else(|_| panic!("cannot load project model configuration"));
            let spec = ModelSpec::parse(&config.provider);
            assert_eq!(
                spec.provider, "copilot",
                "set BOB_EVAL_MODEL or configure Copilot; no fallback backend"
            );
            if config.model.is_empty() {
                spec.model
            } else {
                config.model
            }
        }
        Err(_) => panic!("BOB_EVAL_MODEL must be Unicode"),
    };
    let model = model.strip_prefix("copilot:").unwrap_or(&model).trim();
    assert!(
        !model.is_empty() && !model.contains([':', '/']),
        "set a nonempty Copilot model ID in BOB_EVAL_MODEL or project configuration"
    );
    let repeats = std::env::var("BOB_EVAL_REPEATS").map_or(2, |value| {
        value
            .parse::<usize>()
            .expect("BOB_EVAL_REPEATS must be a positive integer")
    });
    assert!(repeats > 0, "BOB_EVAL_REPEATS must be positive");
    let provider = create_provider_with_options(
        &format!("copilot:{model}"),
        None,
        StreamTimeouts {
            startup: Duration::from_secs(30),
            idle: Duration::from_secs(30),
        },
    )
    .await
    .unwrap_or_else(|_| {
        panic!("Copilot initialization failed; check authentication and model availability")
    });
    eprintln!("Copilot model: {model}; repetitions: {repeats}");
    let mut failures = Vec::new();
    for repeat in 1..=repeats {
        for scenario in [
            Scenario::Lookup,
            Scenario::Broad,
            Scenario::Independent,
            Scenario::StalePath,
        ] {
            let dir = fixture();
            let mut hooks = Hooks::default();
            if matches!(scenario, Scenario::StalePath) {
                let from = dir.join("src/config.py");
                let to = dir.join("src/settings.py");
                std::fs::rename(&to, &from).unwrap();
                hooks.pre.push(Arc::new(MoveBeforeRead {
                    from,
                    to,
                    moved: AtomicBool::new(false),
                }));
            }
            let shell = Arc::new(CommandShell::resolve(dir.path(), None));
            let mut permissions = PermissionEngine::new(Decision::Deny, None);
            permissions.add(Arc::new(|request| {
                Some(
                    if matches!(
                        request.tool.as_str(),
                        "read_file"
                            | "glob"
                            | "grep"
                            | "list_dir"
                            | "explore"
                            | "task"
                            | "workflow"
                            | "spawn_agent"
                            | "send_message"
                            | "list_agents"
                            | "stop_agent"
                            | "todo_write"
                            | "job_status"
                            | "job_output"
                    ) {
                        Decision::Allow
                    } else {
                        Decision::Deny
                    },
                )
            }));
            let bus = EventBus::new();
            let events = Arc::new(Mutex::new(Vec::new()));
            let observed = events.clone();
            let _subscription = bus.subscribe(Arc::new(move |event| {
                if matches!(
                    event,
                    AgentEvent::ToolCall { .. }
                        | AgentEvent::ToolResult { .. }
                        | AgentEvent::Completion { .. }
                ) {
                    observed.lock().unwrap().push(event.clone());
                }
            }));
            let jobs = JobRegistry::new();
            let team = AgentRegistry::new();
            let mut root = build_root_agent(RootAgentParams {
                provider: provider.clone(),
                permissions: Arc::new(permissions),
                bus,
                jobs: jobs.clone(),
                team: team.clone(),
                cwd: dir.path().to_string_lossy().into_owned(),
                system_prompt: build_system_prompt(Some(BASE_PROMPT), dir.path(), &shell),
                shell,
                mcp_tools: Vec::new(),
                extra_tools: Vec::new(),
                lsp: None,
                user_asker: Arc::new(NoQuestions),
                max_turns: Some(40),
                definitions: Default::default(),
                hooks,
                memory: None,
            });
            let cancel = root.cancel_handle();
            let mut run =
                tokio::spawn(async move { root.run_with_budget(scenario.prompt(), 40).await });
            let result = tokio::time::timeout(Duration::from_secs(180), &mut run).await;
            cancel.store(true, Ordering::Relaxed);
            team.stop_descendants(ROOT_AGENT_ID);
            if result.is_err()
                && tokio::time::timeout(Duration::from_secs(5), &mut run)
                    .await
                    .is_err()
            {
                run.abort();
                let _ = run.await;
            }
            let mut settled = jobs.shutdown().await;
            if !settled {
                settled = jobs.shutdown().await;
            }
            settled &= !team.has_unsettled_descendants(ROOT_AGENT_ID);
            let (status, answer) = match result {
                Ok(Ok(Ok(outcome))) => (
                    format!("{:?}", outcome.stop_reason),
                    (outcome.stop_reason == RunStopReason::Completed).then_some(outcome.text),
                ),
                Ok(Ok(Err(_))) => ("provider/runtime error".into(), None),
                Ok(Err(_)) => ("root task failed".into(), None),
                Err(_) => ("timeout".into(), None),
            };
            let events = events.lock().unwrap();
            let calls: Vec<_> = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| match event {
                    AgentEvent::ToolCall {
                        agent_id,
                        tool_use_id,
                        name,
                        input,
                    } if agent_id == ROOT_AGENT_ID => {
                        Some((index, tool_use_id, name.as_str(), input))
                    }
                    _ => None,
                })
                .collect();
            let result_for = |call_index: usize, id: &str| {
                events.iter().enumerate().skip(call_index + 1).find_map(
                    |(index, event)| match event {
                        AgentEvent::ToolResult {
                            agent_id,
                            tool_use_id,
                            output,
                            is_error,
                        } if agent_id == ROOT_AGENT_ID && tool_use_id == id => {
                            Some((index, output.model_text().into_owned(), *is_error))
                        }
                        _ => None,
                    },
                )
            };
            let explores: Vec<_> = calls.iter().filter(|call| call.2 == "explore").collect();
            let lookup = |path: &str, error: bool| {
                calls.iter().find_map(|call| {
                    (call.2 == "read_file"
                        && call.3["path"]
                            .as_str()
                            .is_some_and(|p| p.replace('\\', "/").ends_with(path)))
                    .then(|| result_for(call.0, call.1).filter(|result| result.2 == error))
                    .flatten()
                })
            };
            let selection = match scenario {
                Scenario::Lookup => {
                    lookup("src/settings.py", false).is_some()
                        && calls
                            .iter()
                            .find(|call| {
                                matches!(call.2, "read_file" | "grep" | "glob" | "list_dir")
                            })
                            .is_some_and(|call| call.2 == "read_file")
                        && !calls.iter().any(|call| {
                            matches!(call.2, "explore" | "task" | "spawn_agent" | "workflow")
                        })
                }
                Scenario::Broad => explores.first().is_some_and(|first| {
                    calls
                        .iter()
                        .filter(|call| {
                            call.0 < first.0
                                && matches!(call.2, "read_file" | "grep" | "glob" | "list_dir")
                        })
                        .count()
                        <= 3
                }),
                Scenario::Independent => explores.iter().any(|first| {
                    result_for(first.0, first.1).is_some_and(|result| {
                        explores
                            .iter()
                            .any(|second| first.0 < second.0 && second.0 < result.0)
                    })
                }),
                Scenario::StalePath => lookup("src/config.py", true).is_some_and(|failed| {
                    lookup("src/settings.py", false).is_some_and(|found| {
                        found.0 > failed.0
                            && calls.iter().any(|call| {
                                failed.0 < call.0
                                    && call.0 < found.0
                                    && matches!(call.2, "glob" | "grep" | "list_dir")
                            })
                    })
                }),
            };
            let facts = scenario.facts();
            let correct = answer.as_ref().is_some_and(|text| {
                let text = text.to_lowercase();
                facts.iter().all(|fact| text.contains(*fact)) && text.contains(".py:")
            });
            let delegated = matches!(scenario, Scenario::Broad | Scenario::Independent);
            let first_report = explores
                .iter()
                .filter_map(|call| result_for(call.0, call.1))
                .filter(|result| !result.2)
                .map(|result| result.0)
                .min();
            let repeated_probes = first_report.map_or(0, |report| {
                calls
                    .iter()
                    .filter(|call| {
                        call.0 > report
                            && matches!(call.2, "read_file" | "grep" | "glob" | "list_dir")
                    })
                    .count()
            });
            let reused = !delegated || repeated_probes <= 2;
            let evidence = if delegated {
                explores
                    .iter()
                    .filter_map(|call| result_for(call.0, call.1))
                    .filter(|result| !result.2)
                    .map(|result| result.1)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                lookup("src/settings.py", false).map_or(String::new(), |result| result.1)
            }
            .to_lowercase();
            let sources = events.iter().filter_map(|event| match event {
                AgentEvent::ToolResult { agent_id, tool_use_id, output, is_error: false }
                    if (!delegated || agent_id != ROOT_AGENT_ID) && events.iter().any(|call| matches!(call,
                        AgentEvent::ToolCall { agent_id: owner, tool_use_id: id, name, .. }
                        if owner == agent_id && id == tool_use_id && matches!(name.as_str(), "read_file" | "grep"))) =>
                    Some(output.model_text().into_owned()),
                _ => None,
            }).collect::<Vec<_>>().join("\n").to_lowercase();
            let grounded = [&evidence, &sources].iter().all(|text| {
                if delegated {
                    text.contains("auth_renewal_required")
                        && text.contains("sign in again")
                        && (!matches!(scenario, Scenario::Independent) || text.contains("317"))
                } else {
                    text.contains("73")
                }
            });
            let mut usage = Usage::default();
            for event in events.iter() {
                if let AgentEvent::Completion {
                    usage: completion, ..
                } = event
                {
                    usage.add(completion);
                }
            }
            let order = calls
                .iter()
                .map(|call| call.2)
                .collect::<Vec<_>>()
                .join(", ");
            let errors = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolResult { is_error: true, .. }))
                .count();
            eprintln!("{scenario:?} #{repeat}: {status}; selection={selection} correct={correct} evidence={grounded} reuse={reused} repeated_probes={repeated_probes} cleanup={settled}; tokens in={} out={}; tool_errors={errors}; root_tools=[{order}]", usage.total_input(), usage.output_tokens);
            if !(selection && correct && grounded && reused && settled) {
                for call in &calls {
                    eprintln!(
                        "  {} input={} error={:?}",
                        call.2,
                        call.3,
                        result_for(call.0, call.1).map(|result| result.2)
                    );
                }
                failures.push(format!("{scenario:?} #{repeat}"));
            }
            assert!(settled, "agent cleanup failed; stopping live evaluation");
        }
    }
    assert!(
        failures.is_empty(),
        "behavioral checks failed: {}",
        failures.join(", ")
    );
}
