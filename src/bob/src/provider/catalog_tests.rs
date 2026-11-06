use super::*;
use async_trait::async_trait;
use bob_core::auth::copilot::ModelLimits;
use bob_core::providers::provider::{ModelEntry, Provider};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

struct CatalogLeaf {
    model: String,
    catalog: Vec<ModelEntry>,
    lists: Arc<AtomicUsize>,
    fail_list: bool,
    pending_list: bool,
}

#[async_trait]
impl Provider for CatalogLeaf {
    fn name(&self) -> &str {
        "fixture"
    }
    fn model(&self) -> &str {
        &self.model
    }
    async fn generate(&self, _: GenerateOptions) -> Result<Completion> {
        panic!("catalog must not generate")
    }
    async fn stream(&self, _: GenerateOptions) -> Result<mpsc::Receiver<StreamEvent>> {
        panic!("catalog must not stream")
    }
    fn context_window(&self) -> usize {
        98_765
    }
    async fn list_models_detailed(&self) -> Result<Vec<ModelEntry>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        ensure!(!self.fail_list, "enumeration unavailable");
        if self.pending_list {
            std::future::pending::<()>().await;
        }
        Ok(self.catalog.clone())
    }
}

#[derive(Default)]
struct Factory {
    calls: Mutex<Vec<(String, StreamTimeouts)>>,
    lists: Arc<AtomicUsize>,
    copilot_calls: AtomicUsize,
    fail: AtomicBool,
    change_model: AtomicBool,
    empty_catalog: bool,
    fail_list: bool,
    pending_list: bool,
    pause_create: AtomicBool,
    create_entered: Notify,
    create_release: Notify,
}

fn limits(id: &str) -> ModelLimits {
    ModelLimits {
        id: id.into(),
        max_context_window_tokens: Some(128_000),
        max_prompt_tokens: Some(120_000),
        max_output_tokens: Some(8000),
    }
}

#[async_trait]
impl ProviderFactory for Factory {
    async fn create(&self, spec: &str, timeouts: StreamTimeouts) -> Result<Arc<dyn Provider>> {
        self.calls.lock().unwrap().push((spec.to_owned(), timeouts));
        if self.pause_create.load(Ordering::SeqCst) {
            self.create_entered.notify_one();
            self.create_release.notified().await;
        }
        ensure!(!self.fail.load(Ordering::SeqCst), "HTTP 429 fixture");
        let selection = Selection::parse(spec)?;
        let model = if self.change_model.load(Ordering::SeqCst) {
            "wrong-model".to_owned()
        } else if selection.model.is_empty() {
            "factory-default".to_owned()
        } else {
            selection.model
        };
        Ok(Arc::new(CatalogLeaf {
            model,
            catalog: if self.empty_catalog {
                Vec::new()
            } else {
                ["shared", "alternate", "factory-default"]
                    .into_iter()
                    .map(|id| ModelEntry {
                        id: id.into(),
                        context_window: Some(123_456),
                    })
                    .collect()
            },
            lists: self.lists.clone(),
            fail_list: self.fail_list,
            pending_list: self.pending_list,
        }))
    }

    async fn copilot_limits(&self) -> Result<Vec<ModelLimits>> {
        self.copilot_calls.fetch_add(1, Ordering::SeqCst);
        if self.empty_catalog {
            return Ok(Vec::new());
        }
        Ok(vec![
            limits("shared"),
            limits("alternate"),
            limits("factory-default"),
            ModelLimits {
                id: "embedding-without-limits".into(),
                ..Default::default()
            },
        ])
    }
}

fn timeouts() -> StreamTimeouts {
    StreamTimeouts {
        startup: Duration::from_secs(7),
        idle: Duration::from_secs(11),
    }
}

async fn create(factory: Arc<Factory>, models: &[&str]) -> ProviderService {
    ProviderService::create_with_factory(
        &models.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>(),
        timeouts(),
        factory,
    )
    .await
    .unwrap()
}

