//! Token cost calculation.

use crate::types::{Cost, Model, ModelCost, Usage};

/// Compute dollar costs for `usage` using the model's pricing, writing into `usage.cost`.
pub fn calculate_cost(model: &Model, usage: &mut Usage) -> Cost {
    let rates = select_rates(&model.cost, usage);
    let cost = Cost {
        input: (rates.input / 1_000_000.0) * usage.input as f64,
        output: (rates.output / 1_000_000.0) * usage.output as f64,
        cache_read: (rates.cache_read / 1_000_000.0) * usage.cache_read as f64,
        cache_write: (rates.cache_write / 1_000_000.0) * usage.cache_write as f64,
        total: 0.0,
    };
    let mut cost = cost;
    // Anthropic-style 1h cache writes charged at 2× input rate when present.
    if let Some(cache_write_1h) = usage.cache_write_1h {
        cost.cache_write += (rates.input * 2.0 / 1_000_000.0) * cache_write_1h as f64;
    }
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    usage.cost = cost;
    cost
}

struct Rates {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

fn select_rates(cost: &ModelCost, usage: &Usage) -> Rates {
    let total_input = usage.input + usage.cache_read + usage.cache_write;
    let mut rates = Rates {
        input: cost.input,
        output: cost.output,
        cache_read: cost.cache_read,
        cache_write: cost.cache_write,
    };
    if let Some(tiers) = &cost.tiers {
        let mut best_above = 0u64;
        for tier in tiers {
            if total_input > tier.input_tokens_above && tier.input_tokens_above >= best_above {
                best_above = tier.input_tokens_above;
                rates = Rates {
                    input: tier.input,
                    output: tier.output,
                    cache_read: tier.cache_read,
                    cache_write: tier.cache_write,
                };
            }
        }
    }
    rates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InputModality, ModelCostTier};

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
}
