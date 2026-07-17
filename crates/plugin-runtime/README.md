# ardur-plugin-runtime

The §8.8 claim-activation governance layer, scoped down from
`plans/8.8-plugin-runtime-claims-tool-channel-provider-extension-blueprint.md`
to what the codebase actually has substrate for.

## What this crate does

A plugin manifest (see `crates/cli/src/marketplace.rs`'s `publish`/`install`
support for `kind = "plugin"` manifests) declares zero or more `runtime_claims`
— each naming a trait family (`tool` or `channel`) and a pre-namespace
extension name. This crate:

1. **Validates the claim set's closure invariants** (`ClaimSet::validate_closure`):
   bounded count, valid plugin-id/claim-name charset, no duplicate
   `(family, name)` pair.
2. **Activates one claim at a time** (`PluginRuntimeHost::activate_tool_claim`
   / `activate_channel_claim`), given a caller-supplied concrete
   implementation:
   - Namespaces the registration identity to `plugin/<plugin_id>/<name>`, so a
     plugin can never collide with a built-in tool/channel name, and two
     plugins claiming the same name land at different registry keys.
   - For `Tool` claims: refuses if the supplied `Tool::required_capabilities()`
     is not a subset of the claim's `declared_capabilities` — the ceiling
     from the plugin's *signed, admitted manifest* — so a supplied
     implementation cannot claim more at activation time than what was
     signed off on.
   - Derives a cap-token strictly narrower than the plugin's admission token
     via `RestrictTools([namespaced_id])` (`ardur_cap_token`'s Biscuit
     attenuation) — so a claim's cap-token cannot reach a sibling claim's
     tool, even when the parent admission cap legitimately spans both (see
     `activated_claim_cap_token_cannot_authorize_a_sibling_claims_tool` in
     `tests/activation.rs` for the proof).
   - Registers the wrapped implementation into the real
     `ToolRegistry`/`GatewayRegistry` the caller passes in.

## What this crate deliberately does NOT do

- **No dynamic loading of untrusted code.** There is no wasm engine, no
  process sandbox, no container backend here. The host process must already
  have a concrete `Box<dyn Tool>` / `Box<dyn MessagingGateway>` to activate —
  today that means a boot-time built-in bridge or a test, not arbitrary
  third-party plugin binaries. Building a *safe* sandboxed executor for
  untrusted code is a substantial, separate effort (the plan itself sequences
  it as a later phase); pretending to have one here would be worse than not
  having one.
- **No `Provider` extension family.** `ardur_provider_runtime::ProviderRegistry`
  exists as a type but `FusedRuntime` doesn't consult it live — it picks one
  provider once at boot. Bridging a plugin-supplied `Provider` into a registry
  nothing reads would be inert. Follow-up once `FusedRuntime` gains live
  provider consultation.
- **No cap-token narrowing for `Channel` claims.** Ardur's Biscuit
  attenuation primitives (`RestrictAudience` / `EarlierExpiry` / `ReduceBudget`
  / `RestrictTools`) have no channel-scoped equivalent, and
  `MessagingGateway::send_message`/`receive` are not gated through the same
  per-tool cap-token check `FusedRuntime` applies to tools. `activate_channel_claim`
  still namespaces the registration (preventing collisions) but returns the
  plugin's admission cap-token unchanged — this is documented on
  `ActivatedChannelClaim::claim_cap_token`, not silently pretended to be
  narrowed.
- **No refresh-on-update claim diffing, no per-claim revoke, no Cedar
  gating.** All real plan features; none of the substrate they'd hook into
  (a live claim ledger, a Cedar policy bundle wired to `plugin:extend_*`
  actions) exists yet.
