# Cite-or-refuse grounding receipt schema

Emit the receipt as JSON in a fenced `json` block. Use `null` only when the
field is not available from the retrieval/tooling layer; do not invent metadata.

```json
{
  "mode": "cite_or_refuse",
  "status": "answered_with_citations | refused_empty_retrieval | refused_unsupported_claim",
  "retrieval_query": "exact retrieval query or task",
  "retrieved_span_count": 0,
  "cited_spans": [
    {
      "id": "S1",
      "source_id": "stable document or memory id",
      "title": "document title when available",
      "uri": "source URI when available",
      "offset": {
        "kind": "bytes | lines | seconds | unknown",
        "start": null,
        "end": null
      }
    }
  ],
  "answer_claims": [
    {
      "claim": "short summary of a corpus-dependent claim",
      "supported_by": ["S1"]
    }
  ],
  "refusal_reason": null
}
```

Status guidance:

- `answered_with_citations`: retrieval returned usable spans and every
  corpus-dependent claim is supported by at least one cited span.
- `refused_empty_retrieval`: retrieval returned no usable spans, or corpus access
  failed closed.
- `refused_unsupported_claim`: retrieval returned some spans, but they did not
  support the user's requested claim.
