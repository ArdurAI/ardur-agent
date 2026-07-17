# API design review checklist

Walk this top to bottom. Each line is a yes/no the design should be able to
answer. A "no" is not automatically a blocker, but it is a finding worth stating.

## Contract shape
- [ ] Resource names are domain nouns, not internal table names.
- [ ] Field names are consistent across every endpoint (casing, suffixes, units).
- [ ] Each endpoint does one thing; no behavior-switching mode flags.
- [ ] Enum-valued fields document their full set of values.

## Idempotency & safety
- [ ] `GET` and `HEAD` are side-effect free.
- [ ] `PUT` and `DELETE` are idempotent.
- [ ] `POST` creates accept an idempotency key for safe retry.
- [ ] Multi-step operations have a defined end state on partial failure.

## Errors
- [ ] One error envelope shape across the whole surface.
- [ ] Status codes match semantics (4xx client, 5xx server, no 200-with-error).
- [ ] Errors carry a machine-readable reason code, not just prose.
- [ ] Error messages are actionable and do not leak secrets or stack traces.
- [ ] All error responses are documented in the contract.

## Versioning & compatibility
- [ ] Version selection mechanism is defined and uniform.
- [ ] Every field of this change is classified additive vs breaking.
- [ ] No new required request field on an existing endpoint without a version bump.
- [ ] No removed/renamed/retyped response field without a version bump.
- [ ] Breaking changes have a deprecation window and a client-detectable signal.

## Auth
- [ ] Single, consistent authentication scheme.
- [ ] Authorization checked per resource, not only per route (no IDOR/BOLA).
- [ ] Not-found vs forbidden chosen to avoid existence disclosure.
- [ ] Tokens/scopes are least-privilege.

## Limits
- [ ] Rate limit exists and is advertised (`429`, `Retry-After`, limit headers).
- [ ] List endpoints paginate with a bounded max page size.
- [ ] Request body and array sizes are bounded.

## Observability
- [ ] Request/correlation id accepted or assigned, echoed in responses.
- [ ] Per-operation success/error rates are externally measurable.

## Data & lifecycle
- [ ] Timestamps are UTC, ISO 8601.
- [ ] Money/precise numbers avoid binary float representation.
- [ ] Absent vs null is meaningful and documented.
- [ ] Behavior for unknown client-sent fields is defined.
