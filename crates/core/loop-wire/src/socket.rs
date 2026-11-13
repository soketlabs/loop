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
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;

use crate::transport::{Transport, MAX_MESSAGE_BYTES};

/// Listen on a Unix socket at `path`, readable and writable by its owner
/// only. A stale socket file left by an earlier run is replaced; the parent
/// directory is verified as user-owned and set to mode 0700. Callers must
/// hold their single-instance lock while probing and reclaiming a socket.
#[cfg(unix)]
pub fn unix_listen(path: &std::path::Path) -> io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let metadata = std::fs::symlink_metadata(parent)?;
        // SAFETY: geteuid has no arguments or preconditions.
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Runtime socket directory must be owned by the current user",
            ));
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Runtime socket path exists and is not a socket",
                ));
            }
            // Nonblocking even when a live peer's accept queue is full. Only an
            // unambiguous refusal proves staleness; other errors leave it intact.
            let probe = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
            probe.set_nonblocking(true)?;
            match probe.connect(&socket2::SockAddr::unix(path)?) {
                Ok(()) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "Runtime already listening",
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?;
                }
                Err(error) => return Err(error),
            }
        }
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

/// Handshake headers by which an Edge Gateway asserts the end user it
/// connects for ([`websocket_accept_edge`]). An ordinary WebSocket that
/// sends any of them is refused.
pub mod edge_headers {
    /// `person` or `service`.
    pub const PRINCIPAL_KIND: &str = "loop-principal-kind";
    /// Who vouches for the id, such as an OIDC issuer URL. Never `local`.
    pub const PRINCIPAL_ISSUER: &str = "loop-principal-issuer";
    /// The id within the issuer, such as an OIDC `sub`.
    pub const PRINCIPAL_ID: &str = "loop-principal-id";
    /// The tenant. Never `local` or `system`.
    pub const TENANT: &str = "loop-tenant";
    /// All of them.
    pub const ALL: [&str; 4] = [PRINCIPAL_KIND, PRINCIPAL_ISSUER, PRINCIPAL_ID, TENANT];
}

/// Most bytes of all identity headers together. The principal and tenant
/// limits are smaller; this bounds the work before they are checked.
const MAX_ASSERTION_BYTES: usize = 1024;

fn refusal(status: StatusCode, reason: &str) -> ErrorResponse {
    let mut refused = ErrorResponse::new(Some(reason.into()));
    *refused.status_mut() = status;
    refused
}

fn bearer_matches(request: &Request, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    let given = request
        .headers()
        .get("authorization")
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    !token.is_empty() && constant_time_eq(given, expected.as_bytes())
}

/// The caller an edge handshake asserts. Each header must be present once.
fn asserted_caller(request: &Request) -> Result<loop_protocol::Caller, String> {
    use loop_protocol::{Caller, Principal, PrincipalKind, Tenant};
    let headers = request.headers();
    let total: usize = edge_headers::ALL
        .iter()
        .flat_map(|name| headers.get_all(*name))
        .map(HeaderValue::len)
        .sum();
    if total > MAX_ASSERTION_BYTES {
        return Err("the identity assertion is too large".into());
    }
    let one = |name: &'static str| -> Result<&str, String> {
        let mut values = headers.get_all(name).iter();
        match (values.next(), values.next()) {
            (Some(value), None) => value
                .to_str()
                .map_err(|_| format!("{name} is not visible ASCII")),
            (None, _) => Err(format!("{name} is required")),
            (Some(_), Some(_)) => Err(format!("{name} is given more than once")),
        }
    };
    let kind = match one(edge_headers::PRINCIPAL_KIND)? {
        "person" => PrincipalKind::Person,
        "service" => PrincipalKind::Service,
        _ => return Err(loop_protocol::IdentityError::UnknownKind.to_string()),
    };
    let principal = Principal::new(
        kind,
        one(edge_headers::PRINCIPAL_ISSUER)?,
        one(edge_headers::PRINCIPAL_ID)?,
    )
    .map_err(|e| e.to_string())?;
    let tenant = Tenant::new(one(edge_headers::TENANT)?).map_err(|e| e.to_string())?;
    Caller::asserted(principal, tenant).map_err(|e| e.to_string())
}

