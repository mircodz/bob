use crate::core::types::{
    Completion, ContentBlock, GenerateOptions, Message, Role, StopReason, StreamEvent, Usage,
};
use crate::providers::provider::Provider;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
pub enum MockReply {
    Text(String),
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    Truncated {
        name: String,
        input: serde_json::Value,
        output_tokens: u64,
    },
}

#[derive(Clone)]
pub struct MockRule {
    pub needle: String,
    pub reply: MockReply,
}

#[derive(Clone)]
pub struct MockProvider {
    rules: Arc<Vec<MockRule>>,
    default: Arc<MockReply>,
    calls: Arc<AtomicUsize>,
    model: String,
    delay: std::time::Duration,
}

impl MockProvider {
    pub fn new(rules: Vec<MockRule>) -> Self {
        MockProvider {
            rules: Arc::new(rules),
            default: Arc::new(MockReply::Text("ok".to_string())),
            calls: Arc::new(AtomicUsize::new(0)),
            model: "mock-model".to_string(),
            delay: std::time::Duration::ZERO,
        }
    }

    pub fn with_default(mut self, reply: MockReply) -> Self {
        self.default = Arc::new(reply);
        self
    }

    pub fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn reply_for(&self, messages: &[Message]) -> MockReply {
        let last = messages
            .iter()
            .rev()
            .find(|m| m.role != Role::Assistant)
            .map(|m| m.text())
            .unwrap_or_default();
        for rule in self.rules.iter() {
            if last.contains(&rule.needle) {
                return rule.reply.clone();
            }
        }
        (*self.default).clone()
    }

    fn completion_for(&self, reply: MockReply) -> Completion {
        let (content, stop_reason, output_tokens) = match reply {
            MockReply::Text(text) => (vec![ContentBlock::Text { text }], StopReason::EndTurn, 5),
            MockReply::ToolCall { name, input } => (
                vec![ContentBlock::ToolUse {
                    id: format!("mock_{}", self.calls.load(Ordering::Relaxed)),
                    name,
                    input,
                }],
                StopReason::ToolUse,
                5,
            ),
            MockReply::Truncated {
                name,
                input,
                output_tokens,
            } => (
                vec![ContentBlock::ToolUse {
                    id: format!("mock_{}", self.calls.load(Ordering::Relaxed)),
                    name,
                    input,
                }],
                StopReason::MaxTokens,
                output_tokens,
            ),
        };
        Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: Usage {
                input_tokens: 10,
                output_tokens,
                ..Default::default()
            },
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn model(&self) -> &str {
        &self.model
    }

    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(self.completion_for(self.reply_for(&opts.messages)))
    }

    async fn stream(
        &self,
        opts: GenerateOptions,
    ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let completion = self.completion_for(self.reply_for(&opts.messages));
        let delay = self.delay;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let _ = tx.send(StreamEvent::MessageStop { completion });
        });
        Ok(rx)
    }
}
