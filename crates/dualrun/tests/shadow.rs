//! Shadow-mode guarantees (gh#502 B4).
//!
//! The load-bearing property is negative: the AAT path must **never** change a
//! decision while in shadow mode, including when it is stricter and would look
//! like the safer choice. A comparator that quietly lets the shadow path win is
//! no longer shadow mode, and its agreement statistics would be measuring a
//! system nobody is running.

use ardur_dualrun::{
    AatVerdict, GateState, LegacyVerdict, MISMATCH_VERB, MismatchDirection, PROMOTION_MIN_SECONDS,
    PROMOTION_MIN_STEPS, Path, PathVerdict, ShadowComparator, Verdict,
};

const T0: u64 = 1_800_000_000;
const WEEK: u64 = PROMOTION_MIN_SECONDS;

fn legacy(v: Verdict) -> LegacyVerdict {
    LegacyVerdict::new(v)
}

fn aat(v: Verdict) -> AatVerdict {
    AatVerdict::new(v)
}

#[test]
fn agreement_decides_and_records_nothing() {
    let mut c = ShadowComparator::new();
    let out = c.compare(&legacy(Verdict::Compliant), &aat(Verdict::Compliant), T0);

    assert_eq!(out.decided, Verdict::Compliant);
    assert!(
        out.mismatch.is_none(),
        "agreement must not record a mismatch"
    );
    assert_eq!(c.mismatches(), 0);
    assert_eq!(
        c.steps(),
        1,
        "an agreeing step still counts toward the gate"
    );
}

#[test]
fn legacy_decides_even_when_the_aat_path_would_deny() {
    // The whole point of shadow mode. A stricter shadow verdict is exactly the
    // case where "just be safe" is tempting and wrong: acting on an unvalidated
    // verifier is the outage this rollout posture exists to avoid.
    let mut c = ShadowComparator::new();
    let out = c.compare(&legacy(Verdict::Compliant), &aat(Verdict::Violation), T0);

    assert_eq!(
        out.decided,
        Verdict::Compliant,
        "shadow mode must not let the AAT path deny a step legacy allowed"
    );
    assert!(out.decided.permits_execution());
    let m = out.mismatch.expect("the disagreement must be recorded");
    assert_eq!(
        m.direction,
        MismatchDirection::AatMoreRestrictive,
        "an allow/deny split must be marked as the AAT path being stricter"
    );
}

#[test]
fn legacy_decides_even_when_the_aat_path_would_allow() {
    // The opposite direction is an authorization gap, not an outage risk, and
    // must be distinguishable from it.
    let mut c = ShadowComparator::new();
    let out = c.compare(&legacy(Verdict::Violation), &aat(Verdict::Compliant), T0);

    assert_eq!(
        out.decided,
        Verdict::Violation,
        "shadow mode must not let the AAT path permit a step legacy denied"
    );
    assert!(!out.decided.permits_execution());
    let m = out.mismatch.expect("the disagreement must be recorded");
    assert_eq!(
        m.direction,
        MismatchDirection::AatMorePermissive,
        "a deny/allow split is the AAT path being more permissive"
    );
}

#[test]
fn insufficient_evidence_never_reads_as_permission_on_either_path() {
    assert!(!Verdict::InsufficientEvidence.permits_execution());

    let mut c = ShadowComparator::new();
    let out = c.compare(
        &legacy(Verdict::Compliant),
        &aat(Verdict::InsufficientEvidence),
        T0,
    );
    let m = out
        .mismatch
        .expect("compliant vs insufficient is a disagreement");
    assert_eq!(
        m.direction,
        MismatchDirection::AatMoreRestrictive,
        "insufficient evidence blocks, so it is the stricter answer"
    );
}

#[test]
fn a_mismatch_carries_both_digests_and_they_differ() {
    let mut c = ShadowComparator::new();
    let out = c.compare(&legacy(Verdict::Compliant), &aat(Verdict::Violation), T0);
    let m = out.mismatch.expect("recorded");

    assert_eq!(m.verb, MISMATCH_VERB);
    assert!(m.legacy_digest.starts_with("sha-256:"));
    assert!(m.aat_digest.starts_with("sha-256:"));
    assert_ne!(
        m.legacy_digest, m.aat_digest,
        "two different verdicts must not digest alike, or the record proves nothing"
    );
    // The plain verdicts travel too: a reader should not have to invert a hash
    // to learn what disagreed.
    assert_eq!(m.legacy_verdict, Verdict::Compliant);
    assert_eq!(m.aat_verdict, Verdict::Violation);
}

#[test]
fn digests_are_stable_across_calls() {
    let v = PathVerdict::with_code(Path::Aat, Verdict::Violation, "policy_denied");
    assert_eq!(v.digest(), v.digest(), "a digest must be deterministic");
}

