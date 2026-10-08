#![cfg(unix)]
use home_client::{FileStore, Profile, SecretStore, StoredHome};
use home_protocol::{DeviceKeys, HomePins};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

struct Probe;
static KEY: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static KEY_LEN: AtomicUsize = AtomicUsize::new(0);
static UNWIPED: AtomicUsize = AtomicUsize::new(0);
static READERS: AtomicUsize = AtomicUsize::new(0);

struct ProbeWindow;
impl Drop for ProbeWindow {
    fn drop(&mut self) {
        // Disarm on assertion failures too, before the retained key is freed.
        KEY.store(std::ptr::null_mut(), Ordering::SeqCst);
        while READERS.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
        }
    }
}

fn inspect(ptr: *mut u8, size: usize) {
    READERS.fetch_add(1, Ordering::SeqCst);
    let key = KEY.load(Ordering::SeqCst);
    let len = KEY_LEN.load(Ordering::SeqCst);
    if !key.is_null() && len > 0 && size >= len {
        // SAFETY: allocations are initialized by Probe, still live here, and
        // the marker belongs to the retained key for the entire probe window.
        let (allocation, marker) = unsafe {
            (
                std::slice::from_raw_parts(ptr, size),
                std::slice::from_raw_parts(key, len),
            )
        };
        if allocation.windows(len).any(|window| window == marker) {
            UNWIPED.fetch_add(1, Ordering::SeqCst);
        }
    }
    READERS.fetch_sub(1, Ordering::SeqCst);
}
// SAFETY: delegates ownership/layout to System. Zero initialization permits
// scanning capacity before release without reading uninitialized memory.
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        inspect(ptr, layout.size());
        // Inspect first, then wipe even the deliberate positive-control copy.
        use zeroize::Zeroize;
        unsafe { std::slice::from_raw_parts_mut(ptr, layout.size()) }.zeroize();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let new_layout = Layout::from_size_align(size, layout.align()).unwrap();
        let new_ptr = unsafe { System.alloc_zeroed(new_layout) };
        if !new_ptr.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(size)) };
            unsafe { self.dealloc(ptr, layout) };
        }
        new_ptr
    }
}
#[global_allocator]
static ALLOCATOR: Probe = Probe;

#[test]
fn load_and_error_paths_leave_no_freed_pem_copy() {
    let temp = tempfile::tempdir().unwrap();
    let store = FileStore::new(temp.path().canonicalize().unwrap().join("ardur"));
    let home = StoredHome {
        profile: Profile {
            schema_version: 1,
            url: "https://home.test".into(),
            home_name: "Home".into(),
            pins: HomePins {
                instance_id: "home".into(),
                fingerprint: "a".repeat(64),
                certificate_fingerprint: "b".repeat(64),
            },
            grant_id: "grant".into(),
            space_id: "space".into(),
        },
        private_key: DeviceKeys::generate().unwrap().private_key,
    };
    store.save(&home).unwrap();
    let path = store.path().join("paired-home.json");
    let valid = std::fs::read(&path).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&valid).unwrap();
    let mut cases = vec![(valid, true)];
    let mut invalid_profile = state.clone();
    invalid_profile["profile"]["schemaVersion"] = serde_json::json!(0);
    cases.push((serde_json::to_vec(&invalid_profile).unwrap(), false));
    let mut invalid_utf8 = state.clone();
    invalid_utf8["privateKey"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(255));
    cases.push((serde_json::to_vec(&invalid_utf8).unwrap(), false));
    let mut invalid_byte = state.clone();
    invalid_byte["privateKey"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(256));
    cases.push((serde_json::to_vec(&invalid_byte).unwrap(), false));
    let mut oversized = state.clone();
    oversized["privateKey"]
        .as_array_mut()
        .unwrap()
        .resize(4097, serde_json::json!(0));
    cases.push((serde_json::to_vec(&oversized).unwrap(), false));
    let mut legacy = state.clone();
    legacy["privateKey"] = serde_json::json!(&*home.private_key);
    cases.push((serde_json::to_vec(&legacy).unwrap(), false));
    let mut truncated = serde_json::to_vec(&state).unwrap();
    truncated.pop();
    cases.push((truncated, false));

    KEY_LEN.store(home.private_key.len(), Ordering::SeqCst);
    KEY.store(home.private_key.as_ptr().cast_mut(), Ordering::SeqCst);
    let _window = ProbeWindow;
    // Positive control proves the probe catches the original failure class.
    drop(home.private_key.as_bytes().to_vec());
    assert_eq!(UNWIPED.swap(0, Ordering::SeqCst), 1);
    store.save(&home).unwrap();
    assert_eq!(UNWIPED.load(Ordering::SeqCst), 0);
    for (raw, success) in &cases {
        std::fs::write(&path, raw).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.is_ok(), *success);
        if let Ok(loaded) = &loaded {
            assert!(
                loaded.private_key.as_bytes() == home.private_key.as_bytes(),
                "Loaded key differs."
            );
        }
        drop(loaded);
        assert_eq!(UNWIPED.load(Ordering::SeqCst), 0);
    }
}
