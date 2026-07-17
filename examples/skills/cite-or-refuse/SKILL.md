---
name: cite-or-refuse
description: Ground answers in retrieved user-corpus spans; cite every claim or refuse when retrieval is empty.
metadata:
  category: grounding
  version: 1
  policy: cite-or-refuse
---
# Cite-or-refuse grounded mode

Use this skill whenever the user requests an answer that depends on private,
workspace, or user-corpus knowledge rather than general reasoning. The answer is
allowed only when it is grounded in retrieved spans from the user corpus.

## Mandatory preflight

1. Query the configured user-corpus retriever before drafting the answer.
2. Treat the retrieval result as empty when there are zero spans, every span has
   an unusable body, or the retrieval layer reports an authorization/error state
   that prevents reading the corpus.
3. If retrieval is empty, do **not** answer from memory or model prior knowledge.
   Refuse with the template below and include a grounding receipt showing
   `status: refused_empty_retrieval`.
4. If retrieval returns spans, use only those spans for corpus-dependent claims.
   General-language glue is fine, but factual claims about the corpus require a
   citation.

## Citation rules

- Cite each corpus-dependent sentence with one or more source span markers.
- Source markers use the stable span id form `[S<n>]`, where each marker maps to
  one retrieved source span in the source list.
- Never cite a document as a whole when the retriever returned narrower spans.
- Do not merge nearby spans unless the retrieval layer explicitly returned the
  merged span.
- If a requested detail is not present in the retrieved spans, say that the
  retrieved corpus does not support that detail instead of guessing.

## Required response shape

When grounded spans are available, respond in this order:

1. `Answer` — concise answer with inline `[S<n>]` citations on every
   corpus-dependent claim.
2. `Sources` — one bullet per cited span, including span id, document/source
   title or URI, byte/line/time offsets when available, and a short quoted
   excerpt. Keep excerpts minimal and do not invent offsets.
3. `Grounding receipt` — a compact machine-readable block following
   `@./receipt-schema.md`.

When retrieval is empty, use this refusal template:

> I can't answer that from the user corpus because retrieval returned no usable
> spans. I can try a different query, use a source you provide, or answer without
> corpus grounding if you explicitly allow an ungrounded answer.

Then include `Grounding receipt` with `status: refused_empty_retrieval` and an
empty `cited_spans` list.

## Grounding receipt requirements

The receipt is an audit record for the turn. It must include:

- `mode: cite_or_refuse`
- `status`: `answered_with_citations`, `refused_empty_retrieval`, or
  `refused_unsupported_claim`
- `retrieval_query`: the exact query or task sent to retrieval
- `retrieved_span_count`: number of usable spans returned
- `cited_spans`: stable ids for every span cited in the answer
- `answer_claims`: a list of claim summaries with the span ids supporting each
  claim
- `refusal_reason`: present for refused turns

See `@./receipt-schema.md` for the exact JSON shape.

## Safety checks before finalizing

- Every corpus-dependent sentence has a citation.
- Every citation appears in `Sources` and `cited_spans`.
- Every `Sources` item is copied from retrieval metadata/span text, not invented.
- The answer contains no unsupported private-corpus facts.
- Empty retrieval produced a refusal, not a best-effort answer.
