mod catalog;
#[cfg(test)]
mod catalog_tests;
mod failure;

use anyhow::{bail, ensure, Context, Result};
use bob_core::providers::provider::StreamTimeouts;
use bob_kernel::{
    ModelActivity, ModelCapabilities, ModelFailureKind, ModelInfo, ModelProgress, ModelUpdate,
    RequestSpec,
};
use bob_types::{Completion, GenerateOptions, StreamEvent};
use catalog::{CoreFactory, ModelBinding, ProviderFactory, ProviderKind, Selection};
use failure::AdapterFailure;
use std::sync::{Arc, RwLock};
use tokio::sync::{watch, Mutex};

pub fn normalize_model_id(value: &str) -> Result<String> {
    Ok(Selection::parse(value)?.id())
}

/// Whether `value` starts with a known provider, as in `deepseek:model` or `openai/model`.
pub fn is_provider_qualified(value: &str) -> bool {
    value
        .split_once([':', '/'])
        .is_some_and(|(provider, _)| ProviderKind::parse(provider).is_ok())
}

pub fn qualify_model_id(value: &str, current: &str) -> Result<String> {
    let prefix = value.split('/').next().unwrap_or(value);
    if value.contains(':') || ProviderKind::parse(prefix).is_ok() {
        return normalize_model_id(value);
    }
    let current = Selection::parse(current)?;
    normalize_model_id(&format!("{}:{value}", current.provider.name()))
}

#[derive(Clone)]
pub struct ProviderService {
    catalog: Arc<RwLock<Catalog>>,
    loading: Arc<Mutex<()>>,
    timeouts: StreamTimeouts,
    factory: Arc<dyn ProviderFactory>,
}

struct Catalog {
    models: Vec<Arc<ModelBinding>>,
    defaults: Vec<(ProviderKind, String)>,
}

impl Catalog {
    fn binding(&self, value: &str) -> Result<Arc<ModelBinding>> {
        let selection = Selection::parse(value)?;
        let id = if selection.model.is_empty() {
            self.defaults
                .iter()
                .find(|(kind, _)| *kind == selection.provider)
                .map(|(_, id)| id.clone())
                .with_context(|| format!("provider default {value:?} has not been loaded"))?
        } else {
            selection.id()
        };
        self.models
            .iter()
            .find(|model| model.info.id == id)
            .cloned()
            .with_context(|| format!("model {id:?} is not selected in this executor's catalog"))
    }
}

impl ProviderService {
    pub(crate) async fn create(models: &[String]) -> Result<Self> {
        Self::create_configured(models, StreamTimeouts::default()).await
    }

    pub(crate) async fn create_configured(
        models: &[String],
        timeouts: StreamTimeouts,
    ) -> Result<Self> {
        Self::create_with_factory(models, timeouts, Arc::new(CoreFactory)).await
    }

    async fn create_with_factory(
        models: &[String],
        timeouts: StreamTimeouts,
        factory: Arc<dyn ProviderFactory>,
    ) -> Result<Self> {
        let selections = catalog::requested_models(models)?;
        let (models, defaults) = tokio::time::timeout(
            timeouts.startup,
            catalog::discover(&selections, factory.as_ref(), timeouts),
        )
        .await
        .map_err(|_| failure::timeout("catalog discovery"))??;
        Ok(Self {
            catalog: Arc::new(RwLock::new(Catalog {
                models: models.into_iter().map(Arc::new).collect(),
                defaults,
            })),
            loading: Arc::new(Mutex::new(())),
            timeouts,
            factory,
        })
    }

    pub async fn ensure_model(&self, value: &str) -> Result<String> {
        let selection = Selection::parse(value)?;
        tokio::time::timeout(self.timeouts.startup, async {
            let _loading = self.loading.lock().await;
            if let Ok(binding) = self.binding(value) {
                binding
                    .provider(self.factory.as_ref(), self.timeouts)
                    .await?;
                return Ok(binding.info.id.clone());
            }
            let (models, defaults) =
                catalog::discover(&[selection], self.factory.as_ref(), self.timeouts).await?;
            let discovered = Catalog {
                models: models.into_iter().map(Arc::new).collect(),
                defaults,
            };
            let id = discovered.binding(value)?.info.id.clone();
            let mut catalog = self.catalog.write().unwrap();
            for binding in discovered.models {
                if let Some(existing) = catalog
                    .models
                    .iter_mut()
                    .find(|m| m.info.id == binding.info.id)
                {
                    if existing.provider.get().is_none() && binding.provider.get().is_some() {
                        *existing = binding;
                    }
                } else {
                    catalog.models.push(binding);
                }
            }
            catalog.defaults.extend(discovered.defaults);
            Ok(id)
        })
        .await
        .map_err(|_| failure::timeout("model discovery"))?
    }

