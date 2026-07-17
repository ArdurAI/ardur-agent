use ardur_acp::{
    ACP_METHOD_INITIALIZE, ACP_METHOD_SESSION_NEW, ACP_PROTOCOL_VERSION, AcpDelegationResponse,
    AcpError, AcpErrorObject, AcpMessage, AcpNotification, AcpRequest, AcpRequestId, AcpResponse,
    AcpResponsePayload, AcpWireCodec, RECEIPT_ACP_PEER_DISCOVERED, RECEIPT_ACP_TASK_DELEGATED_OUT,
    RECEIPT_ACP_TASK_RECEIVED_IN, RECEIPT_ACP_TRUST_REFUSED,
};
use serde_json::json;

#[test]
fn initialize_request_round_trips_as_one_newline_delimited_frame() {
    let request = AcpRequest::new(
        1_i64,
        ACP_METHOD_INITIALIZE,
        Some(json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {}
        })),
    );
    let message = AcpMessage::Request(request);

    let frame = AcpWireCodec::encode_message(&message).expect("encode initialize request");

    assert!(frame.ends_with(b"\n"));
    assert!(!frame[..frame.len() - 1].contains(&b'\n'));
    assert!(!frame[..frame.len() - 1].contains(&b'\r'));
    assert_eq!(
        AcpWireCodec::decode_line(&frame).expect("decode initialize request"),
        message
    );
}

#[test]
fn session_new_uses_current_acp_slash_method_name() {
    let request = AcpRequest::new(
        AcpRequestId::from("req-2"),
        ACP_METHOD_SESSION_NEW,
        Some(json!({ "cwd": "/tmp/project" })),
    );
    let frame = AcpWireCodec::encode_message(&AcpMessage::Request(request))
        .expect("encode session/new request");
    let decoded = AcpWireCodec::decode_line(&frame).expect("decode session/new request");

    match decoded {
        AcpMessage::Request(request) => assert_eq!(request.method, "session/new"),
        other => panic!("expected request, got {other:?}"),
    }
}

#[test]
fn response_error_and_notification_round_trip() {
    let response = AcpResponse::success(
        1_i64,
        json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "agentCapabilities": {}
        }),
    );
    let error = AcpResponse::failure(
        2_i64,
        AcpErrorObject::new(
            -32601,
            "method not found",
            Some(json!({ "method": "oldName" })),
        ),
    );
    let notification = AcpNotification::new("session/update", Some(json!({ "sessionId": "s1" })));

    for message in [
        AcpMessage::Response(response),
        AcpMessage::Response(error),
        AcpMessage::Notification(notification),
    ] {
        let frame = AcpWireCodec::encode_message(&message).expect("encode message");
        assert_eq!(
            AcpWireCodec::decode_line(&frame).expect("decode message"),
            message
        );
    }
}

#[test]
fn response_payload_requires_exactly_one_result_or_error() {
    let response = AcpResponse {
        jsonrpc: "2.0".to_owned(),
        id: 1_i64.into(),
        result: None,
        error: None,
    };

    assert!(matches!(
        response.payload(),
        Err(AcpError::InvalidMessage(message)) if message.contains("result or error")
    ));
}

#[test]
fn decoder_rejects_blank_non_object_bad_version_and_embedded_newline() {
    assert!(matches!(
        AcpWireCodec::decode_line(b"\n"),
        Err(AcpError::EmptyFrame)
    ));
    assert!(matches!(
        AcpWireCodec::decode_line(b"[\"not-an-object\"]\n"),
        Err(AcpError::NonObjectFrame)
    ));
    assert!(matches!(
        AcpWireCodec::decode_line(br#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#),
        Err(AcpError::InvalidJsonRpcVersion(version)) if version == "1.0"
    ));
    assert!(matches!(
        AcpWireCodec::decode_line(b"{\"jsonrpc\":\"2.0\"}\n{\"jsonrpc\":\"2.0\"}"),
        Err(AcpError::EmbeddedNewline)
    ));
}

#[test]
fn plan_receipt_verbs_are_exposed_as_constants() {
    assert_eq!(RECEIPT_ACP_PEER_DISCOVERED, "acp.peer.discovered.v1");
    assert_eq!(RECEIPT_ACP_TASK_DELEGATED_OUT, "acp.task.delegated_out.v1");
    assert_eq!(RECEIPT_ACP_TASK_RECEIVED_IN, "acp.task.received_in.v1");
    assert_eq!(RECEIPT_ACP_TRUST_REFUSED, "acp.trust.refused.v1");
}

#[test]
fn refusal_response_carries_structured_reason_without_receipt_side_effects() {
    let response = AcpDelegationResponse::refused("peer is outside trust policy");

    assert!(!response.accepted);
    assert!(response.session_id.is_none());
    assert!(response.receipt_id.is_none());
    assert_eq!(
        response.body,
        json!({ "reason": "peer is outside trust policy" })
    );
}

#[test]
fn successful_response_payload_is_accessible_after_validation() {
    let response = AcpResponse::success("ok", json!({ "accepted": true }));

    match response.payload().expect("valid response payload") {
        AcpResponsePayload::Result(value) => assert_eq!(value, json!({ "accepted": true })),
        AcpResponsePayload::Error(error) => panic!("unexpected error payload: {error:?}"),
    }
}
