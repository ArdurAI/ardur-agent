//! smoke — compiles + passes on stable Rust. Asserts the public contract
//! surface exists and is name-stable. No behavior is exercised at Phase 0
//! (the `unimplemented!()` paths are never called).
use ardur_cap_token::{CapScope, CapToken, CapTokenAttenuator, CapTokenIssuer, Caveat, HolderId};

struct Dummy;
impl CapTokenIssuer for Dummy {}
impl CapTokenAttenuator for Dummy {}

#[test]
fn trait_objects_construct() {
    let _holder = HolderId("smoke".into());
    let _scope = CapScope::default();
    let _caveat = Caveat::default();

    // A rename of either trait breaks construction of the trait object here.
    let _issuer: &dyn CapTokenIssuer = &Dummy;
    let _attenuator: &dyn CapTokenAttenuator = &Dummy;

    // Name-stability for the token type without constructing a real Biscuit.
    let _token: Option<CapToken> = None;
}
