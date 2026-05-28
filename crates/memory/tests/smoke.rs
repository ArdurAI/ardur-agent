//! smoke — compiles + passes on stable Rust. Asserts the public contract
//! surface exists and is name-stable. No behavior is exercised at Phase 0.
use ardur_memory::{EntityRef, MemoryRecord, MemoryRuntime, RecordId};

struct DummyRuntime;
impl MemoryRuntime for DummyRuntime {}

#[test]
fn trait_objects_construct() {
    let _entity = EntityRef("session:smoke".into());
    let _record_id = RecordId("record:smoke".into());

    // Both traits are object-safe (all receivers are &self / &mut self).
    let _runtime: &dyn MemoryRuntime = &DummyRuntime;
    let _record: Option<Box<dyn MemoryRecord>> = None;
}
