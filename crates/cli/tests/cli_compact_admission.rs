//! gh#533 black-box guard tests: `/compact` (apply AND preview) and
//! `/background` must reach the provider ONLY through the same Cedar + cost
//! admission an ordinary chat turn passes.
//!
//! Each scenario drives the REAL `ardur chat --plain` binary against an
//! OpenAI-compatible HTTP fixture bound to loopback — no inherited
//! credentials, a canonical disposable HOME — and counts requests at the
//! PEER (the fixture), not process exits. The matrix mirrors the issue's
//! executed contrast:
//!
//! | State                       | ordinary chat | `/compact preview` | `/compact` | `/background` |
//! |-----------------------------|---------------|--------------------|------------|---------------|
//! | no Cedar policy (deny-all)  | 0 (policy)    | 0 (policy)         | 0 (policy) | 0 (policy)    |
//! | starter policy, 0 budget    | 0 (cost)      | 0 (cost)           | 0 (cost)   | 0 (cost)      |
//! | starter policy, funded      | >=1           | 1                  | 1          | 1             |
//!
//! The funded row is the positive control: an authorized session really
//! reaches the peer, so the denials cannot be vacuous.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::Command;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A loopback OpenAI-compatible fixture counting requests at the peer.
struct PeerFixture {
    server: MockServer,
    hits: Arc<AtomicUsize>,
}

impl PeerFixture {
    async fn start() -> Self {
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(move |_request: &wiremock::Request| {
                counter.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "gen-fixture",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "fixture completion"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
                }))
            })
            .mount(&server)
            .await;
        Self { server, hits }
    }

    fn request_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// The starter policy `ardur setup` writes — chat + tools + the gh#533
/// control-plane actions.
const STARTER_POLICY: &str = "permit(principal, action == Action::\"Submit\", resource);\n\
permit(principal, action == Action::\"ToolInvoke\", resource);\n\
permit(principal, action == Action::\"ContextCompact\", resource);\n\
permit(principal, action == Action::\"TaskBackground\", resource);\n";

/// Boot the real binary against the loopback fixture. `policy` writes a
/// cedar.policies file when `Some`; `None` leaves the install without one
/// (the fail-closed deny-all default).
fn chat_against_fixture(
    home: &std::path::Path,
    base_url: &str,
    policy: Option<&str>,
    budget_cents: u64,
    stdin: &str,
) -> (String, String) {
    if let Some(policy) = policy {
        let ardur = home.join(".ardur");
        std::fs::create_dir_all(&ardur).expect("create .ardur");
        std::fs::write(ardur.join("cedar.policies"), policy).expect("write policy");
    }
    let mut cmd = Command::cargo_bin("ardur").expect("the ardur binary builds");
    cmd.arg("chat")
        .arg("--plain")
        .arg("--budget-cents")
        .arg(budget_cents.to_string())
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
    let output = cmd
        .write_stdin(stdin)
        .output()
        .expect("the chat process runs");
    assert!(
        output.status.success(),
        "exit: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// No Cedar policy file at all: the fail-closed deny-all default. Ordinary
/// chat, `/compact preview`, `/compact`, and `/background` are ALL denied
/// with the policy reason, and the peer sees ZERO requests.
#[tokio::test]
async fn missing_policy_denies_chat_compact_preview_and_background_at_the_peer() {
    let fixture = PeerFixture::start().await;
    let home = tempfile::tempdir().expect("temp HOME");

    let (stdout, _stderr) = chat_against_fixture(
        home.path(),
        &fixture.server.uri(),
        None,
        1000,
        "hello\n/compact preview\n/compact\n/background do a thing\n/quit\n",
    );

    assert_eq!(
        fixture.request_count(),
        0,
        "no peer request may leave under the deny-all default; stdout: {stdout}"
    );
    assert!(
        stdout.contains("policy denied"),
        "the denial reason must be surfaced for chat: {stdout}"
    );
}

/// The starter policy with a ZERO budget: everything is denied with the cost
/// reason, and the peer sees ZERO requests — the exact `--budget-cents 0`
/// contrast from the issue.
#[tokio::test]
async fn zero_budget_denies_compact_preview_and_background_at_the_peer() {
    let fixture = PeerFixture::start().await;
    let home = tempfile::tempdir().expect("temp HOME");

    let (stdout, _stderr) = chat_against_fixture(
        home.path(),
        &fixture.server.uri(),
        Some(STARTER_POLICY),
        0,
        "hello\n/compact preview\n/compact\n/background do a thing\n/quit\n",
    );

    assert_eq!(
        fixture.request_count(),
        0,
        "no peer request may leave under a zero budget; stdout: {stdout}"
    );
    assert!(
        stdout.contains("cost ceiling exceeded"),
        "the cost denial reason must be surfaced: {stdout}"
    );
}

/// Positive control: the starter policy and a funded budget really reach the
/// peer — chat turns, `/compact`, `/compact preview`, and `/background` each
/// dispatch exactly once. Without this row the denials above prove nothing.
#[tokio::test]
async fn funded_starter_session_reaches_the_peer_for_all_four_paths() {
    let fixture = PeerFixture::start().await;
    let home = tempfile::tempdir().expect("temp HOME");

    let (stdout, _stderr) = chat_against_fixture(
        home.path(),
        &fixture.server.uri(),
        Some(STARTER_POLICY),
        1000,
        // The /compact status lines are purely local (no provider call); they
        // give the concurrently-spawned background task time to dispatch
        // before /quit tears the process down.
        &format!(
            "hello\n/compact\n/compact preview\n/background do a thing\n{}\n/tasks\n/quit\n",
            "/compact status\n".repeat(200)
        ),
    );

    assert_eq!(
        fixture.request_count(),
        4,
        "chat + compact + preview + background each dispatch exactly once; stdout: {stdout}"
    );
    assert!(
        stdout.contains("compacted: checkpoint"),
        "the applied compaction should be confirmed: {stdout}"
    );
    assert!(
        stdout.contains("fixture completion"),
        "the preview and chat outputs should surface the fixture's completion: {stdout}"
    );
    assert!(
        stdout.contains("started background task"),
        "the background task should be confirmed: {stdout}"
    );
}

/// An explicit operator forbid of the control action: chat still works
/// (Submit is permitted), but compaction is policy-denied — the actions are
/// separable, which is the point of distinct Cedar actions.
#[tokio::test]
async fn forbidding_the_control_action_denies_compact_but_not_chat() {
    let fixture = PeerFixture::start().await;
    let home = tempfile::tempdir().expect("temp HOME");
    let policy = format!(
        "{STARTER_POLICY}forbid(principal, action == Action::\"ContextCompact\", resource);\n"
    );

    let (stdout, _stderr) = chat_against_fixture(
        home.path(),
        &fixture.server.uri(),
        Some(&policy),
        1000,
        "hello\n/compact preview\n/quit\n",
    );

    assert_eq!(
        fixture.request_count(),
        1,
        "the chat turn dispatches; the denied compaction does not; stdout: {stdout}"
    );
    assert!(
        stdout.contains("fixture completion"),
        "the chat turn's completion should be printed: {stdout}"
    );
    assert!(
        stdout.contains("policy denied"),
        "the compact denial must surface the policy reason: {stdout}"
    );
}
