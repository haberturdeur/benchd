#![cfg(feature = "transport")]

use benchd_core::protocol::{self, Hello};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn compatibility_uses_protocol_not_package_or_build_version() {
    let local = Hello::current();
    let mut peer = local.clone();
    peer.version = "different-release".into();
    peer.build = "different-build".into();
    assert!(local.check(&peer).is_ok());
    peer.protocol += 1;
    let error = local.check(&peer).unwrap_err().to_string();
    assert!(error.contains("protocol mismatch"), "{error}");
    assert!(error.contains(&local.build), "{error}");
    assert!(error.contains("different-build"), "{error}");
}

#[tokio::test]
async fn matching_peers_exchange_versions_before_application_bytes() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    let (sent, accepted) = tokio::join!(
        protocol::connect(&mut client),
        protocol::accept(&mut server)
    );
    assert_eq!(sent.unwrap(), Hello::current());
    assert_eq!(accepted.unwrap(), Hello::current());
    client.write_all(b"request\n").await.unwrap();
    let mut bytes = [0; 8];
    server.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"request\n");
}

#[tokio::test]
async fn incompatible_peer_receives_our_version_and_is_refused() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    let mut peer = Hello::current();
    peer.protocol += 1;
    peer.build = "old-deployment".into();
    client
        .write_all(format!("{}\n", serde_json::to_string(&peer).unwrap()).as_bytes())
        .await
        .unwrap();
    let error = protocol::accept(&mut server).await.unwrap_err();
    assert!(error.to_string().contains("old-deployment"));
    let mut bytes = vec![0; 1024];
    let n = client.read(&mut bytes).await.unwrap();
    let reply: Hello = serde_json::from_slice(&bytes[..n]).unwrap();
    assert_eq!(reply, Hello::current());
}

#[tokio::test]
async fn version_hello_does_not_consume_pipelined_binary_bytes() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    let mut data = format!("{}\n", serde_json::to_string(&Hello::current()).unwrap()).into_bytes();
    let payload = [0x01, 0x11, 0x80, 0x03, 0xff, 0, 0, 0];
    data.extend_from_slice(&payload);
    client.write_all(&data).await.unwrap();
    protocol::accept(&mut server).await.unwrap();
    let mut actual = [0; 8];
    server.read_exact(&mut actual).await.unwrap();
    assert_eq!(actual, payload);
}

#[tokio::test]
async fn legacy_registration_is_refused_with_a_host_readable_error() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    client
        .write_all(b"{\"msg\":\"register\",\"bench\":{}}\n")
        .await
        .unwrap();
    let error = protocol::accept(&mut server).await.unwrap_err();
    assert!(error.to_string().contains("handshake required"));
    let mut bytes = vec![0; 2048];
    let n = client.read(&mut bytes).await.unwrap();
    let reply: benchd_core::wire::ToHost = serde_json::from_slice(&bytes[..n]).unwrap();
    assert!(
        matches!(reply, benchd_core::wire::ToHost::Rejected { reason } if reason.contains("upgrade"))
    );
}

#[tokio::test]
async fn legacy_server_error_is_reported_as_a_handshake_problem() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    server
        .write_all(b"{\"msg\":\"error\",\"error\":\"unrecognised message\"}\n")
        .await
        .unwrap();
    let error = protocol::connect(&mut client)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("handshake"), "{error}");
    assert!(error.contains("unrecognised message"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn silent_peer_has_a_bounded_handshake_deadline() {
    let (mut client, _server) = tokio::io::duplex(4096);
    let error = protocol::connect(&mut client).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("handshake"));
}

#[tokio::test]
async fn oversized_hello_is_rejected_without_waiting_for_a_newline() {
    let (mut client, mut server) = tokio::io::duplex(16384);
    client.write_all(&vec![b'x'; 8192]).await.unwrap();
    let error = protocol::accept(&mut server).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("too long"));
}

#[tokio::test]
async fn wrong_message_kind_cannot_pass_as_a_hello() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    let mut value = serde_json::to_value(Hello::current()).unwrap();
    value["msg"] = "register".into();
    client
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
    assert!(protocol::accept(&mut server).await.is_err());
}

#[tokio::test]
async fn legacy_client_gets_its_request_id_back_in_the_rejection() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    client
        .write_all(b"{\"msg\":\"open_session\",\"request\":42,\"name\":\"old\"}\n")
        .await
        .unwrap();
    assert!(protocol::accept(&mut server).await.is_err());
    let mut bytes = vec![0; 2048];
    let n = client.read(&mut bytes).await.unwrap();
    let reply: benchd_core::wire::ToClient = serde_json::from_slice(&bytes[..n]).unwrap();
    assert!(
        matches!(reply, benchd_core::wire::ToClient::Error {request, retryable: false, error} if request.0 == 42 && error.contains("upgrade"))
    );
}

#[tokio::test]
async fn dialer_rejects_a_server_with_a_different_protocol() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    let mut hello = Hello::current();
    hello.protocol += 1;
    hello.build = "incompatible-coordinator".into();
    server
        .write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
        .await
        .unwrap();
    let error = protocol::connect(&mut client)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("protocol mismatch"), "{error}");
    assert!(error.contains("incompatible-coordinator"), "{error}");
}
