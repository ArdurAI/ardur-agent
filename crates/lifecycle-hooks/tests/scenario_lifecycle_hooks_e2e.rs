//! §11.17 E2E scenario — the full redaction chain across runtime +
//! provider-runtime + receipt + memory.
//!
//! A `RedactingHook` replaces user-message `SECRET` → `[REDACTED]` at
//! pre-submit. We then assert, end to end, that:
//!   (a) the provider received the redacted request (never the original),
//!   (b) the minted receipt's payload digest covers the redacted text — and is
//!       NOT the digest of the original `SECRET` text,
//!   (c) the `RecordingHook` observed both a pre-submit and a post-receipt
//!       event, and
//!   (d) the memory write persisted the redacted response, not the secret.
//!
//! (Lane C's `crates/e2e-tests` has not merged, so this scenario lives in the
//! owning crate's `tests/` per the §11.17 task contract.)

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{HookEvent, HookRegistry, HookedRuntime, RecordingHook};
use ardur_memory::{HolderId, InMemoryMemoryRuntime, MemoryRuntime, UnixTsMillis};
use ardur_receipt::Sha256Digest;
use ardur_runtime::ChatRuntime;

use support::{CapturingPostReceiptHook, EchoProvider, RedactingHook, test_model, user_request};

const ORIGINAL: &str = "tell me the SECRET please";
const REDACTED: &str = "tell me the [REDACTED] please";

#[tokio::test]
async fn redaction_flows_through_provider_receipt_and_memory() {
    let provider = Arc::new(EchoProvider::new());
    let memory = Arc::new(InMemoryMemoryRuntime::new());

    // Redactor runs first (priority -10); capturer + recorder observe.
    let redactor = Arc::new(RedactingHook::new("redactor", -10));
    let capturer = Arc::new(CapturingPostReceiptHook::new("capturer"));
    let recorder = Arc::new(RecordingHook::new("recorder"));
    let log = recorder.event_log();
    let capturer_handle = capturer.clone();

    let mut registry = HookRegistry::new();
    registry.register(redactor);
    registry.register(capturer);
    registry.register(recorder);

    let runtime = HookedRuntime::new(Arc::new(registry), provider.clone(), test_model())
        .with_memory(memory.clone());

    let req = user_request(ORIGINAL, "cap-e2e");
    let session_id = req.session_id;

    let result = runtime.submit(req).await.expect("redacted turn succeeds");

    // (a) The provider saw the redacted request — and never the secret.
    assert_eq!(provider.call_count(), 1);
    let seen = provider.last_request().expect("provider saw a request");
    assert_eq!(seen.messages[0].content, REDACTED);
    assert!(!seen.messages[0].content.contains("SECRET"));

    // The echoed assistant response is the redacted text.
    assert_eq!(result.response.content, REDACTED);

    // (b) The receipt's payload digest covers the redacted text, not the
    //     original secret.
    let captured = capturer_handle
        .captured()
        .expect("post-receipt hook captured the receipt");
    let redacted_digest = Sha256Digest::of(REDACTED.as_bytes()).to_hex();
    let original_digest = Sha256Digest::of(ORIGINAL.as_bytes()).to_hex();
    assert_eq!(
        captured.payload_digest_hex, redacted_digest,
        "receipt digest must cover the redacted text"
    );
    assert_ne!(
        captured.payload_digest_hex, original_digest,
        "receipt digest must NOT cover the original secret"
    );
    assert!(captured.response_content.contains("[REDACTED]"));
    assert!(!captured.response_content.contains("SECRET"));

    // (c) The recorder saw both a pre-submit and a post-receipt event.
    let events = log.lock().clone();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HookEvent::OnPreSubmit { .. })),
        "recorder must have seen a pre-submit event"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HookEvent::OnPostReceipt { .. })),
        "recorder must have seen a post-receipt event"
    );

    // (d) Memory persisted the redacted response, not the secret.
    let subject = HolderId(session_id.0.to_string());
    let records = memory.at_time(&subject, UnixTsMillis(u64::MAX - 1));
    assert_eq!(records.len(), 1, "exactly one turn record was written");
    let stored = records[0].payload["response"]
        .as_str()
        .expect("the record carries the response text");
    assert_eq!(stored, REDACTED);
    assert!(!stored.contains("SECRET"));
}