#[test]
fn normalization_never_infers_a_paid_provider_from_a_model_name() {
    for (input, expected) in [
        ("gpt-5", "copilot:gpt-5"),
        ("claude-sonnet-5", "copilot:claude-sonnet-5"),
        ("copilot:m", "copilot:m"),
        ("copilot/m", "copilot:m"),
        ("openai:m", "openai:m"),
        ("anthropic/m", "anthropic:m"),
        (
            "copilot:vendor/model:preview",
            "copilot:vendor/model:preview",
        ),
        ("copilot", "copilot"),
        ("openai", "openai"),
        ("anthropic", "anthropic"),
    ] {
        assert_eq!(normalize_model_id(input).unwrap(), expected);
    }
    for invalid in [
        "",
        " m",
        "m ",
        "copilot:",
        "openai/",
        "copilot:m\n",
        "unknown:m",
        "other/m",
    ] {
        assert!(normalize_model_id(invalid).is_err(), "{invalid:?}");
    }
    let selections = catalog::requested_models(&[
        "m".into(),
        "copilot/m".into(),
        "copilot:m".into(),
        "openai:m".into(),
    ])
    .unwrap();
    assert_eq!(
        selections.iter().map(Selection::id).collect::<Vec<_>>(),
        ["copilot:m", "openai:m"]
    );
}

#[tokio::test]
async fn full_copilot_catalog_is_lazy_and_does_not_touch_paid_auth() {
    let factory = Arc::new(Factory::default());
    let service = create(factory.clone(), &["shared", "alternate"]).await;
    assert_eq!(
        service
            .models()
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        [
            "copilot:shared",
            "copilot:alternate",
            "copilot:factory-default"
        ]
    );
    assert_eq!(
        factory
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(spec, _)| spec.as_str())
            .collect::<Vec<_>>(),
        ["copilot/shared"]
    );
    assert_eq!(factory.lists.load(Ordering::SeqCst), 0);
    assert_eq!(factory.copilot_calls.load(Ordering::SeqCst), 1);
    assert!(service.resolved_model_id("openai:shared").is_err());
    assert!(service
        .resolved_model_id("embedding-without-limits")
        .is_err());
    assert_eq!(
        service.capabilities("shared").max_input_tokens,
        Some(120_000)
    );
    let switched = service.binding("alternate").unwrap();
    let (first, second) = tokio::join!(
        switched.provider(factory.as_ref(), timeouts()),
        switched.provider(factory.as_ref(), timeouts()),
    );
    assert!(Arc::ptr_eq(first.unwrap(), second.unwrap()));
    assert_eq!(switched.provider.get().unwrap().model(), "alternate");
    assert_eq!(factory.calls.lock().unwrap().len(), 2);
    for (spec, configured) in factory.calls.lock().unwrap().iter() {
        assert!(spec.starts_with("copilot/"));
        assert_eq!(configured.startup, timeouts().startup);
        assert_eq!(configured.idle, timeouts().idle);
    }
}

