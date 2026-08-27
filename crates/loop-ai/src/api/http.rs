//! Shared reqwest client builders with sensible timeouts.

use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP keep-alive helps detect half-open connections during long SSE streams.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
/// Pool idle timeout prevents reusing stale connections.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Build a client for short-lived requests (model listing, auth checks, etc.).
///
/// Has both a connect timeout and an overall request timeout.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Build a client for long-running streaming requests (SSE chat completions).
///
/// Has a connect timeout but **no** overall request timeout so streams can run
/// as long as needed. Individual chunk-level timeouts are applied in the SSE
/// reader loop.
pub fn streaming_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build()
        .unwrap_or_default()
}
