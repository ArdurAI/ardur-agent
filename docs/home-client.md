# Rust paired-home client (Stage 2)

The builder and local-first user can pair a device, check its current read grant and list saved bots
without starting another bot runtime. This is a separate `ardur-rs` executable.
The existing runtime and TypeScript CLI are not replaced in this stage.

```sh
ardur-rs pair --file pairing-code.txt --name "Command line" --json
# Or read the code from stdin, not a command argument:
ardur-rs pair --file - --json
ardur-rs status --json
ardur-rs bots list --json
```

Every finite JSON command writes one versioned object, including errors:

```json
{"schemaVersion":1,"ok":true,"command":"status","data":{"homeName":"Home","instanceId":"home-id","valid":true}}
```

`status` performs a signed `tasks` read. It checks the current grant and read
scope, not service health. Bot listing uses signed `rpc` with
`{"procedure":"bots/list","input":{}}` and returns
`{bots:[{id,name,threadId,status,modelProvider,modelId,thinkingLevel,runtimeKind}]}`.
It does not return bot instructions, credentials or runtime configuration.
Pair returns `{homeName,instanceId,paired:true}`.

Exit codes: 0 success; 1 transport/protocol/expired nonce; 2 access, changed
identity or unsafe storage; 3 invalid input. JSON failures also include
`error:{code,message}` and `exitCode`. Arbitrary remote errors and invalid
arguments are never echoed. Human output escapes terminal controls and bidi
format controls. JSON preserves the actual home/bot metadata with JSON escaping.

## Identity and transport

The TypeScript home owns the protocol. Pairing codes are strict version-1
payloads, accepted as JSON or exported base64url. The first hint must be an
HTTPS origin, with no credentials, query or fragment. No HTTP fallback,
redirect following, proxy interception or automatic application retry is added.
Each POST is bounded to 15 seconds and a two-MiB response.

TLS validates the exact full-certificate SHA-256 pin and validity interval
before HTTP headers or body are sent. TLS handshake signatures are verified;
0-RTT and TLS session resumption are disabled, including when a caller reuses
one transport. Each POST still creates a fresh transport in the client flow.
Public-CA trust and hostname checks do not replace the
out-of-band home pin. Every nonce exchange additionally checks the home
instance, public-key SPKI SHA-256 pin and fresh client-challenge proof.

The current home certificate generator uses **RSA**. Home proof verification
accepts RSA PKCS#1 v1.5/SHA-256 (2048–8192 bits) and P-256 ECDSA/SHA-256.
Device keys remain two separate **P-256** pairs. Request signatures are standard
base64 **DER ECDSA**, not JWS/P1363. Public keys are standard base64 SPKI DER.
The presence private key is discarded, matching the ordinary TypeScript CLI;
this client does not assert user presence or approve consequential work.

A fresh request nonce and timestamp are obtained before each signed request.
Nonce lengths and the 60-second timestamp window match the TypeScript client.
The home consumes the nonce and owns replay prevention, revocation, membership
and scopes. The CLI cannot grant itself permissions. Source/server homes must
offer device-only HTTPS using the exact paired certificate; a different
reverse-proxy certificate will be refused.

## Private-file storage

This stage deliberately uses an **unencrypted private file**, not a keychain.
It never silently falls back from an OS credential store. `SecretStore` is the
boundary for a later protected OS backend.

The file is `paired-home.json` under:

- macOS: `~/Library/Application Support/ardur`
- Linux: `$XDG_CONFIG_HOME/ardur`, otherwise `~/.config/ardur`
- Windows path convention: `%APPDATA%\\ardur`, but storage is **refused** until
  a protected current-user ACL backend is implemented.

Unix requires a current-user-owned 0700 directory and 0600 regular file with
one link. Directory traversal and file opens refuse symlinks. Reads are bounded
to 64 KiB, checked on the opened descriptor, and keys are validated before use.
The private key is stored as a JSON array of bytes, limited to 4096 bytes. The
loader borrows that raw field and decodes directly into one fixed-capacity
zeroizing allocation, then transfers it to the retained key without a PEM copy.
Fixed read and write buffers also avoid reallocated copies of the encoded key.
Partial keys and invalid UTF-8 are wiped on errors. The JSON parser never
decodes a PEM string. Earlier draft files with a string key are refused; pair
again to write the new format. There is no automatic legacy migration.
Writes use an exclusive private temporary file, file sync, atomic rename and
directory sync. Unsafe existing state is never overwritten or repaired silently.

The typed profile retains version, HTTPS origin, home name, instance/pins, grant
and space. It does not retain the one-time pairing challenge or a presence key.
Profile and request key are saved together atomically. It is separate from the
TypeScript CLI's state; migration and packaging cutover remain later stages.

Protect backups and the local account. File permissions do not encrypt data
or defend against an attacker already acting as the same user. Public fixtures
contain synthetic public keys/certificates and signatures only. CLI output
intentionally includes the selected home/bot metadata, not arbitrary fields.

## Conformance and disposable tests

Fixture provenance: `ArdurAI/ardur-bot` revision
`158ec7beb782ea8e95c8ae9b88d1568b10b4ee9f`. The fixture embeds source paths and
revision. `scripts/home-fixtures.mjs` imports the TypeScript functions directly
from a read-only oracle checkout and writes only in this repository. It imports
the server signature verifier without opening a database.

