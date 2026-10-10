# Redaction oracle

`text.ts` and `redaction.ts` are byte-identical public source snapshots from the
revision and paths in `provenance.json`. SHA-256 digests identify the copied bytes.
Only these pure text functions are loaded. No application, profile, environment
file, database or service is used.

`scripts/home-redaction-fixtures.mjs` generates the 175 named cases and combined
saved-answer fixture, plus 4,931 differential cases: 4,096 seeded combinations,
all 25 JavaScript whitespace characters and terminal-control anchors. The fixture
header records seed `0x59503009`, counts and source digests. Expected strings come
from the TypeScript functions, never from the Rust implementation.

Run `node scripts/home-redaction-fixtures.mjs --check` to reproduce both corpora
offline. `scripts/check-home-vectors.mjs` also runs this check in CI using exactly Node 26.7.0.
Node 24.21.0 has a different terminal grammar; broad major-version pins do not
reproduce this corpus. The generator rejects a different Node version explicitly.
An optional read-only source checkout and revision can verify the snapshots
against the public source before generation. The source repository is never modified.

Terminal behavior intentionally follows Node's `stripVTControlCharacters` grammar,
including its handling of incomplete and C1 sequences; it is not a claim that
every possible terminal payload is recognized. Synthetic credentials and harmless
prose test both redaction and preservation.

Named cases cover glued credential chains, literal escapes, plural credential
keys, named counters and references, closed and unterminated private-key blocks
containing U+0085, and a 1 MiB value ending in `|:`. The two U+0085 cases also
appear as named differential anchors. Existing inputs remain intact; the
TypeScript snapshot supplies every expectation.

The safe text wrapper removes U+0085 before private-key matching. A Rust unit
test therefore also exercises the PEM matcher directly, before that filtering,
to guard its all-character span independently of terminal sanitization.

JSON credential-key masking is additional Rust-only hardening, outside these
text parity corpora. It uses the text rules' credential families and metadata
exceptions, without interpreting literal escapes in keys. Under a credential
key, every descendant string is masked; arrays, objects, numbers, booleans and
null retain their types. Counters and references stay readable. Answer fields
(`message`, `messages`, `prompt`, `body`, `query`) receive only text redaction
unless they sit beneath a credential key. The TypeScript CLI prints selected
fields, while the Rust CLI also serializes whole responses.
