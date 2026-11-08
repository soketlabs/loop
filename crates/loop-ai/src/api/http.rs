//! Shared reqwest client builders with sensible timeouts.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP keep-alive helps detect half-open connections during long SSE streams.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
/// Pool idle timeout prevents reusing stale connections.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Names a Unix socket served by a proxy that holds the API keys (a hosting
/// gateway). When set, requests to model providers and keyed web APIs go over
/// that socket as plain HTTP with the original host, and the proxy adds the key
/// and TLS. Unset (the normal case), requests go straight to the provider.
pub const API_PROXY_SOCKET_ENV: &str = "LOOP_API_PROXY_SOCKET";

/// The API proxy socket from [`API_PROXY_SOCKET_ENV`], read once per process.
/// Always `None` off Unix.
pub fn api_proxy_socket() -> Option<&'static Path> {
    static SOCKET: OnceLock<Option<PathBuf>> = OnceLock::new();
    SOCKET
        .get_or_init(|| {
            if !cfg!(unix) {
                return None;
            }
            std::env::var_os(API_PROXY_SOCKET_ENV)
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
        })
        .as_deref()
}

/// `url` as sent to the API proxy: `https://` becomes `http://` (the proxy
/// speaks TLS upstream). Unchanged without a proxy.
pub fn api_proxy_url(url: &str) -> Cow<'_, str> {
    match (api_proxy_socket(), url.strip_prefix("https://")) {
        (Some(_), Some(rest)) => Cow::Owned(format!("http://{rest}")),
        _ => Cow::Borrowed(url),
    }
}

#[cfg(unix)]
fn through_api_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    match api_proxy_socket() {
        Some(socket) => builder.unix_socket(socket),
        None => builder,
    }
}

#[cfg(not(unix))]
fn through_api_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
}

/// Build a client for short-lived requests (model listing, auth checks, etc.).
///
/// Has both a connect timeout and an overall request timeout.
pub fn http_client() -> reqwest::Client {
    through_api_proxy(
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .tcp_keepalive(TCP_KEEPALIVE)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT),
    )
    .build()
    .unwrap_or_default()
}

/// Build a client for long-running streaming requests (SSE chat completions).
///
/// Has a connect timeout but **no** overall request timeout so streams can run
/// as long as needed. Individual chunk-level timeouts are applied in the SSE
/// reader loop.
pub fn streaming_http_client() -> reqwest::Client {
    streaming_http_client_with_connect(Some(CONNECT_TIMEOUT))
}

/// [`streaming_http_client`] with its own connection deadline (`None`: no
/// limit), for a `timeouts.connectionMs` setting. Keep-alive and pooling stay
/// the same.
pub fn streaming_http_client_with_connect(connect: Option<Duration>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .tcp_keepalive(TCP_KEEPALIVE)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT);
    if let Some(connect) = connect {
        builder = builder.connect_timeout(connect);
    }
    through_api_proxy(builder).build().unwrap_or_default()
}
