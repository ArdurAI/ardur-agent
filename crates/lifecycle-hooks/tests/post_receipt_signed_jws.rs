//! ARD-18: post-receipt hooks observe the signed ES256 JWS envelope, not only
//! the decoded receipt body.

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{HookError, HookId, HookRegistry, HookedRuntime, LifecycleHook};
use ardur_receipt::{Es256SigningKey, Jwks, ReceiptBody, ReceiptVerifier, Sha256Digest};
use ardur_runtime::ChatRuntime;
use async_trait::async_trait;
use parking_lot::Mutex;

use support::{EchoProvider, test_model, user_request};

#[derive(Clone, Debug)]
struct ObservedSignedReceipt {
    verified_body: ReceiptBody,
    body_from_ctx: ReceiptBody,
    kid: String,
    jws_compact: String,
}

struct VerifyingPostReceiptHook {
    jwks: Jwks,
    observed: Arc<Mutex<Option<Result<ObservedSignedReceipt, String>>>>,
}

impl VerifyingPostReceiptHook {
    fn new(jwks: Jwks) -> Self {
        Self {
            jwks,
            observed: Arc::new(Mutex::new(None)),
        }
    }

    fn observed(&self) -> Option<Result<ObservedSignedReceipt, String>> {
        self.observed.lock().clone()
    }
}

#[async_trait]
impl LifecycleHook for VerifyingPostReceiptHook {
    async fn on_post_receipt(
        &self,
        ctx: &ardur_lifecycle_hooks::PostReceiptCtx<'_>,
    ) -> Result<(), HookError> {
        let observed = ReceiptVerifier::verify(ctx.signed_receipt, &self.jwks)
            .map(|verified| ObservedSignedReceipt {
                verified_body: verified.body,
                body_from_ctx: ctx.receipt.clone(),
                kid: verified.kid,
                jws_compact: ctx.signed_receipt.jws_compact().to_string(),
            })
            .map_err(|e| e.to_string());
        *self.observed.lock() = Some(observed);
        Ok(())
    }

    fn hook_id(&self) -> HookId {
        HookId::new("verifies-signed-receipt")
    }
}

#[tokio::test]
async fn post_receipt_hook_can_verify_signed_es256_jws() {
    let receipt_key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&receipt_key.public_key());
    let hook = Arc::new(VerifyingPostReceiptHook::new(jwks));

    let mut registry = HookRegistry::new();
    registry.register(hook.clone());

    let provider = Arc::new(EchoProvider::new());
    let runtime = HookedRuntime::new(Arc::new(registry), provider, test_model())
        .with_receipt_key(receipt_key.clone());

    let outcome = runtime
        .submit(user_request("signed receipt please", "cap-ard-18"))
        .await
        .expect("turn succeeds");

    let observed = hook
        .observed()
        .expect("post-receipt hook observed the signed receipt")
        .expect("signed receipt verifies against the runtime public key");

    assert_eq!(observed.kid, receipt_key.key_id());
    assert_eq!(observed.verified_body, observed.body_from_ctx);
    assert_eq!(observed.verified_body.receipt_id, outcome.receipt_id.0);
    assert_eq!(
        observed.verified_body.payload_digest,
        Sha256Digest::of(outcome.response.content.as_bytes())
    );
    assert_eq!(
        observed.jws_compact.split('.').count(),
        3,
        "hook sees compact JWS serialization"
    );
}
