// Offline public-fixture check. The actual TypeScript oracle check is home-fixtures.mjs --verify-rust.
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { createHash, createPublicKey, verify, X509Certificate } from "node:crypto";
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
assert.equal(copied.requests.length,14);
// The newer revision inserted Stage 4 operations before "stop", so match by content.
const copiedByText=new Map(copied.requests.map((r)=>[r.signedText,r]));
for (const r of stage3.requests) {
  const original=copiedByText.get(r.signedText);
  assert(original,`stage 3 vector missing from the copied golden bytes: ${r.signedText}`);
  assert.equal(canonical(r.body),original.canonicalBody);
  assert(verify("sha256",Buffer.from(r.signedText),createPublicKey({key:Buffer.from(stage3.publicKey,"base64"),type:"spki",format:"der"}),Buffer.from(r.signature,"base64")));
}
assert.equal(copied.rejected.length,4);
console.log("Stage 3 vectors still match the copied golden bytes; public TypeScript signatures verified.");

const stage4=JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/typescript-stage4.json","utf8"));
assert.equal(stage4.requests.length,copied.requests.length);
assert.equal(stage4.provenance.repository,"ArdurAI/ardur-bot");
assert.equal(stage4.provenance.revision,"12475e3af2e2b10605a27f0245f3db71b8e47f71");
assert.equal(stage4.provenance.source,"apps/cli/fixtures/device-operations.json");
assert.equal(stage4.provenance.sha256,createHash("sha256").update(readFileSync("crates/home-protocol/tests/fixtures/device-operations.json","utf8")).digest("hex"));
const stage4Key=createPublicKey({key:Buffer.from(stage4.publicKey,"base64"),type:"spki",format:"der"});
assert.equal(stage4Key.asymmetricKeyDetails.namedCurve,"prime256v1");
for (let i=0;i<stage4.requests.length;i++) {
  const r=stage4.requests[i], original=copied.requests[i];
  assert.equal(canonical(r.body),original.canonicalBody);
  assert.equal(r.signedText,original.signedText);
  assert(verify("sha256",Buffer.from(r.signedText),stage4Key,Buffer.from(r.signature,"base64")));
  assert(!verify("sha256",Buffer.from(r.signedText+"changed"),stage4Key,Buffer.from(r.signature,"base64")));
}
assert(stage4.requests.every((r)=>copiedByText.has(r.signedText)));
const work=copied.responses.find((r)=>r.operation==="rooms/send"&&r.body.kind==="work");
assert.equal(work.body.runIds.length,2);
const greeting=copied.responses.find((r)=>r.operation==="rooms/send"&&r.body.kind==="receipt-only");
assert.equal(greeting.body.taskId,undefined);
assert.equal(greeting.body.runId,undefined);
const denial=copied.responses.find((r)=>r.procedure==="board/show"&&r.body.problem);
assert.equal(denial.body.problem.code,"access_lost");
assert.equal(copied.responses.length,7);
console.log("Stage 4 copied golden bytes, provenance SHA-256 and public TypeScript signatures verified.");

const eventsRaw=readFileSync("crates/home-protocol/tests/fixtures/device-events.json");
const events=JSON.parse(eventsRaw);
const eventsProvenance=JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/device-events-provenance.json","utf8"));
assert.equal(eventsProvenance.repository,"ArdurAI/ardur-bot");
assert.equal(eventsProvenance.revision,"2910f63e231ebc8fbf9c4177d4fc1681f04b3752");
assert.equal(eventsProvenance.source,"apps/cli/fixtures/device-operations.json");
assert.equal(eventsProvenance.sha256,"f5939feb81f958b8aa2bbbd38784fcd1c6dcdfdbde86440785f79a820fa5a3f3");
assert.equal(createHash("sha256").update(eventsRaw).digest("hex"),eventsProvenance.sha256);
assert.equal(events.requests.length,17);
assert.equal(events.requests.filter((r)=>r.operation==="events").length,3);
for (const r of events.requests) {
  assert.equal(canonical(r.body),r.canonicalBody);
  assert.equal(canonical(["ardur-device-v1",events.instanceId,events.proof.grantId,events.proof.nonce,events.proof.timestamp,r.operation,r.body]),r.signedText);
}
const stage5=JSON.parse(readFileSync("crates/home-protocol/tests/fixtures/typescript-stage5.json","utf8"));
assert.deepEqual(stage5.provenance,eventsProvenance);
assert.equal(stage5.requests.length,events.requests.length);
const stage5Key=createPublicKey({key:Buffer.from(stage5.publicKey,"base64"),type:"spki",format:"der"});
assert.equal(stage5Key.asymmetricKeyDetails.namedCurve,"prime256v1");
for (let i=0;i<stage5.requests.length;i++) {
  const r=stage5.requests[i];
  assert.equal(r.signedText,events.requests[i].signedText);
  assert.deepEqual(r.body,events.requests[i].body);
  assert(verify("sha256",Buffer.from(r.signedText),stage5Key,Buffer.from(r.signature,"base64")));
  assert(!verify("sha256",Buffer.from(r.signedText+"changed"),stage5Key,Buffer.from(r.signature,"base64")));
}
assert.equal(events.eventStreams.length,9);
for (const stream of events.eventStreams) {
  assert.equal(Buffer.concat(stream.utf8HexChunks.map((c)=>Buffer.from(c,"hex"))).toString("utf8"),stream.wire);
}
console.log("Stage 5 signed event bytes, SSE chunks and provenance SHA-256 verified offline.");

// Regenerate both corpora from committed, hash-pinned pure TypeScript sources.
execFileSync(process.execPath, ["scripts/home-redaction-fixtures.mjs", "--check"], { stdio: "inherit" });
