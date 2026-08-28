//! Run Loop as a streamable-HTTP MCP server.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use loop_agent::harness::mcp::LoopToolProvider;
use loop_mcp::server::McpServer;

use loop_app_core::Runtime;

/// Bearer token auth middleware. If `expected_token` is `Some`, every request
/// must carry a matching `Authorization: Bearer <token>` header.
async fn bearer_auth(
    expected: Arc<Option<String>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(token) = expected.as_ref() {
        let auth_header = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());

        match auth_header {
            Some(value) if value == format!("Bearer {token}") => {}
            _ => {
                return (StatusCode::UNAUTHORIZED, "Unauthorized: invalid or missing Bearer token")
                    .into_response();
            }
        }
    }
    next.run(req).await
}

/// Start the MCP HTTP server and block until shutdown.
pub async fn run_mcp_server(
    runtime: Runtime,
    port: u16,
    token: Option<String>,
) -> anyhow::Result<()> {
    let harness = Arc::clone(&runtime.harness);
    let ct = CancellationToken::new();

    let service = StreamableHttpService::new(
        move || {
            let tools = harness.tools_snapshot();
            let provider = Arc::new(LoopToolProvider::new(tools));
            Ok(McpServer::new(provider))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let expected_token = Arc::new(token.clone());
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(move |req, next| {
            let expected = Arc::clone(&expected_token);
            bearer_auth(expected, req, next)
        }));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    if token.is_some() {
        eprintln!("Loop MCP server listening on http://{addr}/mcp (auth: Bearer token)");
    } else {
        eprintln!("Loop MCP server listening on http://{addr}/mcp (auth: none)");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            ct.cancel();
        })
        .await?;

    runtime.mcp_client.disconnect_all().await;
    Ok(())
}
