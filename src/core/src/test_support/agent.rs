use crate::agent::agent::{AgentConfig, DEFAULT_MAX_TURNS};
use crate::agent::hooks::Hooks;
use crate::core::events::EventBus;
use crate::core::types::{
    Completion, ContentBlock, GenerateOptions, Message, Role, StopReason, StreamEvent, Usage,
};
use crate::providers::provider::Provider;
use crate::tools::jobs::JobRegistry;
use crate::tools::registry::ToolRegistry;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{mpsc, Notify};

pub(crate) enum Step {
    Complete(Vec<ContentBlock>),
    Pending,
    Parked,
    Transient,
    Overflow,
    Partial,
    Events(Vec<StreamEvent>),
}

pub(crate) enum Summary {
    Complete,
    Pending,
    Error,
}

pub(crate) struct ScriptProvider {
    steps: Mutex<VecDeque<Step>>,
    summaries: Mutex<VecDeque<Summary>>,
    pub(crate) requests: Mutex<Vec<Vec<Message>>>,
    pub(crate) systems: Mutex<Vec<Option<String>>>,
    pub(crate) caps: Mutex<Vec<Option<u32>>>,
    pub(crate) calls: AtomicUsize,
    pub(crate) started: Notify,
    parked: Mutex<Vec<mpsc::UnboundedSender<StreamEvent>>>,
}

impl ScriptProvider {
    pub(crate) fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            summaries: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            systems: Mutex::new(Vec::new()),
            caps: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            parked: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn with_summary(self, summary: Summary) -> Self {
        self.summaries.lock().unwrap().push_back(summary);
        self
    }

    pub(crate) fn assert_exhausted(&self) {
        assert!(
            self.steps.lock().unwrap().is_empty(),
            "unused stream responses"
        );
        assert!(
            self.summaries.lock().unwrap().is_empty(),
            "unused summary responses"
        );
    }
}

pub(crate) fn text(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.into() }
}

pub(crate) fn tool(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

pub(crate) fn completion(content: Vec<ContentBlock>) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content,
        },
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 3,
        },
    }
}

#[async_trait]
impl Provider for ScriptProvider {
    fn name(&self) -> &str {
        "script"
    }
    fn model(&self) -> &str {
        "script-model"
    }

    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        assert!(opts.tools.is_empty());
        self.calls.fetch_add(1, Ordering::Relaxed);
        let summary = self
            .summaries
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected non-streaming provider call");
        match summary {
            Summary::Complete => Ok(completion(vec![text("saved handoff")])),
            Summary::Pending => {
                self.started.notify_one();
                std::future::pending().await
            }
            Summary::Error => anyhow::bail!("summary HTTP 401: invalid credential"),
        }
    }

    async fn stream(
        &self,
        opts: GenerateOptions,
    ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.caps.lock().unwrap().push(opts.max_tokens);
        self.requests.lock().unwrap().push(opts.messages);
        self.systems.lock().unwrap().push(opts.system);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider call");
        let (tx, rx) = mpsc::unbounded_channel();
        match step {
            Step::Complete(content) => {
                tx.send(StreamEvent::MessageStop {
                    completion: completion(content),
                })
                .unwrap();
            }
            Step::Pending => {
                self.started.notify_one();
                return std::future::pending().await;
            }
            Step::Parked => {
                self.parked.lock().unwrap().push(tx);
                self.started.notify_one();
            }
            Step::Transient => anyhow::bail!("HTTP 503"),
            Step::Overflow => anyhow::bail!("Your input exceeds the context window of this model."),
            Step::Partial => {
                tx.send(StreamEvent::TextDelta {
                    text: "partial answer".into(),
                })
                .unwrap();
            }
            Step::Events(events) => {
                for event in events {
                    tx.send(event).unwrap();
                }
            }
        }
        Ok(rx)
    }
}

pub(crate) fn config(provider: Arc<dyn Provider>) -> AgentConfig {
    AgentConfig {
        provider,
        tools: ToolRegistry::new(None),
        bus: EventBus::new(),
        system: None,
        cwd: ".".into(),
        max_turns: DEFAULT_MAX_TURNS,
        id: Some("root".into()),
        context_window: 200_000,
        compact_threshold: 0.8,
        keep_recent: 6,
        jobs: JobRegistry::new(),
        user_asker: None,
        lsp: None,
        inbox: None,
        team: None,
        name: "root".into(),
        depth: 0,
        parent_cancel: None,
        cancel: None,
        hooks: Hooks::default(),
    }
}
