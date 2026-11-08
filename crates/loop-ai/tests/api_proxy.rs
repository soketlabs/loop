//! With `LOOP_API_PROXY_SOCKET` set, provider requests reach that Unix socket as
//! plain HTTP addressed to the provider's host. Its own test binary, so the
//! variable never leaks into other tests.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;

#[tokio::test]
async fn provider_requests_go_through_the_api_proxy_socket() {
    let dir = std::env::temp_dir().join(format!("loop-api-proxy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("api.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    std::env::set_var(loop_ai::API_PROXY_SOCKET_ENV, &socket);

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let body = r#"{"data":[{"id":"gpt-proxied"}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        String::from_utf8(head).unwrap()
    });

    assert_eq!(
        loop_ai::api_proxy_url("https://api.openai.com/v1/models"),
        "http://api.openai.com/v1/models"
    );
    let models = loop_ai::list_openai_models(
        "https://api.openai.com/v1",
        Some("placeholder"),
        &Default::default(),
    )
    .await
    .unwrap();
    let head = server.join().unwrap().to_ascii_lowercase();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(head.starts_with("get /v1/models http/1.1\r\n"), "{head}");
    assert!(head.contains("\r\nhost: api.openai.com\r\n"), "{head}");
    assert!(
        head.contains("\r\nauthorization: bearer placeholder\r\n"),
        "{head}"
    );
    assert_eq!(models[0].id, "gpt-proxied");
}
