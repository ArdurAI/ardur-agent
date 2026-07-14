# Signed marketplace capability manifests

Ardur marketplace packages can be distributed as signed capability manifests for either a `skill` or a `plugin`.

A manifest is a JSON file with:

- `schema_version`: currently `1`
- `kind`: `skill` or `plugin`
- `id`, `name`, `version`
- `capabilities`: capability strings the package requests, such as `cap.shell_exec` (bounded to 32 entries)
- `artifacts`: relative artifact paths and expected SHA-256 digests (bounded to 64 entries, 10 MiB each)
- `runtime_claims`: for `kind = "plugin"` only — declared `{ "name": ..., "family": "tool" | "channel" | "provider" }` extension points (bounded to 16 entries). Declaring a claim does not activate it; activation is the job of `ardur-plugin-runtime` at process boot, outside this CLI's scope.
- `signature`: `{ "alg": "ES256", "value": "..." }`

The signature covers a deterministic canonical payload derived from the manifest identity, capability list, artifact digests, and runtime claims. The artifact files are read from the manifest directory and must stay inside that directory; absolute paths and `..` traversal are rejected. The manifest file itself is capped at 256 KiB.

## Lifecycle

Eight verbs, all operating on local state under `${ArdurHome}/skills/` (one JSON record per installed skill/plugin) and `${ArdurHome}/skills_catalog/<id>/SKILL.md` (a verified-artifact copy the filesystem `SKILL.md` tool loader scans):

```sh
# Browse / search installed skills and plugins (aliases: `list`).
ardur marketplace browse
ardur marketplace search <query>

# Install from a local signed manifest. Signature-verified by default —
# refused unless you pass --key or explicitly accept the risk with
# --allow-unsigned. Remote/URL sources are not yet implemented.
ardur marketplace install ./manifest.json --key ./publisher-public-key.pem

# Inspect an installed entry: signature state, capability risk annotations,
# declared runtime claims, and whether it's wired into the local tool catalog
# (alias: `show`).
ardur marketplace inspect <id>

# Update to a new manifest version. Refuses a version downgrade or a same-
# version reinstall unless --force is passed; reports capability diffs.
ardur marketplace update <id> ./new-manifest.json --key ./publisher-public-key.pem

# Audit one (or, with no id, every) installed entry for unsigned installs,
# high-risk capabilities, and source-manifest drift since install.
ardur marketplace audit [<id>]

# Remove an installed entry, including its skill-catalog copy (alias: `remove`).
ardur marketplace uninstall <id>

# Sign a local skill/plugin directory's manifest, producing an installable
# bundle (manifest.json + a sibling SKILL.md).
ardur marketplace publish ./my-skill skill.my-helper "My Helper" 0.1.0 \
  --capability cap.fs_read --key ./publisher-private-key.pem --out ./dist/manifest.json

# Publish a plugin declaring runtime claims (repeatable --claim <name>:<family>).
ardur marketplace publish ./my-plugin plugin.demo "Demo Plugin" 0.1.0 \
  --kind plugin --claim translate:tool --key ./publisher-private-key.pem
```

`validate` and `verify` remain available as standalone signature/artifact checks:

```sh
ardur marketplace validate ./manifest.json --key ./publisher-public-key.pem
ardur marketplace verify --key ./publisher-public-key.pem
```

`verify --key` re-validates installed local manifest sources. Records without signatures, or installed with `--allow-unsigned`, are reported as untrusted.

## Cedar policy gating (optional hardening)

The built-in signature/bounds checks verify a manifest is *authentic and well-formed* — they do not restrict what a fully-signed manifest may *declare*. Nothing stops a well-formed, signed manifest from requesting `cap.shell_exec`. `install`, `update`, `uninstall`, `publish`, and single-target `audit` additionally evaluate a Cedar policy bundle so an operator can restrict that without a recompile:

```sh
ardur marketplace install ./manifest.json --key ./pub.pem --policy ./no-shell-exec.cedar
# or: export ARDUR_MARKETPLACE_POLICY=./no-shell-exec.cedar
```

The built-in default (no `--policy`/env var) permits every action — this is opt-in hardening, not a new default restriction. An operator-authored `.cedar` file is composed on top; a `forbid` in it always wins over the default `permit`. Example, denying any install that declares `shell_exec`:

```cedar
forbid(principal, action == Action::"skill_install", resource)
when { resource.high_risk_capabilities.contains("shell_exec") };
```

Resource attributes available to policy rules: `kind` (`"skill"`/`"plugin"`), `capabilities` (set of capability strings with the `cap.` prefix stripped), `high_risk_capabilities` (the subset in `shell_exec`/`process_spawn`/`network_out`/`fs_write`), `high_risk` (bool), `signed` (bool — was `--key` supplied), `claims_count`, `version`. Actions: `skill_install`, `skill_update`, `skill_uninstall`, `skill_publish`, `skill_audit`. The principal is `MarketplacePrincipal::"<name>"`, where `<name>` defaults to `local-user` and is overridable via `ARDUR_MARKETPLACE_PRINCIPAL` — this CLI has no multi-user auth system, so the principal is a self-declared identity for policies that want to distinguish operators on a shared machine, not an authenticated one.
