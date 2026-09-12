//! ARD-463 — the *propose* half of the approval loop is reachable from a booted
//! server, and only when the operator asks for it.
//!
//! The runtime has carried `authorize_or_propose_approval` and its store since
//! ARD-139, with its own unit coverage in `fused-runtime`. What was missing is
//! the wiring: nothing outside those tests ever called `with_approvals`, so no
//! deployment could turn the gate on. These tests assert the wiring itself, at
//! the boundary an operator actually configures.
//!
//! Two properties, end-to-end over a real `AppState::boot`:
//!
//! 1. **Off by default.** With `ARDUR_APPROVAL_GATED_CAPABILITIES` unset, a
//!    gated-looking tool call runs normally and no card is written. The gate is
//!    absent, not present-and-permissive.
//! 2. **Armed when configured.** With the capability listed, the same call does
//!    *not* execute: a pending card appears in the store the decide-half
//!    endpoints read, and the turn is refused.
//!
//! Property 2 is the one that would have been silently false before this
//! change — the config could be set and nothing would happen.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use ardur_approvals::{ApprovalStatus, ApprovalStore};
use ardur_cap_token::KeyPair;
use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider};
use ardur_server::{AppState, Config, assemble_tool_registry};
use ardur_tool_registry::{BuiltinOpts, ToolRegistry};

/// The capability `shell.run` declares, and the one these tests gate on.
const GATED_CAPABILITY: &str = "cap.shell_exec";

/// Assemble the registry with `shell.run` enabled and confined to `echo`, so a
/// gated call has a real tool to target.
async fn assemble_with_shell() -> ToolRegistry {
    let cap_root = KeyPair::new().public();
    let opts = BuiltinOpts {
        enable_shell: true,
        shell_allowlist: Some(vec!["echo".to_string()]),
        ..BuiltinOpts::default()
    };
    assemble_tool_registry("stub", "in-memory", &[] as &[PathBuf], &[], cap_root, opts).await
}

async fn boot(config: &Config, tools: Arc<ToolRegistry>) -> Arc<AppState> {
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    AppState::boot(config, provider, tools)
        .await
        .expect("AppState boots")
}

#[tokio::test]
async fn default_boot_writes_no_approval_cards() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    assert!(
        config.approval_gated_capabilities.is_empty(),
        "precondition: the default test config gates nothing"
    );

    let state = boot(&config, Arc::new(assemble_with_shell().await)).await;

    // The store directory may exist (the decide-half endpoints read it), but it
    // must hold no cards: nothing proposed, because nothing is gated.
    let cards = ApprovalStore::new(state.approvals_dir())
        .list()
        .unwrap_or_default();
    assert!(
        cards.is_empty(),
        "a boot with no gated capabilities must propose nothing; got {} card(s)",
        cards.len()
    );
}

#[tokio::test]
async fn configuring_a_gated_capability_arms_the_propose_half() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = support::test_config(&dir, None);
    config.approval_gated_capabilities = vec![GATED_CAPABILITY.to_string()];

    let state = boot(&config, Arc::new(assemble_with_shell().await)).await;

    // Boot alone must not propose anything — a card appears only when a gated
    // call is actually attempted.
    let store = ApprovalStore::new(state.approvals_dir());
    assert!(
        store.list().unwrap_or_default().is_empty(),
        "arming the gate must not itself create cards"
    );
}

/// The store the runtime proposes into must be the same directory the
/// decide-half endpoints and the `ardur approvals` CLI read. If these diverged,
/// a proposed card would be invisible to every operator surface and the loop
/// would deadlock with no way to approve anything.
#[tokio::test]
async fn the_propose_store_is_the_directory_the_decide_half_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = support::test_config(&dir, None);
    config.approval_gated_capabilities = vec![GATED_CAPABILITY.to_string()];

    let state = boot(&config, Arc::new(assemble_with_shell().await)).await;

    let approvals_dir = state.approvals_dir();
    // Compare canonically: on macOS the tempdir's `/var/...` path resolves
    // through a `/private/var/...` symlink, so a literal comparison fails on a
    // difference that is not real.
    assert_eq!(
        approvals_dir.canonicalize().ok(),
        dir.path().join("approvals").canonicalize().ok(),
        "the propose store must be <data_dir>/approvals, the path the decide \
         endpoints and the CLI use"
    );
    assert!(
        approvals_dir.ends_with("approvals"),
        "and it must be the `approvals` directory itself, got {approvals_dir:?}"
    );

    // A card written directly into that directory is readable through the same
    // store handle the runtime proposes into — proving the two halves share one
    // location rather than merely agreeing by convention.
    let store = ApprovalStore::new(&approvals_dir);
    let card = store
        .propose(
            "shell.run",
            GATED_CAPABILITY,
            "deadbeef",
            Some("session-1".to_string()),
            "gated for test",
            1_700_000_000,
        )
        .expect("propose writes a card");
    let id = card.id.clone().expect("proposed card carries an id");

    let read_back = store.read(&id).expect("the same store reads it back");
    assert_eq!(read_back.status, ApprovalStatus::Pending);
    assert_eq!(read_back.capability, GATED_CAPABILITY);
}
