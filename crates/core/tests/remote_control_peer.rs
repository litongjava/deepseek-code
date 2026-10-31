use deepseek_code_core::services::remote_control::peer::{self, Token};
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;

async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    (listener, address)
}

#[tokio::test]
async fn channel_carries_frames_of_any_size_both_ways() {
    let (listener, address) = listener().await;
    let token = Token::generate().unwrap();
    let host_token = token.clone();
    let host = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut channel = peer::handshake(stream, &host_token, false).await.unwrap();
        let frame = channel.recv().await.unwrap().unwrap();
        channel.send(&json!({"echo": frame})).await.unwrap();
        // A closed client ends the stream.
        assert!(channel.recv().await.unwrap().is_none());
    });
    let mut client = peer::connect(&address, &token).await.unwrap();
    let large = "x".repeat(300_000);
    client.send(&json!({"data": large})).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply["echo"]["data"], large);
    drop(client);
    tokio::time::timeout(Duration::from_secs(5), host)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn wrong_tokens_fail_the_handshake_on_both_sides() {
    let (listener, address) = listener().await;
    let token = Token::generate().unwrap();
    let host = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        peer::handshake(stream, &token, false).await.err()
    });
    let wrong = Token::generate().unwrap();
    assert!(peer::connect(&address, &wrong).await.is_err());
    let error = tokio::time::timeout(Duration::from_secs(5), host)
        .await
        .unwrap()
        .unwrap()
        .expect("host rejects the client");
    assert!(format!("{error:#}").contains("wrong token"));
}
