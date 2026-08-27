//! HTTP SSE proxy StreamFn for browser/backend proxy setups.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use loop_ai::{
    create_assistant_message_event_stream, AssistantMessage, AssistantMessageEvent, Context, Model,
    SimpleStreamOptions, StopReason,
};
use serde::Serialize;

use crate::stream_fn::StreamFn;

/// Options for [`stream_proxy`].
#[derive(Clone)]
pub struct ProxyStreamOptions {
    /// Proxy base URL (POST `{proxy_url}/api/stream`).
    pub proxy_url: String,
    /// Bearer auth token.
    pub auth_token: Option<String>,
    /// Underlying simple stream options (serializable subset applied server-side).
    pub stream: SimpleStreamOptions,
}

#[derive(Serialize)]
struct ProxyRequest {
    model: Model,
    context: Context,
    api_key: Option<String>,
    session_id: Option<String>,
}

/// Build a [`StreamFn`] that posts to a proxy and reconstructs assistant events from SSE JSON.
pub fn stream_proxy(options: ProxyStreamOptions) -> StreamFn {
    let options = Arc::new(options);
    Arc::new(move |model, context, stream_opts| {
        let options = Arc::clone(&options);
        Box::pin(async move {
            let stream = create_assistant_message_event_stream();
            let handle = stream.handle();
            let client = loop_ai::streaming_http_client();
            let url = format!(
                "{}/api/stream",
                options.proxy_url.trim_end_matches('/')
            );
            let body = ProxyRequest {
                model: model.clone(),
                context,
                api_key: stream_opts.base.api_key.clone(),
                session_id: stream_opts
                    .base
                    .session_id
                    .clone()
                    .or_else(|| options.stream.base.session_id.clone()),
            };
            tokio::spawn(async move {
                let mut req = client.post(&url).json(&body);
                if let Some(token) = &options.auth_token {
                    req = req.bearer_auth(token);
                }
                let res = match req.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        push_error(&handle, &model, e.to_string());
                        return;
                    }
                };
                if !res.status().is_success() {
                    push_error(
                        &handle,
                        &model,
                        format!("proxy status {}", res.status()),
                    );
                    return;
                }
                let mut byte_stream = res.bytes_stream();
                let mut buf = String::new();
                while let Some(chunk) = byte_stream.next().await {
                    let Ok(bytes) = chunk else { break };
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        buf.push_str(text);
                    }
                    while let Some(pos) = buf.find("\n\n") {
                        let frame = buf[..pos].to_string();
                        buf = buf[pos + 2..].to_string();
                        if let Some(data) = frame.strip_prefix("data: ") {
                            if data.trim() == "[DONE]" {
                                continue;
                            }
                            if let Ok(event) =
                                serde_json::from_str::<AssistantMessageEvent>(data)
                            {
                                let terminal = event.is_terminal();
                                handle.push(event);
                                if terminal {
                                    return;
                                }
                            }
                        }
                    }
                    let _ = Bytes::new();
                }
                // If stream ended without terminal, synthesize error
                let mut partial = AssistantMessage::pending(&model);
                partial.stop_reason = StopReason::Error;
                partial.error_message = Some("proxy stream ended early".into());
                handle.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: partial,
                });
            });
            stream
        })
    })
}

fn push_error(
    handle: &loop_ai::AssistantMessageEventStreamHandle,
    model: &Model,
    message: String,
) {
    let mut partial = AssistantMessage::pending(model);
    partial.stop_reason = StopReason::Error;
    partial.error_message = Some(message);
    handle.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });
    handle.push(AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error: partial,
    });
}
