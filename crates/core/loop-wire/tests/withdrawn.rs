//! A Surface learns that another Surface answered its approval.
use loop_protocol::rpc::PeerInfo;
use loop_protocol::{
    AgentBackend, HostServices, InteractionOutcome, InteractionRequest, SessionUpdate,
};
use loop_wire::connection::{Connection, Incoming};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

struct NoHost;
#[async_trait::async_trait]
impl HostServices for NoHost {
    async fn interact(&self, _: InteractionRequest) -> InteractionOutcome {
        InteractionOutcome::Cancelled
    }
}

#[tokio::test]
async fn a_withdrawn_interaction_reaches_subscribers_as_resolved() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (client_side, server_side) = tokio::net::UnixStream::pair().unwrap();
        let (connection, mut incoming) = Connection::new(loop_wire::socket::unix(server_side));
        let server = tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Request { id, method, .. } = message {
                    if method == "initialize" {
                        connection.respond(id, Ok(json!({"protocol_version":1,"server_info":{"name":"loop","version":"0"},"instance":"native","capabilities":{}})));
                    } else {
                        // Any later request: the other Surface just answered.
                        connection
                            .notify(
                                "host/interact_withdrawn",
                                json!({"session":{"instance":"native","key":"s1","native_id":"s1"},"id":"ask-1"}),
                            )
                            .unwrap();
                        connection.respond(id, Err((-32603, "done".into())));
                        break;
                    }
                }
            }
            // Keep the connection open until the client has read everything.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let remote = loop_wire::RemoteBackend::connect(
            loop_wire::socket::unix(client_side),
            PeerInfo { name: "t".into(), version: "0".into() },
            Arc::new(NoHost),
        )
        .await
        .unwrap();
        let mut updates = remote.subscribe();
        let _ = remote.list().await;
        let envelope = updates.recv().await.unwrap();
        assert_eq!(envelope.session.as_str(), "s1");
        assert!(matches!(
            envelope.update,
            SessionUpdate::InteractionResolved { ref id, .. } if id.to_string() == "ask-1"
        ));
        server.abort();
    })
    .await
    .unwrap();
}
