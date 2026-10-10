//! Output parity with the home CLI's safeDiagnostic; fixtures pin the pure oracle.
use regex::{Captures, Regex};
use serde_json::Value;
use std::sync::LazyLock;

const REDACTED: &str = "[Redacted]";
// ECMAScript WhiteSpace + LineTerminator, not Unicode's White_Space property.
// In particular JavaScript includes FEFF and excludes NEL (0085).
const JS_SPACE: &str = r"[\x09-\x0d\x20\x{00a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]";
macro_rules! pattern {
    ($name:ident, $source:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(&$source.replace(r"\s", JS_SPACE).replace(r"\d", "[0-9]"))
                .expect("fixed output regex")
        });
    };
}
// Match Node's stripVTControlCharacters grammar, including string payloads and
// ST terminators. Generic ANSI stripping differs on malformed and C1 inputs.
pattern!(
    VT,
    r"[\x1b\x{009b}][\[\]()#;?]*(?:(?:(?:(?:;[-a-zA-Z\d/#&.:=?%@~_]+)*|[a-zA-Z\d]+(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?(?:\x07|\x1b\\|\x{009c}))|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]))"
);
pattern!(
    PEM,
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)"
);
pattern!(URL, r"(?i)(https?://)[^\s/:@]+:[^\s/@]+@");
pattern!(EMAIL, r"(?i)[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}(?-u:\b)");
pattern!(
    ASSIGNMENT,
    r#""([^"'\r\n]*)"\s*[:=]\s*|'([^"'\r\n]*)'\s*[:=]\s*|([A-Za-z0-9_-]+)["']?\s*[:=]\s*"#
);
pattern!(
    SENSITIVE,
    r"(?i)password|passwd|secret|token|credential|authorization|cookie|(?:api|private|access|client|auth)key"
);
pattern!(
    METADATA,
    r"(?i)^(?:(?:max|min|num|total)tokens?|(?:used|remaining|input|output|prompt|completion|cached|reasoning|context)tokens|tokens?(?:used|remaining)|(?:known|required|missing)secrets|secret(?:names|ref|store))$|(?:secret|token|credential)(?:id|count|absent|present)$"
);
pattern!(
    PLACEHOLDER,
    r"(?i)^(?:|\[Redacted\]|[.*…]+|<[^<>]+>|\$\{[^{}]+\}|\{\{[^{}]+\}\})$"
);
pattern!(SCHEME, r"^[A-Za-z][A-Za-z0-9._+-]*$");
pattern!(SPACED_ASSIGNMENT, r"^[A-Za-z0-9_-]+\s+[:=]");
pattern!(BEARER, r#"(?-u:\b)(?i:Bearer\s+)[^\s"',;&}]+"#);
pattern!(
    TOKENS,
    r"(?-u:\b)(?:gh[pousr]_|github_pat_)[A-Za-z0-9_]+(?-u:\b)|(?-u:\b)(?:AKIA|ASIA)[A-Z0-9]{16}(?-u:\b)|(?-u:\b)(?:sk-|xai-)[A-Za-z0-9_-]{8,}(?-u:\b)|(?-u:\b)(?:ak_|ck_)[A-Za-z0-9]+(?-u:\b)"
);
pattern!(AWS, r"[A-Za-z0-9/+]{40}=?");
pattern!(
    JWT,
    r"([A-Za-z0-9_-]+)\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+(?-u:\b)"
);
pattern!(JWT_HEADER, r"(?-u:\b)eyJ[A-Za-z0-9_-]+");
pattern!(
    IDENTIFIER,
    r"^[A-Za-z_$][A-Za-z0-9_$]*(?:\.[A-Za-z_$][A-Za-z0-9_$]*)*"
);
pattern!(CODE_SUFFIX, r"^\??[ \t]*(?:[,;}\]\r\n]|$|\\[ntr])");
pattern!(NUMBER, r"(?i)^[+-]?(?:\d+(?:\.\d+)?|0x[\da-f]+)$");
pattern!(
    CODE_TYPE,
    r"^(?:string|number|boolean|unknown|never|any|void|bigint|symbol|object)$"
);
pattern!(TEMPLATE, r"^(?:\$\{[^{}]+\}|\{\{[^{}]+\}\})");
pattern!(
    TOKEN_PREFIX,
    r"^(?:gh[pousr]_|github_pat_|AKIA|ASIA|sk-|xai-|ak_|ck_)"
);

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
fn key_byte(b: u8) -> bool {
    word(b) || b == b'-'
}
fn sensitive_key(key: &str, escaped: bool) -> bool {
    // Read both a literal escape followed by a key and a backslash before a key.
    // Drop the escape letter only when the remainder names a credential family.
    let normalized: String = key.chars().filter(char::is_ascii_alphanumeric).collect();
    let key = if escaped && key.starts_with(['n', 't', 'r']) && SENSITIVE.is_match(&normalized[1..])
    {
        &normalized[1..]
    } else {
        &normalized
    };
    !METADATA.is_match(key) && SENSITIVE.is_match(key)
}
fn js_whitespace(ch: char) -> bool {
    matches!(ch, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}')
}
fn delimiter(ch: char) -> bool {
    js_whitespace(ch) || "\"',;}&]".contains(ch)
}
fn token_end(text: &str, start: usize) -> usize {
    text[start..]
        .char_indices()
        .find(|(_, ch)| delimiter(*ch))
        .map_or(text.len(), |(offset, _)| start + offset)
}
fn quoted_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = start + 1;
    while end < bytes.len() && bytes[end] != bytes[start] {
        end += if bytes[end] == b'\\' { 2 } else { 1 };
    }
    end.min(bytes.len())
}
fn container_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut closing = vec![if bytes[start] == b'{' { b'}' } else { b']' }];
    let mut quote = 0;
    let mut end = start + 1;
    while end < bytes.len() {
        let ch = bytes[end];
        if quote != 0 {
            if ch == b'\\' {
                end += 1;
            } else if ch == quote {
                quote = 0;
            }
        } else if ch == b'"' || ch == b'\'' {
            quote = ch;
        } else if ch == b'{' || ch == b'[' {
            closing.push(if ch == b'{' { b'}' } else { b']' });
        } else if closing.last() == Some(&ch) {
            closing.pop();
            if closing.is_empty() {
                return end + 1;
            }
        }
        end += 1;
    }
    text.len()
}
fn unquoted_value_end(text: &str, mut start: usize) -> usize {
    let bytes = text.as_bytes();
    while start < bytes.len() {
        let quote = bytes[start];
        if quote == b'"' || quote == b'\'' {
            let end = quoted_end(text, start);
            return end + usize::from(bytes.get(end) == Some(&quote));
        }
        if quote == b'{' || quote == b'[' {
            return container_end(text, start);
        }

        // Literal escapes remain inside the value; only real delimiters end it.
        let mut end = token_end(text, start);
        let mut credential = end;
        while bytes
            .get(credential)
            .is_some_and(|b| *b == b' ' || *b == b'\t')
        {
            credential += 1;
        }
        if end > start
            && credential > end
            && SCHEME.is_match(&text[start..end])
            && !SPACED_ASSIGNMENT.is_match(&text[credential..])
        {
            end = credential;
            if bytes.get(end).is_some_and(|b| *b == b'"' || *b == b'\'') {
                let quote = bytes[end];
                end = quoted_end(text, end);
                end += usize::from(bytes.get(end) == Some(&quote));
            } else {
                end = token_end(text, end);
            }
        }

        // A consumed run can end with another sensitive key. Scan backward only
        // over that key, then consume its value too; repeat for arbitrary chains.
        if end <= start
            || !matches!(bytes[end - 1], b':' | b'=')
            || !bytes.get(end).is_some_and(|b| matches!(b, b' ' | b'\t'))
        {
            return end;
        }
        let separator = end - 1;
        let mut key_start = separator;
        while key_start > start && key_byte(bytes[key_start - 1]) {
            key_start -= 1;
        }
        if key_start == separator
            || !sensitive_key(
                &text[key_start..separator],
                key_start > 0 && bytes[key_start - 1] == b'\\',
            )
        {
            return end;
        }
        start = end;
        while bytes.get(start).is_some_and(|b| matches!(b, b' ' | b'\t')) {
            start += 1;
        }
    }
    start
}
fn code_value(text: &str, start: usize) -> bool {
    let rest = &text[start..];
    if rest.starts_with(['"', '\'', '\x60']) {
        return false;
    }
    if TEMPLATE.is_match(rest) {
        return true;
    }
    if rest.starts_with(['{', '[']) {
        return false;
    }
    let end = token_end(rest, 0);
    let value = &rest[..end];
    if PLACEHOLDER.is_match(value) {
        return true;
    }
    if (value.len() >= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_+/=-".contains(&b)))
        || TOKEN_PREFIX.is_match(value)
    {
        return false;
    }
    if NUMBER.is_match(value) {
        return true;
    }
    let Some(identifier) = IDENTIFIER.find(rest) else {
        return false;
    };
    let suffix = &rest[identifier.end()..];
    if suffix.starts_with('(') {
        return true;
    }
    if !CODE_SUFFIX.is_match(suffix) {
        return false;
    }
    let identifier = identifier.as_str();
    if identifier.contains('.') {
        return true;
    }
    CODE_TYPE.is_match(identifier) && suffix.trim_start_matches([' ', '\t']).starts_with(';')
}
fn assignments(text: &str, command_output: bool) -> String {
    let mut output = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    let bytes = text.as_bytes();
    while let Some(captures) = ASSIGNMENT.captures_at(text, cursor) {
        let matched = captures.get(0).expect("assignment match");
        cursor = matched.end();
        let quoted = captures.get(1).or_else(|| captures.get(2));
        let key = quoted
            .or_else(|| captures.get(3))
            .expect("assignment key")
            .as_str();
        if quoted.is_none() && matched.start() > 0 && key_byte(bytes[matched.start() - 1]) {
            continue;
        }
        let escaped =
            quoted.is_none() && matched.start() > 0 && bytes[matched.start() - 1] == b'\\';
        let sensitive = sensitive_key(key, escaped);
        let privacy = if quoted.is_some() {
            key.to_ascii_lowercase().contains("email")
        } else {
            key.eq_ignore_ascii_case("key")
                || (escaped
                    && key.len() == 4
                    && key.starts_with(['n', 'N', 't', 'T', 'r', 'R'])
                    && key[1..].eq_ignore_ascii_case("key"))
        };
        if !sensitive && !privacy {
            continue;
        }
        let start = matched.end();
        if command_output && quoted.is_none() && code_value(text, start) {
            continue;
        }
        let quote = bytes.get(start).copied().unwrap_or(0);
        let mut end;
        let replacement = if quote == b'"' || quote == b'\'' || (command_output && quote == b'\x60')
        {
            end = quoted_end(text, start);
            let closed = bytes.get(end) == Some(&quote);
            let value = &text[start + 1..end];
            if closed {
                end += 1;
            }
            cursor = end;
            if closed && PLACEHOLDER.is_match(value) {
                continue;
            }
            format!(
                "{}{REDACTED}{}",
                quote as char,
                if closed {
                    (quote as char).to_string()
                } else {
                    String::new()
                }
            )
        } else {
            end = unquoted_value_end(text, start);
            cursor = end;
            if end == start {
                continue;
            }
            if quoted.is_some() {
                format!("\"{REDACTED}\"")
            } else {
                REDACTED.into()
            }
        };
        output.push_str(&text[copied..start]);
        output.push_str(&replacement);
        copied = end;
    }
    output.push_str(&text[copied..]);
    output
}
fn replace_bounded(
    text: &str,
    regex: &Regex,
    boundary: fn(u8) -> bool,
    check_end: bool,
    replacement: impl Fn(&Captures<'_>) -> String,
) -> String {
    regex
        .replace_all(text, |captures: &Captures<'_>| {
            let m = captures.get(0).expect("text match");
            if m.start() > 0 && boundary(text.as_bytes()[m.start() - 1])
                || check_end && text.as_bytes().get(m.end()).copied().is_some_and(boundary)
            {
                m.as_str().into()
            } else {
                replacement(captures)
            }
        })
        .into_owned()
}
fn redact_text(text: &str, command_output: bool) -> String {
    let result = URL.replace_all(text, |c: &Captures<'_>| format!("{}{REDACTED}@", &c[1]));
    let result = replace_bounded(
        &result,
        &EMAIL,
        |b| word(b) || b"._%+-".contains(&b),
        false,
        |_| REDACTED.into(),
    );
    let result = assignments(&result, command_output);
    let result = BEARER.replace_all(&result, |c: &Captures<'_>| {
        let m = c.get(0).expect("bearer match").as_str();
        let end = m
            .find(|c: char| !c.is_ascii_alphabetic())
            .expect("bearer scheme");
        let whitespace = m[end..]
            .chars()
            .take_while(|c| js_whitespace(*c))
            .map(char::len_utf8)
            .sum::<usize>();
        format!("{}{REDACTED}", &m[..end + whitespace])
    });
    let result = TOKENS.replace_all(&result, REDACTED);
    let result = replace_bounded(
        &result,
        &AWS,
        |b| b.is_ascii_alphanumeric() || b"/+=".contains(&b),
        true,
        |c| {
            let value = c.get(0).expect("AWS match").as_str();
            if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                value.into()
            } else {
                REDACTED.into()
            }
        },
    );
    if !result.contains("eyJ") || !result.contains('.') {
        return result;
    }
    replace_bounded(&result, &JWT, key_byte, false, |c| {
        let header = c.get(1).expect("JWT header").as_str();
        if JWT_HEADER.is_match(header) {
            REDACTED.into()
        } else {
            c.get(0).expect("JWT match").as_str().into()
        }
    })
}
pub fn safe_output(text: &str) -> String {
    let text = VT.replace_all(text, "");
    let text: String = text
        .chars()
        .filter(|c| {
            *c == '\t' || *c == '\n' || (*c as u32 >= 32 && !('\u{7f}'..='\u{9f}').contains(c))
        })
        .collect();
    let text = PEM.replace_all(&text, REDACTED);
    redact_text(&redact_text(&text, true), false)
}
/// Apply before serialization, including dynamic JSON keys.
pub fn redact(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(safe_output(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(redact).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (safe_output(&k), redact(v)))
                .collect(),
        ),
        other => other,
    }
}
