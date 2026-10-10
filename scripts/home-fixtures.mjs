// Synthetic conformance vectors. Imports the oracle without writing to it.
// Run with the oracle's installed tsx loader; see docs/home-client.md.
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createHash, generateKeyPairSync, sign as cryptoSign, verify, X509Certificate } from "node:crypto";
import { execFileSync } from "node:child_process";

const oracle = resolve(process.argv[2] ?? ".");
const load = (p) => import(pathToFileURL(resolve(oracle, p)).href);
// Oracle modules load lazily so offline modes (--stage4) need no checkout.
const oracleModules = async () => {
  const [contracts, crypto, client, grants, api] = await Promise.all([
    load("packages/contracts/src/dispatch.ts"),
    load("apps/cli/src/crypto.ts"),
    load("apps/cli/src/client.ts"),
    load("packages/db/src/device-grants.ts"),
    load("apps/api/src/instance-certificate.js"),
  ]);
  return { contracts, crypto, client, verifyDeviceSignature: grants.verifyDeviceSignature, generateInstanceCertificate: api.generateInstanceCertificate };
};
const revision = () => execFileSync("git", ["rev-parse", "HEAD"], { cwd: oracle, encoding: "utf8" }).trim();
if (process.argv[3] === "--stage4") {
  // Sign the committed Stage 4 golden bytes with a fresh synthetic device key.
  // The signedText strings are the home's own bytes; this adds public DER
  // signatures so Rust and the offline checker can verify the exact vectors.
  // The home revision that produced the committed bytes is a required argument.
  const sourceRevision = process.argv[4];
  if (!sourceRevision) throw new Error("Usage: home-fixtures.mjs <oracle> --stage4 <home-revision>");
  const raw = readFileSync("crates/home-protocol/tests/fixtures/device-operations.json", "utf8");
  const sha256 = (v) => createHash("sha256").update(v).digest("hex");
  const fixture = JSON.parse(raw);
  const { privateKey, publicKey } = generateKeyPairSync("ec", { namedCurve: "prime256v1" });
  const spki = publicKey.export({ type: "spki", format: "der" }).toString("base64");
  const key = privateKey;
  for (const r of fixture.requests) {
    r.signature = cryptoSign(null, Buffer.from(r.signedText), key).toString("base64");
    if (!verify(null, Buffer.from(r.signedText), publicKey, Buffer.from(r.signature, "base64")))
      throw new Error("Stage 4 signature mismatch");
  }
  fixture.publicKey = spki;
  // Raw lone-surrogate vectors stay only in the copied golden bytes; this
  // public companion must parse with any strict JSON reader.
  delete fixture.rejected;
  fixture.provenance = {
    repository: "ArdurAI/ardur-bot",
    revision: sourceRevision,
    source: "apps/cli/fixtures/device-operations.json",
    sha256: sha256(raw),
  };
  writeFileSync("crates/home-protocol/tests/fixtures/typescript-stage4.json", JSON.stringify(fixture, null, 2) + "\n");
  console.log("Signed Stage 4 golden bytes with a fresh public device key.");
} else if (process.argv[3] === "--verify-rust") {
  const { contracts, verifyDeviceSignature } = await oracleModules();
  const vectors = JSON.parse(readFileSync(process.argv[4], "utf8"));
  for (const v of vectors) {
    const text = v.kind === "pairing" ? contracts.pairingSignedText(v.payload.challenge,v.payload.instanceId,v.publicKey,v.presencePublicKey) : contracts.deviceSignedText(v.instanceId,v.proof,v.operation,v.body);
    if (text !== v.text) throw new Error("Rust canonical text mismatch");
    if (!verifyDeviceSignature(v.publicKey, v.text, v.signature)) throw new Error("Rust signature rejected");
    if (verifyDeviceSignature(v.publicKey, v.text + "changed", v.signature)) throw new Error("Alteration accepted");
  }
  console.log("TypeScript server verifier accepted all Rust signatures; alterations rejected.");
} else if (process.argv[3] === "--stage3") {
  const { contracts, crypto, verifyDeviceSignature } = await oracleModules();
  const raw = readFileSync(resolve(oracle, "apps/cli/fixtures/device-operations.json"), "utf8");
  const fixture = JSON.parse(raw);
  const keys = crypto.createDeviceKeys();
  for (const r of fixture.requests) {
    if (contracts.canonicalDispatchJson(r.body) !== r.canonicalBody ||
        contracts.deviceSignedText(fixture.instanceId, fixture.proof, r.operation, r.body) !== r.signedText) throw new Error("Stage 3 oracle mismatch");
    r.signature = crypto.signRequest({instanceId:fixture.instanceId,grantId:fixture.proof.grantId,privateKey:keys.privateKey},fixture.proof.nonce,fixture.proof.timestamp,r.operation,r.body).signature;
    if (!verifyDeviceSignature(keys.publicKey,r.signedText,r.signature)) throw new Error("Stage 3 signature mismatch");
  }
  for (const r of fixture.rejected) {
    let refused = false;
    try { contracts.canonicalDispatchJson(r.body); } catch { refused = true; }
    if (!refused) throw new Error("Malformed Unicode accepted");
  }
  writeFileSync("crates/home-protocol/tests/fixtures/device-operations.json",raw);
  delete fixture.rejected;
  fixture.publicKey = keys.publicKey;
  fixture.provenance = {repository:"ArdurAI/ardur-bot",revision:revision(),source:"apps/cli/fixtures/device-operations.json"};
  writeFileSync("crates/home-protocol/tests/fixtures/typescript-stage3.json",JSON.stringify(fixture,null,2)+"\n");
  console.log("Copied Stage 3 golden bytes and generated public TypeScript signatures.");
} else if (process.argv[3] === "--numbers") {
  const { contracts } = await oracleModules();
  const inputs = ["333333333.33333329","8.256320039984491e-05","0.84551240822557006","1.2345678901234568","2.2250738585072014e-308"];
  const fixture = {schemaVersion:1,provenance:{repository:"ArdurAI/ardur-bot",revision:revision(),source:"packages/contracts/src/dispatch.ts"},cases:inputs.map(raw=>({raw,canonical:contracts.canonicalDispatchJson(JSON.parse(raw))}))};
  writeFileSync("crates/home-protocol/tests/fixtures/numbers.json",JSON.stringify(fixture,null,2)+"\n");
  console.log("Generated TypeScript decimal-parse boundary vectors.");
} else {
  const { contracts, crypto, client, verifyDeviceSignature, generateInstanceCertificate } = await oracleModules();
  const keys = crypto.createDeviceKeys();
  const material = await generateInstanceCertificate();
  const cert = new X509Certificate(material.certificate);
  const sha = (v) => createHash("sha256").update(v).digest("hex");
  const payload = {version:1,challenge:"synthetic-challenge-".repeat(3),instanceId:"fixture-home",homeName:"Home 雪",
    fingerprint:sha(cert.publicKey.export({type:"spki",format:"der"})),certificateFingerprint:sha(cert.raw),
    hints:["https://home.example.test"]};
  const proof = {grantId:"fixture-grant",nonce:"n".repeat(43),timestamp:1791429078000};
  const bodies = [
    {text:"雪 😀 é \u0000 \u2028", nested:{z:[true,null,{β:"value"}],a:1}, omitted:undefined},
    {"\uE000":1,"😀":2,"2":3,"10":4,numbers:[-0,1e-7,1e-6,1e20,1e21,Number.MAX_SAFE_INTEGER,Number.MIN_VALUE,1.7976931348623157e308]},
    {procedure:"bots/list",input:{}},
  ];
  const requests = bodies.map(body => {
    const text = contracts.deviceSignedText(payload.instanceId, proof, "rpc", body);
    // Keep the original expression visible: this wire normalization removes
    // undefined members and negative zero. Raw-input vectors live separately.
    return {body:JSON.parse(JSON.stringify(body)),canonical:contracts.canonicalDispatchJson(body),text,
      signature:crypto.signRequest({...payload,grantId:proof.grantId,privateKey:keys.privateKey},proof.nonce,proof.timestamp,"rpc",body).signature};
  });
  const clientChallenge = "c".repeat(43);
  const homeText = contracts.homeSignedText(payload.instanceId,payload.fingerprint,clientChallenge);
  const identity = {instanceId:payload.instanceId,fingerprint:payload.fingerprint,certificate:cert.raw.toString("base64"),
    signature:cryptoSign("sha256",Buffer.from(homeText),material.privateKey).toString("base64")};
  const pairingText = contracts.pairingSignedText(payload.challenge,payload.instanceId,keys.publicKey,keys.presencePublicKey);
  const fixture = {schemaVersion:1,provenance:{repository:"ArdurAI/ardur-bot",revision:revision(),
    sources:["apps/cli/src/crypto.ts","apps/cli/src/client.ts","packages/contracts/src/dispatch.ts","packages/db/src/device-grants.ts"]},
    homeKeyAlgorithm:cert.publicKey.asymmetricKeyType,payload,code:Buffer.from(JSON.stringify(payload)).toString("base64url"),publicKey:keys.publicKey,
    presencePublicKey:keys.presencePublicKey,pairingText,pairingSignature:crypto.signPairing(payload,keys),proof,requests,
    clientChallenge,homeText,identity,validAt:Date.parse(cert.validFrom)+1000,expiredAt:Date.parse(cert.validTo)};
  if (JSON.stringify(client.decodePairingCode(fixture.code)) !== JSON.stringify(payload)) throw new Error("decode failed");
  for (const r of requests) if (!verifyDeviceSignature(keys.publicKey,r.text,r.signature)) throw new Error("oracle mismatch");
  writeFileSync("crates/home-protocol/tests/fixtures/typescript.json",JSON.stringify(fixture,null,2)+"\n");
  console.log("Generated synthetic public vectors from "+fixture.provenance.revision+"; no private keys retained.");
}
