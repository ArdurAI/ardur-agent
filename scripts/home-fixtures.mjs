// Synthetic conformance vectors. Imports the oracle without writing to it.
// Run with the oracle's installed tsx loader; see docs/home-client.md.
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createHash, sign, X509Certificate } from "node:crypto";
import { execFileSync } from "node:child_process";

const oracle = resolve(process.argv[2]);
const load = (p) => import(pathToFileURL(resolve(oracle, p)).href);
const contracts = await load("packages/contracts/src/dispatch.ts");
const crypto = await load("apps/cli/src/crypto.ts");
const client = await load("apps/cli/src/client.ts");
const { verifyDeviceSignature } = await load("packages/db/src/device-grants.ts");
const { generateInstanceCertificate } = await load("apps/api/src/instance-certificate.js");
const revision = execFileSync("git", ["rev-parse", "HEAD"], { cwd: oracle, encoding: "utf8" }).trim();
if (process.argv[3] === "--verify-rust") {
  const vectors = JSON.parse(readFileSync(process.argv[4], "utf8"));
  for (const v of vectors) {
    const text = v.kind === "pairing" ? contracts.pairingSignedText(v.payload.challenge,v.payload.instanceId,v.publicKey,v.presencePublicKey) : contracts.deviceSignedText(v.instanceId,v.proof,v.operation,v.body);
    if (text !== v.text) throw new Error("Rust canonical text mismatch");
    if (!verifyDeviceSignature(v.publicKey, v.text, v.signature)) throw new Error("Rust signature rejected");
    if (verifyDeviceSignature(v.publicKey, v.text + "changed", v.signature)) throw new Error("Alteration accepted");
  }
  console.log("TypeScript server verifier accepted all Rust signatures; alterations rejected.");
} else {
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
    return {body:JSON.parse(JSON.stringify(body)),canonical:contracts.canonicalDispatchJson(body),text,
      signature:crypto.signRequest({...payload,grantId:proof.grantId,privateKey:keys.privateKey},proof.nonce,proof.timestamp,"rpc",body).signature};
  });
  const clientChallenge = "c".repeat(43);
  const homeText = contracts.homeSignedText(payload.instanceId,payload.fingerprint,clientChallenge);
  const identity = {instanceId:payload.instanceId,fingerprint:payload.fingerprint,certificate:cert.raw.toString("base64"),
    signature:sign("sha256",Buffer.from(homeText),material.privateKey).toString("base64")};
  const pairingText = contracts.pairingSignedText(payload.challenge,payload.instanceId,keys.publicKey,keys.presencePublicKey);
  const fixture = {schemaVersion:1,provenance:{repository:"ArdurAI/ardur-bot",revision,
    sources:["apps/cli/src/crypto.ts","apps/cli/src/client.ts","packages/contracts/src/dispatch.ts","packages/db/src/device-grants.ts"]},
    homeKeyAlgorithm:cert.publicKey.asymmetricKeyType,payload,code:Buffer.from(JSON.stringify(payload)).toString("base64url"),publicKey:keys.publicKey,
    presencePublicKey:keys.presencePublicKey,pairingText,pairingSignature:crypto.signPairing(payload,keys),proof,requests,
    clientChallenge,homeText,identity,validAt:Date.parse(cert.validFrom)+1000,expiredAt:Date.parse(cert.validTo)};
  if (JSON.stringify(client.decodePairingCode(fixture.code)) !== JSON.stringify(payload)) throw new Error("decode failed");
  for (const r of requests) if (!verifyDeviceSignature(keys.publicKey,r.text,r.signature)) throw new Error("oracle mismatch");
  writeFileSync("crates/home-protocol/tests/fixtures/typescript.json",JSON.stringify(fixture,null,2)+"\n");
  console.log("Generated synthetic public vectors from "+revision+"; no private keys retained.");
}
