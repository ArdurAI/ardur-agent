// Pure text oracle only. Never imports the application or accesses a service.
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { stripTypeScriptTypes } from "node:module";
import { createHash } from "node:crypto";
import { resolve } from "node:path";

const nodeVersion = "26.7.0";
if (process.versions.node !== nodeVersion) throw new Error("Oracle generation requires Node " + nodeVersion);
const snapshot = "crates/home-client/tests/fixtures/oracle";
const paths = ["apps/cli/src/text.ts", "packages/logging/src/redaction.ts"];
const checking = process.argv.includes("--check");
const oracleArg = process.argv[2] === "--check" ? undefined : process.argv[2];
const oracle = oracleArg ? resolve(oracleArg) : undefined;
const revision = oracle ? execFileSync("git", ["rev-parse", process.argv[3] ?? "HEAD"], { cwd: oracle, encoding: "utf8" }).trim()
  : JSON.parse(readFileSync(snapshot + "/provenance.json", "utf8")).revision;
const source = (path) => {
  const target = snapshot + "/" + path.split("/").at(-1);
  if (!oracle) return readFileSync(target, "utf8");
  const text = execFileSync("git", ["show", revision + ":" + path], { cwd: oracle, encoding: "utf8" });
  if (checking) {
    if (readFileSync(target, "utf8") !== text) throw new Error("Oracle source changed: " + path);
  } else { mkdirSync(snapshot, { recursive: true }); writeFileSync(target, text); }
  return text;
};
const moduleUrl = (text) => "data:text/javascript;base64," + Buffer.from(stripTypeScriptTypes(text)).toString("base64");
const loggingSource = source(paths[1]);
const cliSource = source(paths[0]);
const provenance = { repository: "ArdurAI/ardur-bot", revision, nodeVersion, sources: paths.map((path, i) => ({ path,
  sha256: createHash("sha256").update(i === 0 ? cliSource : loggingSource).digest("hex") })) };
