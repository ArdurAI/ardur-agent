//! §4.0 Phase 1: every `MessageBody` variant survives a JSON round-trip
//! unchanged under the adjacently-tagged representation.

use ardur_messaging_gateway::{MessageBody, UserRef};

fn assert_roundtrips(body: &MessageBody) {
    let json = serde_json::to_string(body).expect("serializes");
    let back: MessageBody = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(body, &back, "round-trip mismatch for {json}");
}

#[test]
fn each_message_body_variant_roundtrips() {
    assert_roundtrips(&MessageBody::Text("plain text".to_owned()));
    assert_roundtrips(&MessageBody::Markdown("**bold** _italic_".to_owned()));
    assert_roundtrips(&MessageBody::Attachment {
        name: "report.txt".to_owned(),
        mime_type: "text/plain".to_owned(),
        bytes: vec![0, 1, 2, 255],
    });
    assert_roundtrips(&MessageBody::Mention {
        user_ref: UserRef("bob".to_owned()),
        body: "ping".to_owned(),
    });
}
