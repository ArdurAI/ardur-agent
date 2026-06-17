use ardur_acp::{
    ACP_METHOD_INITIALIZE, AcpError, AcpMessage, AcpRequest, AcpResponse, AcpTransport,
    StdioAcpTransport,
};
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};

#[tokio::test]
async fn stdio_transport_sends_and_receives_json_rpc_frames() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (server_read, server_write) = tokio::io::split(server_io);
    let client = StdioAcpTransport::new(BufReader::new(client_read), client_write);
    let server = StdioAcpTransport::new(BufReader::new(server_read), server_write);

    let initialize = AcpMessage::Request(AcpRequest::new(
        1_i64,
        ACP_METHOD_INITIALIZE,
        Some(json!({ "protocolVersion": 1 })),
    ));
    client
        .send(initialize.clone())
        .await
        .expect("client sends initialize");
    assert_eq!(
        server.receive().await.expect("server receives frame"),
        Some(initialize)
    );

    let response = AcpMessage::Response(AcpResponse::success(1_i64, json!({ "accepted": true })));
    server
        .send(response.clone())
        .await
        .expect("server sends response");
    assert_eq!(
        client.receive().await.expect("client receives response"),
        Some(response)
    );
}

#[tokio::test]
async fn stdio_transport_rejects_invalid_frames() {
    let (mut peer_io, transport_io) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(transport_io);
    let transport = StdioAcpTransport::new(BufReader::new(read), write);

    peer_io
        .write_all(b"{\"jsonrpc\":\"1.0\",\"id\":1,\"method\":\"initialize\"}\n")
        .await
        .expect("write invalid frame");

    let err = transport
        .receive()
        .await
        .expect_err("bad JSON-RPC version is rejected");
    assert!(matches!(err, AcpError::InvalidJsonRpcVersion(version) if version == "1.0"));
}
