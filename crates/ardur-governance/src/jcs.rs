//! A minimal RFC 8785 JSON Canonicalization Scheme (JCS) serializer.
//!
//! The MCEP Execution Receipt spec (§8) requires hashes over nested JSON — in
//! this crate `invocation_digest` and `arguments_hash` — to be computed over
//! RFC 8785 canonical output, not implementation-dependent serializer output.
//!
//! This implementation covers the constrained shapes a normalized tool
//! invocation actually carries: objects, arrays, strings, booleans, `null`, and
//! JSON integers. Object members are emitted with keys sorted by UTF-16 code
//! unit and no insignificant whitespace; strings use the RFC 8785 minimal
//! escape set.
//!
//! **Caveat (documented, not silently wrong):** RFC 8785 mandates the
//! ECMAScript `Number::toString` algorithm for non-integer numbers. Full
//! IEEE-754 shortest-round-trip float formatting is out of scope for the
//! prototype; a finite non-integer `f64` is formatted with Rust's shortest
//! `Display`, which agrees with JCS for the overwhelming majority of decimal
//! values but is not byte-guaranteed for all subnormals/exponent forms. Tool
//! arguments in this crate are integer/string/bool/object/array shaped, so this
//! path is not exercised by the governed receipt digests today. A follow-up can
//! swap in a full ECMAScript number formatter without changing this API.

use serde_json::Value;

/// Serialize `value` to its RFC 8785 (JCS) canonical UTF-8 byte string.
pub fn to_canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_value(value, &mut out);
    out.into_bytes()
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // RFC 8785 §3.2.3: sort object members by the UTF-16 code units of
            // their keys.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| utf16_cmp(a, b));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(&map[*key], out);
            }
            out.push('}');
        }
    }
}

/// RFC 8785 §3.2.2.2 minimal string escaping.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{0009}' => out.push_str("\\t"),
            '\u{000A}' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\u{000D}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Compare two strings by their UTF-16 code-unit sequences.
fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_object_keys_and_strips_whitespace() {
        let v = json!({ "b": 1, "a": 2, "c": { "z": 3, "y": 4 } });
        assert_eq!(
            String::from_utf8(to_canonical_bytes(&v)).unwrap(),
            r#"{"a":2,"b":1,"c":{"y":4,"z":3}}"#
        );
    }

    #[test]
    fn escapes_control_and_quote() {
        let v = json!({ "k": "a\"b\n\t\u{1}" });
        assert_eq!(
            String::from_utf8(to_canonical_bytes(&v)).unwrap(),
            "{\"k\":\"a\\\"b\\n\\t\\u0001\"}"
        );
    }

    #[test]
    fn canonical_form_is_stable_across_input_key_order() {
        let a = json!({ "x": [1, 2, 3], "m": true, "n": null });
        let b = json!({ "n": null, "m": true, "x": [1, 2, 3] });
        assert_eq!(to_canonical_bytes(&a), to_canonical_bytes(&b));
    }
}