#[tokio::test]
async fn explicit_worker_provider_gets_its_own_catalog_and_identity() {
    let factory = Arc::new(Factory::default());
    let service = create(factory.clone(), &["shared", "anthropic/shared"]).await;
    assert_eq!(service.models()[0].id, "copilot:shared");
    assert_eq!(service.models()[1].id, "anthropic:shared");
    assert_eq!(service.models().len(), 6);
    assert_eq!(service.models()[1].context_window, 123_456);
    assert_eq!(
        service.resolved_model_id("shared").unwrap(),
        "copilot:shared"
    );
    assert_eq!(
        service.resolved_model_id("anthropic:alternate").unwrap(),
        "anthropic:alternate"
    );
    assert!(service.resolved_model_id("openai:shared").is_err());
    assert_eq!(
        factory
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(spec, _)| spec.as_str())
            .collect::<Vec<_>>(),
        ["copilot/shared", "anthropic/shared"]
    );
    assert_eq!(factory.lists.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_only_defaults_come_from_factory_and_catalog() {
    for kind in ["copilot", "openai", "anthropic"] {
        let factory = Arc::new(Factory::default());
        let service = create(factory.clone(), &[kind]).await;
        let expected = format!("{kind}:factory-default");
        assert_eq!(service.resolved_model_id(kind).unwrap(), expected);
        assert_eq!(service.models()[0].id, expected);
        assert_eq!(factory.calls.lock().unwrap()[0].0, kind);
        assert_eq!(
            factory.copilot_calls.load(Ordering::SeqCst),
            usize::from(kind == "copilot")
        );
        assert!(service
            .models()
            .iter()
            .all(|m| m.id.starts_with(&format!("{kind}:"))));
    }
}

#[tokio::test]
async fn failed_copilot_discovery_never_falls_back_to_paid_catalogs() {
    for empty in [false, true] {
        let factory = Arc::new(Factory {
            empty_catalog: empty,
            ..Default::default()
        });
        let result =
            ProviderService::create_with_factory(&["unknown".into()], timeouts(), factory.clone())
                .await;
        let error = result
            .err()
            .expect("unadvertised model must fail")
            .to_string();
        assert!(error.contains("unsupported copilot model"), "{error}");
        assert_eq!(factory.calls.lock().unwrap().len(), 1);
        assert_eq!(factory.calls.lock().unwrap()[0].0, "copilot/unknown");
        assert_eq!(factory.lists.load(Ordering::SeqCst), 0);
    }
    let factory = Arc::new(Factory {
        fail: AtomicBool::new(true),
        ..Default::default()
    });
    assert!(
        ProviderService::create_with_factory(&["shared".into()], timeouts(), factory.clone())
            .await
            .is_err()
    );
    assert_eq!(factory.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn empty_openai_catalog_uses_normal_responses_picker_without_copilot_discovery() {
    let factory = Arc::new(Factory {
        empty_catalog: true,
        ..Default::default()
    });
    let service = create(factory.clone(), &["openai"]).await;
    assert_eq!(service.models()[0].id, "openai:factory-default");
    for id in bob_core::providers::RESPONSES_MODELS {
        assert_eq!(
            service.resolved_model_id(&format!("openai:{id}")).unwrap(),
            format!("openai:{id}")
        );
    }
    assert_eq!(factory.copilot_calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn lazy_factory_failure_is_not_cached_or_retried_by_adapter() {
    let factory = Arc::new(Factory::default());
    let service = create(factory.clone(), &["shared"]).await;
    let switched = service.binding("copilot:alternate").unwrap();
    factory.fail.store(true, Ordering::SeqCst);
    let before = snapshot(&service);
    let error = service.ensure_model("alternate").await.unwrap_err();
    assert_eq!(snapshot(&service), before);
    assert_eq!(service.classify_error(&error), ModelFailureKind::Transient);
    assert_eq!(factory.calls.lock().unwrap().len(), 2);
    assert!(switched.provider.get().is_none());
    factory.fail.store(false, Ordering::SeqCst);
    factory.change_model.store(true, Ordering::SeqCst);
    assert!(switched
        .provider(factory.as_ref(), timeouts())
        .await
        .is_err());
    assert!(switched.provider.get().is_none());
    assert_eq!(factory.calls.lock().unwrap().len(), 3);
    factory.change_model.store(false, Ordering::SeqCst);
    assert_eq!(
        switched
            .provider(factory.as_ref(), timeouts())
            .await
            .unwrap()
            .model(),
        "alternate"
    );
    assert_eq!(factory.calls.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn enumeration_outage_keeps_explicit_normal_model_usable() {
    for kind in ["openai", "anthropic"] {
        let factory = Arc::new(Factory {
            fail_list: true,
            ..Default::default()
        });
        let requested = format!("{kind}:custom-model");
        let service = create(factory.clone(), &[&requested]).await;
        assert_eq!(service.models()[0].id, requested);
        assert_eq!(service.models()[0].context_window, 98_765);
        assert_eq!(service.ensure_model(&requested).await.unwrap(), requested);
        assert_eq!(factory.calls.lock().unwrap().len(), 1);
        assert_eq!(factory.lists.load(Ordering::SeqCst), 1);
        assert_eq!(factory.copilot_calls.load(Ordering::SeqCst), 0);
        assert!(service.models().iter().all(|m| m.id.starts_with(kind)));
    }
}

#[tokio::test]
async fn stalled_optional_enumeration_has_a_bounded_best_effort_budget() {
    let factory = Arc::new(Factory {
        pending_list: true,
        ..Default::default()
    });
    let service = ProviderService::create_with_factory(
        &["anthropic:custom-model".into()],
        StreamTimeouts {
            startup: Duration::from_millis(100),
            ..timeouts()
        },
        factory.clone(),
    )
    .await
    .unwrap();
    assert_eq!(service.models()[0].context_window, 98_765);
    assert_eq!(factory.calls.lock().unwrap().len(), 1);
    assert_eq!(factory.copilot_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_switch_discovers_only_requested_provider_and_shares_snapshot() {
    let factory = Arc::new(Factory::default());
    let service = create(factory.clone(), &["shared"]).await;
    let observer = service.clone();
    assert!(observer.resolved_model_id("anthropic:shared").is_err());
    assert_eq!(factory.calls.lock().unwrap().len(), 1);
    assert_eq!(
        service.ensure_model("anthropic/shared").await.unwrap(),
        "anthropic:shared"
    );
    assert_eq!(
        observer.resolved_model_id("anthropic:shared").unwrap(),
        "anthropic:shared"
    );
    assert_eq!(observer.models().len(), 6);
    assert_eq!(
        observer.capabilities("anthropic:shared").max_input_tokens,
        Some(123_456)
    );
    assert!(observer.resolved_model_id("openai:shared").is_err());
    assert_eq!(
        factory
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(spec, _)| spec.clone())
            .collect::<Vec<_>>(),
        ["copilot/shared", "anthropic/shared"]
    );
    assert_eq!(factory.copilot_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.lists.load(Ordering::SeqCst), 1);
    assert_eq!(
        service.ensure_model("anthropic:alternate").await.unwrap(),
        "anthropic:alternate"
    );
    assert_eq!(factory.calls.lock().unwrap().len(), 3);
    assert_eq!(factory.calls.lock().unwrap()[2].0, "anthropic/alternate");
    assert_eq!(factory.lists.load(Ordering::SeqCst), 1);
}

fn snapshot(service: &ProviderService) -> Vec<(String, usize, u32)> {
    service
        .models()
        .into_iter()
        .map(|model| {
            let output = service.capabilities(&model.id).max_output_tokens;
            (model.id, model.context_window, output)
        })
        .collect()
}

#[tokio::test]
async fn failed_explicit_switches_preserve_catalog_and_can_be_retried_explicitly() {
    let factory = Arc::new(Factory::default());
    let service = create(factory.clone(), &["copilot"]).await;
    let before = snapshot(&service);
    factory.fail.store(true, Ordering::SeqCst);
    assert!(service.ensure_model("openai:shared").await.is_err());
    assert_eq!(snapshot(&service), before);
    assert_eq!(
        service.resolved_model_id("copilot").unwrap(),
        "copilot:factory-default"
    );
    assert!(service.resolved_model_id("openai:shared").is_err());
    factory.fail.store(false, Ordering::SeqCst);
    factory.change_model.store(true, Ordering::SeqCst);
    assert!(service.ensure_model("openai:shared").await.is_err());
    assert_eq!(snapshot(&service), before);
    factory.change_model.store(false, Ordering::SeqCst);
    assert!(service.ensure_model("copilot:unadvertised").await.is_err());
    assert_eq!(snapshot(&service), before);
    assert!(service.ensure_model("other:model").await.is_err());
    assert_eq!(factory.calls.lock().unwrap().len(), 4);
    assert_eq!(
        service.ensure_model("openai:shared").await.unwrap(),
        "openai:shared"
    );
    assert_eq!(factory.calls.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn copilot_discovery_outage_never_uses_normal_model_limits() {
    let factory = Arc::new(Factory {
        empty_catalog: true,
        ..Default::default()
    });
    let service = create(factory.clone(), &["anthropic:shared"]).await;
    let before = snapshot(&service);
    assert!(service.ensure_model("gpt-5").await.is_err());
    assert_eq!(snapshot(&service), before);
    assert_eq!(
        factory
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(spec, _)| spec.clone())
            .collect::<Vec<_>>(),
        ["anthropic/shared", "copilot/gpt-5"]
    );
    assert_eq!(factory.lists.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_explicit_selection_initializes_provider_and_requested_model_once() {
    let factory = Arc::new(Factory::default());
    let service = create(factory.clone(), &["shared"]).await;
    for (first_id, second_id, count) in [
        ("anthropic/shared", "anthropic:shared", 2),
        ("anthropic/alternate", "anthropic:alternate", 3),
    ] {
        let before = snapshot(&service);
        factory.pause_create.store(true, Ordering::SeqCst);
        let first_service = service.clone();
        let first = tokio::spawn(async move { first_service.ensure_model(first_id).await });
        tokio::time::timeout(Duration::from_secs(2), factory.create_entered.notified())
            .await
            .unwrap();
        let second_service = service.clone();
        let second = tokio::spawn(async move { second_service.ensure_model(second_id).await });
        assert_eq!(snapshot(&service), before);
        assert_eq!(factory.calls.lock().unwrap().len(), count);
        factory.pause_create.store(false, Ordering::SeqCst);
        factory.create_release.notify_one();
        let (first, second) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(first, second)
        })
        .await
        .unwrap();
        assert_eq!(first.unwrap().unwrap(), second_id);
        assert_eq!(second.unwrap().unwrap(), second_id);
        assert_eq!(factory.calls.lock().unwrap().len(), count);
    }
    assert_eq!(factory.lists.load(Ordering::SeqCst), 1);
    assert_eq!(factory.copilot_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn model_discovery_timeout_leaves_snapshot_and_gate_usable() {
    let factory = Arc::new(Factory::default());
    let mut service = create(factory.clone(), &["shared"]).await;
    service.timeouts.startup = Duration::from_millis(30);
    let before = snapshot(&service);
    factory.pause_create.store(true, Ordering::SeqCst);
    let error = service.ensure_model("openai:shared").await.unwrap_err();
    assert!(error
        .to_string()
        .contains("model discovery deadline exceeded"));
    assert_eq!(snapshot(&service), before);
    assert_eq!(factory.calls.lock().unwrap().len(), 2);
    factory.pause_create.store(false, Ordering::SeqCst);
    assert_eq!(
        service.ensure_model("openai:shared").await.unwrap(),
        "openai:shared"
    );
    assert_eq!(factory.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn cancelled_model_discovery_leaves_snapshot_and_gate_usable() {
    let factory = Arc::new(Factory::default());
    let service = create(factory.clone(), &["shared"]).await;
    let before = snapshot(&service);
    factory.pause_create.store(true, Ordering::SeqCst);
    let loader = service.clone();
    let task = tokio::spawn(async move { loader.ensure_model("anthropic:shared").await });
    tokio::time::timeout(Duration::from_secs(2), factory.create_entered.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(snapshot(&service), before);
    factory.pause_create.store(false, Ordering::SeqCst);
    assert_eq!(
        service.ensure_model("anthropic:shared").await.unwrap(),
        "anthropic:shared"
    );
    assert_eq!(factory.calls.lock().unwrap().len(), 3);
}

#[test]
fn typed_adapter_failures_and_existing_core_predicates_drive_classification() {
    for (message, expected) in [
        ("HTTP 429 fixture", ModelFailureKind::Transient),
        ("HTTP 503 unavailable", ModelFailureKind::Transient),
        (
            "provider stream ended before a final completion",
            ModelFailureKind::Transient,
        ),
        ("context_length_exceeded", ModelFailureKind::ContextOverflow),
        ("HTTP 401 unauthorized", ModelFailureKind::Fatal),
        ("malformed SSE JSON", ModelFailureKind::Fatal),
        ("JSON column 429", ModelFailureKind::Fatal),
        ("provider resource limit exceeded", ModelFailureKind::Fatal),
        ("generation cancelled", ModelFailureKind::Fatal),
    ] {
        assert_eq!(
            failure::classify(&anyhow::anyhow!(message)),
            expected,
            "{message}"
        );
    }
    let invalid_binding = anyhow::anyhow!("context window mismatch").context(AdapterFailure {
        kind: ModelFailureKind::Fatal,
        message: "invalid provider request".into(),
    });
    assert_eq!(failure::classify(&invalid_binding), ModelFailureKind::Fatal);
    assert_eq!(
        failure::classify(&failure::timeout("startup")),
        ModelFailureKind::Transient
    );
}
