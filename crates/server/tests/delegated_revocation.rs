//! Revocation through the production server assembly path, including real
//! process exits/restarts. Tokens travel only over private stdin pipes; keys
//! live only in the fixture's temporary data directory and are never logged.

mod support;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, BiscuitCapTokenIssuer, CapScope, CapToken,
    CapTokenAttenuator, CapTokenIssuer, HolderId, KeyPair,
};
use ardur_fused_runtime::SharedDenyList;
use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider};
use ardur_runtime::{CapTokenRef, RuntimeError, SessionId};
use ardur_server::{AUDIENCE, AppState, ChatSubmitError, Config, issuer_public_key};
use ardur_tool_registry::{InvocationId, ToolContext, ToolError, ToolId};
use biscuit_auth::{Algorithm, PrivateKey};
use serde_json::{Value, json};

fn configured(dir: &tempfile::TempDir) -> Config {
    let mut config = support::test_config_http_only(dir);
    config.mcp_enabled = true;
    config.mcp_bearer_tokens = vec!["local-test-mcp".to_string()];
    config
}

async fn boot(config: &Config) -> Arc<AppState> {
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    AppState::boot_configured(config, provider)
        .await
        .expect("configured server boots")
}

fn fixture_tokens(root: &Path) -> Value {
    issuer_public_key(root).expect("create the fixture issuer using server key custody");
    let key_hex = std::fs::read_to_string(root.join("keys/issuer.key")).expect("fixture key");
    let private =
        PrivateKey::from_bytes_hex(key_hex.trim(), Algorithm::Ed25519).expect("fixture key parses");
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::from(&private));
    let mint = || {
        issuer
            .issue(
                HolderId("delegation-test".to_string()),
                CapScope {
                    audience: AUDIENCE.to_string(),
                    expires_unix: 4_000_000_000,
                    budget_remaining: 10_000,
                    tool_allowlist: vec!["chat.submit".to_string(), "delegate_task".to_string()],
                },
            )
            .expect("mint fixture")
    };
    let parent = mint();
    let delegated = BiscuitCapTokenAttenuator
        .attenuate(&parent, AttenuationRule::ReduceBudget(1_000).into())
        .expect("attenuate a real delegated token");
    json!({
        "parent": parent.to_base64().expect("serialize fixture"),
        "delegated": delegated.to_base64().expect("serialize fixture"),
        "control": mint().to_base64().expect("serialize fixture"),
    })
}

async fn delegation(state: &AppState, token: &str) -> Result<(), ToolError> {
    // Call the actual assembled tool directly: an outer fused-runtime denial
    // must not conceal a delegate tool that still has a private empty list.
    let tool = state
        .mcp()
        .expect("MCP surface")
        .registry
        .get(&ToolId::new("delegate_task"))
        .expect("assembled delegate tool");
    let context = ToolContext {
        cap_token: CapTokenRef(token.to_string()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        cost_budget_cents: 1_000,
    };
    let result = tool
        .invoke(&context, json!({"goal": "local delegation probe"}))
        .await?;
    assert_eq!(result.content["outcome"], "completed");
    Ok(())
}

fn assert_revoked(result: Result<(), ToolError>) {
    match result {
        Err(ToolError::CapTokenDenied { .. }) => {}
        _ => panic!("expected typed ToolError::CapTokenDenied after revocation"),
    }
}

// This helper is selected explicitly in a subprocess. Ordinary discovery is
// a no-op; the parent below requires each subprocess to emit its completion
// marker and to exit successfully, so an empty selection cannot pass the proof.
#[test]
fn delegation_process_step() {
    let Some(mode) = std::env::var_os("ARDUR_TEST_REVOCATION_STEP") else {
        return;
    };
    let mode = mode.to_str().expect("test mode");
    let root = PathBuf::from(std::env::var_os("ARDUR_TEST_REVOCATION_ROOT").expect("test root"));
    let payload: Value = serde_json::from_reader(std::io::stdin()).expect("private fixture pipe");
    let config_dir = tempfile::tempdir().expect("config fixture");
    let mut config = configured(&config_dir);
    config.data_dir = root.clone();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let state = boot(&config).await;
            let delegated = payload["delegated"].as_str().expect("delegated fixture");
            match mode {
                "allow" => delegation(&state, delegated)
                    .await
                    .expect("pre-revocation success"),
                "revoke" => {
                    delegation(&state, delegated)
                        .await
                        .expect("success before write");
                    let parent = CapToken::from_base64(
                        payload["parent"].as_str().expect("parent fixture"),
                        state.cap_issuer_public_key(),
                    )
                    .expect("parse parent");
                    // Supported embedding writer API. It opens the exact path
                    // used by the binary, independently of the verifier handle.
                    SharedDenyList::open_file(root.join("security/deny.list"))
                        .expect("independent revoker")
                        .revoke_token(&parent)
                        .expect("durable revocation acknowledged");
                    assert_revoked(delegation(&state, delegated).await);
                }
                "denied" => assert_revoked(delegation(&state, delegated).await),
                _ => panic!("unknown test mode"),
            }
            delegation(
                &state,
                payload["control"].as_str().expect("control fixture"),
            )
            .await
            .expect("unrelated authority remains allowed");
            state.shutdown();
        });
    println!("REVOCATION_PROCESS_STEP_OK:{mode}");
}