/// Accept a WebSocket on `stream`, which must present
/// `Authorization: Bearer <token>`. Anything else is refused with 401. A
/// handshake that carries an [`edge_headers`] identity is refused with 400:
/// only an edge connection can assert one.
pub async fn websocket_accept<S>(stream: S, token: &str) -> io::Result<Transport>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // The handshake callback's type is tungstenite's; its error is large by design.
    #[allow(clippy::result_large_err)]
    let check = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        if !bearer_matches(request, token) {
            return Err(refusal(
                StatusCode::UNAUTHORIZED,
                "a bearer token is required",
            ));
        }
        if edge_headers::ALL
            .iter()
            .any(|name| request.headers().contains_key(*name))
        {
            return Err(refusal(
                StatusCode::BAD_REQUEST,
                "this connection cannot assert an identity",
            ));
        }
        Ok(response)
    };
    handshake(stream, check).await
}

/// Accept an Edge Gateway connection on `stream`. It must present
/// `Authorization: Bearer <edge_token>` (401 otherwise) and assert one end
/// user with every [`edge_headers`] header, once each (400 otherwise). The
/// returned caller is fixed for the connection's life: it is never read
/// again, and no request can change it. Serve the transport with
/// [`crate::serve_with_caller`] and remote authority.
pub async fn websocket_accept_edge<S>(
    stream: S,
    edge_token: &str,
) -> io::Result<(Transport, loop_protocol::Caller)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut asserted = None;
    // The handshake callback's type is tungstenite's; its error is large by design.
    #[allow(clippy::result_large_err)]
    let check = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        if !bearer_matches(request, edge_token) {
            return Err(refusal(
                StatusCode::UNAUTHORIZED,
                "an edge bearer token is required",
            ));
        }
        match asserted_caller(request) {
            Ok(caller) => {
                asserted = Some(caller);
                Ok(response)
            }
            Err(reason) => {
                tracing::warn!(%reason, "edge identity assertion refused");
                Err(refusal(StatusCode::BAD_REQUEST, &reason))
            }
        }
    };
    let transport = handshake(stream, check).await?;
    let caller = asserted.ok_or_else(|| io::Error::other("edge handshake gave no identity"))?;
    Ok((transport, caller))
}

async fn handshake<S, C>(stream: S, check: C) -> io::Result<Transport>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: tokio_tungstenite::tungstenite::handshake::server::Callback + Unpin,
{
    let ws = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio_tungstenite::accept_hdr_async_with_config(stream, check, Some(ws_config())),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "WebSocket handshake timed out"))?
    .map_err(io::Error::other)?;
    Ok(websocket(ws))
}

/// Connect to a WebSocket at `url` (`ws://host:port/path`) with a bearer
/// token.
pub async fn websocket_connect(url: &str, token: &str) -> io::Result<Transport> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request().map_err(io::Error::other)?;
    let uri = request.uri();
    if uri.scheme_str() != Some("ws")
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || uri.query().is_some()
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            "use a ws:// Runtime URL without credentials or query parameters; TLS is not yet supported"));
    }
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the token is not a valid header",
        )
    })?;
    request.headers_mut().insert("authorization", value);
    let (ws, _) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio_tungstenite::connect_async_with_config(request, Some(ws_config()), false),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "WebSocket connection timed out"))?
    .map_err(handshake_error)?;
    Ok(websocket(ws))
}