#[test]
fn the_audit_code_is_part_of_the_digest() {
    let a = PathVerdict::with_code(Path::Aat, Verdict::Violation, "policy_denied");
    let b = PathVerdict::with_code(Path::Aat, Verdict::Violation, "budget_exhausted");
    assert_ne!(
        a.digest(),
        b.digest(),
        "the same verdict for different reasons must be distinguishable in evidence"
    );
}

#[test]
fn a_free_form_code_cannot_forge_another_verdicts_digest() {
    // Note on scope, because an over-claiming test is worse than none: with
    // `path` and `verdict` drawn from fixed vocabularies and the only free-form
    // field (`audit_code`) LAST, a true field-shift collision is not
    // constructible today - a mutation removing the length prefixes still
    // passes, because differing field lengths change the byte stream anyway.
    //
    // Length-prefixing remains correct defensively: it is what keeps the digest
    // safe if a field is ever added AFTER `audit_code`. What this test can
    // honestly pin is that holder-influenced content is fully covered, so a
    // crafted code cannot collide with a different verdict's record.
    let crafted = PathVerdict::with_code(Path::Aat, Verdict::Violation, "compliant");
    let plain = PathVerdict::new(Path::Aat, Verdict::Compliant);
    assert_ne!(
        crafted.digest(),
        plain.digest(),
        "an audit code echoing another verdict must not collide with it"
    );

    let a = PathVerdict::with_code(Path::Aat, Verdict::Violation, "ab");
    let b = PathVerdict::with_code(Path::Aat, Verdict::Violation, "a\u{0}b");
    assert_ne!(
        a.digest(),
        b.digest(),
        "a NUL inside a code must not collapse two distinct records"
    );
}

#[test]
fn a_differing_audit_code_alone_is_not_a_mismatch() {
    // Two paths may agree on the answer while reasoning differently. Treating
    // that as a mismatch would make the promotion gate unreachable for a
    // difference that changes no decision.
    let mut c = ShadowComparator::new();
    let out = c.compare(
        &LegacyVerdict::with_code(Verdict::Violation, "policy_denied"),
        &AatVerdict::with_code(Verdict::Violation, "cedar_forbid"),
        T0,
    );
    assert!(
        out.mismatch.is_none(),
        "same verdict, different internal reason, is agreement"
    );
    assert_eq!(c.mismatches(), 0);
}

#[test]
fn promotion_requires_volume_and_time_and_zero_mismatches() {
    let mut c = ShadowComparator::new();
    for i in 0..PROMOTION_MIN_STEPS {
        c.compare(
            &legacy(Verdict::Compliant),
            &aat(Verdict::Compliant),
            T0 + i,
        );
    }
    // Volume reached, but all inside ~1000 seconds.
    assert!(
        !c.promotion_ready(),
        "volume alone must not promote: a thousand steps can happen in an afternoon"
    );
    assert!(
        c.promotion_blockers()
            .iter()
            .any(|b| b.contains("seconds elapsed")),
        "the operator must be told the time bound is what is missing: {:?}",
        c.promotion_blockers()
    );

    // One more step a week later satisfies the span.
    c.compare(
        &legacy(Verdict::Compliant),
        &aat(Verdict::Compliant),
        T0 + WEEK,
    );
    assert!(
        c.promotion_ready(),
        "blockers: {:?}",
        c.promotion_blockers()
    );
}

#[test]
fn time_alone_does_not_promote() {
    let mut c = ShadowComparator::new();
    c.compare(&legacy(Verdict::Compliant), &aat(Verdict::Compliant), T0);
    c.compare(
        &legacy(Verdict::Compliant),
        &aat(Verdict::Compliant),
        T0 + WEEK * 4,
    );
    assert!(
        !c.promotion_ready(),
        "two steps a month apart must not satisfy a 1000-step gate"
    );
    assert!(
        c.promotion_blockers()
            .iter()
            .any(|b| b.contains("governed steps")),
        "blockers: {:?}",
        c.promotion_blockers()
    );
}

#[test]
fn a_single_mismatch_blocks_promotion_forever() {
    let mut c = ShadowComparator::new();
    // One disagreement early on.
    c.compare(&legacy(Verdict::Compliant), &aat(Verdict::Violation), T0);
    // Then a full week of perfect agreement at volume.
    for i in 0..PROMOTION_MIN_STEPS {
        c.compare(
            &legacy(Verdict::Compliant),
            &aat(Verdict::Compliant),
            T0 + WEEK + i,
        );
    }
    assert_eq!(c.mismatches(), 1);
    assert!(
        !c.promotion_ready(),
        "the gate is ZERO mismatches; later agreement does not retire an earlier one"
    );
    assert!(
        c.promotion_blockers()
            .iter()
            .any(|b| b.contains("requires zero")),
        "blockers: {:?}",
        c.promotion_blockers()
    );
}

