const REDACTED = "[Redacted]";
const REDACT_KEYS = new Set([
  "email",
  "prompt",
  "message",
  "messages",
  "body",
  "query",
  "rawheaders",
  "headers",
]);
// Bindings and text assignments must recognize the same credential families.
// Credential words anywhere in a key include plural collections and value suffixes.
// Only explicit counters and named references below are exempt from masking.
const SENSITIVE_KEY =
  /password|passwd|authorization|cookie|(?:api|private|access|client|auth)key|secret|token|credential/i;
// References, counts and presence flags describe credentials without containing them.
const METADATA_KEY =
  /^(?:(?:max|min|num|total)tokens?|(?:used|remaining|input|output|prompt|completion|cached|reasoning|context)tokens|tokens?(?:used|remaining)|(?:known|required|missing)secrets|secret(?:names|ref|store))$|(?:secret|token|credential)(?:id|count|absent|present)$/i;

function isSensitiveKey(key: string): boolean {
  const normalized = key.replace(/[^a-z0-9]/gi, "");
  return !METADATA_KEY.test(normalized) && SENSITIVE_KEY.test(normalized);
}

// After a literal backslash, "\npassword" is an escape and a key, but "\token" is a
// backslash and a key. Drop the escape letter only when the rest still names a
// credential word; otherwise classify the whole word, so neither reading leaks.
function isSensitiveEscapedKey(key: string, escaped: boolean): boolean {
  if (!escaped || !/^[ntr]/.test(key)) return isSensitiveKey(key);
  const rest = key.slice(1);
  return SENSITIVE_KEY.test(rest.replace(/[^a-z0-9]/gi, ""))
    ? isSensitiveKey(rest)
    : isSensitiveKey(key);
}

function redactValue(value: unknown, seen = new WeakSet<object>()): unknown {
  if (value === null || value === undefined) return value;
  if (typeof value === "string") return redactSensitiveText(value);
  if (typeof value !== "object") return value;
  if (seen.has(value)) return "[Circular]";
  seen.add(value);
  if (Array.isArray(value)) {
    return value.map((item) => redactValue(item, seen));
  }
  if (value instanceof Error) {
    const output: Record<string, unknown> = {
      name: redactSensitiveText(value.name || "Error"),
      message: redactSensitiveText(value.message),
    };
    if (typeof value.stack === "string" && value.stack.length > 0) {
      output.stack = redactSensitiveText(value.stack);
    }
    if (value.cause !== undefined) {
      output.cause =
        typeof value.cause === "string"
          ? redactSensitiveText(value.cause)
          : redactValue(value.cause, seen);
    }
    return output;
  }
  if (value instanceof Date) return value;
  const output: Record<string, unknown> = {};
  for (const [key, nested] of Object.entries(value)) {
    output[key] = shouldRedactKey(key) ? REDACTED : redactValue(nested, seen);
  }
  return output;
}

export function redactBindings(bindings: Record<string, unknown>): Record<string, unknown> {
  return redactValue(bindings) as Record<string, unknown>;
}

