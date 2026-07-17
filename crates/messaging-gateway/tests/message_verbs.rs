//! §4.11 Phase 0: message operations are typed verbs, not ad-hoc strings.

use ardur_messaging_gateway::{
    CapTokenRef, ChannelId, GatewayError, InProcessGateway, MESSAGE_DELETED_EVENT,
    MESSAGE_EDITED_EVENT, MESSAGE_FORWARDED_EVENT, MESSAGE_OP_REFUSED_EVENT, MESSAGE_PINNED_EVENT,
    MESSAGE_QUOTED_EVENT, MESSAGE_REACTED_EVENT, MESSAGE_SENT_EVENT, MESSAGE_UNPINNED_EVENT,
    MessageBody, MessageTarget, MessageVerb, MessageVerbRequest, MessagingGateway, UserRef,
};
use uuid::Uuid;

fn assert_roundtrips(verb: &MessageVerb) {
    let json = serde_json::to_string(verb).expect("serializes");
    let back: MessageVerb = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(verb, &back, "round-trip mismatch for {json}");
}

#[test]
fn each_message_verb_variant_roundtrips() {
    let target = Uuid::new_v4();
    assert_roundtrips(&MessageVerb::Send {
        body: MessageBody::Text("hello".to_owned()),
    });
    assert_roundtrips(&MessageVerb::Edit {
        target_message_id: target,
        body: MessageBody::Markdown("**fixed**".to_owned()),
    });
    assert_roundtrips(&MessageVerb::Delete {
        target_message_id: target,
    });
    assert_roundtrips(&MessageVerb::React {
        target_message_id: target,
        emoji: "thumbsup".to_owned(),
    });
    assert_roundtrips(&MessageVerb::Pin {
        target_message_id: target,
    });
    assert_roundtrips(&MessageVerb::Unpin {
        target_message_id: target,
    });
    assert_roundtrips(&MessageVerb::Forward {
        source_message_id: target,
        destination: MessageTarget::User(UserRef("bob".to_owned())),
    });
    assert_roundtrips(&MessageVerb::Quote {
        quoted_message_id: target,
        body: MessageBody::Text("quoting this".to_owned()),
    });
}

#[test]
fn verb_metadata_matches_receipt_vocabulary() {
    let target = Uuid::new_v4();
    let cases = [
        (
            MessageVerb::Send {
                body: MessageBody::Text("hello".to_owned()),
            },
            "send",
            MESSAGE_SENT_EVENT,
            true,
            false,
        ),
        (
            MessageVerb::Edit {
                target_message_id: target,
                body: MessageBody::Text("edited".to_owned()),
            },
            "edit",
            MESSAGE_EDITED_EVENT,
            true,
            true,
        ),
        (
            MessageVerb::Delete {
                target_message_id: target,
            },
            "delete",
            MESSAGE_DELETED_EVENT,
            false,
            true,
        ),
        (
            MessageVerb::React {
                target_message_id: target,
                emoji: "eyes".to_owned(),
            },
            "react",
            MESSAGE_REACTED_EVENT,
            false,
            true,
        ),
        (
            MessageVerb::Pin {
                target_message_id: target,
            },
            "pin",
            MESSAGE_PINNED_EVENT,
            false,
            true,
        ),
        (
            MessageVerb::Unpin {
                target_message_id: target,
            },
            "unpin",
            MESSAGE_UNPINNED_EVENT,
            false,
            true,
        ),
        (
            MessageVerb::Forward {
                source_message_id: target,
                destination: MessageTarget::User(UserRef("alice".to_owned())),
            },
            "forward",
            MESSAGE_FORWARDED_EVENT,
            true,
            false,
        ),
        (
            MessageVerb::Quote {
                quoted_message_id: target,
                body: MessageBody::Text("quoted".to_owned()),
            },
            "quote",
            MESSAGE_QUOTED_EVENT,
            true,
            false,
        ),
    ];

    for (verb, id, event, emits_content, mutates_prior) in cases {
        assert_eq!(verb.id(), id);
        assert_eq!(verb.success_event(), event);
        assert_eq!(verb.emits_content(), emits_content);
        assert_eq!(verb.mutates_prior_message(), mutates_prior);
    }
    assert_eq!(MESSAGE_OP_REFUSED_EVENT, "channel.message.op.refused.v1");
}

#[tokio::test]
async fn send_verb_dispatches_through_existing_send_path() {
    let channel = ChannelId("in-process://verbs".to_owned());
    let gateway = InProcessGateway::new(channel.clone());
    let operation_id = Uuid::new_v4();

    let receipt = gateway
        .dispatch_message_verb(MessageVerbRequest {
            operation_id,
            channel_id: channel.clone(),
            target: MessageTarget::User(UserRef("alice".to_owned())),
            verb: MessageVerb::Send {
                body: MessageBody::Text("hello via verb".to_owned()),
            },
            cap_token: CapTokenRef("cap-token-abc".to_owned()),
            parent_message_id: None,
        })
        .await
        .expect("send verb succeeds");

    assert_eq!(receipt.delivered_to, channel);
    let incoming = gateway.receive().await.expect("receive succeeds");
    assert_eq!(incoming.message_id, operation_id);
    assert_eq!(
        incoming.body,
        MessageBody::Text("hello via verb".to_owned())
    );
}

#[tokio::test]
async fn unsupported_verbs_refuse_with_the_stable_verb_id() {
    let gateway = InProcessGateway::new(ChannelId("in-process://verbs".to_owned()));
    let target = Uuid::new_v4();

    let err = gateway
        .dispatch_message_verb(MessageVerbRequest {
            operation_id: Uuid::new_v4(),
            channel_id: gateway.channel_id(),
            target: MessageTarget::User(UserRef("alice".to_owned())),
            verb: MessageVerb::React {
                target_message_id: target,
                emoji: "eyes".to_owned(),
            },
            cap_token: CapTokenRef("cap-token-abc".to_owned()),
            parent_message_id: None,
        })
        .await
        .expect_err("react is not implemented by the in-process gateway");

    assert!(matches!(
        err,
        GatewayError::MessageVerbUnsupported { verb } if verb == "react"
    ));
}
