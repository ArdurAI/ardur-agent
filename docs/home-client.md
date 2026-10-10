# Rust paired-home client (Stages 2–5)

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

## Stage 3 task commands

These commands need the Stage 3 home operations in home PR #187, including its
review-round receipt checks. Pair/status/bots keep their Stage 2 outputs and exits.

```sh
ardur-rs send <bot> <text> --request-id <id> [--wait] [--timeout 180s] [--json]
ardur-rs wait --run <id> [--timeout 180s] [--json]
ardur-rs runs list [--cursor <run-id>] [--limit 50] [--json]
ardur-rs runs show <id> [--json]
ardur-rs tasks show <id> [--json]
ardur-rs stop <task-id> [--json]
```

The bot is an exact ID or an unambiguous saved name. A request ID is required for
Rust send and must be 16–128 UTF-16 units. Text is nonblank and at most 32,000
UTF-16 units. The home owns execution and saved model/computer pins.

New commands use the TypeScript version-1 result family:

```json
{"version":1,"command":"wait","bot":null,"runId":"run-id","taskId":"task-id","verdict":"pass","replyText":"Saved answer","elapsedMs":250,"failureReason":null,"data":{"run":{}}}
```

The example abbreviates the run detail; actual data carries the complete typed
record. A successful show/list exits 0 even when a saved run failed. Wait exits 2
for failed/stopped runs, missing or mismatched answers, or protocol/transport
failure; 3 for a deadline; 4 for usage/access/input/storage/identity refusal.
Exit 1 is reserved for mismatch (there is no Rust test-bot command in this stage).

Timeouts accept milliseconds, seconds or minutes (`100ms`, `1.5s`, `3m`, or bare
seconds), up to 2,147,483,647 milliseconds, default 180 seconds. The whole send
and wait flow uses one deadline, including nonce/signature exchange, dispatch,
saved-state reads and sleep. Polling intervals are two seconds. A deadline drops
the pending wait, never sends stop and never undoes admission. It cannot guarantee
that a request already received by home did not take effect.

Recovery is explicit: rerun send with the same request ID, selected bot and text.
It maps to the existing `clientNonce`; fresh nonce/proof exchanges accompany each
request. Home returns the original admission for identical canonical input and
refuses changed input. No automatic retry or second recovery system is added.

`stop` prints “Cancellation requested for <task-id>.” Its response is only
`{cancelRequested:true}`; it does not confirm the task stopped. Wait keeps polling
when cancellation is requested but unconfirmed. Input and approval waits return
an action sentence instead of polling forever.

## Stage 4 daily reads and room sends

These commands need the Stage 4 home operations in home PR #212. Pair/status/bots
and the Stage 3 task commands keep their existing outputs and exits.

```sh
ardur-rs computers list [--json]
ardur-rs board list --workspace <id> [--filter <json>] [--search <text>] [--json]
ardur-rs board show --workspace <id> <item> [--json]
ardur-rs rooms list [--json]
ardur-rs rooms send (--room <name> | --room-id <id>) [--thread <id>] <text> [--json]
```

`computers list` signs `rpc` with `{"procedure":"computer/list","input":null}`
and prints each shared computer once: bot id, name, and its computer status
(kind, state, screen, control and update fields). `board list` signs
`board/snapshot` with `{workspaceId, filter?, search?}` and keeps the full typed
work items plus `readyIds`/`blockedIds`; `--filter` takes a JSON object such as
`{"status":"open"}`. `board show` signs `board/show` with `{workspaceId, id}`.
Board work items, comments and history never enter output as free-form remote
blobs; the typed record keeps the scalar fields and comment counts.

Board reads keep the board's own access checks. A denial (HTTP 403) or a known
board problem (HTTP 400, shared `BoardProblem` codes such as `access_lost` or
`no_board`) answers with the home's own fixed sentence and exits 4; unknown
failures keep the generic home error mapping.

`rooms list` signs `rooms/list` with `{}` and prints each room's id, name,
thread and members. `rooms send` chooses exactly one of `groupId` or
`roomName` (matching is case-insensitive inside the paired owner's space) with
an optional `threadId`, and signs `rooms/send` with
`{groupId?|roomName?, threadId?, clientNonce, text}`. Names that match zero or
multiple rooms refuse with the home's own answer (“This record is unavailable
from this device.”, exit 4); use `rooms list` and the room id. Scope, grant or
membership refusals keep the fixed access sentence.

