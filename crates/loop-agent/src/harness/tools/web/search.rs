//! Web search backends for [`super::web_search`].
//!
//! A backend is a small REST call plus a response parser, selected by
//! `webSearch.provider` in settings. Backends are resolved eagerly at tool
//! creation so a missing API key is reported once, not on every query.

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::guard::{validate_request_url, FetchPolicy};

/// Configuration for the `web_search` tool (`webSearch` in settings.json).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchConfig {
    /// Backend id: `tavily` or `brave`.
    pub provider: String,
    /// Env vars that may hold the API key, checked in order. Falls back to the
    /// backend's default (`TAVILY_API_KEY` / `BRAVE_API_KEY`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_key_env: Vec<String>,
    /// Override the backend's API base URL (self-hosted proxies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Allow loopback/private base URLs. Intended for self-hosted mirrors; the
    /// default fails closed.
    #[serde(default)]
    pub allow_private_addresses: bool,
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Result title.
    pub title: String,
    /// Result URL.
    pub url: String,
    /// Short snippet from the page.
    pub snippet: String,
}

/// Built-in backend registry — the single source of truth for backend ids,
/// display names and defaults. `/login` validation (loop-app-core) and the
/// login wizard (loop-cli) consult it; adding a backend is one entry here.
#[derive(Debug)]
pub struct WebSearchBackendInfo {
    /// Backend id (`webSearch.provider`).
    pub id: &'static str,
    /// Human-readable name.
    pub display_name: &'static str,
    /// Env var checked when neither `apiKeyEnv` nor a stored key resolves.
    pub default_api_key_env: &'static str,
    /// API base URL unless `webSearch.baseUrl` overrides it.
    pub default_base_url: &'static str,
}

/// The built-in web-search backends.
pub const WEB_SEARCH_BACKENDS: &[WebSearchBackendInfo] = &[
    WebSearchBackendInfo {
        id: "tavily",
        display_name: "Tavily",
        default_api_key_env: "TAVILY_API_KEY",
        default_base_url: "https://api.tavily.com",
    },
    WebSearchBackendInfo {
        id: "brave",
        display_name: "Brave Search",
        default_api_key_env: "BRAVE_API_KEY",
        default_base_url: "https://api.search.brave.com",
    },
];

/// Look up a built-in backend by id (case-insensitive, whitespace trimmed).
pub fn web_search_backend(id: &str) -> Option<&'static WebSearchBackendInfo> {
    WEB_SEARCH_BACKENDS
        .iter()
        .find(|info| info.id.eq_ignore_ascii_case(id.trim()))
}

#[derive(Clone)]
enum SearchBackend {
    Tavily { base_url: Url, api_key: String },
    Brave { base_url: Url, api_key: String },
}

impl SearchBackend {
    /// Resolve the backend from config. The API key comes from `apiKeyEnv`
    /// env vars, the stored key (auth.json), or the backend's default env var,
    /// in that order. `Err` explains why the tool cannot be created.
    fn resolve(config: &WebSearchConfig, stored_key: Option<&str>) -> Result<Self, String> {
        let info = web_search_backend(&config.provider).ok_or_else(|| {
            format!(
                "unknown provider '{}' (expected 'tavily' or 'brave')",
                config.provider.trim()
            )
        })?;
        let api_key =
            resolve_api_key(config, stored_key, info.default_api_key_env).ok_or_else(|| {
                format!(
                    "no API key: run /login {}, or set {}",
                    info.id, info.default_api_key_env
                )
            })?;
        Self::new(info, api_key, config)
    }

    /// Build a backend for an explicit registry record and key (the
    /// verification path also uses this).
    fn new(
        info: &'static WebSearchBackendInfo,
        api_key: String,
        config: &WebSearchConfig,
    ) -> Result<Self, String> {
        if api_key.trim().is_empty() {
            return Err(format!("{}: API key is empty", info.id));
        }
        let policy = FetchPolicy {
            allow_private_addresses: config.allow_private_addresses,
        };
        let base_url = resolve_base_url(
            config.base_url.as_deref().unwrap_or(info.default_base_url),
            policy,
        )?;
        Ok(match info.id {
            "brave" => Self::Brave { base_url, api_key },
            _ => Self::Tavily { base_url, api_key },
        })
    }