fn process_step(root: &Path, mode: &str, payload: &Value) {
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", "delegation_process_step", "--nocapture"])
        .env_clear()
        .env("ARDUR_TEST_REVOCATION_STEP", mode)
        .env("ARDUR_TEST_REVOCATION_ROOT", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in ["PATH", "HOME", "TMPDIR", "DYLD_FALLBACK_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().expect("spawn server assembly process");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(&serde_json::to_vec(payload).expect("encode fixture"))
        .expect("write fixture through pipe");
    let start = Instant::now();
    loop {
        if child.try_wait().expect("poll child").is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(60) {
            child.kill().expect("kill timed-out child");
            child.wait().expect("reap child");
            panic!("server assembly process timed out: {mode}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = child.wait_with_output().expect("collect child");
    // Preserve useful assertion evidence, but never render stdin or a token.
    let mut diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    for field in ["parent", "delegated", "control"] {
        diagnostics = diagnostics.replace(
            payload[field].as_str().expect("fixture token"),
            "[redacted]",
        );
    }
    assert!(
        result.status.success(),
        "server assembly process failed: {mode}: {diagnostics}"
    );
    assert!(
        String::from_utf8_lossy(&result.stdout)
            .contains(&format!("REVOCATION_PROCESS_STEP_OK:{mode}")),
        "missing executed-step marker"
    );
}

#[test]
fn delegated_revocation_survives_server_process_restart() {
    let dir = tempfile::tempdir().expect("durable state fixture");
    let root = dir.path().canonicalize().expect("canonical data path");
    let tokens = fixture_tokens(&root);
    process_step(&root, "allow", &tokens);
    process_step(&root, "revoke", &tokens);
    // Both earlier processes have exited. A new server assembly must reject
    // the same attenuated token using only the persisted revocation state.
    process_step(&root, "denied", &tokens);
}

#[tokio::test]
async fn configured_boot_refuses_malformed_deny_file() {
    let dir = tempfile::tempdir().expect("fixture");
    let config = configured(&dir);
    std::fs::create_dir_all(config.data_dir.join("security")).expect("security directory");
    std::fs::write(config.data_dir.join("security/deny.list"), "not-hex\n")
        .expect("malformed fixture");
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    match AppState::boot_configured(&config, provider).await {
        Err(error) => assert!(error.to_string().contains("opening revocation deny list")),
        Ok(state) => {
            state.shutdown();
            panic!("malformed revocation state must not boot with an empty list");
        }
    }
}

#[tokio::test]
async fn running_server_and_delegate_fail_closed_when_deny_file_disappears() {
    let dir = tempfile::tempdir().expect("fixture");
    let config = configured(&dir);
    let tokens = fixture_tokens(&config.data_dir);
    let state = boot(&config).await;
    let token = tokens["delegated"].as_str().expect("delegated fixture");
    delegation(&state, token)
        .await
        .expect("pre-fault delegation succeeds");
    state
        .submit_chat("local pre-fault probe".to_string(), SessionId::new())
        .await
        .expect("pre-fault server turn succeeds");
    std::fs::remove_file(config.data_dir.join("security/deny.list")).expect("remove backing file");
    assert_revoked(delegation(&state, token).await);
    // Separately assert the fused verifier handoff: testing only the direct
    // delegate path would miss AppState::boot dropping its deny parameter.
    let result = state
        .submit_chat("must not dispatch".to_string(), SessionId::new())
        .await;
    state.shutdown();
    assert!(
        matches!(
            result,
            Err(ChatSubmitError::Runtime(RuntimeError::CapDenied { .. }))
        ),
        "missing revocation state must deny in the fused server path"
    );
}
