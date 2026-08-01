//! Auto-detect OpenAI Completions compatibility flags from a base URL.

use crate::types::{
    MaxTokensField, OpenAICompletionsCompat, ResolvedOpenAICompletionsCompat, ThinkingFormat,
};

/// Infer sensible compat defaults from `base_url`.
pub fn detect_compat(base_url: &str) -> OpenAICompletionsCompat {
    let url = base_url.to_lowercase();
    let is_local = url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("0.0.0.0")
        || url.contains("[::1]");
    let is_openai = url.contains("api.openai.com");
    let is_openrouter = url.contains("openrouter.ai");
    let is_deepseek = url.contains("deepseek");
    let is_together = url.contains("together");
    let is_groq = url.contains("groq.com");

    if is_local {
        return OpenAICompletionsCompat {
            supports_store: Some(false),
            supports_developer_role: Some(false),
            supports_reasoning_effort: Some(false),
            supports_usage_in_streaming: Some(true),
            supports_finish_reason: Some(true),
            max_tokens_field: Some(MaxTokensField::MaxTokens),
            requires_tool_result_name: Some(false),
            requires_assistant_after_tool_result: Some(false),
            requires_thinking_as_text: Some(false),
            thinking_format: Some(ThinkingFormat::Openai),
            session_affinity_format: None,
        };
    }

    if is_openai {
        return OpenAICompletionsCompat {
            supports_store: Some(true),
            supports_developer_role: Some(true),
            supports_reasoning_effort: Some(true),
            supports_usage_in_streaming: Some(true),
            supports_finish_reason: Some(true),
            max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
            requires_tool_result_name: Some(false),
            requires_assistant_after_tool_result: Some(false),
            requires_thinking_as_text: Some(false),
            thinking_format: Some(ThinkingFormat::Openai),
            session_affinity_format: None,
        };
    }

    let thinking_format = if is_openrouter {
        ThinkingFormat::Openrouter
    } else if is_deepseek {
        ThinkingFormat::Deepseek
    } else if is_together {
        ThinkingFormat::Together
    } else {
        ThinkingFormat::Openai
    };

    OpenAICompletionsCompat {
        supports_store: Some(false),
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(is_openai || is_openrouter || is_deepseek),
        supports_usage_in_streaming: Some(true),
        supports_finish_reason: Some(true),
        max_tokens_field: Some(if is_groq {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        }),
        requires_tool_result_name: Some(false),
        requires_assistant_after_tool_result: Some(false),
        requires_thinking_as_text: Some(false),
        thinking_format: Some(thinking_format),
        session_affinity_format: None,
    }
}

/// Merge URL-detected defaults with explicit model overrides.
pub fn resolve_compat(model_base_url: &str, override_compat: Option<&OpenAICompletionsCompat>) -> ResolvedOpenAICompletionsCompat {
    let detected = detect_compat(model_base_url);
    let o = override_compat.cloned().unwrap_or_default();

    ResolvedOpenAICompletionsCompat {
        supports_store: o.supports_store.or(detected.supports_store).unwrap_or(false),
        supports_developer_role: o
            .supports_developer_role
            .or(detected.supports_developer_role)
            .unwrap_or(false),
        supports_reasoning_effort: o
            .supports_reasoning_effort
            .or(detected.supports_reasoning_effort)
            .unwrap_or(false),
        supports_usage_in_streaming: o
            .supports_usage_in_streaming
            .or(detected.supports_usage_in_streaming)
            .unwrap_or(true),
        supports_finish_reason: o
            .supports_finish_reason
            .or(detected.supports_finish_reason)
            .unwrap_or(true),
        max_tokens_field: o
            .max_tokens_field
            .or(detected.max_tokens_field)
            .unwrap_or(MaxTokensField::MaxTokens),
        requires_tool_result_name: o
            .requires_tool_result_name
            .or(detected.requires_tool_result_name)
            .unwrap_or(false),
        requires_assistant_after_tool_result: o
            .requires_assistant_after_tool_result
            .or(detected.requires_assistant_after_tool_result)
            .unwrap_or(false),
        requires_thinking_as_text: o
            .requires_thinking_as_text
            .or(detected.requires_thinking_as_text)
            .unwrap_or(false),
        thinking_format: o
            .thinking_format
            .or(detected.thinking_format)
            .unwrap_or(ThinkingFormat::Openai),
        session_affinity_format: o
            .session_affinity_format
            .or(detected.session_affinity_format),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_disables_openai_specifics() {
        let c = detect_compat("http://localhost:11434/v1");
        assert_eq!(c.supports_store, Some(false));
        assert_eq!(c.supports_developer_role, Some(false));
        assert_eq!(c.max_tokens_field, Some(MaxTokensField::MaxTokens));
    }

    #[test]
    fn openai_cloud_defaults() {
        let c = detect_compat("https://api.openai.com/v1");
        assert_eq!(c.supports_developer_role, Some(true));
        assert_eq!(
            c.max_tokens_field,
            Some(MaxTokensField::MaxCompletionTokens)
        );
    }

    #[test]
    fn override_wins() {
        let resolved = resolve_compat(
            "http://localhost:8080/v1",
            Some(&OpenAICompletionsCompat {
                supports_store: Some(true),
                ..Default::default()
            }),
        );
        assert!(resolved.supports_store);
        assert!(!resolved.supports_developer_role);
    }
}
