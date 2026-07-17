# Session lifecycle operations

The CLI exposes session lifecycle commands under `ardur sessions`. The singular spelling
`ardur session` remains a supported alias, so existing scripts may use either form.

Durable session data lives below the configured Ardur state root:

```text
journals/sessions/<session-uuid>/
├── journal.jsonl
└── metadata.json
```

Session identifiers accepted by `resume` and `export` must be UUIDs. The CLI rejects
path-like or malformed identifiers before resolving a journal path.

## List sessions

```bash
ardur sessions list
ardur sessions list --workspace <filter>
```

`list` emits JSON with an object for every durable session. Each object includes:

- creation and update timestamps (Unix milliseconds);
- provider, model, source (`cli` for CLI-created sessions), and workspace name;
- cumulative finalized cost in cents;
- turn, journal-entry, and receipt counts;
- the last receipt identifier;
- journal size; and
- receipt status.

Receipt status is one of:

- `none`: the journal has no receipt-linked entries;
- `chain-linked`: every journal receipt identifier exists in an ES256-signature-
  authenticated, hash-link-verified persisted receipt chain;
- `missing`: at least one journal receipt identifier isn't present in the chain; or
- `corrupt`: the persisted receipt chain can't be decoded, its compact JWS signature
  doesn't verify against the persisted receipt key, its decoded body differs from the
  authenticated payload, its hash links don't verify, or the receipt key is unavailable
  or malformed.

Legacy sessions created before `metadata.json` was introduced show `unknown` when
provider, model, or source can't be reconstructed and `null` for workspace. The
`--workspace` filter compares the captured workspace name case-insensitively and
excludes legacy sessions that have no workspace metadata. Cumulative cost comes from
the verified receipt chain when all referenced receipts are available. When receipt
identifiers exist but authenticated receipt evidence is unavailable (corrupt, missing,
or unverifiable), cost is reported as unknown (`null`) rather than falling back to
unauthenticated journal-derived spend. Legacy sessions with no receipt identifiers
fall back to journal `CostFinalized` entries. When
neither trusted receipt cost nor legacy finalized-cost evidence is available, JSON
listing emits `cost_cents: null` rather than incorrectly treating unknown spend as zero.
Opening a legacy session through `ardur chat --session-id <uuid>` records current
metadata without replacing its original journal.

## Resume a session

Print the persisted transcript, enter the chat loop with that transcript restored, and
append follow-up turns to the same journal and receipt chain:

```bash
ardur sessions resume <session-uuid>
```

The lower-level equivalent remains available for scripts that need chat flags:

```bash
ardur chat --session-id <session-uuid>
```

A missing journal or malformed UUID fails closed with a non-zero exit status.

## Export a redacted bundle

```bash
ardur sessions export <session-uuid> --format markdown
ardur sessions export <session-uuid> --format json
ardur sessions export <session-uuid> --format jsonl
ardur sessions export <session-uuid> --format jsonl --output session.jsonl
```

All export formats redact recognized credentials and secret assignments from message,
checkpoint, and invalidation text. Exported paths are state-root-relative so bundles
don't disclose the local account or home directory. Files written with `--output` are
created with owner-only permissions on Unix; descriptor-relative parent traversal and
no-follow replacement refuse both parent- and final-component symlink swaps.
Session journal, metadata, receipt-key, and receipt-log reads apply the same parent-aware
no-follow policy. JSON and Markdown bundles
include chain status plus the canonical compact JWS and decoded body for every linked
receipt only when every JWS signature, authenticated body, and chain link verifies and
every referenced receipt is present. JSONL remains a stream of redacted journal entries
for replay tools.
Receipt identifiers and finalized-cost entries remain available for audit correlation.

Redaction is defense in depth, not permission to place credentials in prompts. Review a
bundle before sharing it outside its original trust boundary.

## Prune old sessions

Pruning is a dry run unless `--confirm` is present:

```bash
ardur sessions prune --older-than 30
ardur sessions prune --older-than 30 --confirm
```

The dry run prints the exact UUID-named session directories that would be removed. Age
comes from persisted metadata and journal event timestamps; filesystem modification time
is used only for empty legacy directories. The confirmed command deletes only real
directories with valid UUID names; symlinks and unrecognized directory names are skipped.
Confirmed recursive deletion is descriptor-relative on Unix and unlinks symlink children
rather than traversing them.

## Inspect and verify receipts

```bash
ardur receipts list
ardur receipts show <receipt-uuid>
ardur receipts verify
```

These commands decode the persisted compact-JWS log, authenticate every ES256 signature
against the configured local receipt key, compare each decoded body with its authenticated
payload, verify the complete hash chain, and reject duplicate receipt identifiers before
displaying evidence. `list` and `show` expose the authenticated receipt body plus its
canonical `jws_compact`; malformed, forged, wrong-key, symlinked, or hash-broken logs fail
closed instead of being displayed as trusted JSON.

## Startup recovery

CLI and server startup authenticate the existing receipt chain against the configured
receipt key and reconcile any receipt that crossed the receipt commit boundary before a
journal append failed. New receipts carry the UUID of the durable journal that owns them;
each sweep authenticates the complete global chain but compares only receipts assigned to
its own journal. Legacy receipts without that additive field remain verifiable but are not
guessed into an arbitrary session. The server reuses one stable UUIDv7-shaped audit-
journal identity across boots so reconciliation compares against the same durable journal
instead of replicating the chain into a fresh directory. Recovery appends synthetic journal
evidence and is idempotent; startup fails rather than accepting an unauthenticated or
undecidable chain.

Cost finalization precedes receipt persistence. If the receipt append fails, the runtime
rolls back the finalized debit before returning the turn error. If receipt append succeeds
but journal append fails, spend remains committed because the authenticated receipt is the
authoritative cost boundary; startup reconciliation repairs the orphan journal evidence.
