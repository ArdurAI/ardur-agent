//! §6.0 Phase 1 — a tool's `required_capabilities` reports exactly what it was
//! built to declare.

mod common;

use ardur_tool_registry::{Capability, Tool};
use common::CapTool;

#[test]
fn required_capabilities_reports_declared_set() {
    let tool = CapTool::new("fs.writer", vec![Capability::FsWrite]);
    assert_eq!(tool.required_capabilities(), &[Capability::FsWrite]);
}

#[test]
fn a_tool_can_declare_multiple_capabilities() {
    let tool = CapTool::new("fs.copy", vec![Capability::FsRead, Capability::FsWrite]);
    assert_eq!(
        tool.required_capabilities(),
        &[Capability::FsRead, Capability::FsWrite]
    );
}

#[test]
fn a_capability_free_tool_declares_none() {
    let tool = CapTool::new("noop", vec![]);
    assert!(tool.required_capabilities().is_empty());
}
