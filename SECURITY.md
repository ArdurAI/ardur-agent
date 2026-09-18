# Security policy

## Reporting vulnerabilities

Please report suspected vulnerabilities privately through GitHub Security Advisories for this repository. Do not open a public issue with exploit details or secrets.

If GitHub advisory access is unavailable, contact the repository owner and include:

- affected component and version/commit,
- impact and prerequisites,
- minimal reproduction steps,
- whether credentials, tokens, or private data may have been exposed.

## Workload identity: what the SPIFFE-style naming does and does not mean

Principals throughout this codebase are written as SPIFFE-style URIs
(`spiffe://ardur/user/alice`). **This is a naming convention, not an
integration.** There is no SPIFFE or SPIRE dependency in any `Cargo.toml`, none
in `Cargo.lock`, no SVID issuance, no workload attestation, and no Workload API
client. `HolderId` is an opaque string that happens to be URI-shaped.

The convention is deliberate — it keeps identifiers stable and hierarchical, and
leaves room for real SVIDs later — but it must not be read as evidence that
workload identity is attested.

### What actually authenticates a principal today

Local, single-operator CLI use is the primary deployment, and there the identity
model is an **operator-scoped keypair**: the operator's own key signs the receipt
chain and issues capability tokens. The subject in a token is asserted by that
key, not attested by an independent authority.

This is a real and deliberate gap. In SPIFFE terms the workload would be
attesting itself, which on a single host is circular: any process able to read
the signing key can also assert any subject it likes. No local deployment of
SPIRE removes that circularity, because the attestor and the attested share a
trust boundary.

### Where real identity attestation is required

Multi-tenant or multi-host deployments — the server binary, channel workers, and
any remote verifier — are where attested identity carries weight, because there
the asserting party and the relying party are genuinely separate. Real SPIRE
integration (SVID issuance, SVID binding at the governance-plane boundary) is
tracked as a distinct piece of work rather than implied by the naming.

Treat the SPIFFE-style strings as **server/fleet-facing preparation**. Until that
integration exists, do not describe Ardur as having SPIFFE workload identity; it
has SPIFFE-shaped identifiers under operator-scoped keys.

## Required repository security settings

Repository administrators must keep these controls enabled on both `main` and `dev`:

1. GitHub secret scanning.
2. Push protection for supported secret patterns.
3. Dependency graph and Dependabot alerts.
4. Dependabot security updates.
5. Ruleset `.github/rulesets/main-dev-security-gates.json` or an equivalent active GitHub ruleset requiring CI, DCO, signed commits, code-owner review, and linear history.

The repository cannot enable secret scanning or push protection from source code alone; those are GitHub repository/org settings. This file documents the required posture, while CI runs a redacted Gitleaks scan as an additional open-source gate.
