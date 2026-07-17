//! §6.0 Phase 1 — `find_by_capability` returns exactly the tools that declare
//! the queried capability.

mod common;

use ardur_tool_registry::{Capability, ToolId, ToolRegistry};
use common::CapTool;

#[test]
fn find_by_capability_returns_only_matching_tools() {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(CapTool::new("fs.read", vec![Capability::FsRead])))
        .expect("register fs.read");
    reg.register(Box::new(CapTool::new(
        "fs.write",
        vec![Capability::FsWrite],
    )))
    .expect("register fs.write");
    reg.register(Box::new(CapTool::new(
        "fs.copy",
        vec![Capability::FsRead, Capability::FsWrite],
    )))
    .expect("register fs.copy");
    reg.register(Box::new(CapTool::new(
        "net.get",
        vec![Capability::NetworkOut],
    )))
    .expect("register net.get");

    let mut found: Vec<String> = reg
        .find_by_capability(&Capability::FsRead)
        .iter()
        .map(|t| t.id().to_string())
        .collect();
    found.sort();

    assert_eq!(found, vec!["fs.copy".to_string(), "fs.read".to_string()]);
}

#[test]
fn find_by_capability_with_no_match_is_empty() {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(CapTool::new("fs.read", vec![Capability::FsRead])))
        .expect("register fs.read");

    assert!(reg.find_by_capability(&Capability::ShellExec).is_empty());
    // The matching tool still resolves by id.
    assert!(reg.get(&ToolId::new("fs.read")).is_some());
}
