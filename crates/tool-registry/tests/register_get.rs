//! §6.0 Phase 1 — register an `EchoTool`, then resolve it by id.

use ardur_tool_registry::{EchoTool, ToolId, ToolRegistry};

#[test]
fn register_then_get_finds_the_tool() {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool::new()))
        .expect("first registration succeeds");

    let tool = reg
        .get(&ToolId::new(EchoTool::ID))
        .expect("the registered tool resolves by id");

    assert_eq!(tool.id(), ToolId::new(EchoTool::ID));
}

#[test]
fn get_missing_id_returns_none() {
    let reg = ToolRegistry::new();
    assert!(reg.get(&ToolId::new("nope")).is_none());
}