// Start only at a local-part boundary, never at every character of a long token.
const EMAIL = /(?<![A-Z0-9._%+-])[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi;
const CREDENTIAL_URL = /(https?:\/\/)[^\s/:@]+:[^\s/@]+@/gi;
const ASSIGNMENT_KEY =
  /(["'])([^"'\r\n]*)\1\s*[:=]\s*|(?<![A-Za-z0-9_-])([A-Za-z0-9_-]+)["']?\s*[:=]\s*/g;
// Empty, elided and template values are documentation placeholders, not secrets.
const PLACEHOLDER = /^(?:|\[Redacted\]|[.*…]+|<[^<>]+>|\$\{[^{}]+\}|\{\{[^{}]+\}\})$/i;

function quotedEnd(text: string, start: number): number {
  const quote = text[start];
  let end = start + 1;
  // Consume a value once, including escapes; an unterminated value is secret too.
  while (end < text.length && text[end] !== quote) {
    end += text[end] === "\\" ? 2 : 1;
  }
  return Math.min(end, text.length);
}

function containerEnd(text: string, start: number): number {
  const closing = [text[start] === "{" ? "}" : "]"];
  let quote = "";
  for (let end = start + 1; end < text.length; end++) {
    const character = text[end]!;
    if (quote) {
      if (character === "\\") end++;
      else if (character === quote) quote = "";
    } else if (character === '"' || character === "'") {
      quote = character;
    } else if (character === "{" || character === "[") {
      closing.push(character === "{" ? "}" : "]");
    } else if (character === closing.at(-1)) {
      closing.pop();
      if (closing.length === 0) return end + 1;
    }
  }
  return text.length;
}

// Literal escapes can be part of a credential, even before key-like text.
// Only real delimiters end an unquoted value; ambiguous suffixes stay masked.
function valueEnd(text: string, start: number): number {
  let end = start;
  while (end < text.length) {
    const character = text[end]!;
    if (/[\s"',;}&\]]/.test(character)) break;
    end++;
  }
  return end;
}

function unquotedValueEnd(text: string, start: number): number {
  while (start < text.length) {
    const quote = text[start];
    if (quote === '"' || quote === "'") {
      const end = quotedEnd(text, start);
      return text[end] === quote ? end + 1 : end;
    }
    if (quote === "{" || quote === "[") return containerEnd(text, start);

    let end = valueEnd(text, start);
    // A scheme and credential can occur under any sensitive key. Preserve
    // a following spaced assignment, not '=' or ':' within the credential.
    let credential = end;
    while (text[credential] === " " || text[credential] === "\t") credential++;
    if (
      end > start &&
      credential > end &&
      /^[A-Za-z][A-Za-z0-9._+-]*$/.test(text.slice(start, end)) &&
      !/^[A-Za-z0-9_-]+\s+[:=]/.test(text.slice(credential))
    ) {
      end = credential;
      if (text[end] === '"' || text[end] === "'") {
        const credentialQuote = text[end];
        end = quotedEnd(text, end);
        if (text[end] === credentialQuote) end++;
      } else {
        end = valueEnd(text, end);
      }
    }

    // The run may have swallowed the next sensitive key, glued after an escape or
    // any other character. Keep it masked and include its value; repeat for chains.
    // Scan back from the separator: an end-anchored regex backtracks on long runs.
    const separator = end - 1;
    if (end <= start || !/[:=]/.test(text[separator]!) || !/[ \t]/.test(text[end] ?? ""))
      return end;
    let keyStart = separator;
    while (keyStart > start && /[A-Za-z0-9_-]/.test(text[keyStart - 1]!)) keyStart--;
    const gluedKey = text.slice(keyStart, separator);
    if (!gluedKey || !isSensitiveEscapedKey(gluedKey, text[keyStart - 1] === "\\")) return end;
    start = end;
    while (text[start] === " " || text[start] === "\t") start++;
  }
  return start;
}

// Command output may contain source. Only bare, plainly executable/type-like values
// qualify; token-shaped values and quoted strings still follow the conservative path.
function isCodeValue(text: string, start: number): boolean {
  if (/^["'`]/.test(text.slice(start, start + 1))) return false;
  if (/^(?:\$\{[^{}]+\}|\{\{[^{}]+\}\})/.test(text.slice(start))) return true;
  if (/^[{[]/.test(text.slice(start, start + 1))) return false;
  const value = text.slice(start, valueEnd(text, start));
  if (PLACEHOLDER.test(value)) return true;
  if (
    (value.length >= 32 && /^[A-Za-z0-9_+/=-]+$/.test(value)) ||
    /^(?:gh[pousr]_|github_pat_|AKIA|ASIA|sk-|xai-|ak_|ck_)/.test(value)
  )
    return false;
  if (/^[+-]?(?:\d+(?:\.\d+)?|0x[\da-f]+)$/i.test(value)) return true;
  const rest = text.slice(start);
  // Do not mistake an authorization scheme followed by an opaque value for source.
  const identifier = /^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*/.exec(rest)?.[0];
  if (!identifier) return false;
  const suffix = rest.slice(identifier.length);
  if (suffix.startsWith("(")) return true;
  if (!/^\??[ \t]*(?:[,;}\]\r\n]|$|\\[ntr])/.test(suffix)) return false;
  if (identifier.includes(".")) return true;
  // A bare word can equally be a YAML/environment credential. Preserve only
  // explicit type syntax. Named collection references are handled by key classification.
  return (
    /^(?:string|number|boolean|unknown|never|any|void|bigint|symbol|object)$/.test(identifier) &&
    /^[ \t]*;/.test(suffix)
  );
}

function redactAssignments(text: string, credentialsOnly = false, commandOutput = false): string {
  const keys = new RegExp(ASSIGNMENT_KEY);
  const parts: string[] = [];
  let copied = 0;
  for (let match = keys.exec(text); match; match = keys.exec(text)) {
    const key = match[2] ?? match[3]!;
    // A literal escape's letter can be captured as the next bare key's prefix.
    const escaped = match[2] === undefined && text[match.index - 1] === "\\";
    // Diagnostics also hide addresses and bare "key" assignments; a credential check keeps them.
    const privacyKey =
      !credentialsOnly &&
      (match[2] !== undefined
        ? /email/i.test(key)
        : (escaped ? /^[ntr]?key$/i : /^key$/i).test(key));
    if (!isSensitiveEscapedKey(key, escaped) && !privacyKey) continue;
    const start = keys.lastIndex;
    const quote = text[start];
    if (commandOutput && match[2] === undefined && isCodeValue(text, start)) continue;
    let end = start;
    let replacement = REDACTED;
    if (quote === '"' || quote === "'" || (commandOutput && quote === "`")) {
      end = quotedEnd(text, start);
      const closed = text[end] === quote;
      const value = text.slice(start + 1, end);
      if (closed) end++;
      keys.lastIndex = end;
      if (closed && PLACEHOLDER.test(value)) continue;
      replacement = `${quote}${REDACTED}${closed ? quote : ""}`;
    } else {
      end = unquotedValueEnd(text, start);
      keys.lastIndex = end;
      if (end === start) continue;
      // Replacing a JSON scalar or container must leave valid JSON, too.
      if (match[2] !== undefined) replacement = `"${REDACTED}"`;
    }
    parts.push(text.slice(copied, start), replacement);
    copied = end;
  }
  return copied === 0 ? text : [...parts, text.slice(copied)].join("");
}

// Each unbounded run has a non-overlapping start or a delimiter outside its
// alphabet. In particular, URL user names exclude ':' so password scans cannot
// restart at every colon, and JWT segments cannot restart inside another segment.
const TEXT_REDACTIONS: readonly [RegExp, string][] = [
  [/\b(Bearer\s+)[^\s"',;&}]+/gi, `$1${REDACTED}`],
  [/\b(?:gh[pousr]_|github_pat_)[A-Za-z0-9_]+\b/g, REDACTED],
  [/\b(?:AKIA|ASIA)[A-Z0-9]{16}\b/g, REDACTED],
  [/\b(?:sk-|xai-)[A-Za-z0-9_-]{8,}\b/g, REDACTED],
  [/\b(?:ak_|ck_)[A-Za-z0-9]+\b/g, REDACTED],
];
const AWS_SECRET = /(?<![A-Za-z0-9/+=])[A-Za-z0-9/+]{40}(?:=)?(?![A-Za-z0-9/+=])/g;
const JWT = /(?<![A-Za-z0-9_-])([A-Za-z0-9_-]+)\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/g;

export function redactSensitiveText(text: string): string {
  return redactText(text, false);
}

/**
 * Redacts credentials only: passwords, tokens, keys, credential URLs, PEM blocks and the
 * like. Addresses and other private-but-not-secret text stay, so a memory or note that
 * mentions an email address is not mistaken for a leaked credential.
 */
export function redactCredentialText(text: string): string {
  return redactText(text, true);
}

/** Source-aware redaction for retained shell output only; logs and memories stay conservative. */
export function redactCommandOutput(text: string): string {
  // Remove blocks before assignment scanning can consume their opening marker.
  const withoutPrivateKeys = text.replace(
    /-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)/g,
    REDACTED,
  );
  return redactText(withoutPrivateKeys, false, true);
}

function redactText(text: string, credentialsOnly: boolean, commandOutput = false): string {
  // Hide URL credentials before their password/host suffix can look like an email.
  let result = text.includes("@") ? text.replace(CREDENTIAL_URL, `$1${REDACTED}@`) : text;
  if (!credentialsOnly && result.includes("@")) result = result.replace(EMAIL, REDACTED);
  result = redactAssignments(result, credentialsOnly, commandOutput);
  for (const [pattern, replacement] of TEXT_REDACTIONS)
    result = result.replace(pattern, replacement);
  // SHA-1 commit IDs share the AWS key length but are public provenance, not credentials.
  // A bare 40-character run is only a hint, so the credential check leaves it alone: notes
  // are full of identifiers that length, and a key id (AKIA...) is caught on its own above.
  if (!credentialsOnly)
    result = result.replace(AWS_SECRET, (value: string) =>
      /^[a-f0-9]{40}$/i.test(value) ? value : REDACTED,
    );
  // Ordinary dotted text must not invoke the JWT callback for every three words.
  return result.includes("eyJ") && result.includes(".")
    ? result.replace(JWT, (token: string, header: string) =>
        /\beyJ[A-Za-z0-9_-]+/.test(header) ? REDACTED : token,
      )
    : result;
}

function shouldRedactKey(key: string): boolean {
  const normalized = key.toLowerCase().replace(/[^a-z0-9]/g, "");
  return REDACT_KEYS.has(normalized) || isSensitiveKey(key);
}
