//! smoke — compiles + passes on stable Rust. Asserts the public contract
//! surface exists and is name-stable. No behavior is exercised at Phase 0.
use ardur_cedar_policy::{Decision, PolicyBundle};

struct Placeholder;
impl PolicyBundle for Placeholder {}

#[test]
fn trait_objects_construct() {
    // `load` carries `where Self: Sized`, so the trait stays object-safe.
    let _bundle: &dyn PolicyBundle = &Placeholder;

    // Name-stability for the crate's own decision type.
    let _decision: Option<Decision> = None;
}