const provenanceText = JSON.stringify(provenance, null, 2) + "\n";
if (!checking) writeFileSync(snapshot + "/provenance.json", provenanceText);
else if (readFileSync(snapshot + "/provenance.json", "utf8") !== provenanceText) throw new Error("Oracle provenance changed");
const redactions = moduleUrl(loggingSource);
const textSource = cliSource.replace('"@ardurbot/logging"', JSON.stringify(redactions));
const { safeDiagnostic } = await import(moduleUrl(textSource));
const cases = [];
const add = (name, input, credentials = []) => cases.push({ name, input, expected: safeDiagnostic(input), credentials });
const assignment = (key, value, separator = "=") => key + separator + value;
const synthetic = (suffix) => "synthetic-" + suffix;
const token = (prefix, suffix) => prefix + suffix;
// All credentials below are fabricated. Nothing comes from profiles or environment state.
for (const prefix of ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "sk-", "xai-", "ak_", "ck_"]) {
  const value = token(prefix, "FAKEsynthetic1234");
  add("token " + prefix, "before " + value + " after", [value]);
}
for (const prefix of ["AKIA", "ASIA"]) {
  const value = token(prefix, "FAKE000000000000");
  add("AWS id " + prefix, "before " + value + " after", [value]);
}
const jwt = ["eyJfakeHeader", "fakePayload", "fakeSignature"].join(".");
add("JWT", "before " + jwt + " after", [jwt]);
const aws = "FakeAwsSynthetic123".repeat(3).slice(0, 40);
add("AWS secret", "before " + aws + " after", [aws]);
for (const scheme of ["Bearer", "bEaReR"]) {
  const value = synthetic("short");
  add("short bearer " + scheme, "before " + scheme + " " + value + " after", [value]);
}
const url = "https://" + "synthetic-user" + ":" + "synthetic-pass" + "@gateway.example.test/path";
add("URL credentials", url, ["synthetic-user", "synthetic-pass"]);
add("email privacy", "hello fixture@example.test", ["fixture@example.test"]);
for (const key of ["password", "passwd", "secret", "accessToken", "Api_Key", "private-key", "access_key",
  "clientKey", "auth-key", "credential", "authorization", "cookie", "key"]) {
  const value = synthetic("assignment-" + key);
  add("assignment " + key, assignment(key, value) + "; status=ready", [value]);
}
for (const scheme of ["Bearer", "bAsIc", "Token", "Fixture.Scheme", "fixture+scheme"]) {
  for (const separator of [" ", "\t"]) {
    const value = synthetic("auth-value");
    add("scheme " + cases.length, assignment("Authorization", scheme + separator + value, ": ") + "\r\nstatus=ready", [value]);
  }
}
for (const value of [synthetic("quoted"), 'synthetic "quoted" value', 123456, true, false, null, [],
  { nested: [synthetic("container"), { value: synthetic("inner") }] }]) {
  add("JSON " + cases.length, JSON.stringify({ apiKey: value, status: "ready" }),
    typeof value === "string" ? [value] : (value && !Array.isArray(value) && typeof value === "object"
      ? [synthetic("container"), synthetic("inner")] : []));
}
add("escaped single quotes", assignment("token", "'synthetic \\'quoted\\' value'") + " status=ready", ["synthetic \\'quoted\\' value"]);
add("backtick command", assignment("apiKey", String.fromCharCode(96) + synthetic("backtick-value") + String.fromCharCode(96)) + "; status=ready", [synthetic("backtick-value")]);
add("unterminated quote", assignment("password", '"' + synthetic("unterminated")), [synthetic("unterminated")]);
add("unterminated container", assignment("secret", '{"nested":"' + synthetic("container-value") + '"'), [synthetic("container-value")]);
for (const marker of ["", "RSA ", "EC ", "ENCRYPTED "]) {
  add("PEM " + marker, "before\n-----BEGIN " + marker + "PRIVATE KEY-----\n" + synthetic("private-material")
    + "\n-----END " + marker + "PRIVATE KEY-----\nafter", [synthetic("private-material")]);
}
add("unterminated PEM", "before\n-----BEGIN PRIVATE KEY-----\n" + synthetic("private-material"), [synthetic("private-material")]);
add("terminal controls", "\u001b[31mBearer syn\u0007thetic-controls\u001b[0m", ["synthetic-controls"]);
for (const input of [
  "ordinary.version.string", "unicode 雪 😀 é", "0123456789abcdef0123456789abcdef01234567",
  token("xAKIA", "FAKE111111111111y"), token("xsk-", "nearmiss1234"),
  assignment("tokenId", "fixture") + " " + assignment("tokenCount", "42") + " " + assignment("credentialPresent", "true"),
  JSON.stringify({ key: "fixture-setting", credentialId: "fixture-connection", scopeKey: "fixture" }),
  assignment("knownSecrets", "secrets;", ": ") + " " + assignment("token", "string;", ": ") + " " + assignment("API_KEY", "config.key;"),
  "no secrets here", "Bearer [Redacted]",
  ...["<placeholder>", "$" + "{PLACEHOLDER}", "{{placeholder}}", "...", "''", "", "[]", "123"].map((value) => assignment("apiKey", value) + "; status=ready"),
  assignment("password", "fixture") + " status = ready",
  "fixture-boundary@example.test+suffix", "fixture-boundary@example.test_suffix",
  ["xeyJheader", "payload", "sig-x"].join("."), "雪 " + token("sk-", "boundary1234"),
  assignment("apiKey", "雪😀"), assignment("password", "\u00a0" + synthetic("unicode")),
  assignment("secret", '"雪\\😀value"'), "Bearer\u00a0" + synthetic("unicode"),
  "\u001b]0;title\u0007Bearer " + synthetic("osc"),
  assignment("secret", JSON.stringify({ nested: ["雪😀", { value: "synthetic" }] })),
  assignment("prefix.apiKey", "synthetic") + "; ready=yes",
  assignment("authorization", 'Scheme "synthetic quoted"') + " status=ready",
  assignment("token", '"<placeholder>"'), JSON.stringify({ "email hint": "fixture-boundary@example.test" }),
  assignment("email", "fixture-boundary@example.test"),
]) add("boundary " + cases.length, input);

add("round 3 FEFF assignment", "password\ufeff=\ufeffsynthetic-feff; status=ready", ["synthetic-feff"]);
add("round 3 FEFF Bearer", "Bearer\ufeffsynthetic-feff-bearer", ["synthetic-feff-bearer"]);
add("round 3 hyphen JWT", "-" + jwt, [jwt]);
add("round 3 terminal string", "\u001bPsyntheticTerminalPayload\u001b\\Bearer synthetic-terminal-bearer", ["synthetic-terminal-bearer", "syntheticTerminalPayload"]);
add("round 3 Unicode value boundary", "password=synthetic-boundary\u00a0following prose", ["synthetic-boundary"]);

// Complete malformed examples in the combined answer so adjacent cases stay independent;
// the standalone cases still exercise unterminated inputs exactly as written.
const input = cases.map((c) => c.input + (c.name === "unterminated quote" ? '"'
  : c.name === "unterminated container" ? "}" : c.name === "unterminated PEM" ? "\n-----END PRIVATE KEY-----" : "")).join("\n");
const credentials = [...new Set(cases.flatMap((c) => c.credentials))];
const fixture = { schemaVersion: 1, provenance: { repository: "ArdurAI/ardur-bot", revision,
  sources: ["apps/cli/src/text.ts", "packages/logging/src/redaction.ts"] },
  cases, savedAnswer: { input, expected: safeDiagnostic(input), credentials } };
for (const credential of credentials) {
  if (fixture.savedAnswer.expected.includes(credential)) throw new Error("Synthetic credential survived oracle redaction in " + cases.filter((c) => c.credentials.includes(credential)).map((c) => c.name).join(", "));
}
const target = "crates/home-client/tests/fixtures/redaction.json";
const encoded = JSON.stringify(fixture, null, 2) + "\n";
if (checking) {
  if (readFileSync(target, "utf8") !== encoded) throw new Error("Redaction fixture changed");
} else writeFileSync(target, encoded);
console.log("Verified " + cases.length + " TypeScript redaction cases and combined saved-answer corpus.");

