# Fresh-machine validation runbook

Use this checklist on a machine (or a throwaway `HOME`) that does not already
have `~/.ardur/` state. It is the operator path for a `v*` beta tag: prove the
offline stub, then optionally one live provider, then a private Slack channel.

Do not paste secret values into tickets, logs, or chat. Report only
`present` / `missing`.

The reviewed implementation inventory lives in
[current-status.md](current-status.md). Server and channel configuration live
in [RUN.md](../RUN.md).

## 0. Inputs

| Input | Required for | Notes |
| --- | --- | --- |
| Rust toolchain matching `rust-toolchain.toml` (currently `1.98.1`) | offline + live | `rustup show` |
| `cargo` | offline + live | workspace build |
| No provider keys in the isolated `HOME` | offline stub | keys in the *ambient* shell are stripped below |
| One of `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_COMPAT_API_KEY`, local Ollama, or a logged-in `codex`/`claude` CLI | live provider opt-in | pick **one** |
| `SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET`, `SLACK_APP_ID` | Slack private channel | plus a private channel the bot is invited to |

If a row's credentials are missing, skip that path and record it as not
exercised. Do not stub-fake a green.

## 1. Isolated state directory

Never validate against a developer `~/.ardur/` tree. Use a throwaway home.
On macOS `/tmp` is a symlink to `/private/tmp`; the CLI refuses a symlink
anywhere in a trusted state path, so canonicalize:

```sh
FRESH="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/ardur-fresh.XXXXXX")" && pwd -P)"
export HOME="$FRESH"
unset ARDUR_DATA_DIR ARDUR_DEV_PERMISSIVE_POLICY
unset ARDUR_CEDAR_POLICY_PATH ARDUR_CLI_BUDGET_CENTS ARDUR_CLI_PER_TURN_CENTS
# Offline path must not inherit a live provider from the developer shell.
unset ARDUR_PROVIDER ARDUR_MODEL
cd /path/to/ardur-agent
```

## 2. Offline stub path (required)

Strip provider keys for this section even if they exist in the parent shell:

```sh
env -u ANTHROPIC_API_KEY -u OPENROUTER_API_KEY -u OPENAI_API_KEY \
    -u OPENAI_COMPAT_API_KEY -u OLLAMA_API_KEY -u OLLAMA_BASE_URL \
    cargo build -p ardur-cli --bins

env -u ANTHROPIC_API_KEY -u OPENROUTER_API_KEY -u OPENAI_API_KEY \
    -u OPENAI_COMPAT_API_KEY -u OLLAMA_API_KEY -u OLLAMA_BASE_URL \
    cargo run -p ardur-cli -- setup --yes

env -u ANTHROPIC_API_KEY -u OPENROUTER_API_KEY -u OPENAI_API_KEY \
    -u OPENAI_COMPAT_API_KEY -u OLLAMA_API_KEY -u OLLAMA_BASE_URL \
    cargo run -p ardur-cli -- doctor

printf 'hello fused substrate\n/quit\n' | \
env -u ANTHROPIC_API_KEY -u OPENROUTER_API_KEY -u OPENAI_API_KEY \
    -u OPENAI_COMPAT_API_KEY -u OLLAMA_API_KEY -u OLLAMA_BASE_URL \
    cargo run -p ardur-cli -- chat --plain
```

Pass criteria:

- `setup --yes` prints `created starter Cedar policy` and writes
  `$HOME/.ardur/cedar.policies` that permits `Action::"Submit"` and
  `Action::"ToolInvoke"` (not the permit-all dev fallback).
- `doctor` is usable without `--require-api-key`.
- Chat prints an `offline mode` notice and a `[anthropic stub]` completion.
- `$HOME/.ardur/receipts/chain.jsonl` exists.
- `cargo run -p ardur-cli -- receipts verify` reports `ES256 signatures OK`.

The no-key fused tests cover the same contract in CI:

