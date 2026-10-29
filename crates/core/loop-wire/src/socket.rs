//! Sockets as [`Transport`]s: Unix sockets (Linux and macOS) and WebSockets.
//!
//! - A Unix socket carries one message per line ([`transport::lines`]). It is
//!   created with mode 0600 inside a directory with mode 0700, so only its
//!   owner can connect.
//! - A WebSocket carries one message per text frame. It always needs a bearer
//!   token, even on loopback: any web page can open `ws://127.0.0.1`.
//!
//! [`transport::lines`]: crate::transport::lines

use std::io;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::transport::{Transport, MAX_MESSAGE_BYTES};

/// Listen on a Unix socket at `path`, readable and writable by its owner
/// only. A stale socket file left by an earlier run is replaced; the parent
/// directory is created with mode 0700 when missing.
#[cfg(unix)]
pub fn unix_listen(path: &std::path::Path) -> io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// One connection on a Unix socket, from either end.
#[cfg(unix)]
pub fn unix(stream: tokio::net::UnixStream) -> Transport {
    let (read, write) = stream.into_split();
    crate::transport::lines(read, write)
}

/// Accept a WebSocket on `stream`, which must present
/// `Authorization: Bearer <token>`. Anything else is refused with 401.
pub async fn websocket_accept<S>(stream: S, token: &str) -> io::Result<Transport>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let expected = format!("Bearer {token}");
    // The handshake callback's type is tungstenite's; its error is large by design.
    #[allow(clippy::result_large_err)]
    let check = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let given = request
            .headers()
            .get("authorization")
            .map(HeaderValue::as_bytes)
            .unwrap_or_default();
        if constant_time_eq(given, expected.as_bytes()) {
            Ok(response)
        } else {
            let mut refused = ErrorResponse::new(Some("a bearer token is required".into()));
            *refused.status_mut() = StatusCode::UNAUTHORIZED;
            Err(refused)
        }
    };
    let ws = tokio_tungstenite::accept_hdr_async_with_config(stream, check, Some(ws_config()))
        .await
        .map_err(io::Error::other)?;
    Ok(websocket(ws))
}

/// Connect to a WebSocket at `url` (`ws://host:port/path`) with a bearer
/// token.
pub async fn websocket_connect(url: &str, token: &str) -> io::Result<Transport> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request().map_err(io::Error::other)?;
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the token is not a valid header",
        )
    })?;
    request.headers_mut().insert("authorization", value);
    let (ws, _) = tokio_tungstenite::connect_async_with_config(request, Some(ws_config()), false)
        .await
        .map_err(io::Error::other)?;
    Ok(websocket(ws))
}

fn ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

/// One message per text frame. Binary frames are skipped; dropping the
/// outgoing side sends a close frame.
fn websocket<S>(ws: WebSocketStream<S>) -> Transport
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let (incoming_tx, incoming) = mpsc::unbounded_channel();
    let (outgoing, mut to_send) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(message) = to_send.recv().await {
            if sink.send(Message::text(message)).await.is_err() {
                return;
            }
        }
        let _ = sink.close().await;
    });
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(text)) => {
                    if incoming_tx.send(text.as_str().to_owned()).is_err() {
                        return;
                    }
                }
                Ok(Message::Close(_)) => return,
                // Pings are answered by the library; binary isn't ours.
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "websocket read failed");
                    return;
                }
            }
        }
    });
    Transport { incoming, outgoing }
}

/// Compare without leaking how many leading bytes matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison() {
        assert!(constant_time_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer abd", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer ab", b"Bearer abc"));
        assert!(!constant_time_eq(b"", b"Bearer abc"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_unix_socket_is_private_and_replaces_a_stale_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/loop.sock");
        let first = unix_listen(&path).unwrap();
        drop(first);
        let _listener = unix_listen(&path).expect("a stale socket file is replaced");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[tokio::test]
    async fn a_websocket_without_the_token_is_refused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            websocket_accept(stream, "secret").await.is_ok()
        });
        let refused = websocket_connect(&format!("ws://{addr}/"), "wrong").await;
        assert!(refused.is_err());
        assert!(!server.await.unwrap());
    }

    #[tokio::test]
    async fn websocket_messages_cross_both_ways() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut transport = websocket_accept(stream, "secret").await.unwrap();
            let got = transport.incoming.recv().await.unwrap();
            transport.outgoing.send(format!("echo {got}")).unwrap();
            transport
        });
        let mut client = websocket_connect(&format!("ws://{addr}/"), "secret")
            .await
            .unwrap();
        client.outgoing.send("hi".into()).unwrap();
        assert_eq!(client.incoming.recv().await.unwrap(), "echo hi");
        let server = server.await.unwrap();
        drop(server);
        assert!(
            client.incoming.recv().await.is_none(),
            "closing ends the stream"
        );
    }
}
