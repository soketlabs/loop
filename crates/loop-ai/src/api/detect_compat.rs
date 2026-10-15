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
    // Soket's endpoint accepts the `metadata` object (session affinity);
    // everywhere else it is an undocumented field that strict servers
    // (e.g. Z.ai, error 1210) reject outright.
    let is_soket = url.contains("soket.ai") || url.contains("tensorstudio");
    let is_zai = url.contains("z.ai") || url.contains("bigmodel");
    // `metadata` is only sent where it is known to be accepted.
    let supports_metadata = Some(is_soket);

    if is_local {
        return OpenAICompletionsCompat {
            supports_store: Some(false),
            supports_metadata,
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
            supports_metadata,
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
    } else if is_zai {
        ThinkingFormat::Zai
    } else if is_deepseek {
        ThinkingFormat::Deepseek
    } else if is_together {
        ThinkingFormat::Together
    } else {
        ThinkingFormat::Openai
    };

    OpenAICompletionsCompat {
        supports_store: Some(false),
        supports_metadata,
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

/// Explicit override → URL-detected default → `fallback`.
fn pick<T>(overridden: Option<T>, detected: Option<T>, fallback: T) -> T {
    overridden.or(detected).unwrap_or(fallback)
}

/// Merge URL-detected defaults with explicit model overrides.
pub fn resolve_compat(
    model_base_url: &str,
    override_compat: Option<&OpenAICompletionsCompat>,
) -> ResolvedOpenAICompletionsCompat {
    let detected = detect_compat(model_base_url);
    let o = override_compat.cloned().unwrap_or_default();

    ResolvedOpenAICompletionsCompat {
        supports_store: pick(o.supports_store, detected.supports_store, false),
        supports_metadata: pick(o.supports_metadata, detected.supports_metadata, false),
        supports_developer_role: pick(
            o.supports_developer_role,
            detected.supports_developer_role,
            false,
        ),
        supports_reasoning_effort: pick(
            o.supports_reasoning_effort,
            detected.supports_reasoning_effort,
            false,
        ),
        supports_usage_in_streaming: pick(
            o.supports_usage_in_streaming,
            detected.supports_usage_in_streaming,
            true,
        ),
        supports_finish_reason: pick(
            o.supports_finish_reason,
            detected.supports_finish_reason,
            true,
        ),
        max_tokens_field: pick(
            o.max_tokens_field,
            detected.max_tokens_field,
            MaxTokensField::MaxTokens,
        ),
        requires_tool_result_name: pick(
            o.requires_tool_result_name,
            detected.requires_tool_result_name,
            false,
        ),
        requires_assistant_after_tool_result: pick(
            o.requires_assistant_after_tool_result,
            detected.requires_assistant_after_tool_result,
            false,
        ),
        requires_thinking_as_text: pick(
            o.requires_thinking_as_text,
            detected.requires_thinking_as_text,
            false,
        ),
        thinking_format: pick(
            o.thinking_format,
            detected.thinking_format,
            ThinkingFormat::Openai,
        ),
        // No fallback: `None` means "no session affinity".
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

    #[test]
    fn metadata_only_for_soket() {
        // Soket's endpoint accepts the `metadata` object.
        assert_eq!(
            detect_compat("https://api.soket.ai/v1").supports_metadata,
            Some(true)
        );
        // Everything else — including Z.ai, which rejects unknown fields
        // with HTTP 400 "Invalid API parameter" — defaults to not sending it.
        for url in [
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
            "https://api.openai.com/v1",
            "https://openrouter.ai/api/v1",
            "http://localhost:11434/v1",
        ] {
            assert_eq!(
                detect_compat(url).supports_metadata,
                Some(false),
                "metadata should be off for {url}"
            );
            assert!(!resolve_compat(url, None).supports_metadata);
        }
        // An explicit override can re-enable it (self-hosted gateways).
        assert!(
            resolve_compat(
                "https://api.z.ai/api/paas/v4",
                Some(&OpenAICompletionsCompat {
                    supports_metadata: Some(true),
                    ..Default::default()
                })
            )
            .supports_metadata
        );
    }

    #[test]
    fn zai_uses_the_zai_thinking_format() {
        assert_eq!(
            detect_compat("https://api.z.ai/api/paas/v4").thinking_format,
            Some(ThinkingFormat::Zai)
        );
        assert_eq!(
            detect_compat("https://open.bigmodel.cn/api/paas/v4").thinking_format,
            Some(ThinkingFormat::Zai)
        );
    }
}
