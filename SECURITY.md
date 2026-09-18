# Security policy

## Reporting vulnerabilities

Please report suspected vulnerabilities privately through GitHub Security Advisories for this repository. Do not open a public issue with exploit details or secrets.

If GitHub advisory access is unavailable, contact the repository owner and include:

- affected component and version/commit,
- impact and prerequisites,
- minimal reproduction steps,
- whether credentials, tokens, or private data may have been exposed.

## Workload identity: what the SPIFFE-style naming does and does not mean

Many identifiers in this codebase are written as SPIFFE-style URIs
(`spiffe://ardur/user/alice`), chiefly in tests, fixtures and the governance
crate's examples. **This is a naming convention, not an integration.** There is
no SPIFFE or SPIRE dependency in any manifest or in `Cargo.lock`, no SVID
issuance, no workload attestation, and no Workload API client. `HolderId` is an
opaque string that happens to be URI-shaped.

The shipped principals do not all follow the convention, and it would be
misleading to imply otherwise: the CLI runs as `cli-session` and
`local-operator`, and the server's gateway subject is `ardur:slack-gateway`.
None of these are SPIFFE IDs. Treat the URI-shaped strings as a forward-looking
convention in the places that use it, not as a description of the identity model
in force.

### What actually authenticates a principal today

Local, single-operator CLI use is the primary deployment. Two **separate**
operator-held keys are involved, and they are not interchangeable:

| Key | File | Algorithm | What it authorizes |
| --- | --- | --- | --- |
| Issuer | `keys/issuer.key` | Ed25519 | mints capability tokens, so it decides what any subject may do |
| Receipt | `keys/receipt.pem` | P-256 (ES256) | signs the receipt chain, so it decides what the audit record says |

Their compromise consequences differ and should not be conflated. A leaked
**receipt** key lets an attacker forge or rewrite audit history, but cannot mint
a token granting new authority. A leaked **issuer** key lets an attacker mint
tokens for an arbitrary subject, but cannot rewrite the receipt chain to hide
the fact. Back them up and rotate them independently.

In both cases the subject named in a token is *asserted* by the operator's key,
not *attested* by an independent authority.

### The attestation gap, and what would actually close it

That assertion model is a real gap. In SPIFFE terms the workload attests itself:
any process that can read `issuer.key` can mint a token naming any subject, so
the identity claim rests entirely on filesystem permissions.

A local SPIRE deployment is **not** ruled out as a hardening step, and it is
worth being precise about what it would and would not buy. A privileged SPIRE
Agent can attest an unprivileged Workload API caller from OS-derived selectors
(uid, gid, binary path) and issue only the SPIFFE ID registered for those
selectors, without ever handing the workload an issuer key. That is a genuine
improvement: one compromised local process could no longer assert another
process's subject, and identity would rest on kernel-observed process attributes
rather than on read access to a key file.

What it does not do is remove the root of trust from the host. An attacker with
root, or with control of the SPIRE Agent itself, can still forge selectors or
issue arbitrary identities, because the attestor and the attested ultimately sit
inside the same administrative boundary. Local SPIRE raises the bar from "read a
file" to "compromise a privileged daemon"; it does not make the identity claim
externally verifiable.

Today Ardur does neither: there is no SPIRE agent and no attestation of any kind.
Local hardening along these lines is a legitimate option rather than a
contradiction, and is tracked with the wider SPIRE work.

### Where attested identity carries the most weight

Multi-tenant and multi-host deployments — the server binary, channel workers, and
any remote verifier — are where attestation matters most, because there the
asserting party and the relying party are genuinely separate organisations or
machines. Real SPIRE integration (SVID issuance, SVID binding at the
governance-plane boundary) is tracked as a distinct piece of work rather than
implied by the naming.

Until that integration exists, do not describe Ardur as having SPIFFE workload
identity. It has SPIFFE-shaped identifiers in some places, and operator-held
keys everywhere.

## Required repository security settings

Repository administrators must keep these controls enabled on both `main` and `dev`:

1. GitHub secret scanning.
2. Push protection for supported secret patterns.
3. Dependency graph and Dependabot alerts.
4. Dependabot security updates.
5. Ruleset `.github/rulesets/main-dev-security-gates.json` or an equivalent active GitHub ruleset requiring CI, DCO, signed commits, code-owner review, and linear history.

The repository cannot enable secret scanning or push protection from source code alone; those are GitHub repository/org settings. This file documents the required posture, while CI runs a redacted Gitleaks scan as an additional open-source gate.
