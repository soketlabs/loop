//! First-class web tools: `web_fetch` and `web_search`.
//!
//! Both tools run in the harness process (host network), not inside the
//! sandbox environment, and every outbound URL passes the [`guard`] SSRF
//! checks: http/https only, no URL credentials, and connections only to
//! public addresses (redirects included). Redirects are followed manually so
//! each hop is re-validated; downloads are capped and truncated before they
//! reach the model context.
//!
//! - `web_fetch` is always available; `webFetch.allowPrivateAddresses` opts a
//!   deployment into intranet mirrors (default: fail closed).
//! - `web_search` needs a backend config (`webSearch` in settings.json); its
//!   API key comes from `/login tavily|brave` (auth.json), `apiKeyEnv` env
//!   vars, or the backend's default env var. See [`search`].

mod guard;
mod html;
mod search;

pub use search::{
    create_web_search_tool, verify_web_search_key, web_search_backend, WebSearchBackendInfo,
    WebSearchConfig, WEB_SEARCH_BACKENDS,
};

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::header::LOCATION;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::harness::utils::{truncate_head, DEFAULT_MAX_BYTES};
use crate::types::{AgentTool, AgentToolResult};
use guard::{validate_request_url, FetchPolicy};
use loop_ai::{TextContent, ToolResultContent};

/// Hard cap on bytes downloaded per page (the model sees [`DEFAULT_MAX_BYTES`]).
const MAX_DOWNLOAD_BYTES: usize = 2 * 1024 * 1024;
/// Connection establishment cap per request hop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Overall cap per request hop (redirects each get a fresh budget, bounded by hop count).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Maximum followed redirects, each re-validated.
const MAX_REDIRECTS: usize = 5;

/// Configuration for the `web_fetch` tool (`webFetch` in settings.json).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchConfig {
    /// Allow loopback, private, link-local and reserved addresses. Intended
    /// for intranet mirrors; the default fails closed.
    #[serde(default)]
    pub allow_private_addresses: bool,
}

impl WebFetchConfig {
    fn policy(self) -> FetchPolicy {
        FetchPolicy {
            allow_private_addresses: self.allow_private_addresses,
        }
    }
}

/// One pooled client per policy, shared for the whole process.
fn shared_client(policy: FetchPolicy) -> Result<reqwest::Client, String> {
    static STRICT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    static PRIVATE_HOSTS_ALLOWED: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let cell = if policy.allow_private_addresses {
        &PRIVATE_HOSTS_ALLOWED
    } else {
        &STRICT
    };
    cell.get_or_init(|| build_client(policy))
        .clone()
        .ok_or_else(|| "HTTP client init failed".to_string())
}

fn build_client(policy: FetchPolicy) -> Option<reqwest::Client> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        // Redirects are followed (and re-validated) by `fetch_url`.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("loop/", env!("CARGO_PKG_VERSION")))
        .dns_resolver(std::sync::Arc::new(guard::GuardResolver::new(policy)))
        .build();
    if let Err(error) = &client {
        tracing::error!("web tools: HTTP client init failed: {error}");
    }
    client.ok()
}

/// Build the `web_fetch` tool. `Err` only covers HTTP client init failure.
pub fn create_web_fetch_tool(config: WebFetchConfig) -> Result<AgentTool, String> {
    let policy = config.policy();
    let client = shared_client(policy)?;
    Ok(AgentTool::simple(
        "web_fetch",
        "Web fetch",
        "Fetch an HTTP(S) URL and return its body as text (HTML pages are converted to plain text). Follows up to 5 redirects; only public internet hosts are reachable.",
        schema_object(
            json!({
                "url": {"type": "string", "description": "Absolute http(s) URL to fetch"}
            }),
            &["url"],
        ),
        move |_id, args, cancel, _on_update| {
            let client = client.clone();
            async move {
                let raw_url = arg_str(&args, "url")?;
                fetch_url(&client, &raw_url, policy, cancel.as_ref()).await
            }
        },
    ))
}

