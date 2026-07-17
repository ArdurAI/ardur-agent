---
name: api-design-review
description: Review a new or changed API surface against the contract concerns that bite later.
metadata:
  type: code-review
  version: 1
---
# Reviewing an API design

An API is a contract you cannot easily take back. Once a client depends on it,
every wart is permanent. Review the *contract* — the shape clients see — before
the implementation, because the implementation can change and the contract
mostly cannot.

Review against the dimensions below. For a long-form checklist you can walk
item by item, ask for `@./checklist.md`. The dimensions here are the reasoning;
the checklist is the enumeration.

## 1. Resource & operation model

- Do the nouns and verbs match the domain, or do they leak internal structure?
  Clients should not have to understand your database schema to call the API.
- Is each operation doing one thing? An endpoint that branches on a `mode` flag
  into unrelated behaviors is two endpoints wearing a trenchcoat.
- Are names consistent across the surface (`createdAt` here, `created_time`
  there is a bug waiting to confuse)?

## 2. Idempotency & safety

- Which operations are safe (no side effects) and which are idempotent
  (repeatable without changing the result beyond the first)? `GET` must be safe;
  `PUT`/`DELETE` should be idempotent.
- For non-idempotent creates (`POST`), is there an idempotency key so a client
  that retries after a timeout does not create duplicates? Networks retry; if
  the API has no idempotency story, the client's retry *is* the bug.
- What happens on a partial failure of a multi-step operation? Is there a
  defined, observable end state, or can the resource be left half-built?

## 3. Error model

- Is there one consistent error shape across every endpoint (code, message,
  machine-readable reason, optionally a correlation id)?
- Do status codes mean what they say? `200` with an error body, `404` for
  authorization failures, or `500` for client mistakes all break clients that
  reason about codes.
- Are errors actionable? "Invalid request" forces a support ticket; "field
  `email` must be a valid address" lets the client self-correct.
- Are error responses documented as part of the contract, not discovered in
  production?

## 4. Versioning & backwards-compatibility

- How does a client pin a version (URL path, header, media type)? Pick one and
  hold it across the surface.
- Of the proposed change, which parts are **additive** (new optional field, new
  endpoint — safe) and which are **breaking** (removed/renamed field, narrowed
  type, new required input, changed default, tightened validation)?
- A breaking change needs a migration path: a new version, a deprecation window,
  and a way for clients to detect they are on the old one. "We will email the
  clients" is not a migration path.
- Adding a required request field to an existing endpoint is breaking. So is
  making a previously optional response field absent. Treat both as version
  bumps.

## 5. Authentication & authorization

- How does a caller authenticate, and is it the same mechanism across the
  surface? Mixed schemes are a security smell.
- Is authorization checked per resource, not just per endpoint? An endpoint that
  authenticates the caller but returns another tenant's data is the classic
  IDOR/BOLA bug.
- Do error responses avoid leaking existence? "403 forbidden" vs "404 not found"
  can disclose whether a resource exists to someone not allowed to see it.
- Are scopes/permissions least-privilege, or does one token do everything?

## 6. Rate limits & resource bounds

- Is there a rate limit, and does the API tell the client about it
  (`429` plus `Retry-After` and limit headers)? An API with no limit is a
  denial-of-service vector against itself.
- Are list endpoints paginated with a bounded maximum page size? An unbounded
  `GET /things` becomes a slow query and a memory spike the day the table grows.
- Are request bodies and array fields size-bounded? Unbounded input is both a
  performance and a security problem.

## 7. Observability

- Does every request carry (or get assigned) a correlation/request id that
  appears in responses and logs, so a client report can be traced?
- Are the operations and their error rates measurable from the outside — can an
  operator tell this endpoint is degraded without reading code?

## 8. Data & lifecycle

- Are timestamps timezone-explicit (UTC, ISO 8601)? Naive local times are a
  recurring class of bug.
- Are monetary and high-precision values represented without float rounding
  surprises (minor units / decimal strings, not `float`)?
- Are nullable fields distinguishable from absent fields, and is the difference
  meaningful and documented?
- Is there a defined behavior for unknown fields the client sends — ignored or
  rejected? Pick one and document it.

## How to deliver the review

Lead with the **breaking and security findings** — those are the ones that are
expensive to fix after launch. Separate "must change before this ships" from
"worth doing" from "fine as a follow-up". For each finding, name the concrete
client-visible consequence ("a client retrying a timed-out `POST /orders` will
double-charge"), not the abstract principle. A principle without a consequence
is easy to wave away; a consequence is not.
