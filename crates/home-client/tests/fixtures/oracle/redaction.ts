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
const SENSITIVE_KEY =
  /password|passwd|secret|token|credential|authorization|cookie|(?:api|private|access|client|auth)key/i;
// References, counts and presence flags describe credentials without containing them.
const METADATA_KEY = /(?:secret|token|credential)(?:id|count|absent|present)$/i;

function isSensitiveKey(key: string): boolean {
  const normalized = key.replace(/[^a-z0-9]/gi, "");
  return !METADATA_KEY.test(normalized) && SENSITIVE_KEY.test(normalized);
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

// Command output may contain source. Only bare, plainly executable/type-like values
// qualify; token-shaped values and quoted strings still follow the conservative path.
function isCodeValue(text: string, start: number, key: string): boolean {
  if (/^["'`]/.test(text.slice(start, start + 1))) return false;
  if (/^(?:\$\{[^{}]+\}|\{\{[^{}]+\}\})/.test(text.slice(start))) return true;
  if (/^[{[]/.test(text.slice(start, start + 1))) return false;
  const value = text.slice(start).match(/^[^\s"',;}&\]]*/)?.[0] ?? "";
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
  if (!/^\??[ \t]*(?:[,;}\]\r\n]|$)/.test(suffix)) return false;
  if (identifier.includes(".")) return true;
  // A bare word can equally be a YAML/environment credential. Preserve only
  // explicit type syntax or a named collection reference such as knownSecrets: secrets.
  return (
    (/^(?:string|number|boolean|unknown|never|any|void|bigint|symbol|object)$/.test(identifier) &&
      /^[ \t]*;/.test(suffix)) ||
    (key === `known${identifier[0]!.toUpperCase()}${identifier.slice(1)}` &&
      /^(?:secrets|credentials)$/.test(identifier))
  );
}

function redactAssignments(text: string, credentialsOnly = false, commandOutput = false): string {
  const keys = new RegExp(ASSIGNMENT_KEY);
  const parts: string[] = [];
  let copied = 0;
  for (let match = keys.exec(text); match; match = keys.exec(text)) {
    const key = match[2] ?? match[3]!;
    // Diagnostics also hide addresses and bare "key" assignments; a credential check keeps them.
    const privacyKey =
      !credentialsOnly && (match[2] !== undefined ? /email/i.test(key) : /^key$/i.test(key));
    if (!isSensitiveKey(key) && !privacyKey) continue;
    const start = keys.lastIndex;
    const quote = text[start];
    if (commandOutput && match[2] === undefined && isCodeValue(text, start, key)) continue;
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
      if (quote === "{" || quote === "[") {
        end = containerEnd(text, start);
      } else {
        while (end < text.length && !/[\s"',;}&\]]/.test(text[end]!)) end++;
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
            while (end < text.length && !/[\s"',;}&\]]/.test(text[end]!)) end++;
          }
        }
      }
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
