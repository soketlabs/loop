//! Token / context stats for the chat composer footer.

use loop_agent::types::AgentMessage;
use loop_ai::{calculate_context_tokens, Message, Usage};
use loop_app_core::Runtime;

/// Live stats shown in the composer footer row.
#[derive(Debug, Clone, Default)]
pub struct ComposerStats {
    pub total_tokens: u64,
    pub context_tokens: Option<u64>,
    pub context_window: u64,
    pub estimated_cost: f64,
}

impl ComposerStats {
    pub fn from_runtime(runtime: &Runtime) -> Self {
        let context_window = runtime
            .models
            .get_model(
                &runtime.settings.default_provider,
                &runtime.settings.default_model,
            )
            .map(|m| m.context_window)
            .unwrap_or(0);
        Self {
            context_window,
            ..Default::default()
        }
    }

    pub async fn refresh(&mut self, runtime: &Runtime) {
        if let Some(m) = runtime.models.get_model(
            &runtime.settings.default_provider,
            &runtime.settings.default_model,
        ) {
            self.context_window = m.context_window;
        }
        if let Ok(stats) = runtime.harness.session_stats().await {
            self.total_tokens = stats.tokens.total_tokens();
            if let Some(ctx) = stats.context_usage {
                self.context_tokens = ctx.tokens;
                self.context_window = ctx.context_window;
            }
        }
    }

    pub fn apply_message(&mut self, message: &AgentMessage) {
        match message {
            AgentMessage::Llm(Message::Assistant(a)) => self.apply_usage(&a.usage),
            AgentMessage::Llm(Message::ToolResult(t)) => {
                if let Some(usage) = &t.usage {
                    self.apply_usage(usage);
                }
            }
            _ => {}
        }
    }

    fn apply_usage(&mut self, usage: &Usage) {
        self.total_tokens = self
            .total_tokens
            .saturating_add(usage.input)
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
        let ctx = calculate_context_tokens(usage);
        if ctx > 0 {
            self.context_tokens = Some(ctx);
        }
    }

    pub fn summary_line(&self) -> String {
        let ctx = self
            .context_tokens
            .map(|t| format!("{t}"))
            .unwrap_or_else(|| "—".into());
        let pct = match (self.context_tokens, self.context_window) {
            (Some(t), w) if w > 0 => format!(" {:.0}%", (t as f64 / w as f64) * 100.0),
            _ => String::new(),
        };
        format!(
            "ctx {ctx}/{}{pct} · tokens {}",
            self.context_window, self.total_tokens
        )
    }
}
