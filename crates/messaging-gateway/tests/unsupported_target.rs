//! §4.0 Phase 1: the in-process gateway has no threading, so a send addressed
//! to a `Thread` target is rejected with `UnsupportedFeature`.

use ardur_messaging_gateway::{
    CapTokenRef, ChannelId, GatewayError, InProcessGateway, MessageBody, MessageTarget,
    MessagingGateway, OutgoingMessage, ThreadRef,
};
use uuid::Uuid;

#[tokio::test]
async fn thread_target_is_unsupported() {
    let channel = ChannelId("in-process://default".to_owned());
    let gateway = InProcessGateway::new(channel.clone());
    assert!(!gateway.supports_threading());

    let outgoing = OutgoingMessage {
        message_id: Uuid::new_v4(),
        channel_id: channel,
        target: MessageTarget::Thread(ThreadRef("thread-1".to_owned())),
        body: MessageBody::Text("reply".to_owned()),
        cap_token: CapTokenRef("cap-token-abc".to_owned()),
        parent_message_id: None,
    };

    let err = gateway
        .send_message(outgoing)
        .await
        .expect_err("threaded delivery is unsupported");
    assert!(matches!(err, GatewayError::UnsupportedFeature(_)));
}
