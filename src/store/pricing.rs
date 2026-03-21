/// Token usage for an agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total_tokens(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Per-1M token pricing in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}

impl ModelPricing {
    pub fn estimate_cost_usd(self, usage: TokenUsage) -> CostEstimate {
        let input_cost = (usage.input_tokens as f64 / 1_000_000.0) * self.input_per_million_usd;
        let output_cost = (usage.output_tokens as f64 / 1_000_000.0) * self.output_per_million_usd;
        CostEstimate {
            input_cost_usd: input_cost,
            output_cost_usd: output_cost,
            total_cost_usd: input_cost + output_cost,
        }
    }
}

/// Cost estimate in USD.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CostEstimate {
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
    pub total_cost_usd: f64,
}
/// Resolve model pricing using a built-in table and normalized model aliases.
pub fn pricing_for_model(model: &str) -> ModelPricing {
    let normalized = model.trim().to_lowercase();
    // OpenAI models
    if normalized == "o3" {
        return ModelPricing {
            input_per_million_usd: 2.0,
            output_per_million_usd: 8.0,
        };
    }
    if normalized == "o4-mini" || normalized.contains("gpt-4.1-mini") {
        return ModelPricing {
            input_per_million_usd: 0.15,
            output_per_million_usd: 0.6,
        };
    }
    if normalized.contains("gpt-4.1") && !normalized.contains("mini") {
        return ModelPricing {
            input_per_million_usd: 2.0,
            output_per_million_usd: 8.0,
        };
    }
    if normalized.contains("opus") {
        return ModelPricing {
            input_per_million_usd: 15.0,
            output_per_million_usd: 75.0,
        };
    }
    if normalized.contains("sonnet") {
        return ModelPricing {
            input_per_million_usd: 3.0,
            output_per_million_usd: 15.0,
        };
    }
    if normalized.contains("haiku") {
        return ModelPricing {
            input_per_million_usd: 0.8,
            output_per_million_usd: 4.0,
        };
    }

    // Fallback baseline when model is unknown.
    ModelPricing {
        input_per_million_usd: 1.0,
        output_per_million_usd: 4.0,
    }
}
