# Redaction oracle

`text.ts` and `redaction.ts` are byte-identical public source snapshots from the
revision and paths in `provenance.json`. SHA-256 digests identify the copied bytes.
Only these pure text functions are loaded. No application, profile, environment
file, database or service is used.

`scripts/home-redaction-fixtures.mjs` generates the 98 named cases and combined
saved-answer fixture, plus 4,929 differential cases: 4,096 seeded combinations,
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
