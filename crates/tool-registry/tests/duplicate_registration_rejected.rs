//! §6.0 Phase 1 — registering a second tool under an already-used id is
//! rejected with `RegistryError::DuplicateId`, leaving the first in place.

use ardur_tool_registry::{EchoTool, RegistryError, ToolId, ToolRegistry};

#[test]
fn second_registration_of_same_id_is_rejected() {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool::new()))
        .expect("first registration succeeds");

    let err = reg
        .register(Box::new(EchoTool::new()))
        .expect_err("second registration of the same id is rejected");

    match err {
        RegistryError::DuplicateId(id) => assert_eq!(id, ToolId::new(EchoTool::ID)),
    }

    // The original registration is still resolvable.
    assert!(reg.get(&ToolId::new(EchoTool::ID)).is_some());
    assert_eq!(reg.list().len(), 1);
}