/// Tell a rejected token from an unreachable Runtime so callers can say which.
fn handshake_error(error: tokio_tungstenite::tungstenite::Error) -> io::Error {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Http(response) if matches!(response.status().as_u16(), 401 | 403) => {
            io::Error::new(io::ErrorKind::PermissionDenied, "unauthorized")
        }
        Error::Io(_) => io::Error::new(io::ErrorKind::ConnectionRefused, "not reachable"),
        _ => {
            io::Error::other("WebSocket connection failed; check the Runtime URL and bearer token")
        }
    }
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
    let closed = CancellationToken::new();
    let (incoming_tx, incoming) = crate::transport::bounded(closed.clone());
    let (outgoing, mut to_send) = crate::transport::bounded(closed.clone());
    let writer_closed = closed.clone();
    tokio::spawn(async move {
        let write = async {
            while let Some(message) = to_send.recv().await {
                if sink.send(Message::text(message)).await.is_err() {
                    return;
                }
            }
            let _ = sink.close().await;
        };
        tokio::select! { biased; _ = writer_closed.cancelled() => {}, _ = write => {} }
        writer_closed.cancel();
    });
    tokio::spawn(async move {
        let read = async {
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
        };
        tokio::select! { biased; _ = closed.cancelled() => {}, _ = read => {} }
        closed.cancel();
    });
    Transport {
        incoming,
        outgoing,
        is_websocket: true,
    }
}

/// Compare without leaking how many leading bytes matched.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_runtime_socket_is_never_unlinked() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("run/runtime.sock");
            let first = unix_listen(&path).unwrap();
            let error = unix_listen(&path).expect_err("live listener must be refused");
            assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
            assert!(error.to_string().contains("Runtime already listening"));
            let _client = tokio::net::UnixStream::connect(&path).await.unwrap();
            first.accept().await.unwrap();
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_non_socket_file_is_not_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sock");
        std::fs::write(&path, "keep me").unwrap();
        assert_eq!(
            unix_listen(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "keep me");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_existing_socket_directory_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("run");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _listener = unix_listen(&parent.join("runtime.sock")).unwrap();
        assert_eq!(
            std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[tokio::test]
    async fn a_websocket_without_the_token_is_refused() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};
        for (expected, given) in [("secret", None), ("secret", Some("wrong")), ("", Some(""))] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            websocket_accept(stream, expected).await.is_ok()
        });
        let mut request = format!("ws://{addr}/").into_client_request().unwrap();
        if let Some(token) = given {
            request.headers_mut().insert("authorization", HeaderValue::from_str(&format!("Bearer {token}")).unwrap());
        }
        let refused = tokio_tungstenite::connect_async(request).await.unwrap_err();
        assert!(matches!(refused, Error::Http(response) if response.status() == StatusCode::UNAUTHORIZED));
        assert!(!server.await.unwrap());
        }
        }).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_websocket_handshake_times_out() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let (stream, _silent_peer) = tokio::io::duplex(1024);
            let error = websocket_accept(stream, "secret").await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_lagging_websocket_is_disconnected_instead_of_queuing_without_limit() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                websocket_accept(stream, "secret").await.unwrap()
            });
            let mut client = websocket_connect(&format!("ws://{addr}/"), "secret")
                .await
                .unwrap();
            let server = server.await.unwrap();
            assert!((0..1024).any(|_| server.outgoing.send("queued".into()).is_err()));
            assert!(client.incoming.recv().await.is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn websocket_connect_names_a_wrong_token_and_an_unreachable_runtime() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let _ = websocket_accept(stream, "secret").await;
            });
            let error = websocket_connect(&format!("ws://{addr}/"), "wrong")
                .await
                .expect_err("wrong token is refused");
            assert_eq!(error.to_string(), "unauthorized");
            server.await.unwrap();
            // The listener is gone now, so the port refuses connections.
            let error = websocket_connect(&format!("ws://{addr}/"), "secret")
                .await
                .expect_err("nothing listens");
            assert_eq!(error.to_string(), "not reachable");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn websocket_messages_cross_both_ways() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
        })
        .await
        .unwrap();
    }
}
