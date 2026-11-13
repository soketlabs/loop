//! Raw HTTP handshakes against the user and edge listeners, byte for byte,
//! so header tricks a WebSocket client library would never send are tested
//! too: other letter case, repeats, folding, padding, non-ASCII bytes and
//! the bearer scheme's case.

use std::time::Duration;

use loop_protocol::Caller;
use loop_wire::socket::{websocket_accept, websocket_accept_edge};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const USER: &str = "user-token";
const EDGE: &str = "edge-token";

enum Listener {
    User,
    Edge,
}

/// What the server did with one handshake.
#[derive(Debug)]
struct Outcome {
    /// The HTTP status line, or empty when the server closed without one.
    status: String,
    /// The edge caller the server accepted, if any.
    caller: Option<Caller>,
    /// The user listener accepted.
    user: bool,
}

async fn raw(listener: Listener, authorization: &str, extra: &str) -> Outcome {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = tcp.accept().await.unwrap();
        match listener {
            Listener::Edge => (
                websocket_accept_edge(stream, EDGE)
                    .await
                    .ok()
                    .map(|(_, c)| c),
                false,
            ),
            Listener::User => (None, websocket_accept(stream, USER).await.is_ok()),
        }
    });
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Authorization: {authorization}\r\n{extra}\r\n"
    );
    client.write_all(request.as_bytes()).await.unwrap();
    let mut buffer = vec![0u8; 512];
    let read = tokio::time::timeout(Duration::from_secs(6), client.read(&mut buffer))
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);
    let status = String::from_utf8_lossy(&buffer[..read])
        .lines()
        .next()
        .unwrap_or("")
        .to_owned();
    drop(client);
    let (caller, user) = server.await.unwrap();
    Outcome {
        status,
        caller,
        user,
    }
}

const KIND_ISSUER: &str = "Loop-Principal-Kind: person\r\nLoop-Principal-Issuer: https://idp\r\n";

fn edge_with(id: &str, tenant: &str) -> String {
    format!("{KIND_ISSUER}Loop-Principal-Id: {id}\r\nLoop-Tenant: {tenant}\r\n")
}

fn accepted(outcome: &Outcome) -> &Caller {
    assert!(outcome.status.contains(" 101 "), "{outcome:?}");
    outcome.caller.as_ref().expect("an accepted edge caller")
}

fn refused(outcome: &Outcome, status: &str) {
    assert!(
        outcome.status.contains(status),
        "want {status}: {outcome:?}"
    );
    assert!(outcome.caller.is_none() && !outcome.user, "{outcome:?}");
}

#[tokio::test]
async fn the_user_listener_refuses_an_identity_header_in_any_letter_case() {
    for extra in [
        "LOOP-TENANT: acme\r\n",
        "loop-PRINCIPAL-id: x\r\n",
        "Loop-Principal-Kind: person\r\n",
    ] {
        refused(
            &raw(Listener::User, &format!("Bearer {USER}"), extra).await,
            " 400 ",
        );
    }
    assert!(
        raw(Listener::User, &format!("Bearer {USER}"), "")
            .await
            .user
    );
}

#[tokio::test]
async fn an_edge_header_given_twice_in_any_form_is_refused() {
    let edge = format!("Bearer {EDGE}");
    // The same header in another letter case is the same header.
    refused(
        &raw(
            Listener::Edge,
            &edge,
            &format!("{}LOOP-TENANT: evil\r\n", edge_with("alice", "acme")),
        )
        .await,
        " 400 ",
    );
    // An empty second value.
    let twice_kind = "Loop-Principal-Kind: person\r\nLoop-Principal-Kind: \r\nLoop-Principal-Issuer: https://idp\r\nLoop-Principal-Id: alice\r\nLoop-Tenant: acme\r\n";
    refused(&raw(Listener::Edge, &edge, twice_kind).await, " 400 ");
    // A folded continuation line (obsolete in HTTP/1.1) never extends a
    // value: the server closes without accepting.
    let folded = raw(
        Listener::Edge,
        &edge,
        &format!("{}  evil\r\n", edge_with("alice", "acme")),
    )
    .await;
    assert!(
        folded.caller.is_none() && !folded.status.contains(" 101 "),
        "{folded:?}"
    );
}

#[tokio::test]
async fn edge_values_are_trimmed_ascii_and_a_tenant_list_is_refused() {
    let edge = format!("Bearer {EDGE}");
    let base = raw(Listener::Edge, &edge, &edge_with("alice", "acme")).await;
    let caller = accepted(&base);
    assert_eq!(
        (caller.principal().id(), caller.tenant().as_str()),
        ("alice", "acme")
    );
    // Padding around a value is not part of it.
    let padded = raw(
        Listener::Edge,
        &edge,
        &format!("{KIND_ISSUER}Loop-Principal-Id:    alice   \r\nLoop-Tenant: \tacme \r\n"),
    )
    .await;
    assert_eq!(accepted(&padded), caller);
    // A byte outside ASCII.
    refused(
        &raw(Listener::Edge, &edge, &edge_with("al\u{e9}ice", "acme")).await,
        " 400 ",
    );
    // A tenant is one name, never a list.
    refused(
        &raw(Listener::Edge, &edge, &edge_with("alice", "acme,evil")).await,
        " 400 ",
    );
}

#[tokio::test]
async fn a_tenant_unsafe_as_a_file_name_is_refused() {
    let edge = format!("Bearer {EDGE}");
    for tenant in ["con", "nul", "com1", "lpt9", "con.x", "acme.", "."] {
        refused(
            &raw(Listener::Edge, &edge, &edge_with("alice", tenant)).await,
            " 400 ",
        );
    }
}

/// A principal id is the identity provider's: any visible ASCII is valid,
/// path-like text included. That is why an id must never be used in a file
/// path as it is (see `loop_protocol::identity`).
#[tokio::test]
async fn a_principal_id_is_kept_exactly_as_asserted() {
    let edge = format!("Bearer {EDGE}");
    for id in ["alice,bob", "..%2F..%2Fetc", "../../etc/passwd"] {
        let outcome = raw(Listener::Edge, &edge, &edge_with(id, "acme")).await;
        assert_eq!(accepted(&outcome).principal().id(), id);
    }
}

/// RFC 7235 §2.1: the scheme name is case-insensitive.
#[tokio::test]
async fn the_bearer_scheme_matches_in_any_case_and_nothing_else_does() {
    let identity = edge_with("alice", "acme");
    for scheme in ["Bearer", "bearer", "BEARER"] {
        accepted(&raw(Listener::Edge, &format!("{scheme} {EDGE}"), &identity).await);
        assert!(
            raw(Listener::User, &format!("{scheme} {USER}"), "")
                .await
                .user
        );
    }
    for authorization in [
        format!("Basic {EDGE}"),
        format!("Bearer  {EDGE}"),
        format!("Bearer {EDGE}x"),
        format!("Bearer {}", &EDGE[..EDGE.len() - 1]),
        EDGE.to_owned(),
        format!("Bearer {USER}"),
    ] {
        refused(
            &raw(Listener::Edge, &authorization, &identity).await,
            " 401 ",
        );
    }
}
