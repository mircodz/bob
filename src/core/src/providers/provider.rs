//! The one trait every provider implements. Deliberately minimal: a blocking
//! `generate` and a streaming `stream`. Everything else is built on these two.

use crate::core::types::{Completion, GenerateOptions, StreamEvent};
use async_trait::async_trait;
use tokio::sync::mpsc;

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion>;
    /// Returns a channel of stream events. The provider spawns a task that
    /// drives the HTTP stream and sends events; the receiver is consumed by
    /// the agent loop.
    async fn stream(&self, opts: GenerateOptions)
        -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>>;

    /// List the model ids this provider offers (via its /models endpoint).
    /// Default: none — providers that can enumerate override this.
    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
}
