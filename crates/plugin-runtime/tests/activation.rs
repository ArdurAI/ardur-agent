//! Integration tests for `ardur-plugin-runtime`'s claim activation pipeline.
//!
//! Drives real registries (`ToolRegistry`, `GatewayRegistry`) and a real
//! `BiscuitCapTokenVerifier`, not mocks — the red-team-relevant proofs
//! (capability-subset enforcement, cross-claim cap-token isolation) exercise
//! the actual Biscuit check evaluation.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::json;

use ardur_cap_token::{
    BiscuitCapTokenIssuer, BiscuitCapTokenVerifier, CapScope, CapTokenIssuer, CapTokenVerifier,
    HashSetDenyList, HolderId, KeyPair, RequiredCaveats,
};
use ardur_messaging_gateway::{
    ChannelId, GatewayRegistry, InProcessGateway, IncomingMessage, OutgoingMessage,
};
use ardur_plugin_runtime::{ClaimError, ClaimSet, PluginRuntimeHost, RuntimeClaim};
use ardur_runtime::{CapTokenRef, SessionId};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};

/// A trivial fixture tool: echoes its args, declaring `required` as its
/// capability ceiling.
struct FixtureTool {
    id: &'static str,
    schema: ToolSchema,
    required: Vec<Capability>,
}

impl FixtureTool {
    fn new(id: &'static str, required: Vec<Capability>) -> Self {
        Self {
            id,
            schema: ToolSchema {
                description: "fixture".to_string(),
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                examples: vec![],
            },
            required,
        }
    }
}

#[async_trait]
impl Tool for FixtureTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.id)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: args.clone(),
            cost: Default::default(),
            receipt_data: args,
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.required
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: Default::default(),
        cwd: std::env::temp_dir(),
        env: Default::default(),
        cost_budget_cents: 1000,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_secs()
}

/// Mint an admission cap-token for a fictional plugin, pre-scoped to the
/// namespaced tool ids the plugin's (already-validated, out-of-scope-here)
/// admission process decided to grant.
fn mint_admission_cap(
    issuer: &BiscuitCapTokenIssuer,
    tool_allowlist: Vec<String>,
) -> ardur_cap_token::CapToken {
    issuer
        .issue(
            HolderId("plugin-admission".to_string()),
            CapScope {
                audience: "ardur".to_string(),
                expires_unix: now_unix() + 3600,
                budget_remaining: 1_000_000,
                tool_allowlist,
            },
        )
        .expect("mint admission cap")
}

fn required(tool: &str) -> RequiredCaveats {
    RequiredCaveats {
        now_unix: now_unix(),
        audience: "ardur".to_string(),
        tool: tool.to_string(),
        cost: 1,
    }
}

#[test]
fn claim_set_closure_rejects_duplicates_bounds_and_bad_names() {
    let dup = ClaimSet::new(
        "demo",
        vec![
            RuntimeClaim::tool("x", vec![]),
            RuntimeClaim::tool("x", vec![]),
        ],
    );
    assert!(matches!(
        dup.validate_closure(),
        Err(ClaimError::DuplicateClaim { .. })
    ));

    let too_many = ClaimSet::new(
        "demo",
        (0..20)
            .map(|i| RuntimeClaim::tool(format!("t{i}"), vec![]))
            .collect(),
    );
    assert!(matches!(
        too_many.validate_closure(),
        Err(ClaimError::TooManyClaims(20, 16))
    ));

    let bad_name = ClaimSet::new("demo", vec![RuntimeClaim::tool("has a space", vec![])]);
    assert!(matches!(
        bad_name.validate_closure(),
        Err(ClaimError::InvalidClaimName(_))
    ));

    let bad_plugin_id = ClaimSet::new("has a space", vec![RuntimeClaim::tool("ok", vec![])]);
    assert!(matches!(
        bad_plugin_id.validate_closure(),
        Err(ClaimError::InvalidPluginId(_))
    ));

    let ok = ClaimSet::new(
        "demo",
        vec![
            RuntimeClaim::tool("translate", vec![Capability::FsRead]),
            RuntimeClaim::channel("slack-bridge"),
        ],
    );
    assert!(ok.validate_closure().is_ok());
}

