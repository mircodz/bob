use crate::core::types::ToolOutput;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub enum PreToolDecision {
    Proceed(Value),
    /// Skips execution; the reason becomes a tool error result.
    Deny(String),
}

#[async_trait]
pub trait PreToolUse: Send + Sync {
    async fn on_pre_tool(&self, tool: &str, input: &Value) -> PreToolDecision;
}

#[async_trait]
pub trait PostToolUse: Send + Sync {
    async fn on_post_tool(&self, tool: &str, output: ToolOutput, is_error: bool) -> ToolOutput;
}

#[derive(Clone, Default)]
pub struct Hooks {
    pub pre: Vec<Arc<dyn PreToolUse>>,
    pub post: Vec<Arc<dyn PostToolUse>>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }

    /// Rewrites chain in registration order; the first denial short-circuits.
    pub async fn run_pre(&self, tool: &str, mut input: Value) -> Result<Value, String> {
        for hook in &self.pre {
            match hook.on_pre_tool(tool, &input).await {
                PreToolDecision::Proceed(next) => input = next,
                PreToolDecision::Deny(reason) => return Err(reason),
            }
        }
        Ok(input)
    }

    /// Run the post-tool hooks in registration order, chaining output rewrites.
    pub async fn run_post(&self, tool: &str, mut output: ToolOutput, is_error: bool) -> ToolOutput {
        for hook in &self.post {
            output = hook.on_post_tool(tool, output, is_error).await;
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Pre {
        seen: Arc<Mutex<Vec<(String, Value)>>>,
        deny: bool,
    }

    #[async_trait]
    impl PreToolUse for Pre {
        async fn on_pre_tool(&self, tool: &str, input: &Value) -> PreToolDecision {
            self.seen.lock().unwrap().push((tool.into(), input.clone()));
            if self.deny {
                PreToolDecision::Deny("policy denied the rewritten call".into())
            } else {
                PreToolDecision::Proceed(json!({"previous": input}))
            }
        }
    }

    struct Post {
        seen: Arc<Mutex<Vec<(String, String, bool)>>>,
        suffix: &'static str,
    }

    #[async_trait]
    impl PostToolUse for Post {
        async fn on_post_tool(&self, tool: &str, output: ToolOutput, is_error: bool) -> ToolOutput {
            self.seen.lock().unwrap().push((
                tool.into(),
                output.as_text().unwrap().to_owned(),
                is_error,
            ));
            format!("{}{}", output.as_text().unwrap(), self.suffix).into()
        }
    }

    #[tokio::test]
    async fn pre_rewrites_chain_and_first_denial_stops_later_hooks() {
        for deny in [false, true] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let hooks = Hooks {
                pre: vec![
                    Arc::new(Pre {
                        seen: seen.clone(),
                        deny: false,
                    }),
                    Arc::new(Pre {
                        seen: seen.clone(),
                        deny,
                    }),
                    Arc::new(Pre {
                        seen: seen.clone(),
                        deny: false,
                    }),
                ],
                post: vec![],
            };
            let input = json!({"path": "a.rs"});
            let once = json!({"previous": input});
            let twice = json!({"previous": once});
            let result = hooks.clone().run_pre("read_file", input.clone()).await;
            let mut expected = vec![("read_file".into(), input), ("read_file".into(), once)];
            if deny {
                assert_eq!(result, Err("policy denied the rewritten call".into()));
            } else {
                assert_eq!(result, Ok(json!({"previous": twice})));
                expected.push(("read_file".into(), twice));
            }
            assert_eq!(*seen.lock().unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn post_rewrites_chain_without_changing_tool_identity_or_error_flag() {
        for is_error in [false, true] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let hooks = Hooks {
                pre: vec![],
                post: vec![
                    Arc::new(Post {
                        seen: seen.clone(),
                        suffix: " first",
                    }),
                    Arc::new(Post {
                        seen: seen.clone(),
                        suffix: " second",
                    }),
                ],
            };
            assert_eq!(
                hooks
                    .run_post("write_file", "result".into(), is_error)
                    .await
                    .as_text(),
                Some("result first second")
            );
            assert_eq!(
                *seen.lock().unwrap(),
                vec![
                    ("write_file".into(), "result".into(), is_error),
                    ("write_file".into(), "result first".into(), is_error),
                ],
            );
        }
    }
}