A work answer keeps every run id: the JSON result carries the full `runIds`
array and the human output lists them all. A greeting can return
`{kind:"receipt-only", seq, receipt}` with no task or run; the client invents no
run id, starts no wait, and prints the receipt text honestly.

Room-send recovery is explicit and durable, like Stage 3 send. Each new send
generates a fresh 43-character `clientNonce` and records the pending send beside
the pairing; rerunning the identical room and text reuses that nonce, so the
home returns the original admission — including all run ids — instead of
starting the message twice. A completed response clears the record. Replaying a
nonce with changed text is refused (“This request changed; send it as a new
task.”). The pending record holds no key material and is invalidated when the
device pairs again.

A completed run without a saved answer is an error, not a pass. The updated home
reports category `other` with the fixed missing-answer sentence. The client also
fails closed when an older home omits that signal. Saved answers must match the
exact message ID, run ID, thread and bot role; only non-reasoning text blocks are
rendered. Tool results and arbitrary provider failure prose are never returned.
Typed records discard extra remote fields, failure messages use fixed safe
sentences, and private-key/token patterns are redacted before JSON serialization.

### Stage 3 wire reads and boundaries

All operations use the existing signed `{operation,body,proof}` request:

- `runs/get {runId}` → `{run:DeviceRunDetail}`
- `tasks/get {taskId}` → `{task:DeviceRunDetail}`
- `runs/list {cursor?,limit?}` → `{runs,nextCursor}` (default 50, 1–100)
- `messages/get {threadId,botId,around:{messageId}}` → bounded message page
- Existing `dispatch {clientNonce,botId,text}` and `stop {taskId}` are unchanged.

DeviceRunDetail includes taskId, runId, threadId, botId, state, cancelRequested,
status, cancelConfirmed, messageId, failure, createdAt, startedAt and completedAt.
State is distinct from raw run status. Failure is null or category/fixed sentence.

The home requires read scope and this device's admission receipt for exact runs,
tasks and cursors. A paired device reads messages only in threads of tasks admitted
to it, with independent thread ownership checks. A thread without its receipt
gets the same refusal as an unknown thread. The client never expands those grants.

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
Rust enforces this now. The Stage 3 home contract rejects lone surrogates in values and keys as well;
the copied rejected vectors prove the accepted string domain in both clients. `parse_json` checks raw canonical JSON;
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

## Stage 3 and Stage 4 fixture provenance and proof

