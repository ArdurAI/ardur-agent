//! The conversational [`Session`]: a stable id, the capability token
//! authorizing its turns, and the ordered message history.

use crate::types::{CapTokenRef, ChatMessage, SessionId};

/// A conversational session: a stable [`SessionId`], the active
/// [`CapTokenRef`] authorizing its turns, and the ordered history of
/// user/assistant messages.
#[derive(Clone, Debug)]
pub struct Session {
    /// The session's stable identifier.
    pub id: SessionId,
    /// The capability token authorizing turns in this session.
    pub cap_token: CapTokenRef,
    history: Vec<ChatMessage>,
}

impl Session {
    /// Open a fresh session bound to `cap_token`, with an empty history.
    #[must_use]
    pub fn new(cap_token: CapTokenRef) -> Self {
        Self {
            id: SessionId::new(),
            cap_token,
            history: Vec::new(),
        }
    }

    /// Append a [`crate::Role::User`] message to the history.
    pub fn append_user(&mut self, content: impl Into<String>) {
        self.history.push(ChatMessage::user(content));
    }

    /// Append a [`crate::Role::Assistant`] message to the history.
    pub fn append_assistant(&mut self, content: impl Into<String>) {
        self.history.push(ChatMessage::assistant(content));
    }

    /// The session's message history, in order from oldest to newest.
    #[must_use]
    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }
}
