//! Frozen pre-refactor REPL bytes; see fixtures/m0/README.md for provenance.

#[path = "support/m0_cases.rs"]
mod cases;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use ardur_cli::{RenderCtx, Theme, ThemeName, TurnStats, drive_fused_turn, render_cost_line};
use ardur_fused_runtime::{FusedEvent, StageKind};
use ardur_runtime::RuntimeError;
use serde::{Deserialize, Serialize};

const BASE: &str = "f3ed034ce7dd9f0aaf9df9890c89e4ed33f84b1f";

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Snapshot {
    transcript: String,
    outcome_debug: Option<String>,
}

fn themes() -> [(String, Theme); 4] {
    [
        ("plain".into(), Theme::named(ThemeName::Night).plain()),
        ("night".into(), Theme::named(ThemeName::Night)),
        ("dawn".into(), Theme::named(ThemeName::Dawn)),
        ("terminal".into(), Theme::named(ThemeName::Terminal)),
    ]
}

// Only the decimal elapsed-seconds token on the final usage line is normalized.
// ANSI, rules, whitespace, cost, and all other bytes remain untouched. A duration
// long enough to change the rule width deliberately fails instead of hiding drift.
fn normalize_elapsed(output: &str) -> String {
    let body = output
        .strip_suffix('\n')
        .expect("cost line ends in one newline");
    let line_start = body.rfind('\n').map_or(0, |index| index + 1);
    let last = &body[line_start..];
    let re = regex::Regex::new(r"( · )(\d+\.\d)(s )").unwrap();
    assert_eq!(
        re.captures_iter(last).count(),
        1,
        "usage elapsed token: {last:?}"
    );
    let captures = re.captures(last).unwrap();
    let elapsed = captures.get(2).unwrap();
    let mut normalized = output.to_string();
    normalized.replace_range(
        line_start + elapsed.start()..line_start + elapsed.end(),
        "<elapsed>",
    );
    normalized
}

#[test]
fn elapsed_normalization_preserves_every_other_byte() {
    let raw = "\r\x1b[2Ktext\n\n─\x1b[38;5;242m 3 tokens in · 2 out · $0.3100 · 1.4s \x1b[0m─\n";
    assert_eq!(
        normalize_elapsed(raw),
        "\r\x1b[2Ktext\n\n─\x1b[38;5;242m 3 tokens in · 2 out · $0.3100 · <elapsed>s \x1b[0m─\n"
    );
    assert!(
        std::panic::catch_unwind(|| normalize_elapsed(&(raw.to_string() + "\n"))).is_err(),
        "an extra newline must not be hidden"
    );
}

#[tokio::test]
async fn frozen_repl_transcripts() {
    let mut actual = BTreeMap::new();
    let mut raw = BTreeMap::new();
    for (theme_name, theme) in themes() {
        for (name, events) in cases::cases() {
            let mut bytes = Vec::new();
            let outcome = drive_fused_turn(
                futures::stream::iter(events),
                &mut bytes,
                &RenderCtx::new(&theme, 80),
            )
            .await
            .unwrap();
            let output = String::from_utf8(bytes).unwrap();
            let key = format!("{theme_name}/{name}");
            raw.insert(key.clone(), output.clone());
            let transcript = if outcome.usage.is_some() {
                normalize_elapsed(&output)
            } else {
                output
            };
            actual.insert(
                key,
                Snapshot {
                    transcript,
                    outcome_debug: Some(format!("{outcome:#?}")),
                },
            );
        }
        for (name, cents, elapsed) in [("zero", 0, 0), ("paid", 31, 1400), ("long", 234, 123_400)] {
            let stats = TurnStats {
                tokens_in: 421,
                tokens_out: 187,
                cost_dollars: cents as f64 / 100.0,
                elapsed: Duration::from_millis(elapsed),
                context_frac: None,
            };
            actual.insert(
                format!("{theme_name}/fixed_cost_{name}"),
                Snapshot {
                    transcript: format!("{}\n", render_cost_line(&stats, &theme, 80)),
                    outcome_debug: None,
                },
            );
        }
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/m0/baseline.json");
    if std::env::var("ARDUR_REGENERATE_M0_GOLDENS").as_deref() == Ok("1") {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must never regenerate goldens"
        );
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(head.status.success());
        assert_eq!(
            String::from_utf8(head.stdout).unwrap().trim(),
            BASE,
            "regenerate on the frozen base only"
        );
        let diff = std::process::Command::new("git")
            .args([
                "diff",
                "--exit-code",
                BASE,
                "--",
                "crates/cli/src",
                "crates/runtime/src/error.rs",
                "crates/fused-runtime/src/streaming.rs",
                "Cargo.toml",
                "Cargo.lock",
                "crates/cli/Cargo.toml",
            ])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            diff.status.success(),
            "production must match the pre-M0 base"
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&actual).unwrap() + "\n").unwrap();
        if let Some(raw_path) = std::env::var_os("ARDUR_M0_RAW_TRANSCRIPTS") {
            std::fs::write(raw_path, serde_json::to_string_pretty(&raw).unwrap() + "\n").unwrap();
        }
    }
    let expected: BTreeMap<String, Snapshot> = serde_json::from_slice(
        &std::fs::read(path).expect("frozen baseline fixture (explicit regeneration only)"),
    )
    .unwrap();
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "fixture inventory drift"
    );
    for (key, snapshot) in actual {
        assert_eq!(snapshot, expected[&key], "frozen transcript {key}");
    }
}