    /// Backend id for result details.
    fn id(&self) -> &'static str {
        match self {
            Self::Tavily { .. } => "tavily",
            Self::Brave { .. } => "brave",
        }
    }

    /// Run a search.
    async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchHit>, String> {
        match self {
            Self::Tavily { base_url, api_key } => {
                let mut url = base_url.clone();
                url.set_path("/search");
                let response = client
                    .post(url)
                    .bearer_auth(api_key)
                    .json(&json!({ "query": query, "max_results": max_results }))
                    .send()
                    .await
                    .map_err(|e| format!("tavily search failed: {e}"))?;
                let status = response.status();
                let body: Value = response
                    .json()
                    .await
                    .map_err(|e| format!("tavily returned invalid JSON: {e}"))?;
                check_status(status, "tavily")?;
                parse_tavily(&body)
            }
            Self::Brave { base_url, api_key } => {
                let mut url = base_url.clone();
                url.set_path("/res/v1/web/search");
                url.query_pairs_mut()
                    .append_pair("q", query)
                    .append_pair("count", &max_results.to_string());
                let response = client
                    .get(url)
                    .header("X-Subscription-Token", api_key)
                    .send()
                    .await
                    .map_err(|e| format!("brave search failed: {e}"))?;
                let status = response.status();
                let body: Value = response
                    .json()
                    .await
                    .map_err(|e| format!("brave returned invalid JSON: {e}"))?;
                check_status(status, "brave")?;
                parse_brave(&body)
            }
        }
    }
}

fn check_status(status: reqwest::StatusCode, backend: &str) -> Result<(), String> {
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("{backend} search failed: HTTP {status}"))
    }
}

/// API key resolution order: `apiKeyEnv` env vars → stored key → default env.
fn resolve_api_key(
    config: &WebSearchConfig,
    stored_key: Option<&str>,
    default_env: &str,
) -> Option<String> {
    let non_empty = |v: String| (!v.trim().is_empty()).then_some(v);
    config
        .api_key_env
        .iter()
        .find_map(|name| std::env::var(name).ok().and_then(non_empty))
        .or_else(|| stored_key.and_then(|k| non_empty(k.to_string())))
        .or_else(|| std::env::var(default_env).ok().and_then(non_empty))
}

fn parse_base_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| format!("invalid base URL '{raw}'"))?;
    if url.host_str().is_none() {
        return Err(format!("invalid base URL '{raw}': no host"));
    }
    Ok(url)
}

/// Parse a configured base URL and apply the same SSRF rules as fetches, so a
/// `baseUrl` override cannot point the search tool at internal addresses
/// unless explicitly allowed.
fn resolve_base_url(raw: &str, policy: FetchPolicy) -> Result<Url, String> {
    let url = parse_base_url(raw)?;
    validate_request_url(&url, policy).map_err(|e| format!("invalid base URL '{raw}': {e}"))?;
    Ok(url)
}