#[test]
fn activate_tool_claim_registers_and_invokes_through_the_real_registry() {
    let plugin_id = "demo";
    let claim_name = "translate";
    let namespaced = format!("plugin/{plugin_id}/{claim_name}");
    let claims = ClaimSet::new(
        plugin_id,
        vec![RuntimeClaim::tool(claim_name, vec![Capability::FsRead])],
    );

    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let admission_cap = mint_admission_cap(&issuer, vec![namespaced.clone()]);

    let mut registry = ToolRegistry::new();
    let host = PluginRuntimeHost::new();
    let activated = host
        .activate_tool_claim(
            &claims,
            claim_name,
            Box::new(FixtureTool::new("translate", vec![Capability::FsRead])),
            &admission_cap,
            &mut registry,
        )
        .expect("activation succeeds");

    assert_eq!(activated.tool_id.as_str(), namespaced);

    let tool = registry
        .get(&activated.tool_id)
        .expect("namespaced tool is registered");
    let out = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(tool.invoke(&ctx(), json!({"hello": "world"})))
        .expect("invoke succeeds");
    assert_eq!(out.content, json!({"hello": "world"}));
}

#[test]
fn activated_claim_cap_token_cannot_authorize_a_sibling_claims_tool() {
    // The plugin's admission cap legitimately spans BOTH claims' tools — this
    // models the real-world shape where admission grants the whole plugin's
    // declared tool set up front. The security property under test: a single
    // claim's activated cap-token must not inherit that breadth.
    let plugin_id = "demo";
    let namespaced_alpha = format!("plugin/{plugin_id}/alpha");
    let namespaced_beta = format!("plugin/{plugin_id}/beta");
    let claims = ClaimSet::new(
        plugin_id,
        vec![
            RuntimeClaim::tool("alpha", vec![]),
            RuntimeClaim::tool("beta", vec![]),
        ],
    );

    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let root = issuer.public_key();
    let admission_cap = mint_admission_cap(
        &issuer,
        vec![namespaced_alpha.clone(), namespaced_beta.clone()],
    );

    let mut registry = ToolRegistry::new();
    let host = PluginRuntimeHost::new();
    let activated = host
        .activate_tool_claim(
            &claims,
            "alpha",
            Box::new(FixtureTool::new("alpha", vec![])),
            &admission_cap,
            &mut registry,
        )
        .expect("activation succeeds");

    let verifier = BiscuitCapTokenVerifier::new(HashSetDenyList::default());

    // The claim's own tool: authorized.
    verifier
        .verify(
            &activated.claim_cap_token,
            &root,
            &required(&namespaced_alpha),
        )
        .expect("the claim's own tool is authorized");

    // SECURITY PROPERTY: the sibling claim's tool — which the *parent*
    // admission cap DOES allow — must be refused by the child. Attenuation
    // only narrows; RestrictTools([alpha]) cannot reach `beta`.
    let err = verifier
        .verify(
            &activated.claim_cap_token,
            &root,
            &required(&namespaced_beta),
        )
        .expect_err("the sibling claim's tool must be refused");
    assert!(
        matches!(err, ardur_cap_token::CapTokenError::ToolNotAllowed),
        "got {err:?}"
    );

    // Sanity: the *parent* admission cap, unattenuated, really did allow both —
    // proving the refusal above comes from the attenuation, not some other gate.
    verifier
        .verify(&admission_cap, &root, &required(&namespaced_alpha))
        .expect("parent cap allows alpha");
    verifier
        .verify(&admission_cap, &root, &required(&namespaced_beta))
        .expect("parent cap allows beta");
}

#[test]
fn activate_tool_claim_refuses_undeclared_capability() {
    let plugin_id = "demo";
    let claims = ClaimSet::new(
        plugin_id,
        vec![RuntimeClaim::tool("shelly", vec![Capability::FsRead])],
    );
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let admission_cap = mint_admission_cap(&issuer, vec!["plugin/demo/shelly".to_string()]);
    let mut registry = ToolRegistry::new();
    let host = PluginRuntimeHost::new();

    let err = host
        .activate_tool_claim(
            &claims,
            "shelly",
            // Requires ShellExec, but the claim only declared FsRead.
            Box::new(FixtureTool::new("shelly", vec![Capability::ShellExec])),
            &admission_cap,
            &mut registry,
        )
        .expect_err("undeclared capability must refuse activation");
    assert!(
        matches!(err, ClaimError::UndeclaredCapability { .. }),
        "got {err:?}"
    );
    assert!(
        registry.get(&ToolId::new("plugin/demo/shelly")).is_none(),
        "a refused claim must not be registered"
    );
}

