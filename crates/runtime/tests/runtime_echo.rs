//! §1.0 Phase 1: the in-memory runtime echoes the last user message back,
//! mints a receipt id, charges nothing, and rejects a request without a cap
//! token.

use std::sync::Arc;

use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, CostTuple, InMemoryRuntime, Role, RuntimeError,
    SessionId, SubmitRequest,
};

fn request(messages: Vec<ChatMessage>, cap_token: &str) -> SubmitRequest {
    SubmitRequest {
        messages,
        cap_token: CapTokenRef(cap_token.to_string()),
        session_id: SessionId::new(),
        requested_provider: None,
    }
}

#[tokio::test]
async fn echoes_last_user_message() {
    let runtime = InMemoryRuntime::new();
    let req = request(
        vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi"),
            ChatMessage::user("echo this"),
        ],
        "cap-abc",
    );

    let result = runtime
        .submit(req)
        .await
        .expect("echo runtime accepts a well-formed request");

    assert_eq!(result.response.role, Role::Assistant);
    assert_eq!(result.response.content, "echo this");
    assert_eq!(result.cost, CostTuple::default());
}

/// H4: `ChatRuntime` is object-safe and its `submit` future is `Send`, so a
/// turn can be driven behind an `Arc<dyn ChatRuntime>` and spawned onto a
/// runtime worker. Both facts fail to compile under the old `-> impl Future`
/// signature — this test *is* the regression guard.
#[tokio::test]
async fn submit_is_object_safe_and_send() {
    // Object safety: erase the concrete type behind a trait object.
    let runtime: Arc<dyn ChatRuntime> = Arc::new(InMemoryRuntime::new());

    // Send: `tokio::spawn` requires the task future to be `Send + 'static`, so
    // this only compiles because `submit`'s future is `Send`.
    let handle = tokio::spawn(async move {
        runtime
            .submit(request(vec![ChatMessage::user("spawned")], "cap-xyz"))
            .await
    });

    let result = handle
        .await
        .expect("the spawned turn joins")
        .expect("the echo runtime accepts the request");
    assert_eq!(result.response.content, "spawned");
}

#[tokio::test]
async fn missing_cap_token_is_rejected() {
    let runtime = InMemoryRuntime::new();
    let req = request(vec![ChatMessage::user("hi")], "");

    assert!(matches!(
        runtime.submit(req).await,
        Err(RuntimeError::CapTokenMissing)
    ));
}
