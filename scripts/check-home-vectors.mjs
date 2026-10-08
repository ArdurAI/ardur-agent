// Offline public-fixture check. The actual TypeScript oracle check is home-fixtures.mjs --verify-rust.
import { readFileSync } from "node:fs";
import { createPublicKey, verify, X509Certificate } from "node:crypto";
import assert from "node:assert/strict";
const canonical = (v) => Array.isArray(v) ? "["+v.map(canonical).join(",")+"]"
  : v && typeof v === "object" ? "{"+Object.entries(v).sort(([a],[b])=>a<b?-1:a>b?1:0).map(([k,v])=>JSON.stringify(k)+":"+canonical(v)).join(",")+"}" : JSON.stringify(v);
const ts = JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/typescript.json","utf8"));
const numbers=JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/numbers.json","utf8"));
for (const c of numbers.cases) assert.equal(canonical(JSON.parse(c.raw)),c.canonical);
const rust = JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/rust.json","utf8"));
for (const r of ts.requests) {
  assert.equal(canonical(r.body),r.canonical);
  assert(verify("sha256",Buffer.from(r.text),createPublicKey({key:Buffer.from(ts.publicKey,"base64"),type:"spki",format:"der"}),Buffer.from(r.signature,"base64")));
}
assert.equal(new X509Certificate(Buffer.from(ts.identity.certificate,"base64")).publicKey.asymmetricKeyType,"rsa");
for (const v of rust) {
  const text = v.kind === "pairing" ? canonical(["ardur-pair-v1",v.payload.instanceId,v.payload.challenge,v.publicKey,v.presencePublicKey])
    : canonical(["ardur-device-v1",v.instanceId,v.proof.grantId,v.proof.nonce,v.proof.timestamp,v.operation,v.body]);
  assert.equal(text,v.text);
  const key=createPublicKey({key:Buffer.from(v.publicKey,"base64"),type:"spki",format:"der"});
  assert.equal(key.asymmetricKeyDetails.namedCurve,"prime256v1");
  assert(verify("sha256",Buffer.from(text),key,Buffer.from(v.signature,"base64")));
  assert(!verify("sha256",Buffer.from(text+"changed"),key,Buffer.from(v.signature,"base64")));
}
console.log("Public TypeScript/Rust vectors verified offline.");
// Raw inputs are retained here: JSON.stringify would erase the differences.
const domain = JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/input-domain.json", "utf8"));
for (const c of domain.cases) {
  if (c.javascriptParseRejects) assert.throws(() => JSON.parse(c.raw));
  else assert.equal(canonical(JSON.parse(c.raw)), c.javascriptCanonical);
}
const normalized = {negativeZero:-0,omitted:undefined};
assert.equal(JSON.stringify(normalized), domain.normalization.wire);
assert.equal(canonical(JSON.parse(JSON.stringify(normalized))), domain.normalization.canonical);
assert(Object.is(JSON.parse("-0"), -0));
assert.equal(JSON.parse("1e400"), Infinity);
console.log("Raw Unicode and number input-domain vectors checked offline.");

const stage3=JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/typescript-stage3.json","utf8"));
const copied=JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/device-operations.json","utf8"));
assert.equal(stage3.requests.length,8);
for (let i=0;i<stage3.requests.length;i++) {
  const r=stage3.requests[i], original=copied.requests[i];
  assert.equal(canonical(r.body),original.canonicalBody);
  assert.equal(r.signedText,original.signedText);
  assert(verify("sha256",Buffer.from(r.signedText),createPublicKey({key:Buffer.from(stage3.publicKey,"base64"),type:"spki",format:"der"}),Buffer.from(r.signature,"base64")));
}
assert.equal(copied.rejected.length,4);
console.log("Stage 3 copied golden bytes and public TypeScript signatures verified.");