#[test]
fn activate_tool_claim_refuses_unknown_claim_name() {
    let claims = ClaimSet::new("demo", vec![RuntimeClaim::tool("alpha", vec![])]);
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let admission_cap = mint_admission_cap(&issuer, vec!["plugin/demo/alpha".to_string()]);
    let mut registry = ToolRegistry::new();
    let host = PluginRuntimeHost::new();

    let err = host
        .activate_tool_claim(
            &claims,
            "not-declared",
            Box::new(FixtureTool::new("x", vec![])),
            &admission_cap,
            &mut registry,
        )
        .expect_err("activating an undeclared claim name must refuse");
    assert!(matches!(err, ClaimError::UnknownClaim(_)), "got {err:?}");
}

#[test]
fn activate_tool_claim_refuses_duplicate_registration() {
    let claims = ClaimSet::new("demo", vec![RuntimeClaim::tool("alpha", vec![])]);
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let admission_cap = mint_admission_cap(&issuer, vec!["plugin/demo/alpha".to_string()]);
    let mut registry = ToolRegistry::new();
    let host = PluginRuntimeHost::new();

    host.activate_tool_claim(
        &claims,
        "alpha",
        Box::new(FixtureTool::new("alpha", vec![])),
        &admission_cap,
        &mut registry,
    )
    .expect("first activation succeeds");

    let err = host
        .activate_tool_claim(
            &claims,
            "alpha",
            Box::new(FixtureTool::new("alpha", vec![])),
            &admission_cap,
            &mut registry,
        )
        .expect_err("re-activating the same claim must refuse");
    assert!(
        matches!(err, ClaimError::RegistrationFailed(_)),
        "got {err:?}"
    );
}

#[test]
fn namespacing_prevents_cross_plugin_tool_name_collision() {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let mut registry = ToolRegistry::new();
    let host = PluginRuntimeHost::new();

    for plugin_id in ["foo", "bar"] {
        let claims = ClaimSet::new(plugin_id, vec![RuntimeClaim::tool("translate", vec![])]);
        let admission_cap =
            mint_admission_cap(&issuer, vec![format!("plugin/{plugin_id}/translate")]);
        host.activate_tool_claim(
            &claims,
            "translate",
            Box::new(FixtureTool::new("translate", vec![])),
            &admission_cap,
            &mut registry,
        )
        .unwrap_or_else(|e| panic!("plugin {plugin_id} activation must succeed: {e}"));
    }

    assert!(registry.get(&ToolId::new("plugin/foo/translate")).is_some());
    assert!(registry.get(&ToolId::new("plugin/bar/translate")).is_some());
}

#[test]
fn activate_channel_claim_registers_and_round_trips_through_the_real_registry() {
    let plugin_id = "demo";
    let claim_name = "loopback";
    let claims = ClaimSet::new(plugin_id, vec![RuntimeClaim::channel(claim_name)]);
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let admission_cap = mint_admission_cap(&issuer, vec![]);
    let mut registry = GatewayRegistry::new();
    let host = PluginRuntimeHost::new();

    let inner_id = ChannelId("inner".to_string());
    let activated = host
        .activate_channel_claim(
            &claims,
            claim_name,
            Box::new(InProcessGateway::new(inner_id)),
            &admission_cap,
            &mut registry,
        )
        .expect("channel activation succeeds");

    assert_eq!(activated.channel_id.0, "plugin/demo/loopback");

    let gateway = registry
        .get(&activated.channel_id)
        .expect("namespaced gateway is registered");
    assert_eq!(gateway.channel_id().0, "plugin/demo/loopback");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let receipt = gateway
            .send_message(OutgoingMessage {
                message_id: uuid::Uuid::new_v4(),
                channel_id: ChannelId("inner".to_string()),
                target: ardur_messaging_gateway::MessageTarget::Channel(
                    ardur_messaging_gateway::ChannelRef("inner".to_string()),
                ),
                body: ardur_messaging_gateway::MessageBody::Text("hi".to_string()),
                cap_token: CapTokenRef(String::new()),
                parent_message_id: None,
            })
            .await
            .expect("send succeeds");
        assert_eq!(receipt.delivered_to.0, "inner");
        let incoming: IncomingMessage = gateway.receive().await.expect("receive succeeds");
        assert_eq!(incoming.channel_id.0, "inner");
    });
}
