//! The canonical tool-manifest digest (INTER-01 / ArdurAI/ardur-agent#537,
//! convergence finding F8).
//!
//! The Mission Declaration author (`ardur-governance`) and the ObservedEvent
//! emitter (`ardur-observed-events`) must agree on the manifest digest
//! byte-for-byte: the verifier's §9.6 drift check is plain string equality
//! between the MD's declared digest and the event's observed digest, so two
//! reasonable-but-different implementations read an *unchanged* registry as
//! manifest drift. That is the failure this module removes by giving both
//! sides one function to call.
//!
//! # Canonical construction
//!
//! 1. Sort the tool ids and de-duplicate them — enumeration order is not
//!    drift, and a repeated registration is not a second tool.
//! 2. Length-prefix each id with its byte length as a big-endian `u64` and
//!    concatenate. A separator (NUL, comma, …) collides as soon as an id can
//!    contain it, and tool ids come from remote/skill registration, so their
//!    content is not fully under the caller's control.
//! 3. SHA-256 over the payload, rendered as lowercase hex with the
//!    `sha-256:` algorithm-tag prefix the MD schema pins
//!    (`^sha-256:[0-9a-f]{64}$`).

use crate::digest::Sha256Digest;

/// The algorithm-tag prefix the MD schema requires on the wire form.
pub const MANIFEST_DIGEST_PREFIX: &str = "sha-256:";

/// Digest a tool manifest into its canonical `sha-256:`-prefixed wire form.
///
/// The digest is order-stable but content-sensitive: enumerating the same
/// registry in a different order (or with duplicate entries) yields the same
/// digest, while adding, removing, or renaming a tool changes it. It pins ids
/// only — a caller that can also pin tool *descriptors* (schema, capabilities)
/// needs a strictly stronger digest and must not expect it to equal this one;
/// see `ardur_governance::tool_manifest_digest_of`.
#[must_use]
pub fn tool_manifest_digest(tool_ids: &[String]) -> String {
    let mut sorted: Vec<&str> = tool_ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut payload = Vec::new();
    for id in sorted {
        payload.extend_from_slice(&(id.len() as u64).to_be_bytes());
        payload.extend_from_slice(id.as_bytes());
    }
    format!(
        "{MANIFEST_DIGEST_PREFIX}{}",
        Sha256Digest::of(&payload).to_hex()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_is_order_independent_but_content_sensitive() {
        let a = tool_manifest_digest(&["file.read".into(), "shell.run".into()]);
        let b = tool_manifest_digest(&["shell.run".into(), "file.read".into()]);
        assert_eq!(a, b, "enumeration order must not read as drift");
        let c = tool_manifest_digest(&["file.read".into(), "file.read".into()]);
        assert_ne!(a, c, "a duplicate-only difference must collapse");
        assert_eq!(c, tool_manifest_digest(&["file.read".into()]));
        assert_ne!(
            a,
            tool_manifest_digest(&["file.read".into(), "shell.run".into(), "http.fetch".into()]),
            "a tool joining the registry must change the digest"
        );
    }

    #[test]
    fn length_prefixes_prevent_separator_collisions() {
        let a = tool_manifest_digest(&["ab".into(), "c".into()]);
        let b = tool_manifest_digest(&["a".into(), "bc".into()]);
        assert_ne!(a, b, "entry boundaries are part of the digest");
        let two = tool_manifest_digest(&["file.read".into(), "shell.run".into()]);
        let one = tool_manifest_digest(&["file.read\u{0}shell.run".into()]);
        assert_ne!(two, one, "a NUL inside an id must not forge a boundary");
    }

    #[test]
    fn the_wire_form_matches_the_md_schema_pattern() {
        let d = tool_manifest_digest(&["file.write".into()]);
        assert!(d.starts_with(MANIFEST_DIGEST_PREFIX));
        let hex = &d[MANIFEST_DIGEST_PREFIX.len()..];
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }
}
