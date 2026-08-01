//! Cost calculation and OpenAI-compat detection tests.

use loop_ai::{
    calculate_cost, detect_compat, resolve_compat, InputModality, MaxTokensField, Model, ModelCost,
    ModelCostTier, OpenAICompletionsCompat, Usage,
};

fn model_with_cost(cost: ModelCost) -> Model {
    Model {
        id: "m".into(),
        name: "m".into(),
        api: "openai-completions".into(),
        provider: "p".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost,
        context_window: 128_000,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

#[test]
fn calculates_basic_cost() {
    let model = model_with_cost(ModelCost {
        input: 1.0,
        output: 2.0,
        cache_read: 0.5,
        cache_write: 1.5,
        tiers: None,
    });
    let mut usage = Usage {
        input: 1_000_000,
        output: 1_000_000,
        cache_read: 0,
        cache_write: 0,
        ..Usage::empty()
    };
    let cost = calculate_cost(&model, &mut usage);
    assert!((cost.input - 1.0).abs() < 1e-9);
    assert!((cost.output - 2.0).abs() < 1e-9);
    assert!((cost.total - 3.0).abs() < 1e-9);
    assert!((usage.cost.total - 3.0).abs() < 1e-9);
}

#[test]
fn selects_highest_matching_tier() {
    let model = model_with_cost(ModelCost {
        input: 1.0,
        output: 1.0,
        cache_read: 0.0,
        cache_write: 0.0,
        tiers: Some(vec![
            ModelCostTier {
                input_tokens_above: 100,
                input: 0.5,
                output: 0.5,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            ModelCostTier {
                input_tokens_above: 1000,
                input: 0.1,
                output: 0.1,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        ]),
    });
    let mut usage = Usage {
        input: 1500,
        output: 0,
        ..Usage::empty()
    };
    let cost = calculate_cost(&model, &mut usage);
    assert!((cost.input - 1500.0 * 0.1 / 1_000_000.0).abs() < 1e-12);
}

#[test]
fn localhost_compat_defaults() {
    let c = detect_compat("http://localhost:11434/v1");
    assert_eq!(c.supports_store, Some(false));
    assert_eq!(c.supports_developer_role, Some(false));
    assert_eq!(c.supports_reasoning_effort, Some(false));
    assert_eq!(c.max_tokens_field, Some(MaxTokensField::MaxTokens));
}

#[test]
fn openai_cloud_compat_defaults() {
    let c = detect_compat("https://api.openai.com/v1");
    assert_eq!(c.supports_store, Some(true));
    assert_eq!(c.supports_developer_role, Some(true));
    assert_eq!(c.supports_reasoning_effort, Some(true));
    assert_eq!(
        c.max_tokens_field,
        Some(MaxTokensField::MaxCompletionTokens)
    );
}

#[test]
fn explicit_compat_overrides_detection() {
    let resolved = resolve_compat(
        "http://127.0.0.1:8080/v1",
        Some(&OpenAICompletionsCompat {
            supports_store: Some(true),
            supports_developer_role: Some(true),
            ..Default::default()
        }),
    );
    assert!(resolved.supports_store);
    assert!(resolved.supports_developer_role);
    // Unspecified fields still come from localhost detection.
    assert!(!resolved.supports_reasoning_effort);
}
