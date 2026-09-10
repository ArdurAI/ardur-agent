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
cd /path/to/ardur-agent

# The CLI stores state under $HOME/.ardur (it does not honor ARDUR_DATA_DIR).
# Capture the real home first so rustup and logged-in CLIs keep working
# after HOME is swapped.
ORIG_HOME="$HOME"
export RUSTUP_HOME="${RUSTUP_HOME:-$ORIG_HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$ORIG_HOME/.cargo}"
export PATH="${CARGO_HOME}/bin:${PATH}"
# Codex/Claude CLI login state lives under the original home.
export CODEX_HOME="${CODEX_HOME:-$ORIG_HOME/.codex}"
export CLAUDE_CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$ORIG_HOME/.claude}"

FRESH="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/ardur-fresh.XXXXXX")" && pwd -P)"
export HOME="$FRESH"
unset ARDUR_DATA_DIR ARDUR_DEV_PERMISSIVE_POLICY
unset ARDUR_CEDAR_POLICY_PATH ARDUR_CLI_BUDGET_CENTS ARDUR_CLI_PER_TURN_CENTS
# Offline path must not inherit a live provider from the developer shell.
unset ARDUR_PROVIDER ARDUR_MODEL
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
- Cost: Anthropic/OpenRouter usually record non-zero cents. Ollama, Codex,
  Claude-CLI, and `openai-compat` (zero rate card unless the gateway sends
  `usage.cost`) may record 0c. A non-stub reply plus a new receipt is the
  pass criterion.

If the chosen credential is missing, record `missing` and stop this path.

## 4. Slack private channel (opt-in)

Requires the Slack app credentials in [RUN.md](../RUN.md) and a **private**
channel. Public channels are out of scope for beta validation.

```sh
cp .env.example .env
# Fill SLACK_BOT_TOKEN, SLACK_SIGNING_SECRET, SLACK_APP_ID, and the
# provider from section 3. Do not commit .env.

# ardur-server reads process env via Config::from_env; it does not load .env.
# Source, then override container-oriented defaults from .env.example:
# ARDUR_DATA_DIR=/var/lib/ardur is not writable for a host-side cargo run,
# and ARDUR_MODEL=claude-opus-4-8 will overwrite the live provider model
# from section 3.
set -a
. ./.env
set +a
export ARDUR_DATA_DIR="${HOME}/.ardur"
export ARDUR_CEDAR_POLICY_PATH="${HOME}/.ardur/cedar.policies"
export ARDUR_PROVIDER=ollama
export ARDUR_MODEL=llama3.2:1b
# If section 3 used a different backend/model, re-export those here instead.

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
the same job's Trivy gate and `/healthz` smoke have passed. It tags and
pushes the scanned `ardur-agent:ci` image (not a second build).

```sh
# Image name is lowercase; the GitHub org/repo may not be.
docker pull ghcr.io/ardurai/ardur-agent:<tag>
docker run -d --name ardur-fresh-smoke --rm -p 3000:3000 \
  -e ARDUR_BIND_ADDR=0.0.0.0:3000 \
  -e SLACK_BOT_TOKEN=dummy \
  -e SLACK_SIGNING_SECRET=dummy \
  -e SLACK_APP_ID=dummy \
  -e ARDUR_PROVIDER=ollama \
  ghcr.io/ardurai/ardur-agent:<tag>
trap 'docker rm -f ardur-fresh-smoke >/dev/null 2>&1 || true' EXIT
ok=0
for _ in $(seq 1 15); do
  if curl -fsS http://127.0.0.1:3000/healthz >/dev/null; then
    ok=1
    break
  fi
  sleep 2
done
test "$ok" -eq 1
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