    fn binding(&self, value: &str) -> Result<Arc<ModelBinding>> {
        self.catalog.read().unwrap().binding(value)
    }

    pub fn resolved_model_id(&self, value: &str) -> Result<String> {
        Ok(self.binding(value)?.info.id.clone())
    }

    pub fn capabilities(&self, id: &str) -> ModelCapabilities {
        self.binding(id)
            .map(|binding| binding.capabilities.clone())
            .unwrap_or_default()
    }

    pub(crate) fn classify_error(&self, error: &anyhow::Error) -> ModelFailureKind {
        failure::classify(error)
    }

    pub fn models(&self) -> Vec<ModelInfo> {
        self.catalog
            .read()
            .unwrap()
            .models
            .iter()
            .map(|m| m.info.clone())
            .collect()
    }

    pub(crate) async fn generate(
        &self,
        request: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        progress: ModelProgress,
    ) -> Result<Completion> {
        let binding = self.binding(&request.model.id)?;
        let options = generation_options(request, &binding).map_err(|error| {
            if error.is::<AdapterFailure>() {
                error
            } else {
                error.context(AdapterFailure {
                    kind: ModelFailureKind::Fatal,
                    message: "invalid provider request".into(),
                })
            }
        })?;
        let attempt = async {
            progress
                .emit(ModelUpdate::Activity(ModelActivity::Waiting))
                .await?;
            let mut events = tokio::time::timeout(self.timeouts.startup, async {
                binding
                    .provider(self.factory.as_ref(), self.timeouts)
                    .await?
                    .stream(options)
                    .await
            })
            .await
            .map_err(|_| failure::timeout("startup"))??;
            while let Some(event) = events.recv().await {
                match event {
                    StreamEvent::TextDelta { text } => {
                        if !text.is_empty() {
                            progress.emit(ModelUpdate::Text(text)).await?;
                        }
                    }
                    StreamEvent::ReasoningDelta {
                        block_id,
                        kind,
                        text,
                    } => {
                        if !text.is_empty() {
                            progress
                                .emit(ModelUpdate::Reasoning {
                                    block_id,
                                    kind,
                                    text,
                                })
                                .await?;
                        }
                    }
                    StreamEvent::ToolUseStart { .. } | StreamEvent::ToolUseInputDelta { .. } => {
                        progress
                            .emit(ModelUpdate::Activity(ModelActivity::ToolCall))
                            .await?;
                    }
                    StreamEvent::MessageStop { completion } => return Ok(completion),
                    StreamEvent::Error { message } => {
                        bail!("provider stream failed: {message}; no retry attempted")
                    }
                }
            }
            bail!("provider stream ended before a final completion; no retry attempted")
        };
        tokio::select! {
            biased;
            _ = bob_kernel::cancelled(&mut cancel) => bail!("generation cancelled"),
            result = attempt => result,
        }
    }
}

