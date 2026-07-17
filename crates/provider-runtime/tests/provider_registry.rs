//! §3.0 Phase 1 — registry register/get/list and same-id replacement.

use std::sync::Arc;

use ardur_provider_runtime::{AnthropicProvider, ModelId, ProviderId, ProviderRegistry};

#[test]
fn register_get_and_list() {
    let mut reg = ProviderRegistry::new();
    let displaced = reg.register(Arc::new(AnthropicProvider::new(
        "sk-test",
        ModelId::new("claude-opus-4-8"),
    )));
    assert!(
        displaced.is_none(),
        "first register has nothing to displace"
    );

    let id = ProviderId("anthropic".to_string());
    let got = reg.get(&id).expect("registered provider resolves by id");
    assert_eq!(got.id(), id);

    assert_eq!(reg.list(), vec![id]);
}

#[test]
fn get_unknown_returns_none() {
    let reg = ProviderRegistry::new();
    assert!(reg.get(&ProviderId("openai".to_string())).is_none());
}

#[test]
fn register_same_id_replaces_and_returns_prior() {
    let mut reg = ProviderRegistry::new();
    reg.register(Arc::new(AnthropicProvider::new("k1", ModelId::new("m1"))));
    let displaced = reg.register(Arc::new(AnthropicProvider::new("k2", ModelId::new("m2"))));

    assert!(
        displaced.is_some(),
        "re-registering the same id returns the displaced provider"
    );
    assert_eq!(reg.list().len(), 1, "the id is not duplicated");
}