// A fixed seed selects combinations, never expectations. Every expectation comes
// from the exact TypeScript functions above, also retained for offline regeneration.
const seed = 0x59503009;
let state = seed;
const random = () => {
  state ^= state << 13; state ^= state >>> 17; state ^= state << 5;
  return state >>> 0;
};
const pick = (values) => values[random() % values.length];
const whitespace = Array.from("\u0009\u000a\u000b\u000c\u000d\u0020\u00a0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a\u2028\u2029\u202f\u205f\u3000\ufeff");
if (whitespace.length !== 25 || whitespace.some((char) => !/\s/.test(char))) throw new Error("JavaScript whitespace set changed");
const terminals = [
  "", "\u001b[31m", "\u001b[0m", "\u009b31m",
  "\u001b]0;synthetic-title\u0007", "\u001b]0;synthetic-title\u001b\\",
  "\u001b]0;synthetic-title\u009c",
  ...["P", "_", "^"].flatMap((kind) => ["\u0007", "\u001b\\", "\u009c"].map((end) => "\u001b" + kind + "synthetic-payload" + end)),
  // C1 and malformed forms matter too: do not assume the utility strips them.
  "\u009d0;synthetic-title\u009c", "\u0090synthetic-payload\u009c",
  "\u009fsynthetic-payload\u009c", "\u009esynthetic-payload\u009c",
  ...["P", "_", "^", "]"].flatMap((kind) => ["\u0007", "\u001b\\", "\u009c"].map((end) => "\u001b" + kind + "syntheticPayload" + end)),
  "\u001b[?25l", "\u001b]unterminated", "\u001bPunterminated", "\u0000", "\u0007",
];
const families = [
  (s) => "password" + s + "=" + s + "synthetic-assignment" + s + "next",
  (s) => '"accessToken"' + s + ":" + s + '"synthetic-quoted"',
  (s) => "Authorization:" + s + "Bearer" + s + "synthetic-bearer",
  (s) => "Bearer" + s + "synthetic-bearer",
  () => "-" + jwt, () => "prefix-" + jwt, () => jwt,
  () => token("ghp_", "FAKEsynthetic1234"),
  () => token("sk-", "FAKEsynthetic1234"), () => token("ASIA", "FAKE000000000000"),
  () => aws, () => url, () => "fixture@example.test",
  (s) => "tokenId" + s + "=" + s + "fixture",
  (s) => "apiKey" + s + "=" + s + "雪😀" + s + "harmless",
  (s) => "password=synthetic" + s + "prose",
  () => "harmless 雪 😀 é ordinary.version.string",
  (s) => "knownSecrets:" + s + "secrets;",
  (s) => "secret=" + s + '{"nested":["synthetic","雪😀"]}',
  (s) => "token=" + s + '"synthetic \\"quoted\\" 雪😀"',
];
const prefixes = ["", "-", "'", '"', "[", "(", "before ", "雪", "x", "_", "https://example.test/"];
const suffixes = ["", " after", "'", '"', "]", ")", "; status=ready", "/path", "?next=ready", "\n"];
const differential = [];
const diff = (name, input) => differential.push({ name, input, expected: safeDiagnostic(input) });
// Exhaustive anchors ensure all reported families and every JS whitespace are
// covered even if seeded selection changes later.
for (const s of whitespace) {
  for (const family of families) diff("whitespace " + differential.length, family(s));
}
for (const terminal of terminals) {
  for (const input of ["Bearer synthetic-control", "-" + jwt, "password=synthetic-control"]) {
    diff("terminal " + differential.length, terminal + input);
    diff("terminal suffix " + differential.length, input + terminal);
    diff("terminal inside " + differential.length, input.slice(0, 3) + terminal + input.slice(3));
  }
}
for (let i = 0; i < 4096; i++) {
  const separator = pick([...whitespace, "\u0085", "\u001c", "", "  "]);
  const input = pick(prefixes) + pick(terminals) + pick(families)(separator)
    + pick(terminals) + pick(suffixes) + pick(whitespace) + pick(families)(separator);
  diff("seeded " + i, input);
}
const generated = { schemaVersion: 1, generator: { seed, randomCases: 4096, count: differential.length,
  whitespaceCodePoints: whitespace.map((char) => char.codePointAt(0)), terminalCount: terminals.length }, provenance, cases: differential };
const differentialTarget = "crates/home-client/tests/fixtures/redaction-differential.json";
const differentialText = JSON.stringify(generated, null, 2) + "\n";
if (checking) {
  if (readFileSync(differentialTarget, "utf8") !== differentialText) throw new Error("Seeded differential fixture changed");
} else writeFileSync(differentialTarget, differentialText);
console.log("Verified " + differential.length + " seeded differential cases.");