```sh
# ORACLE is a read-only ardur-bot checkout with its existing tsx dependencies.
node --import "${ORACLE}/node_modules/tsx/dist/loader.mjs" scripts/home-fixtures.mjs "${ORACLE}"
node --import "${ORACLE}/node_modules/tsx/dist/loader.mjs" scripts/home-fixtures.mjs "${ORACLE}" --numbers
cargo run -p home-protocol --example rust_vectors > crates/home-protocol/tests/fixtures/rust.json
node --import "${ORACLE}/node_modules/tsx/dist/loader.mjs" scripts/home-fixtures.mjs "${ORACLE}" --verify-rust crates/home-protocol/tests/fixtures/rust.json
node scripts/check-home-vectors.mjs
```

Rust checks byte-identical pairing/home/device text and canonical JSON, verifies
TypeScript DER signatures and the RSA home proof, and exports public Rust
vectors. The actual TypeScript server verifier checks those Rust signatures and
canonical text in the reverse direction. The offline Node check in CI is a
public-fixture check, not a claim that CI imported the separate TypeScript repo.

Vectors cover Unicode, UTF-16 key ordering, nested arrays/objects, omitted
optionals, negative zero, small/subnormal numbers, exponent boundaries and
safe-integer/max-finite boundaries. Rust uses JavaScript number formatting,
not Rust's default JSON serialization, for signed bytes. Decimal parsing uses
serde_json's round-trip mode so IEEE-754 parse rounding also matches JavaScript;
separate TypeScript boundary vectors cover the regression before formatting.

The accepted string domain is **well-formed Unicode** in pairing payloads and
all canonical JSON string values and object keys. Valid surrogate pairs decode
normally. Lone high or low surrogates are refused with a typed error and a plain
sentence; they are never replaced with U+FFFD, which would change signed bytes.
Rust enforces this now. The current TypeScript implementation accepts lone
surrogates; Stage 3 must tighten its input checks to this agreed contract before
claiming parity for all accepted inputs. `parse_json` checks raw canonical JSON;
Rust `String` and `Value` already enforce the representable string domain.

The fixture generator's `JSON.stringify` step produces the wire body: it changes
negative zero to zero and removes object members whose value is `undefined`.
That step cannot prove parity on the original inputs. Separate raw-input vectors
therefore pin `-0` to canonical `0`, reject raw `undefined` as invalid JSON, and
check the normalized object with the omitted member absent. Undefined is a
JavaScript-only value outside the accepted JSON input domain.

Finite IEEE-754 numbers are the accepted number domain. JavaScript parses
`1e400` as Infinity and serializes it as `null`; Rust rejects the overflowing raw
number. The agreed behavior is to refuse non-finite input before normalization,
not silently sign it as `null`. Stage 3 must enforce this on the TypeScript side.
The overflow vector records both current JavaScript behavior and Rust rejection.
The raw input-domain vectors are contract tests checked with the offline Node
checker, separate from the fixtures imported from the TypeScript oracle.

The fake home builds pairing and supported request texts independently and uses
a separate DER signature verifier. Its bots/list route is checked against the
committed TypeScript request vector, including rejection of altered text. The
TLS counter records every decoded byte, including incomplete headers and bodies.
A controllable-clock regression reuses a transport across certificate expiry for
TLS 1.2 and TLS 1.3 with a ticket-capable server. Its in-memory positive control
reproduces the defect with resumption enabled; the fixed configuration refuses
expiry with zero decoded HTTP bytes. An allocator probe checks successful loads,
legacy refusal, and parser/validation error paths for freed complete PEM copies.

Tests use disposable loopback HTTPS homes and in-memory grants only.
They exercise pair/status/bots, revoked access, expired/replayed nonces,
altered identity/proof, wrong/expired TLS certificates with zero HTTP bytes,
redirect refusal, malformed/oversized responses, private storage and safe output.
CLI subprocess tests cover file/stdin input, name, JSON envelope, exit codes,
projection and terminal controls. No owner home, database, model or tool runtime
is involved. There is no model-turn cost for these read commands.

```sh
# Local verification uses the already installed stable toolchain; no download.
export RUSTUP_TOOLCHAIN=stable
cargo build -p home-protocol -p home-client -p ardur-rs
cargo test -p home-protocol -p home-client -p ardur-rs
cargo clippy -p home-protocol -p home-client -p ardur-rs --all-targets -- -D warnings
cargo fmt --check
cargo tree -p home-client
```

Local toolchain: Rust 1.94.1. CI uses the repository pin, 1.98.1.
The shipping client graph has no dependency on the old runtime, server,
providers, memory, retrieval or tool-execution crates.

## Follow-ups and acceptance boundary

Independent review, installed-home acceptance and landing are separate from
implementation delivery. This stage does not replace the supported `ardur`
binary, migrate old profiles, add a keychain backend or claim Windows pairing
support.

Repository process follow-up: the drive-specific session-journal convention in
`AGENTS.md` predates registered-computer storage. For this delivery a private
registered-home journal outside the worktree was explicitly approved; all
drive-specific writes were skipped. Make that convention portable without
changing approval boundaries. No machine paths or private journal content belong
in the public repository.
