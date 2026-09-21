//! `governance_cli_smoke` — #502 Seam B7 follow-up: the real `ardur` binary
//! wires the opt-in governance ER mirror (`ARDUR_GOVERNANCE`) into the fused
//! chat engine at session boot.
//!
//! Black-box contract (inherited from #560's `governance_mirror` suite, asserted
//! through the binary at the loopback peer):
//!
//! - default OFF: a full offline chat turn creates no `~/.ardur/governance/`;
//! - ON + one committed turn mints exactly one ER at
//!   `~/.ardur/governance/er-chain.jsonl`, signed by the SAME P-256 custody as
//!   the native receipt chain (`~/.ardur/keys/receipt.pem`);
//! - ON + a turn that never reaches the commit decision (peer 500s) mints
//!   nothing — the mirror stays empty/absent.

use std::path::Path;

use assert_cmd::Command;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The mirror log under a HOME.
fn mirror(home: &Path) -> std::path::PathBuf {
    home.join(".ardur")
        .join("governance")
        .join("er-chain.jsonl")
}

/// Non-empty mirror lines (empty when the file does not exist).
fn mirror_lines(home: &Path) -> Vec<String> {
    std::fs::read_to_string(mirror(home))
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The starter policy `ardur setup` writes — enough for chat turns to be
/// admitted (the mirror must observe a COMMITTED turn, not a denied one).
const STARTER_POLICY: &str = "permit(principal, action == Action::\"Submit\", resource);\npermit(principal, action == Action::\"ToolInvoke\", resource);\n";

/// Boot the real binary's `ardur chat` against a loopback OpenAI-compatible
/// peer, returning (stdout, stderr). `peer_status` controls whether the peer
/// completes the turn (200) or fails it (500) — a 500 abandons the turn before
/// the commit decision.
fn chat(home: &Path, base_url: &str, governance: Option<&str>, stdin: &str) -> (String, String) {
    let ardur = home.join(".ardur");
    std::fs::create_dir_all(&ardur).expect("create .ardur");
    std::fs::write(ardur.join("cedar.policies"), STARTER_POLICY).expect("write starter policy");
    let mut cmd = Command::cargo_bin("ardur").expect("the ardur binary builds");
    cmd.arg("chat")
        .arg("--plain")
        .arg("--budget-cents")
        .arg("500")
        .env("HOME", home.canonicalize().expect("canonical HOME"))
        .env("ARDUR_PROVIDER", "openai-compat")
        .env("OPENAI_COMPAT_API_KEY", "test-key")
        .env("OPENAI_COMPAT_BASE_URL", base_url)
        .env_remove("ARDUR_DEV_PERMISSIVE_POLICY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        .env_remove("ARDUR_CLI_BUDGET_CENTS")
        .env_remove("ARDUR_CLI_PER_TURN_CENTS")
        .env_remove("OLLAMA_BASE_URL");
    match governance {
        Some(value) => {
            cmd.env("ARDUR_GOVERNANCE", value);
        }
        None => {
            cmd.env_remove("ARDUR_GOVERNANCE");
        }
    }
    let output = cmd
        .write_stdin(stdin)
        .output()
        .expect("the chat process runs");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A loopback OpenAI-compatible peer that completes turns.
async fn ok_peer() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "gen-fixture",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "fixture completion"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        })))
        .mount(&server)
        .await;
    server
}

/// A loopback peer that 500s every completion — the turn fails before any
/// commit decision, so no native receipt and no ER.
async fn failing_peer() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("peer is down"))
        .mount(&server)
        .await;
    server
}

/// Default OFF: a full committed turn runs, and no governance mirror is
/// created anywhere under the HOME.
#[tokio::test]
async fn default_off_creates_no_governance_mirror() {
    let home = tempfile::tempdir().expect("temp HOME");
    let peer = ok_peer().await;

    let (stdout, stderr) = chat(home.path(), &peer.uri(), None, "hello mirror\n/quit\n");
    assert!(
        stdout.contains("fixture completion"),
        "the committed turn ran: {stdout}"
    );

    assert!(
        !home.path().join(".ardur").join("governance").exists(),
        "a default chat session must not create the governance mirror; stderr: {stderr}"
    );
}

/// ON + one committed turn: exactly one ER at the DESIGN.md convention, and it
/// verifies against the SAME receipt custody the session used.
#[tokio::test]
async fn on_with_a_committed_turn_mints_one_verifiable_er() {
    let home = tempfile::tempdir().expect("temp HOME");
    let peer = ok_peer().await;

    let (stdout, _stderr) = chat(home.path(), &peer.uri(), Some("1"), "hello mirror\n/quit\n");
    assert!(
        stdout.contains("fixture completion"),
        "the committed turn ran: {stdout}"
    );

    let lines = mirror_lines(home.path());
    assert_eq!(lines.len(), 1, "one ER per committed turn");

    // The ER must verify against the receipt key this session persisted.
    let pem = std::fs::read_to_string(home.path().join(".ardur/keys/receipt.pem"))
        .expect("the session persisted its receipt key");
    let receipt_key = ardur_receipt::Es256SigningKey::from_pkcs8_pem(&pem).expect("key parses");
    let jwks =
        ardur_governance::ErSigningKey::from_pkcs8_pem(&receipt_key.to_pkcs8_pem().expect("pem"))
            .expect("er signing key")
            .jwks();
    let chain = ardur_governance::verify_er_log_lines(&lines, &jwks)
        .expect("the ER verifies against the session's receipt custody");
    assert_eq!(chain.len(), 1);
    assert_eq!(
        chain[0].receipt().verifier_id,
        "spiffe://ardur/verifier/cli",
        "the CLI stamps its own boot-surface verifier id"
    );
    assert!(
        chain[0].receipt().parent_receipt_hash.is_none(),
        "genesis ER"
    );
}

/// ON + a peer-failing turn: the mirror never appears. The turn is abandoned
/// before the commit decision, so no native receipt and no ER.
#[tokio::test]
async fn on_with_a_failing_peer_mints_no_er() {
    let home = tempfile::tempdir().expect("temp HOME");
    let peer = failing_peer().await;

    // The CLI surfaces the provider failure; the session must still exit
    // without minting an ER.
    let _ = chat(home.path(), &peer.uri(), Some("1"), "doomed turn\n/quit\n");

    assert!(
        mirror_lines(home.path()).is_empty(),
        "an abandoned (peer-failing) turn must mint no ER"
    );
}