#[derive(Default)]
struct Writes {
    bytes: Vec<u8>,
    flushed: Vec<Vec<u8>>,
}

struct ObservedWriter(Rc<RefCell<Writes>>);
impl Write for ObservedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.0.borrow_mut();
        let bytes = state.bytes.clone();
        state.flushed.push(bytes);
        Ok(())
    }
}

#[tokio::test]
async fn content_is_flushed_before_the_next_source_poll_including_empty_chunks() {
    let state = Rc::new(RefCell::new(Writes::default()));
    let mut writer = ObservedWriter(state.clone());
    let mut index = 0;
    let stream = futures::stream::poll_fn(|_| {
        let expected: &[&[u8]] = match index {
            0 => &[],
            1 => &[b"first"],
            2 => &[b"first", b"first"],
            _ => &[b"first", b"first", b"firstsecond"],
        };
        assert_eq!(
            state.borrow().flushed,
            expected,
            "flush before poll {index}"
        );
        let item = match index {
            0 => Some(cases::content("first")),
            1 => Some(cases::content("")),
            2 => Some(cases::content("second")),
            _ => None,
        };
        index += 1;
        Poll::Ready(item)
    });
    let theme = Theme::named(ThemeName::Night).plain();
    drive_fused_turn(stream, &mut writer, &RenderCtx::new(&theme, 80))
        .await
        .unwrap();
    assert_eq!(index, 4);
    assert_eq!(state.borrow().bytes, b"firstsecond\n");
}

#[tokio::test]
async fn pending_source_pulses_two_frames_then_empty_content_clears_and_flushes() {
    let state = Rc::new(RefCell::new(Writes::default()));
    let mut writer = ObservedWriter(state.clone());
    let mut delivered = false;
    let stream = futures::stream::poll_fn(|_| {
        if delivered {
            return Poll::Ready(None);
        }
        if state.borrow().flushed.len() < 2 {
            return Poll::Pending;
        }
        delivered = true;
        Poll::Ready(Some(cases::content("")))
    });
    let theme = Theme::named(ThemeName::Night);
    // Real interval, event-gated frame count: no wall-clock speed claim. No
    // test-util feature or production clock seam is needed for this contract.
    tokio::time::timeout(
        Duration::from_secs(5),
        drive_fused_turn(stream, &mut writer, &RenderCtx::new(&theme, 80)),
    )
    .await
    .unwrap()
    .unwrap();
    let state = state.borrow();
    assert_eq!(
        state.bytes,
        "\r\x1b[2K\x1b[38;5;242m·\x1b[0m\r\x1b[2K\x1b[38;5;242m··\x1b[0m\r\x1b[2K".as_bytes()
    );
    assert_eq!(state.flushed.len(), 4, "two frames, clear, empty chunk");
}

