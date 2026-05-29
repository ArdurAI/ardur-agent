//! §4.0 Phase 1: a message sent through the in-process gateway loops back out
//! of `receive` unchanged.

use ardur_messaging_gateway::{
    CapTokenRef, ChannelId, InProcessGateway, MessageBody, MessageTarget, MessagingGateway,
    OutgoingMessage, UserRef,
};
use uuid::Uuid;

#[tokio::test]
async fn send_then_receive_echoes_the_message() {
    let channel = ChannelId("in-process://default".to_owned());
    let gateway = InProcessGateway::new(channel.clone());

    let message_id = Uuid::new_v4();
    let outgoing = OutgoingMessage {
        message_id,
        channel_id: channel.clone(),
        target: MessageTarget::User(UserRef("alice".to_owned())),
        body: MessageBody::Text("hello".to_owned()),
        cap_token: CapTokenRef("cap-token-abc".to_owned()),
        parent_message_id: None,
    };

    let receipt = gateway.send_message(outgoing).await.expect("send succeeds");
    assert_eq!(receipt.delivered_to, channel);
    assert!(receipt.provider_message_id.is_none());

    let incoming = gateway.receive().await.expect("receive succeeds");
    assert_eq!(incoming.message_id, message_id);
    assert_eq!(incoming.channel_id, channel);
    assert_eq!(incoming.body, MessageBody::Text("hello".to_owned()));
    assert!(incoming.thread_id.is_none());
}
