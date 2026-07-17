//! ARD-49 — compile-time proof that [`FusedRuntime`] (and its `submit` future)
//! are `Send`.
//!
//! Before this change the lifecycle-hook registry was `#[async_trait(?Send)]`,
//! so awaiting it made `FusedRuntime::submit`'s future `!Send` — it could not be
//! driven on a work-stealing executor (an axum handler) without a current-thread
//! worker bridge. Flipping `LifecycleHook` to `Send` futures (and tightening
//! `ErrorCtx`'s error to `Send + Sync`) restores `Send`. This test fails to
//! *compile* if that regresses; there is nothing to run.

use std::future::Future;

use ardur_fused_runtime::FusedRuntime;
use ardur_runtime::{RuntimeError, SubmitRequest, SubmitResult};

fn assert_send<T: Send>() {}

#[test]
fn fused_runtime_is_send() {
    // The struct is `Send` — safe to move across threads / store in `Arc` shared
    // state behind an axum handler.
    assert_send::<FusedRuntime>();
}

/// The `submit` future is `Send`. Spelled as a generic bound rather than called,
/// so this only has to *type-check*. If awaiting the hook registry ever makes
/// the future `!Send` again, this fails to compile.
#[allow(dead_code)]
fn submit_future_is_send<F>(_fut: F)
where
    F: Future<Output = Result<SubmitResult, RuntimeError>> + Send,
{
}

#[allow(dead_code)]
fn submit_future_send_witness(rt: &FusedRuntime, req: SubmitRequest) {
    use ardur_runtime::ChatRuntime;
    submit_future_is_send(rt.submit(req));
}