fn parse_tavily(body: &Value) -> Result<Vec<SearchHit>, String> {
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .ok_or("tavily response is missing 'results'")?;
    Ok(results
        .iter()
        .filter_map(|item| {
            Some(SearchHit {
                title: item.get("title")?.as_str()?.to_string(),
                url: item.get("url")?.as_str()?.to_string(),
                snippet: item
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect())
}

fn parse_brave(body: &Value) -> Result<Vec<SearchHit>, String> {
    let results = body
        .pointer("/web/results")
        .and_then(Value::as_array)
        .ok_or("brave response is missing 'web.results'")?;
    Ok(results
        .iter()
        .filter_map(|item| {
            Some(SearchHit {
                title: item.get("title")?.as_str()?.to_string(),
                url: item.get("url")?.as_str()?.to_string(),
                snippet: item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect())
}

/// Build the `web_search` tool. `stored_key` is the key saved by `/login`
/// (auth.json); env vars from `apiKeyEnv` take precedence over it. `Err`
/// (missing key, unknown provider, bad base URL) should be reported by the
/// caller and the tool left unregistered.
pub fn create_web_search_tool(
    config: WebSearchConfig,
    stored_key: Option<&str>,
) -> Result<crate::types::AgentTool, String> {
    let backend = SearchBackend::resolve(&config, stored_key)?;
    let policy = FetchPolicy {
        allow_private_addresses: config.allow_private_addresses,
    };
    let client = super::shared_client(policy)?;
    Ok(crate::types::AgentTool::simple(
        "web_search",
        "Web search",
        "Search the web and return ranked results (title, URL, snippet). Use web_fetch to read a result's page.",
        super::schema_object(
            json!({
                "query": {"type": "string", "description": "Search query"},
                "max_results": {"type": "integer", "description": "Results to return (1-10)", "minimum": 1, "maximum": 10}
            }),
            &["query"],
        ),
        move |_id, args, _cancel, _on_update| {
            let backend = backend.clone();
            let client = client.clone();
            async move {
                let query = super::arg_str(&args, "query")?;
                let max_results = args
                    .get("max_results")
                    .and_then(Value::as_u64)
                    .unwrap_or(5)
                    .clamp(1, 10) as usize;
                let hits = backend.search(&client, &query, max_results).await?;
                if hits.is_empty() {
                    return Ok(crate::types::AgentToolResult::text(format!(
                        "No results for '{query}'."
                    )));
                }
                let text = super::format_hits(&hits);
                let details = json!({
                    "provider": backend.id(),
                    "query": query,
                    "results": hits.iter().map(|hit| json!({
                        "title": hit.title, "url": hit.url, "snippet": hit.snippet
                    })).collect::<Vec<_>>()
                });
                Ok(crate::types::AgentToolResult {
                    content: vec![loop_ai::ToolResultContent::Text(loop_ai::TextContent {
                        text,
                        text_signature: None,
                    })],
                    details,
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            }
        },
    ))
}

/// Verify an API key before saving it: run a one-result probe search.
/// Auth failures (401/403) are reported as a rejected key; a probe costs one
/// query on the backend's free tier.
pub async fn verify_web_search_key(config: &WebSearchConfig, api_key: &str) -> Result<(), String> {
    let info = web_search_backend(&config.provider).ok_or_else(|| {
        format!(
            "unknown provider '{}' (expected 'tavily' or 'brave')",
            config.provider.trim()
        )
    })?;
    let backend = SearchBackend::new(info, api_key.to_string(), config)?;
    let policy = FetchPolicy {
        allow_private_addresses: config.allow_private_addresses,
    };
    let client = super::shared_client(policy)?;
    match backend
        .search(&client, "loop agent connectivity check", 1)
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.contains("401") || e.contains("403") => {
            Err(format!("{} rejected the API key", info.display_name))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tavily_results() {
        let body: Value = serde_json::from_str(
            r#"{"query":"rust","results":[
                {"title":"Rust","url":"https://rust-lang.org","content":"A language"},
                {"title":"Docs","url":"https://doc.rust-lang.org","content":"Learn"}
            ], "answer":"ok"}"#,
        )
        .unwrap();
        let hits = parse_tavily(&body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            SearchHit {
                title: "Rust".into(),
                url: "https://rust-lang.org".into(),
                snippet: "A language".into(),
            }
        );
    }

    #[test]
    fn parses_brave_results() {
        let body: Value = serde_json::from_str(
            r#"{"web":{"results":[
                {"title":"Rust","url":"https://rust-lang.org","description":"Systems lang",
                 "meta":{"foo":1}}
            ]}}"#,
        )
        .unwrap();
        let hits = parse_brave(&body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Systems lang");
    }

    #[test]
    fn missing_results_is_an_error() {
        let body: Value = serde_json::from_str(r#"{"web":{}}"#).unwrap();
        assert!(parse_brave(&body).is_err());
        let body: Value = serde_json::from_str(r#"{}"#).unwrap();
        assert!(parse_tavily(&body).is_err());
    }

    #[test]
    fn entries_missing_required_fields_are_dropped() {
        let body: Value = serde_json::from_str(
            r#"{"results":[
                {"title":"ok","url":"https://ok","content":"c"},
                {"title":"no url"},
                {"url":"https://no-title","content":"c"},
                {"title":"null url","url":null}
            ]}"#,
        )
        .unwrap();
        let hits = parse_tavily(&body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://ok");
    }

    #[test]
    fn whitespace_only_keys_are_treated_as_missing() {
        std::env::remove_var("LOOP_TEST_UNSET_SEARCH_KEY");
        let config = WebSearchConfig {
            provider: "tavily".into(),
            ..Default::default()
        };
        assert!(
            resolve_api_key(&config, Some("   "), "LOOP_TEST_UNSET_SEARCH_KEY").is_none(),
            "a whitespace-only stored key must not count"
        );
        assert!(resolve_api_key(&config, Some("real-key"), "LOOP_TEST_UNSET_SEARCH_KEY").is_some());
    }

    #[test]
    fn base_urls_must_have_a_host() {
        assert!(parse_base_url("https://api.tavily.com").is_ok());
        assert!(parse_base_url("not a url").is_err());
    }
}
