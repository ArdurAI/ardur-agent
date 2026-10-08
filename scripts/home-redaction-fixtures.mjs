// Pure text oracle only. Never imports the application or accesses a service.
import { readFileSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { stripTypeScriptTypes } from "node:module";
import { resolve } from "node:path";

const oracle = resolve(process.argv[2]);
const revision = execFileSync("git", ["rev-parse", process.argv[3] ?? "HEAD"], { cwd: oracle, encoding: "utf8" }).trim();
const source = (path) => execFileSync("git", ["show", revision + ":" + path], { cwd: oracle, encoding: "utf8" });
const moduleUrl = (text) => "data:text/javascript;base64," + Buffer.from(stripTypeScriptTypes(text)).toString("base64");
const redactions = moduleUrl(source("packages/logging/src/redaction.ts"));
const textSource = source("apps/cli/src/text.ts").replace('"@ardurbot/logging"', JSON.stringify(redactions));
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

// Complete malformed examples in the combined answer so adjacent cases stay independent;
// the standalone cases still exercise unterminated inputs exactly as written.
const input = cases.map((c) => c.input + (c.name === "unterminated quote" ? '"'
  : c.name === "unterminated container" ? "}" : c.name === "unterminated PEM" ? "\n-----END PRIVATE KEY-----" : "")).join("\n");
const credentials = [...new Set(cases.flatMap((c) => c.credentials))];
const fixture = { schemaVersion: 1, provenance: { repository: "ArdurAI/ardur-bot", revision,
  sources: ["apps/cli/src/text.ts", "packages/logging/src/redaction.ts"] },
  cases, savedAnswer: { input, expected: safeDiagnostic(input), credentials } };
for (const credential of credentials) {
  if (fixture.savedAnswer.expected.includes(credential)) throw new Error("Synthetic credential survived oracle redaction");
}
const target = "crates/home-client/tests/fixtures/redaction.json";
const encoded = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--check")) {
  if (readFileSync(target, "utf8") !== encoded) throw new Error("Redaction fixture changed");
} else writeFileSync(target, encoded);
console.log("Verified " + cases.length + " TypeScript redaction cases and combined saved-answer corpus.");