`crates/home-protocol/tests/fixtures/device-operations.json` is a byte-for-byte
copy of `apps/cli/fixtures/device-operations.json` from `ArdurAI/ardur-bot`
revision `12475e3af2e2b10605a27f0245f3db71b8e47f71` (home PR #212). It contains
fourteen accepted request vectors — the eight Stage 3 vectors plus the Stage 4
computer, board, room-list and room-send vectors — seven typed responses
(multiple-run work, receipt-only greeting, room list, computer list, board
snapshot, board item and a 403 board denial) and four rejected surrogate
vectors. Its SHA-256 digest is
`86fccb32d98de8d2ed20e6c26509948a9c8a67037d62e42787363afef6509466`. The copied
file is unchanged; provenance and added public TypeScript signatures live in
`typescript-stage4.json`, which records the source repository, revision, source
path and that digest.

`typescript-stage3.json` remains as the previous generation: its eight signed
request vectors from revision `49551df2a61f1e0d34498b9eb06b5883801278c8` still
match the copied golden bytes exactly.

The `--stage3` generator mode imports the canonicalizer, signer and actual home
signature verifier from a read-only oracle checkout. The `--stage4` mode signs
the already-committed Stage 4 golden bytes with a fresh synthetic device key
(the revision that produced those bytes is a required argument) and needs no
checkout. Rust checks canonical bodies and signed text for every copied vector,
verifies the public DER signatures in both TypeScript companions, and parses
the typed responses into the Stage 4 records. The offline Node checker in CI
verifies the committed public vectors, the provenance digest and the response
shapes; it does not import a separate checkout or claim a real home connection.

```sh
node --import "${ORACLE}/node_modules/tsx/dist/loader.mjs" scripts/home-fixtures.mjs "${ORACLE}" --stage3
node scripts/home-fixtures.mjs . --stage4 12475e3af2e2b10605a27f0245f3db71b8e47f71
cargo run -p home-protocol --example rust_vectors > crates/home-protocol/tests/fixtures/rust.json
node --import "${ORACLE}/node_modules/tsx/dist/loader.mjs" scripts/home-fixtures.mjs "${ORACLE}" --verify-rust crates/home-protocol/tests/fixtures/rust.json
node scripts/check-home-vectors.mjs
```

Disposable HTTPS tests exercise all six Stage 3 commands with independently
constructed signed texts and a separate DER verifier. They prove lost-admission
recovery, fresh proofs, changed-body refusal, exact saved answers, requested
versus confirmed cancellation, bounded running/hung waits, missing/mismatched
answers, approval waits, safe failures, cursor/limit bodies and unchanged
legacy commands. Stage 4 tests exercise the four reads and room sends against
the same disposable home: fixture-exact signed bodies, typed projections,
board denial and problem sentences, scope refusals, ambiguous room names,
receipt-only greetings, multiple run ids and durable lost-response recovery
with reused clientNonces and fresh proofs. Unix subprocess tests exercise the
compiled CLI with temporary profiles, never owner storage.
Windows runs protocol and in-memory client tests; private-file pairing remains
refused until a protected ACL backend exists. No real home, database, bot runtime
or model call is involved, and these tests incur no model-turn cost.

## Stage 5 resumable run events

```sh
ardur-rs runs events <run-id> [--follow] [--cursor <n>] [--json]
```

This command needs home PR #221. It resolves the exact run through a signed
read, then signs `events` with its run and thread ids, bot or room id, and
cursor. The cursor is the last thread event sequence read, defaults to `-1`,
and accepts integers through `2147483647`. Gaps are normal: the thread also
contains hidden activity and events from other runs.

Each pinned HTTPS response streams frames as they arrive. It retains only one
incomplete frame, handles UTF-8 split across chunks, ignores heartbeats and
already-read ids, and validates event correlation before output. It enforces
64 KiB per frame, 128 frames and 1 MiB per window. The home bounds windows to
ten seconds and sends heartbeats every two seconds; the existing client
transport adds its fifteen-second request timeout.

Without `--follow`, the command reads one window. With it, `timeout` and `limit`
reconnect from the final `nextCursor` with a fresh nonce and signature.
Interrupted windows discard partial frames and resume from the last complete
printed event. Transport recovery allows five consecutive retries with pauses
from 200 ms to 2 s; successful windows pause 100 ms before reconnecting.
Following drains limit windows even after the run ends, then checks the exact
run after a timeout window to decide whether to stop. `access_lost`,
`payload_too_large`, `error` and `shutdown` stop reconnects with a fixed message.
Oversized frames also stop; they are never truncated or silently skipped.
Ctrl-C cancels reads or backoff and prints the last safe cursor in JSON mode.

JSON mode prints one versioned JSON object per event or window. Event records
contain `event: "event"`, `cursor` and the home event in `data`; window records
contain `event: "window"`, `data: {nextCursor, reason}` and a fixed message.
Human output shows sequence, event type and payload, followed by the window
message. Both projections redact decoded values and remove terminal controls
before output. Exit codes are 0 for normal completion or Ctrl-C, 4 for lost
access or invalid inputs, and 2 for stream or transport failures.

The byte-exact Stage 5 fixture is `device-events.json`, copied from
`apps/cli/fixtures/device-operations.json` in `ArdurAI/ardur-bot` revision
`2910f63e231ebc8fbf9c4177d4fc1681f04b3752`. Its SHA-256 is
`f5939feb81f958b8aa2bbbd38784fcd1c6dcdfdbde86440785f79a820fa5a3f3`;
`device-events-provenance.json` records the origin. The public companion
`typescript-stage5.json` adds synthetic P-256 DER signatures over those exact
bytes; Rust and the offline Node checker verify them and reject changed text.
Regenerate it with `node scripts/home-fixtures.mjs . --stage5
2910f63e231ebc8fbf9c4177d4fc1681f04b3752`. The earlier Stage 4
fixture and signature companion remain intact. Conformance checks all
seventeen canonical request vectors, including bot, room and maximum-cursor
events requests. Reader tests consume all nine supplied SSE cases using their
exact hexadecimal chunks. Disposable TLS tests cover one window, follow,
heartbeat-only responses, split Unicode, duplicates, gaps, oversized frames,
stop reasons, revocation and interrupted-frame recovery. Unix subprocess tests
cover JSONL, readable output and Ctrl-C; their imports are also Unix-gated.
These tests use synthetic homes and incur no model-turn cost.

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