```sh
cargo test -p ardur-cli --test cli_first_run_policy --test cli_fused_offline --test cli_smoke_echo
cargo test -p ardur-e2e-tests
cargo test -p ardur-server --test boot_smoke
```

## 3. One live provider (opt-in)

Keep the isolated `HOME` from section 1 so the starter Cedar policy still
applies. Restore **one** provider. Cap spend. The CLI default model
(`claude-opus-4-8`) is Anthropic-shaped — set `ARDUR_MODEL` to a model the
chosen backend actually serves.

```sh
# Local Ollama (no API key). Daemon must be up and the model pulled.
#   ollama serve    # if needed
#   ollama pull llama3.2:1b
export ARDUR_PROVIDER=ollama
export ARDUR_MODEL=llama3.2:1b
export OLLAMA_BASE_URL=http://127.0.0.1:11434
unset OLLAMA_API_KEY   # a key would retarget https://ollama.com

# Or a cloud/CLI backend:
#   export ARDUR_PROVIDER=anthropic    # needs ANTHROPIC_API_KEY
#   export ARDUR_PROVIDER=codex        # needs `codex login`; set ARDUR_MODEL
#                                      # to a ChatGPT-account-supported id
# Do not print credential values.

printf 'Reply with the single word pong.\n/quit\n' | \
  cargo run -p ardur-cli -- chat --plain --budget-cents 50
```

Pass criteria:

- The reply is not `[anthropic stub]`.
- A new receipt is appended; `receipts verify` still reports
  `ES256 signatures OK`.
- Cost is recorded on the receipt (non-zero cents for billed providers;
  Ollama/Codex/Claude-CLI may be zero).

If the chosen credential is missing, record `missing` and stop this path.

## 4. Slack private channel (opt-in)

Requires the Slack app credentials in [RUN.md](../RUN.md) and a **private**
channel. Public channels are out of scope for beta validation.

```sh
cp .env.example .env
# Fill SLACK_BOT_TOKEN, SLACK_SIGNING_SECRET, SLACK_APP_ID, and the
# provider from section 3. Do not commit .env.

cargo run -p ardur-server
# In a second terminal, expose HTTPS (ngrok or equivalent) and set the
# Slack Event Subscriptions URL to https://<host>/slack/events.
```

Pass criteria:

- Slack URL verification challenge succeeds.
- One message in the private channel produces a reply from the bot.
- The turn writes a signed receipt and a session journal on the server data
  dir (`ARDUR_DATA_DIR`, default `/var/lib/ardur` in the container).

If `SLACK_BOT_TOKEN` (or the signing secret / app id) is missing, record
`missing` and do not claim this path ran.

## 5. Published container (after a `v*` tag)

`.github/workflows/docker.yml` publishes only on version tags, and only after
`build-healthcheck-scan` is green:

```sh
# Image name is lowercase; the GitHub org/repo may not be.
docker pull ghcr.io/ardurai/ardur-agent:<tag>
docker run --rm -p 3000:3000 \
  -e SLACK_BOT_TOKEN=dummy \
  -e SLACK_SIGNING_SECRET=dummy \
  -e SLACK_APP_ID=dummy \
  -e ARDUR_PROVIDER=ollama \
  ghcr.io/ardurai/ardur-agent:<tag>
curl -fsS http://127.0.0.1:3000/healthz
```

The first package created under the org may be private. An org admin must set
GHCR visibility to public before anonymous pulls work. Do not tag `:latest` on
a pre-release.

Release assets (SBOM, SHA256SUMS, cosign signatures, provenance) are attached
by `.github/workflows/release.yml` when a GitHub Release is **published** for
the same tag.

## 6. Evidence to record

For each path, keep:

- the isolated `HOME` path (not its contents),
- the exact commands,
- pass / fail / skipped-missing-credential,
- for a tagged release: the GitHub Release URL, the `release-supply-chain`
  run URL, and the GHCR digest.
