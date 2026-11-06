use super::*;

fn options(provider: Option<&str>, model: Option<&str>) -> LaunchOptions {
    LaunchOptions {
        database: "unused.sqlite".into(),
        workspace: ".".into(),
        provider: provider.map(str::to_owned),
        model: model.map(str::to_owned),
        worker_model: None,
        selection: SessionSelection::Latest,
        page_size: 80,
    }
}

fn profile(model: &str, reasoning: ReasoningEffort) -> Profile {
    Profile {
        model: model.into(),
        instructions: "restored instructions".into(),
        read_only: true,
        max_requests: 12,
        reasoning,
    }
}

#[test]
fn resume_resolves_model_and_reasoning_precedence_independently() {
    let config = BobConfig {
        provider: "copilot".into(),
        model: "configured".into(),
        reasoning: "high".into(),
        ..Default::default()
    };
    for provider in ["anthropic", "openai"] {
        let restored = profile(&format!("{provider}:restored"), ReasoningEffort::Off);
        for (explicit_provider, explicit_model, expected) in [
            (None, None, format!("{provider}:restored")),
            (None, Some("override"), format!("{provider}:override")),
            (None, Some("openai/explicit"), "openai/explicit".into()),
            (
                None,
                Some("anthropic:explicit"),
                "anthropic:explicit".into(),
            ),
            (Some("copilot"), Some("override"), "copilot:override".into()),
            (Some("copilot:embedded"), None, "copilot:embedded".into()),
        ] {
            assert_eq!(
                launch_model_spec(
                    &options(explicit_provider, explicit_model),
                    &config,
                    Some(&restored)
                ),
                expected
            );
            assert_eq!(
                launch_reasoning(&config, Some(&restored)).unwrap(),
                ReasoningEffort::Off
            );
        }
    }
    assert_eq!(
        launch_model_spec(&options(None, None), &config, None),
        "copilot:configured"
    );
    assert_eq!(
        launch_reasoning(&config, None).unwrap(),
        ReasoningEffort::High
    );
}

#[test]
fn provider_only_override_does_not_borrow_another_providers_model() {
    let config = BobConfig {
        provider: "copilot:embedded".into(),
        model: "configured".into(),
        ..Default::default()
    };
    let restored = profile("anthropic:restored", ReasoningEffort::Low);
    assert_eq!(
        launch_model_spec(&options(Some("openai"), None), &config, None),
        "openai"
    );
    assert_eq!(
        launch_model_spec(&options(Some("openai"), None), &config, Some(&restored)),
        "openai"
    );
    assert_eq!(
        launch_model_spec(&options(Some("anthropic"), None), &config, Some(&restored)),
        "anthropic:restored"
    );
    assert_eq!(
        launch_model_spec(
            &options(Some("openai/embedded"), None),
            &config,
            Some(&restored)
        ),
        "openai:embedded"
    );
    assert_eq!(
        launch_model_spec(&options(Some("copilot"), None), &config, Some(&restored)),
        "copilot:configured"
    );
    assert_eq!(
        launch_model_spec(
            &options(Some("openai"), Some("anthropic/explicit")),
            &config,
            Some(&restored)
        ),
        "anthropic/explicit"
    );
}

#[test]
fn restored_reasoning_including_off_wins_even_over_invalid_config() {
    let mut config = BobConfig {
        reasoning: "invalid".into(),
        ..Default::default()
    };
    for effort in [
        ReasoningEffort::Off,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Max,
    ] {
        assert_eq!(
            launch_reasoning(&config, Some(&profile("openai:restored", effort))).unwrap(),
            effort
        );
    }
    assert!(launch_reasoning(&config, None).is_err());
    config.reasoning.clear();
    assert_eq!(
        launch_reasoning(&config, None).unwrap(),
        ReasoningEffort::Off
    );
}

#[test]
fn interactive_model_qualification_uses_current_provider_but_preserves_explicit_choices() {
    for current in ["copilot:old", "anthropic:old", "openai:old"] {
        let provider = current.split_once(':').unwrap().0;
        assert_eq!(
            crate::provider::qualify_model_id("next", current).unwrap(),
            format!("{provider}:next")
        );
        assert_eq!(
            crate::provider::qualify_model_id("vendor/model", current).unwrap(),
            format!("{provider}:vendor/model")
        );
        assert_eq!(
            crate::provider::qualify_model_id("openai/explicit", current).unwrap(),
            "openai:explicit"
        );
        assert_eq!(
            crate::provider::qualify_model_id("anthropic:explicit", current).unwrap(),
            "anthropic:explicit"
        );
        assert_eq!(
            crate::provider::qualify_model_id("copilot", current).unwrap(),
            "copilot"
        );
        assert!(crate::provider::qualify_model_id("unknown:explicit", current).is_err());
        assert!(crate::provider::qualify_model_id("bad model", current).is_err());
    }
}
