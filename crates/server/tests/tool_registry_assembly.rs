//! `tool_registry_assembly` — [`ardur_server::assemble_tool_registry`]'s
//! conditional `voice.transcribe` registration.
//!
//! `assemble_tool_registry` is the function real server boot calls to build
//! the fused runtime's tool registry (`crates/server/src/mcp.rs`); its skills
//! and remote-MCP branches are already exercised by
//! `crates/e2e-tests/tests/scenario_skill_tool.rs`, but the Whisper
//! `voice.transcribe` branch — registered only when
//! `OPENAI_WHISPER_API_KEY`/`OPENAI_API_KEY` is present, and skipped (not
//! panicking) on an invalid override — had no coverage at all.
//!
//! These mutate process-global environment; `#[serial]` alone (not an
//! additional `std::sync::Mutex` held across the `.await` below, which
//! clippy's `await_holding_lock` correctly rejects) serializes them against
//! each other within this file, and every touched variable is saved/restored.

use ardur_media_audio::VoiceTranscribeTool;
use ardur_server::assemble_tool_registry;
use ardur_tool_registry::BuiltinOpts;
use ardur_tool_registry::ToolId;
use serial_test::serial;

const TOUCHED: &[&str] = &[
    "OPENAI_WHISPER_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_WHISPER_BASE_URL",
    "OPENAI_WHISPER_MODEL",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn capture() -> Self {
        let saved = TOUCHED
            .iter()
            .map(|&name| (name, std::env::var(name).ok()))
            .collect();
        for &name in TOUCHED {
            // SAFETY: serialized by `#[serial]`; no other thread reads/writes
            // these vars concurrently for the lifetime of this guard.
            unsafe { std::env::remove_var(name) };
        }
        Self { saved }
    }

    fn set(&self, name: &str, value: &str) {
        // SAFETY: serialized by `#[serial]`, held for the guard's lifetime.
        unsafe { std::env::set_var(name, value) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            // SAFETY: serialized by `#[serial]`.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

fn has_voice_transcribe(registry: &ardur_tool_registry::ToolRegistry) -> bool {
    registry
        .get(&ToolId::new(VoiceTranscribeTool::ID))
        .is_some()
}

#[tokio::test]
#[serial]
async fn voice_transcribe_is_registered_when_a_whisper_key_is_present() {
    let _env = EnvGuard::capture();
    _env.set("OPENAI_WHISPER_API_KEY", "sk-test-whisper-key");

    let registry =
        assemble_tool_registry::<&str>("stub", "in-memory", &[], &[], BuiltinOpts::default()).await;
    assert!(
        has_voice_transcribe(&registry),
        "voice.transcribe must be registered when a Whisper key is configured"
    );
}

#[tokio::test]
#[serial]
async fn voice_transcribe_falls_back_to_the_general_openai_key() {
    let _env = EnvGuard::capture();
    _env.set("OPENAI_API_KEY", "sk-test-general-key");

    let registry =
        assemble_tool_registry::<&str>("stub", "in-memory", &[], &[], BuiltinOpts::default()).await;
    assert!(
        has_voice_transcribe(&registry),
        "OPENAI_API_KEY must be accepted when OPENAI_WHISPER_API_KEY is unset"
    );
}

#[tokio::test]
#[serial]
async fn voice_transcribe_is_absent_without_any_key() {
    let _env = EnvGuard::capture();

    let registry =
        assemble_tool_registry::<&str>("stub", "in-memory", &[], &[], BuiltinOpts::default()).await;
    assert!(
        !has_voice_transcribe(&registry),
        "voice.transcribe must not be registered without a Whisper/OpenAI key"
    );
    // The rest of the default registry must still be present — a missing key
    // degrades this one tool, not the whole boot.
    assert!(registry.get(&ToolId::new("echo")).is_some());
    assert!(registry.get(&ToolId::new("health_check")).is_some());
}

#[tokio::test]
#[serial]
async fn voice_transcribe_is_skipped_not_panicking_on_an_invalid_base_url_override() {
    let _env = EnvGuard::capture();
    _env.set("OPENAI_WHISPER_API_KEY", "sk-test-whisper-key");
    // Neither HTTPS nor loopback HTTP — `validate_base_url` must reject this,
    // and `assemble_tool_registry` must degrade gracefully (log + skip)
    // rather than panicking or aborting the rest of registry assembly.
    _env.set("OPENAI_WHISPER_BASE_URL", "http://whisper.example.com");

    let registry =
        assemble_tool_registry::<&str>("stub", "in-memory", &[], &[], BuiltinOpts::default()).await;
    assert!(
        !has_voice_transcribe(&registry),
        "an invalid base URL override must skip registration, not panic"
    );
    assert!(registry.get(&ToolId::new("echo")).is_some());
}
