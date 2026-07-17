# Security policy

## Reporting vulnerabilities

Please report suspected vulnerabilities privately through GitHub Security Advisories for this repository. Do not open a public issue with exploit details or secrets.

If GitHub advisory access is unavailable, contact the repository owner and include:

- affected component and version/commit,
- impact and prerequisites,
- minimal reproduction steps,
- whether credentials, tokens, or private data may have been exposed.

## Required repository security settings

Repository administrators must keep these controls enabled on both `main` and `dev`:

1. GitHub secret scanning.
2. Push protection for supported secret patterns.
3. Dependency graph and Dependabot alerts.
4. Dependabot security updates.
5. Ruleset `.github/rulesets/main-dev-security-gates.json` or an equivalent active GitHub ruleset requiring CI, DCO, signed commits, code-owner review, and linear history.

The repository cannot enable secret scanning or push protection from source code alone; those are GitHub repository/org settings. This file documents the required posture, while CI runs a redacted Gitleaks scan as an additional open-source gate.