#[tokio::test]
async fn typing_dot_writer_failure_does_not_poll_source_again() {
    struct FailureObservedWriter {
        inner: FailingWriter,
        failed: Rc<std::cell::Cell<bool>>,
    }
    impl Write for FailureObservedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let result = self.inner.write(bytes);
            self.failed.set(result.is_err());
            result
        }
        fn flush(&mut self) -> io::Result<()> {
            let result = self.inner.flush();
            self.failed.set(result.is_err());
            result
        }
    }
    for fail_flush in [false, true] {
        let failed = Rc::new(std::cell::Cell::new(false));
        let mut polls = 0;
        let stream = futures::stream::poll_fn(|_| {
            assert!(!failed.get(), "poll after typing-dot I/O failure");
            polls += 1;
            Poll::<Option<cases::SourceItem>>::Pending
        });
        let theme = Theme::named(ThemeName::Night);
        let error = drive_fused_turn(
            stream,
            &mut FailureObservedWriter {
                inner: FailingWriter { fail_flush },
                failed: failed.clone(),
            },
            &RenderCtx::new(&theme, 80),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.kind(),
            if fail_flush {
                io::ErrorKind::ConnectionReset
            } else {
                io::ErrorKind::BrokenPipe
            }
        );
        assert!(failed.get());
        assert!(polls >= 1);
    }
}

#[tokio::test]
async fn source_is_never_polled_after_runtime_error() {
    let mut polls = 0;
    let stream = futures::stream::poll_fn(|_| {
        polls += 1;
        assert_eq!(polls, 1, "source polled after terminal error");
        Poll::Ready(Some(Err(RuntimeError::ProviderUnavailable)))
    });
    let mut bytes = Vec::new();
    let theme = Theme::named(ThemeName::Night).plain();
    let outcome = drive_fused_turn(stream, &mut bytes, &RenderCtx::new(&theme, 80))
        .await
        .unwrap();
    assert_eq!(polls, 1);
    assert_eq!(outcome.error.as_deref(), Some("provider unavailable"));
}

struct FailingWriter {
    fail_flush: bool,
}
impl Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_flush {
            Ok(bytes.len())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "fixture write failure",
            ))
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "fixture flush failure",
        ))
    }
}

#[tokio::test]
async fn write_and_flush_failures_propagate_without_another_source_poll() {
    for styled in [false, true] {
        for fail_flush in [false, true] {
            let mut polls = 0;
            let stream = futures::stream::poll_fn(|_| {
                polls += 1;
                assert_eq!(polls, 1, "source polled after writer failure");
                Poll::Ready(Some(cases::content("first")))
            });
            let theme = Theme::named(ThemeName::Night);
            let theme = if styled { theme } else { theme.plain() };
            let error = drive_fused_turn(
                stream,
                &mut FailingWriter { fail_flush },
                &RenderCtx::new(&theme, 80),
            )
            .await
            .unwrap_err();
            assert_eq!(
                error.kind(),
                if fail_flush {
                    io::ErrorKind::ConnectionReset
                } else {
                    io::ErrorKind::BrokenPipe
                }
            );
            assert_eq!(polls, 1);
        }
    }
}

#[tokio::test]
async fn only_visible_events_clear_typing_dots_even_for_unknown_results() {
    for visible in [
        cases::content(""),
        cases::result("unknown"),
        Ok(FusedEvent::Finish(
            ardur_provider_runtime::FinishReason::Stop,
        )),
        Err(RuntimeError::CapTokenMissing),
    ] {
        let state = Rc::new(RefCell::new(Writes::default()));
        let mut writer = ObservedWriter(state.clone());
        let mut invisible = vec![
            Ok(FusedEvent::StageStart {
                stage: StageKind::CedarCheck,
            }),
            Ok(FusedEvent::StageEnd {
                stage: StageKind::CedarCheck,
                ok: true,
            }),
            cases::start("a", "read"),
            cases::delta("a", "{}"),
            cases::receipt(1, 0),
        ]
        .into_iter();
        let mut visible = Some(visible);
        let stream = futures::stream::poll_fn(|_| {
            if let Some(event) = invisible.next() {
                assert!(
                    state.borrow().bytes.is_empty(),
                    "invisible events must not erase waiting line"
                );
                return Poll::Ready(Some(event));
            }
            if let Some(event) = visible.take() {
                assert!(state.borrow().bytes.is_empty());
                Poll::Ready(Some(event))
            } else {
                assert!(state.borrow().flushed[0].starts_with(b"\r\x1b[2K"));
                Poll::Ready(None)
            }
        });
        drive_fused_turn(
            stream,
            &mut writer,
            &RenderCtx::new(&Theme::named(ThemeName::Night), 80),
        )
        .await
        .unwrap();
        assert_eq!(state.borrow().flushed[0], b"\r\x1b[2K");
    }
}
