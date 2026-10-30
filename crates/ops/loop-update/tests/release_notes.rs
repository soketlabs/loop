//! Reading a release's published notes.

use std::time::Duration;

use loop_test_support::{FakeHttpServer, FakeResponse};
use loop_update::{ReleaseClient, ReleaseVersion};

fn client(server: &FakeHttpServer) -> ReleaseClient {
    let origin = format!("{}/", server.base_url().trim_end_matches('/'));
    ReleaseClient::new(&origin, &origin, Duration::from_secs(5)).unwrap()
}

#[tokio::test]
async fn notes_come_from_the_release_for_the_tag() {
    let server = FakeHttpServer::start(|request| {
        if request.path == "/tags/v0.5.0" {
            FakeResponse::json(
                r###"{"tag_name":"v0.5.0","name":"Loop v0.5.0","body":"## What's Changed\n* grep tool","html_url":"https://github.com/soketlabs/loop/releases/tag/v0.5.0","draft":false,"prerelease":false}"###,
            )
        } else {
            FakeResponse::new(404, "application/json", r#"{"message":"Not Found"}"#)
        }
    });
    let notes = client(&server)
        .notes(&ReleaseVersion::parse("0.5.0").unwrap())
        .await
        .unwrap();
    assert_eq!(notes.tag, "v0.5.0");
    assert_eq!(notes.body, "## What's Changed\n* grep tool");
    assert_eq!(
        notes.url,
        "https://github.com/soketlabs/loop/releases/tag/v0.5.0"
    );
}

#[tokio::test]
async fn a_version_with_no_release_is_an_error() {
    let server = FakeHttpServer::always(FakeResponse::new(
        404,
        "application/json",
        r#"{"message":"Not Found"}"#,
    ));
    let err = client(&server)
        .notes(&ReleaseVersion::parse("9.9.9").unwrap())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");
}

#[tokio::test]
async fn a_release_with_no_body_has_empty_notes() {
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"tag_name":"v0.5.0","body":null,"html_url":"https://example.test/v0.5.0"}"#,
    ));
    let notes = client(&server)
        .notes(&ReleaseVersion::parse("0.5.0").unwrap())
        .await
        .unwrap();
    assert_eq!(notes.body, "");
}