/// Fetch with validated redirects and a capped, truncated body.
async fn fetch_url(
    client: &reqwest::Client,
    raw_url: &str,
    policy: FetchPolicy,
    cancel: Option<&CancellationToken>,
) -> Result<AgentToolResult, String> {
    let mut url = Url::parse(raw_url).map_err(|e| format!("invalid URL '{raw_url}': {e}"))?;
    // Only remembered when a redirect actually happens (the common fetch does
    // not redirect, and `Url` clones are heap allocations).
    let mut redirected_from: Option<Url> = None;

    for _ in 0..=MAX_REDIRECTS {
        validate_request_url(&url, policy)?;
        let response = client
            .get(url.clone())
            .header(
                "accept",
                "text/html, application/json, application/xml, text/plain;q=0.9, */*;q=0.5",
            )
            .send()
            .await
            .map_err(|e| format!("request to {} failed: {e}", url.as_str()))?;

        let status = response.status();
        if is_redirect(status) {
            let Some(location) = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
            else {
                return Err(format!("redirect without Location from {}", url.as_str()));
            };
            let next = url
                .join(location)
                .map_err(|e| format!("invalid redirect location '{location}': {e}"))?;
            redirected_from.get_or_insert_with(|| url.clone());
            url = next;
            continue;
        }
        if !status.is_success() {
            return Err(format!("HTTP {} from {}", status.as_u16(), url.as_str()));
        }

        // An absent or empty header degrades to text/plain rather than a
        // confusing "unsupported content type ''".
        let header_content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let content_type: &str = if header_content_type.is_empty() {
            "text/plain"
        } else {
            header_content_type.as_str()
        };
        let textual = content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
            || content_type.contains("javascript");
        if !textual {
            return Err(format!(
                "unsupported content type '{content_type}' (expected text, HTML, JSON or XML)"
            ));
        }

        let (bytes, download_truncated) = read_capped(response, cancel).await?;
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        // Some servers prefix a UTF-8 byte-order mark; it is noise to a model.
        if let Some(without_bom) = text.strip_prefix('\u{feff}') {
            text = without_bom.to_string();
        }
        if content_type.contains("html") {
            text = html::html_to_text(&text);
        }
        let (text, truncated) = truncate_head(&text, DEFAULT_MAX_BYTES);
        let mut details = json!({
            "url": url.as_str(),
            "status": status.as_u16(),
            "contentType": content_type,
            "bytes": bytes.len(),
            "truncated": truncated || download_truncated,
        });
        if let Some(original) = &redirected_from {
            details["redirectedFrom"] = json!(original.as_str());
        }
        return Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent {
                text,
                text_signature: None,
            })],
            details,
            usage: None,
            added_tool_names: None,
            terminate: None,
        });
    }
    Err(format!("too many redirects (more than {MAX_REDIRECTS})"))
}

fn is_redirect(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

/// Stream the body up to [`MAX_DOWNLOAD_BYTES`], honouring cancellation.
async fn read_capped(
    mut response: reqwest::Response,
    cancel: Option<&CancellationToken>,
) -> Result<(Vec<u8>, bool), String> {
    // Pre-size from Content-Length (capped) to avoid growth reallocations.
    let expected = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8 * 1024);
    let mut body = Vec::with_capacity(expected.min(MAX_DOWNLOAD_BYTES));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("download interrupted: {e}"))?
    {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            return Err("web_fetch cancelled".into());
        }
        let remaining = MAX_DOWNLOAD_BYTES.saturating_sub(body.len());
        if chunk.len() >= remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// Format a search result list the way `web_search` returns it.
pub(crate) fn format_hits(hits: &[search::SearchHit]) -> String {
    hits.iter()
        .enumerate()
        .map(|(i, hit)| {
            format!(
                "{}. {}\n   {}\n   {}",
                i + 1,
                hit.title,
                hit.url,
                hit.snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) use crate::harness::tools::{arg_str, schema_object};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_config_maps_to_policy() {
        assert_eq!(
            WebFetchConfig::default().policy(),
            FetchPolicy {
                allow_private_addresses: false
            }
        );
        assert_eq!(
            WebFetchConfig {
                allow_private_addresses: true
            }
            .policy(),
            FetchPolicy {
                allow_private_addresses: true
            }
        );
    }

    #[test]
    fn hit_formatting() {
        let hits = vec![search::SearchHit {
            title: "Rust".into(),
            url: "https://rust-lang.org".into(),
            snippet: "A language".into(),
        }];
        assert_eq!(
            format_hits(&hits),
            "1. Rust\n   https://rust-lang.org\n   A language"
        );
    }
}
