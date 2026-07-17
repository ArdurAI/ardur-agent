//! §1.0 Phase 1: a session records its cap token and accumulates
//! user/assistant messages in order; distinct sessions get distinct ids.

use ardur_runtime::{CapTokenRef, Role, Session};

#[test]
fn appends_messages_in_order() {
    let mut session = Session::new(CapTokenRef("cap-xyz".to_string()));
    assert!(session.history().is_empty());
    assert_eq!(session.cap_token, CapTokenRef("cap-xyz".to_string()));

    session.append_user("what is the time?");
    session.append_assistant("4 o'clock");
    session.append_user("thanks");

    let history = session.history();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[0].content, "what is the time?");
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[1].content, "4 o'clock");
    assert_eq!(history[2].role, Role::User);
    assert_eq!(history[2].content, "thanks");
}

#[test]
fn sessions_have_distinct_ids() {
    let a = Session::new(CapTokenRef("cap-a".to_string()));
    let b = Session::new(CapTokenRef("cap-b".to_string()));
    assert_ne!(a.id, b.id);
}
