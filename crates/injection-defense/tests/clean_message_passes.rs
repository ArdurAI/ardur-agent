//! "What's the weather in Tokyo?" → Allow with no flags.

use ardur_injection_defense::{
    ContentSource, InjectionFilter, PatternBasedFilter, ScannableContent, Verdict,
};

#[tokio::test]
async fn clean_message_is_allowed() {
    let filter = PatternBasedFilter::new();
    let content = ScannableContent::UserMessage {
        text: "What's the weather in Tokyo?".to_string(),
        source: ContentSource::Direct,
    };

    let result = filter.scan(&content).await.expect("scan succeeds");

    assert_eq!(result.verdict, Verdict::Allow);
    assert!(
        result.flags.is_empty(),
        "expected no flags, got {:?}",
        result.flags
    );
    assert_eq!(result.confidence, 0.0);
}