#[test]
fn a_fresh_comparator_is_not_promotion_ready() {
    // The zero state must not satisfy a gate by vacuous arithmetic: with no
    // steps, "zero mismatches" is trivially true.
    let c = ShadowComparator::new();
    assert!(
        !c.promotion_ready(),
        "an unused comparator has proven nothing"
    );
    assert_eq!(c.steps(), 0);
    assert_eq!(c.elapsed_secs(), 0);
}

// ---------------------------------------------------------------------------
// Guards for the review findings on PR #524.
// ---------------------------------------------------------------------------

#[test]
fn a_disagreement_where_neither_path_permits_is_its_own_direction() {
    // The bug: a bool collapsed "both deny, differently" into the same value as
    // "AAT is more permissive", which is a genuine authorization gap. The two
    // call for completely different responses, so the statistics the promotion
    // decision rests on were corrupted.
    let mut c = ShadowComparator::new();
    let out = c.compare(
        &legacy(Verdict::Violation),
        &aat(Verdict::InsufficientEvidence),
        T0,
    );
    let m = out.mismatch.expect("differing verdicts disagree");

    assert!(!m.legacy_verdict.permits_execution());
    assert!(!m.aat_verdict.permits_execution());
    assert_eq!(
        m.direction,
        MismatchDirection::BothDeny,
        "neither path permits, so this is not an authorization gap"
    );

    // And the reverse ordering must classify identically, not as a gap.
    let mut c2 = ShadowComparator::new();
    let rev = c2.compare(
        &legacy(Verdict::InsufficientEvidence),
        &aat(Verdict::Violation),
        T0,
    );
    assert_eq!(
        rev.mismatch.expect("disagreement").direction,
        MismatchDirection::BothDeny,
        "the mirror case must not read as AAT being more permissive"
    );
}

#[test]
fn an_absent_audit_code_does_not_digest_like_an_empty_one() {
    // The bug: `unwrap_or_default()` hashed None and Some("") identically, a
    // deterministic collision in evidence whose entire purpose is telling
    // records apart.
    let absent = PathVerdict::new(Path::Aat, Verdict::Violation);
    let empty = PathVerdict::with_code(Path::Aat, Verdict::Violation, "");
    assert_ne!(
        absent.digest(),
        empty.digest(),
        "an absent code and an empty code are different records"
    );
}

#[test]
fn a_mismatch_round_trips_through_owned_deserialization() {
    // The bug: `verb: &'static str` meant the derived deserializer required a
    // 'static buffer, so `Mismatch` did not implement DeserializeOwned and
    // `from_reader` could not reconstruct evidence advertised as durable.
    let mut c = ShadowComparator::new();
    let out = c.compare(&legacy(Verdict::Compliant), &aat(Verdict::Violation), T0);
    let m = out.mismatch.expect("recorded");

    let json = serde_json::to_string(&m).expect("serializes");
    // from_reader is the API a persisted record is actually loaded with, and is
    // exactly what a borrowed verb made impossible.
    let restored: ardur_dualrun::Mismatch =
        serde_json::from_reader(std::io::Cursor::new(json.into_bytes()))
            .expect("durable evidence must be loadable from a reader");

    assert_eq!(restored.verb, MISMATCH_VERB);
    assert_eq!(restored, m, "a round trip must preserve the record exactly");
}

#[test]
fn a_restart_does_not_discharge_an_earlier_mismatch() {
    // The bug: the gate lived only in process memory. A restart reset it, so
    // after another clean run promotion_ready() returned true while the durable
    // chain still contained a mismatch - the opposite of "one mismatch blocks
    // forever".
    let mut before = ShadowComparator::new();
    before.compare(&legacy(Verdict::Compliant), &aat(Verdict::Violation), T0);
    for i in 0..PROMOTION_MIN_STEPS {
        before.compare(
            &legacy(Verdict::Compliant),
            &aat(Verdict::Compliant),
            T0 + WEEK + i,
        );
    }
    assert!(!before.promotion_ready());

    // Persist and resume, as a restarting service would.
    let saved: GateState = before.state();
    let json = serde_json::to_string(&saved).expect("gate state serializes");
    let loaded: GateState = serde_json::from_str(&json).expect("and deserializes");
    let after = ShadowComparator::resume(loaded);

    assert_eq!(
        after.mismatches(),
        1,
        "the mismatch must survive the restart"
    );
    assert!(
        !after.promotion_ready(),
        "a restart must not discharge a recorded mismatch"
    );

    // A comparator that simply forgot would look ready on the same history.
    let mut forgetful = ShadowComparator::new();
    for i in 0..PROMOTION_MIN_STEPS {
        forgetful.compare(
            &legacy(Verdict::Compliant),
            &aat(Verdict::Compliant),
            T0 + WEEK + i,
        );
    }
    forgetful.compare(
        &legacy(Verdict::Compliant),
        &aat(Verdict::Compliant),
        T0 + WEEK * 3,
    );
    assert!(
        forgetful.promotion_ready(),
        "control: without the earlier mismatch this history DOES promote, \
         which is what makes the restored-state assertion meaningful"
    );
}
