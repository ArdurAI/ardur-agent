//! smoke — compiles + passes on stable Rust. Asserts the public contract
//! surface exists and is name-stable. No behavior is exercised at Phase 0.
use ardur_receipt::{ChainHash, ReceiptBody, ReceiptChain, ReceiptSigner, SignedReceipt};

struct Dummy;
impl ReceiptSigner for Dummy {}
impl ReceiptChain for Dummy {}

#[test]
fn trait_objects_construct() {
    let hash = ChainHash([0u8; 32]);
    let _body = ReceiptBody {
        verb: "workspace.scaffold.completed.v1".into(),
        parent_hash: Some(hash),
    };

    let _signer: &dyn ReceiptSigner = &Dummy;
    let _chain: &dyn ReceiptChain = &Dummy;

    // SignedReceipt is constructed only through the signer; assert name only.
    let _signed: Option<SignedReceipt> = None;
}
