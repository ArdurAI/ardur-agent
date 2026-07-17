use ardur_webhook::{EventType, WebhookError, WebhookEvent, verify_signature};
use secrecy::SecretString;

#[test]
fn test_verify_signature_round_trip() {
    let secret = SecretString::new("super-secret".into());
    let body = b"hello webhook";
    let signature = ardur_webhook::signature::sign_body(body, &secret).unwrap();
    assert!(!signature.is_empty());
    assert_eq!(signature.len(), 64); // SHA-256 hex = 64 chars

    let result = verify_signature(body, &secret, &signature);
    assert!(result.is_ok());
}

#[test]
fn test_verify_signature_bad_secret() {
    let secret = SecretString::new("super-secret".into());
    let body = b"hello webhook";
    let signature = ardur_webhook::signature::sign_body(body, &secret).unwrap();

    let bad_secret = SecretString::new("wrong-secret".into());
    let result = verify_signature(body, &bad_secret, &signature);
    assert!(matches!(
        result,
        Err(WebhookError::SignatureVerificationFailed)
    ));
}

#[test]
fn test_verify_signature_tampered_body() {
    let secret = SecretString::new("super-secret".into());
    let body = b"hello webhook";
    let signature = ardur_webhook::signature::sign_body(body, &secret).unwrap();

    let tampered = b"tampered body";
    let result = verify_signature(tampered, &secret, &signature);
    assert!(matches!(
        result,
        Err(WebhookError::SignatureVerificationFailed)
    ));
}

#[test]
fn test_verify_signature_invalid_hex() {
    let secret = SecretString::new("super-secret".into());
    let body = b"hello webhook";

    let result = verify_signature(body, &secret, "not-hex!!!");
    assert!(matches!(
        result,
        Err(WebhookError::SignatureVerificationFailed)
    ));
}

#[test]
fn test_event_creation() {
    let event = WebhookEvent::new(
        EventType::Custom("deploy".to_string()),
        "ci",
        serde_json::json!({"status": "ok"}),
    );
    assert_eq!(event.source, "ci");
    assert_eq!(event.event_type, EventType::Custom("deploy".to_string()));
}

#[test]
fn test_event_json_round_trip() {
    let event = WebhookEvent::new(
        EventType::Push,
        "github",
        serde_json::json!({"ref": "main"}),
    );

    let json = serde_json::to_string(&event).unwrap();
    let decoded: WebhookEvent = serde_json::from_str(&json).unwrap();

    assert_eq!(event.id, decoded.id);
    assert_eq!(event.source, decoded.source);
    assert_eq!(event.event_type, decoded.event_type);
}
