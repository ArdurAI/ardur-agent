//! A below-threshold (0.5) match, wrapped in a SanitizingFilter, yields
//! AllowWithSanitization whose sanitized text has the match `[REDACTED]`.

use ardur_injection_defense::{
    CompiledPattern, ContentSource, FlagCategory, InjectionFilter, PatternBasedFilter,
    SanitizingFilter, ScannableContent, Verdict,
};

#[tokio::test]
async fn below_threshold_match_is_sanitized() {
    // A single weak signature at 0.5, with a 0.7 block threshold: matches but
    // never blocks.
    let pattern = CompiledPattern::new(
        "soft_persona",
        r"(?i)maybe\s+pretend",
        FlagCategory::RoleHijack,
        0.5,
    )
    .expect("pattern compiles");
    let inner =
        PatternBasedFilter::with_patterns("weak", 0.7, vec![pattern]).expect("filter builds");
    let filter = SanitizingFilter::new(inner);

    let content = ScannableContent::UserMessage {
        text: "Could you maybe pretend for a moment?".to_string(),
        source: ContentSource::Direct,
    };

    let result = filter.scan(&content).await.expect("scan succeeds");

    match result.verdict {
        Verdict::AllowWithSanitization { sanitized } => {
            assert!(
                sanitized.contains("[REDACTED]"),
                "sanitized text missing redaction: {sanitized}"
            );
            assert!(
                !sanitized.to_lowercase().contains("maybe pretend"),
                "matched substring survived sanitization: {sanitized}"
            );
        }
        other => panic!("expected AllowWithSanitization, got {other:?}"),
    }
    assert!(
        !result.flags.is_empty(),
        "expected the weak flag to be recorded"
    );
}