fn generation_options(request: RequestSpec, binding: &ModelBinding) -> Result<GenerateOptions> {
    ensure!(
        normalize_model_id(&request.model.id)? == binding.info.id,
        "model binding mismatch"
    );
    ensure!(
        request.model.context_window == binding.info.context_window,
        "request context window differs from the provider's advertised limit"
    );
    ensure!(request.max_tokens > 0, "max_tokens must be positive");
    ensure!(
        request.max_tokens <= binding.capabilities.max_output_tokens,
        "requested output exceeds the provider's {}-token output limit",
        binding.capabilities.max_output_tokens
    );
    let input = bob_kernel::estimate_input_tokens(&request)?;
    let budget = binding
        .capabilities
        .max_input_tokens
        .unwrap_or(binding.info.context_window)
        .min(
            binding
                .info
                .context_window
                .checked_sub(request.max_tokens as usize)
                .context("requested output exceeds the model's total context window")?,
        );
    if input > budget {
        return Err(AdapterFailure {
            kind: ModelFailureKind::ContextOverflow,
            message: format!("context input budget exceeded: estimated {input} tokens, available {budget}; shorten messages/system/tools or lower max_tokens (no request sent)"),
        }.into());
    }
    Ok(GenerateOptions {
        system: Some(request.system),
        messages: request.messages,
        tools: request.tools,
        max_tokens: Some(request.max_tokens),
        reasoning: request.reasoning,
        cache: true,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bob_core::auth::copilot::ModelLimits;
    use bob_core::providers::provider::Provider;
    use bob_kernel::{
        Command, CommandId, Executor, Handle, Invocation, LiveAttempt, LiveSnapshot, Policy,
        PreparedTool, Profile, Runtime, ThreadId, ToolDescriptor, ToolOutcome,
    };
    use bob_types::{
        ContentBlock, Message, ReasoningEffort, ReasoningKind, Role, StopReason, ToolSpec, Usage,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::{mpsc, Notify, OnceCell};

    #[test]
    fn copilot_bindings_use_advertised_limits_not_family_guesses() {
        let binding = catalog::copilot_binding(&ModelLimits {
            id: "m".into(),
            max_context_window_tokens: Some(12345),
            max_prompt_tokens: Some(12000),
            max_output_tokens: Some(1000),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(binding.info.id, "copilot:m");
        assert_eq!(binding.info.context_window, 12345);
        assert_eq!(binding.capabilities.max_input_tokens, Some(12000));
        assert_eq!(binding.capabilities.max_output_tokens, 1000);
        assert!(catalog::copilot_binding(&ModelLimits {
            id: "no-limit".into(),
            ..Default::default()
        })
        .is_err());
    }

    #[derive(Clone, Copy)]
    enum Mode {
        Stream,
        StartupError,
        StartupPending,
    }

    struct Leaf {
        options: Mutex<Vec<GenerateOptions>>,
        calls: AtomicUsize,
        mode: Mode,
        entered: Notify,
        startup_dropped: AtomicBool,
        events: Mutex<Option<mpsc::Receiver<StreamEvent>>>,
    }

    struct StartupGuard<'a>(&'a AtomicBool);

    impl Drop for StartupGuard<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Provider for Leaf {
        fn name(&self) -> &str {
            "copilot-test-leaf"
        }
        fn model(&self) -> &str {
            "advertised"
        }
        async fn generate(&self, _: GenerateOptions) -> Result<Completion> {
            panic!("retrying method must not be used")
        }
        async fn generate_once(&self, _: GenerateOptions) -> Result<Completion> {
            panic!("non-streaming method must not be used")
        }
        async fn stream(&self, options: GenerateOptions) -> Result<mpsc::Receiver<StreamEvent>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.options.lock().unwrap().push(options);
            self.entered.notify_one();
            match self.mode {
                Mode::StartupError => bail!("HTTP 429 fixture"),
                Mode::StartupPending => {
                    let _guard = StartupGuard(&self.startup_dropped);
                    std::future::pending().await
                }
                Mode::Stream => Ok(self
                    .events
                    .lock()
                    .unwrap()
                    .take()
                    .expect("single stream attempt")),
            }
        }
    }

    struct NoDiscovery;

    #[async_trait]
    impl ProviderFactory for NoDiscovery {
        async fn create(&self, _: &str, _: StreamTimeouts) -> Result<Arc<dyn Provider>> {
            panic!("generation must not discover unrequested providers")
        }

        async fn copilot_limits(&self) -> Result<Vec<ModelLimits>> {
            panic!("generation must not discover a catalog")
        }
    }

    fn with_leaf(
        mode: Mode,
    ) -> (
        ProviderService,
        Arc<Leaf>,
        RequestSpec,
        mpsc::Sender<StreamEvent>,
    ) {
        let (sender, events) = mpsc::channel(8);
        let leaf = Arc::new(Leaf {
            options: Default::default(),
            calls: AtomicUsize::new(0),
            mode,
            entered: Notify::new(),
            startup_dropped: AtomicBool::new(false),
            events: Mutex::new(Some(events)),
        });
        let limits = ModelLimits {
            id: "advertised".into(),
            max_context_window_tokens: Some(32000),
            max_prompt_tokens: Some(8000),
            max_output_tokens: Some(2000),
            ..Default::default()
        };
        let mut binding = catalog::copilot_binding(&limits).unwrap();
        binding.provider = OnceCell::new_with(Some(leaf.clone() as Arc<dyn Provider>));
        let info = binding.info.clone();
        let request = RequestSpec {
            model: info.clone(),
            workspace: "unused-by-protocol-leaf".into(),
            system: "exact system\n".into(),
            messages: vec![Message::user_text("日本語\nexact user text")],
            tools: vec![ToolSpec {
                name: "only_this_tool".into(),
                description: "exact schema".into(),
                input_schema: json!({"type":"object", "additionalProperties":false, "properties":{}}),
            }],
            reasoning: ReasoningEffort::High,
            max_tokens: 321,
        };
        (
            ProviderService {
                catalog: Arc::new(RwLock::new(Catalog {
                    models: vec![Arc::new(binding)],
                    defaults: Vec::new(),
                })),
                loading: Arc::new(tokio::sync::Mutex::new(())),
                timeouts: StreamTimeouts::default(),
                factory: Arc::new(NoDiscovery),
            },
            leaf,
            request,
            sender,
        )
    }

    type CapturedResult = std::result::Result<Completion, String>;

    struct TestExecutor {
        provider: ProviderService,
        request: RequestSpec,
        results: mpsc::Sender<CapturedResult>,
    }

    #[async_trait]
    impl Executor for TestExecutor {
        fn models(&self) -> Vec<ModelInfo> {
            self.provider.models()
        }
        fn tools(&self) -> Vec<ToolDescriptor> {
            Vec::new()
        }
        async fn generate(
            &self,
            _: RequestSpec,
            cancel: watch::Receiver<bool>,
            progress: ModelProgress,
        ) -> Result<Completion> {
            let result = self
                .provider
                .generate(self.request.clone(), cancel, progress)
                .await;
            self.results
                .try_send(
                    result
                        .as_ref()
                        .cloned()
                        .map_err(|error| format!("{error:#}")),
                )
                .unwrap();
            result
        }
        async fn prepare(&self, _: Invocation) -> Result<PreparedTool> {
            panic!("stream fragments must never prepare tools")
        }
        async fn execute(&self, _: PreparedTool, _: watch::Receiver<bool>) -> Result<ToolOutcome> {
            panic!("stream fragments must never execute tools")
        }
    }

    struct Harness {
        handle: Handle,
        live: watch::Receiver<Arc<LiveSnapshot>>,
        thread: ThreadId,
        results: mpsc::Receiver<CapturedResult>,
        directory: std::path::PathBuf,
    }

    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("fixture timed out")
    }

    impl Harness {
        async fn start(provider: ProviderService, request: RequestSpec) -> Self {
            let directory =
                std::env::temp_dir().join(format!("bob-provider-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            let model = provider.models()[0].id.clone();
            let (results_tx, results) = mpsc::channel(1);
            let handle = bounded(Runtime::open(
                &directory.join("kernel.db"),
                Arc::new(TestExecutor {
                    provider,
                    request,
                    results: results_tx,
                }),
            ))
            .await
            .unwrap();
            let live = handle.subscribe();
            let receipt = bounded(handle.command(Command::CreateSession {
                id: CommandId::new(),
                workspace: directory.to_string_lossy().into_owned(),
                profile: Profile {
                    model,
                    instructions: "fixture".into(),
                    read_only: false,
                    max_requests: 1,
                    reasoning: ReasoningEffort::High,
                },
                policy: Policy::default(),
            }))
            .await
            .unwrap();
            bounded(handle.command(Command::Submit {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                text: "start fixture".into(),
            }))
            .await
            .unwrap();
            Self {
                handle,
                live,
                thread: receipt.thread,
                results,
                directory,
            }
        }

        async fn attempt(&mut self, predicate: impl Fn(&LiveAttempt) -> bool) -> Arc<LiveAttempt> {
            bounded(async {
                loop {
                    let found = self
                        .live
                        .borrow_and_update()
                        .attempts
                        .iter()
                        .find(|attempt| attempt.thread == self.thread && predicate(attempt))
                        .cloned();
                    if let Some(attempt) = found {
                        return attempt;
                    }
                    self.live.changed().await.unwrap();
                }
            })
            .await
        }

        async fn result(&mut self) -> CapturedResult {
            bounded(self.results.recv())
                .await
                .expect("missing adapter result")
        }

        async fn stop(&mut self) {
            let attempt = self.attempt(|_| true).await;
            bounded(self.handle.command(Command::Stop {
                id: CommandId::new(),
                run: attempt.run.clone(),
            }))
            .await
            .unwrap();
        }

        async fn close(self) {
            bounded(self.handle.shutdown()).await.unwrap();
            std::fs::remove_dir_all(self.directory).unwrap();
        }
    }

    fn completion(text: &str) -> Completion {
        Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
            },
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 23,
                output_tokens: 17,
                ..Default::default()
            },
        }
    }

    fn assert_exact_options(leaf: &Leaf, request: &RequestSpec) {
        assert_eq!(leaf.calls.load(Ordering::SeqCst), 1);
        let options = leaf.options.lock().unwrap();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].system.as_deref(), Some(request.system.as_str()));
        assert_eq!(
            serde_json::to_value(&options[0].messages).unwrap(),
            serde_json::to_value(&request.messages).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&options[0].tools).unwrap(),
            serde_json::to_value(&request.tools).unwrap()
        );
        assert_eq!(options[0].max_tokens, Some(request.max_tokens));
        assert_eq!(options[0].reasoning, request.reasoning);
        assert_eq!(options[0].temperature, None);
        assert!(options[0].cache);
    }

    #[tokio::test]
    async fn generation_passes_exact_input_and_attempts_only_once() {
        let (provider, leaf, request, _sender) = with_leaf(Mode::StartupError);
        let mut harness = Harness::start(provider, request.clone()).await;
        assert!(harness.result().await.unwrap_err().contains("429"));
        assert_exact_options(&leaf, &request);
        harness.close().await;
    }

    #[tokio::test]
    async fn invalid_requests_do_not_reach_provider() {
        for case in 0..5 {
            let (provider, leaf, mut request, _sender) = with_leaf(Mode::Stream);
            let expected = match case {
                0 => {
                    request.system = "語".repeat(12000);
                    "context input budget"
                }
                1 => {
                    request.model.context_window += 1;
                    "advertised limit"
                }
                2 => {
                    request.model.id = "unknown".into();
                    "not selected"
                }
                3 => {
                    request.max_tokens = 2001;
                    "output limit"
                }
                _ => {
                    request.max_tokens = 0;
                    "must be positive"
                }
            };
            let mut harness = Harness::start(provider, request).await;
            assert!(harness.result().await.unwrap_err().contains(expected));
            assert_eq!(leaf.calls.load(Ordering::SeqCst), 0);
            harness.close().await;
        }
    }

    #[tokio::test]
    async fn unloaded_historical_provider_fails_only_the_requested_attempt() {
        let (provider, leaf, mut request, _sender) = with_leaf(Mode::Stream);
        let observer = provider.clone();
        request.model.id = "anthropic:historical-model".into();
        let mut harness = Harness::start(provider, request).await;
        let error = harness.result().await.unwrap_err();
        assert!(error.contains("anthropic:historical-model"));
        assert!(error.contains("not selected"));
        assert_eq!(leaf.calls.load(Ordering::SeqCst), 0);
        assert_eq!(observer.models().len(), 1);
        assert_eq!(
            observer.resolved_model_id("advertised").unwrap(),
            "copilot:advertised"
        );
        harness.close().await;
    }

    #[test]
    fn shared_estimator_accepts_unicode_and_reserves_actual_output() {
        let (provider, leaf, mut request, _sender) = with_leaf(Mode::Stream);
        request.system = "語".repeat(3000);
        let mut catalog = provider.catalog.write().unwrap();
        let binding = Arc::get_mut(&mut catalog.models[0]).unwrap();
        binding.capabilities.preferred_output_tokens = 128;
        assert!(generation_options(request.clone(), binding).is_ok());
        let input = bob_kernel::estimate_input_tokens(&request).unwrap();
        assert!(input < request.system.len());
        binding.info.context_window = input + request.max_tokens as usize;
        request.model = binding.info.clone();
        assert!(generation_options(request.clone(), binding).is_ok());
        request.max_tokens += 1;
        let error = generation_options(request, binding).unwrap_err();
        assert_eq!(failure::classify(&error), ModelFailureKind::ContextOverflow);
        assert_eq!(leaf.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn configured_startup_and_transport_idle_fail_a_single_attempt() {
        for mode in [Mode::StartupPending, Mode::Stream] {
            let (mut provider, leaf, request, sender) = with_leaf(mode);
            provider.timeouts = StreamTimeouts {
                startup: Duration::from_millis(20),
                idle: Duration::from_millis(20),
            };
            if matches!(mode, Mode::Stream) {
                sender
                    .send(StreamEvent::Error {
                        message: "SSE idle deadline exceeded".into(),
                    })
                    .await
                    .unwrap();
            }
            let mut harness = Harness::start(provider, request.clone()).await;
            let error = harness.result().await.unwrap_err();
            assert!(
                error.contains(if matches!(mode, Mode::StartupPending) {
                    "startup deadline exceeded"
                } else {
                    "idle deadline exceeded"
                }),
                "{error}"
            );
            if matches!(mode, Mode::StartupPending) {
                assert!(leaf.startup_dropped.load(Ordering::SeqCst));
            } else {
                bounded(sender.closed()).await;
            }
            assert_exact_options(&leaf, &request);
            harness.close().await;
        }
    }

    #[tokio::test]
    async fn transport_owns_idle_deadlines_not_semantic_event_spacing() {
        let (mut provider, leaf, request, sender) = with_leaf(Mode::Stream);
        provider.timeouts = StreamTimeouts {
            startup: Duration::from_secs(1),
            idle: Duration::from_millis(20),
        };
        let mut harness = Harness::start(provider, request.clone()).await;
        bounded(leaf.entered.notified()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        sender
            .send(StreamEvent::TextDelta {
                text: "waiting".into(),
            })
            .await
            .unwrap();
        harness.attempt(|attempt| attempt.text == "waiting").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        sender
            .send(StreamEvent::MessageStop {
                completion: completion("delayed startup"),
            })
            .await
            .unwrap();
        assert_eq!(
            harness.result().await.unwrap().message.text(),
            "delayed startup"
        );
        assert_exact_options(&leaf, &request);
        harness.close().await;
    }

    #[tokio::test]
    async fn large_public_reasoning_is_separate_from_opaque_final_state() {
        let (provider, leaf, request, sender) = with_leaf(Mode::Stream);
        let mut harness = Harness::start(provider, request.clone()).await;
        let public = "public thought 日本語\n".repeat(4000);
        sender
            .send(StreamEvent::ReasoningDelta {
                block_id: "large".into(),
                kind: ReasoningKind::Thinking,
                text: public.clone(),
            })
            .await
            .unwrap();
        let live = harness
            .attempt(|attempt| {
                attempt
                    .reasoning
                    .first()
                    .is_some_and(|block| block.text == public)
            })
            .await;
        assert!(live.text.is_empty());
        let mut terminal = completion("final answer");
        terminal.message.content.push(ContentBlock::ReasoningItem {
            item: json!({"type": "reasoning", "encrypted_content": "opaque secret".repeat(10_000)}),
        });
        sender
            .send(StreamEvent::MessageStop {
                completion: terminal.clone(),
            })
            .await
            .unwrap();
        let result = harness.result().await.unwrap();
        assert_eq!(
            serde_json::to_value(result.message).unwrap(),
            serde_json::to_value(terminal.message).unwrap()
        );
        let settled = harness
            .attempt(|attempt| attempt.settled_sequence.is_some())
            .await;
        assert_eq!(settled.reasoning[0].text, public);
        assert!(!settled.reasoning[0].text.contains("opaque secret"));
        assert_exact_options(&leaf, &request);
        harness.close().await;
    }

    #[tokio::test]
    async fn public_progress_precedes_authoritative_completion_without_admitting_fragments() {
        let (provider, leaf, request, sender) = with_leaf(Mode::Stream);
        let mut harness = Harness::start(provider, request.clone()).await;
        bounded(leaf.entered.notified()).await;
        let initial = harness.attempt(|_| true).await;
        assert_eq!(initial.activity, ModelActivity::Waiting);
        assert!(initial.text.is_empty());
        assert!(initial.reasoning.is_empty());

        sender
            .send(StreamEvent::TextDelta {
                text: "draft answer".into(),
            })
            .await
            .unwrap();
        let draft = harness
            .attempt(|attempt| attempt.text == "draft answer")
            .await;
        assert_eq!(draft.activity, ModelActivity::Answering);
        assert!(draft.reasoning.is_empty());
        assert!(draft.settled_sequence.is_none());
        assert!(matches!(
            harness.results.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let view = harness
            .handle
            .transcript(Some(harness.thread.clone()), None, 100)
            .await
            .unwrap();
        assert!(!view
            .entries
            .iter()
            .any(|entry| entry.kind == bob_kernel::EntryKind::Assistant));

        for (id, kind, text) in [
            ("summary", ReasoningKind::Summary, "public summary"),
            ("thinking", ReasoningKind::Thinking, "public thinking"),
        ] {
            sender
                .send(StreamEvent::ReasoningDelta {
                    block_id: id.into(),
                    kind: kind.clone(),
                    text: text.into(),
                })
                .await
                .unwrap();
            let live = harness
                .attempt(|attempt| attempt.reasoning.iter().any(|block| block.block_id == id))
                .await;
            assert_eq!(live.activity, ModelActivity::Thinking);
            assert_eq!(live.text, "draft answer");
            let block = live
                .reasoning
                .iter()
                .find(|block| block.block_id == id)
                .unwrap();
            assert_eq!(block.kind, kind);
            assert_eq!(block.text, text);
        }
        sender
            .send(StreamEvent::ToolUseStart {
                id: "partial".into(),
                name: "only_this_tool".into(),
            })
            .await
            .unwrap();
        sender
            .send(StreamEvent::ToolUseInputDelta {
                id: "partial".into(),
                partial_json: "{\"unfinished\":".into(),
            })
            .await
            .unwrap();
        let live = harness
            .attempt(|attempt| attempt.activity == ModelActivity::ToolCall)
            .await;
        assert_eq!(live.text, "draft answer");
        assert_eq!(live.reasoning.len(), 2);

        let mut final_completion = completion("authoritative final answer");
        final_completion.message.content.extend([
            ContentBlock::Thinking {
                thinking: "terminal-only thinking".into(),
                signature: "opaque signature".into(),
            },
            ContentBlock::RedactedThinking {
                data: "opaque redacted bytes".into(),
            },
            ContentBlock::ReasoningItem {
                item: json!({"type":"reasoning", "encrypted_content":"opaque encrypted bytes"}),
            },
        ]);
        sender
            .send(StreamEvent::MessageStop {
                completion: final_completion.clone(),
            })
            .await
            .unwrap();
        let result = harness.result().await.unwrap();
        assert_eq!(
            serde_json::to_value(&result.message).unwrap(),
            serde_json::to_value(&final_completion.message).unwrap()
        );
        assert_eq!(result.stop_reason, final_completion.stop_reason);
        assert_eq!(result.usage.input_tokens, 23);
        assert_eq!(result.usage.output_tokens, 17);
        let settled = harness
            .attempt(|attempt| attempt.settled_sequence.is_some())
            .await;
        assert_eq!(settled.reasoning.len(), 2);
        assert_eq!(settled.reasoning[0].text, "public summary");
        assert_eq!(settled.reasoning[1].text, "public thinking");
        let view = harness
            .handle
            .transcript(Some(harness.thread.clone()), None, 100)
            .await
            .unwrap();
        let answers: Vec<_> = view
            .entries
            .iter()
            .filter(|entry| entry.kind == bob_kernel::EntryKind::Assistant)
            .collect();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].text, "authoritative final answer");
        assert!(!view
            .entries
            .iter()
            .any(|entry| entry.kind == bob_kernel::EntryKind::ToolCall));
        assert_exact_options(&leaf, &request);
        assert!(sender.is_closed());
        harness.close().await;
    }

    #[tokio::test]
    async fn eof_and_errors_fail_after_partial_output_without_retry() {
        for error_event in [false, true] {
            let (provider, leaf, request, sender) = with_leaf(Mode::Stream);
            let mut harness = Harness::start(provider, request.clone()).await;
            sender
                .send(StreamEvent::TextDelta {
                    text: "incomplete answer".into(),
                })
                .await
                .unwrap();
            sender
                .send(StreamEvent::ToolUseStart {
                    id: "partial".into(),
                    name: "only_this_tool".into(),
                })
                .await
                .unwrap();
            sender
                .send(StreamEvent::ToolUseInputDelta {
                    id: "partial".into(),
                    partial_json: "{".into(),
                })
                .await
                .unwrap();
            harness
                .attempt(|attempt| attempt.activity == ModelActivity::ToolCall)
                .await;
            if error_event {
                sender
                    .send(StreamEvent::Error {
                        message: "HTTP 429 fixture".into(),
                    })
                    .await
                    .unwrap();
                let _ = sender
                    .send(StreamEvent::MessageStop {
                        completion: completion("must not win over error"),
                    })
                    .await;
            }
            drop(sender);
            let error = harness.result().await.unwrap_err();
            assert!(
                error.contains(if error_event {
                    "429"
                } else {
                    "before a final completion"
                }),
                "{error}"
            );
            harness
                .attempt(|attempt| attempt.settled_sequence.is_some())
                .await;
            let view = harness
                .handle
                .transcript(Some(harness.thread.clone()), None, 100)
                .await
                .unwrap();
            assert!(!view
                .entries
                .iter()
                .any(|entry| entry.kind == bob_kernel::EntryKind::Assistant
                    || entry.kind == bob_kernel::EntryKind::ToolCall));
            assert_exact_options(&leaf, &request);
            harness.close().await;
        }
    }

    #[tokio::test]
    async fn malformed_reasoning_identity_fails_the_attempt_and_drops_the_stream() {
        let (provider, leaf, request, sender) = with_leaf(Mode::Stream);
        let mut harness = Harness::start(provider, request.clone()).await;
        sender
            .send(StreamEvent::ReasoningDelta {
                block_id: "x".repeat(1025),
                kind: ReasoningKind::Summary,
                text: "visible summary".into(),
            })
            .await
            .unwrap();
        assert!(harness
            .result()
            .await
            .unwrap_err()
            .contains("reasoning block identifier exceeds"));
        bounded(sender.closed()).await;
        assert_exact_options(&leaf, &request);
        harness.close().await;
    }

    #[tokio::test]
    async fn truncated_terminal_completion_keeps_its_stop_reason_and_cannot_admit_tools() {
        let (provider, leaf, request, sender) = with_leaf(Mode::Stream);
        let mut harness = Harness::start(provider, request.clone()).await;
        let mut terminal = completion("truncated final answer");
        terminal.stop_reason = StopReason::MaxTokens;
        terminal.message.content.push(ContentBlock::ToolUse {
            id: "truncated".into(),
            name: "only_this_tool".into(),
            input: json!({}),
        });
        sender
            .send(StreamEvent::MessageStop {
                completion: terminal.clone(),
            })
            .await
            .unwrap();
        let result = harness.result().await.unwrap();
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
        assert_eq!(
            serde_json::to_value(&result.message).unwrap(),
            serde_json::to_value(&terminal.message).unwrap()
        );
        let settled = harness
            .attempt(|attempt| attempt.settled_sequence.is_some())
            .await;
        assert!(settled.error.is_some());
        let view = harness
            .handle
            .transcript(Some(harness.thread.clone()), None, 100)
            .await
            .unwrap();
        assert!(!view
            .entries
            .iter()
            .any(|entry| entry.kind == bob_kernel::EntryKind::ToolCall));
        assert_exact_options(&leaf, &request);
        harness.close().await;
    }

    #[tokio::test]
    async fn cancellation_drops_startup_and_midstream_attempts() {
        for mode in [Mode::StartupPending, Mode::Stream] {
            let (provider, leaf, request, sender) = with_leaf(mode);
            let mut harness = Harness::start(provider, request.clone()).await;
            bounded(leaf.entered.notified()).await;
            if matches!(mode, Mode::Stream) {
                sender
                    .send(StreamEvent::TextDelta {
                        text: "before cancel".into(),
                    })
                    .await
                    .unwrap();
                harness
                    .attempt(|attempt| attempt.text == "before cancel")
                    .await;
            }
            harness.stop().await;
            assert!(harness.result().await.unwrap_err().contains("cancelled"));
            if matches!(mode, Mode::StartupPending) {
                assert!(leaf.startup_dropped.load(Ordering::SeqCst));
            } else {
                bounded(sender.closed()).await;
            }
            assert_exact_options(&leaf, &request);
            harness.close().await;
        }
    }
}
