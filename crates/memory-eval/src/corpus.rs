//! The golden evaluation corpus: the labeled documents and queries the harness
//! measures a retriever against.
//!
//! A [`GoldenSet`] is the ground truth — a set of [`EvalDoc`]s (the searchable
//! memory) plus a set of [`GoldenQuery`]s (each with graded relevance judgments,
//! optional citation expectations, and optional contradiction annotations). It
//! is loaded from JSON so fixtures live in-tree and are diff-reviewable, and it
//! is retriever-agnostic: the same golden set scores dense-only, BM25-only, and
//! hybrid retrievers so their numbers are directly comparable.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::metrics::ContradictionPair;

/// The content kind of a memory document — drives chunking strategy and lets the
/// harness report per-kind retrieval quality (e.g. code vs prose recall).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocKind {
    /// A conversation / meeting transcript turn.
    Transcript,
    /// Source code (identifiers, symbols, exact strings matter most).
    Code,
    /// Markdown prose (docs, notes, decision records).
    Markdown,
    /// An attached document's extracted text (PDF, email, etc.).
    Attachment,
    /// A short structured memory note / fact.
    Note,
}

/// One searchable document in the golden corpus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalDoc {
    /// Stable identifier, matched against relevance judgments and retriever output.
    pub id: String,
    /// The document's searchable text.
    pub text: String,
    /// Its content kind.
    pub kind: DocKind,
    /// Whether this memory is **stale** — invalidated or superseded (mirrors a
    /// `MemoryRecord` whose `invalidation_time` is set / whose `valid_to` has
    /// passed). A retriever surfacing stale docs inflates the stale-memory rate.
    #[serde(default)]
    pub stale: bool,
}

/// The taxonomy of query intents (per V3 §394): the mix a realistic engineering
/// memory must serve. Reported per-type so a retriever's weak spots are visible
/// (e.g. exact `file_path` lookups favour BM25; `decision_history` favours dense).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryType {
    /// A single-fact lookup ("what port does the server bind?").
    Factoid,
    /// An exact file / path / identifier lookup.
    FilePath,
    /// "Why / when did we decide X?" — decision provenance.
    DecisionHistory,
    /// Time-scoped ("what was the config *before* the migration?").
    Temporal,
    /// Negation ("which providers do *not* support streaming?").
    Negation,
    /// Requires joining facts across documents.
    MultiHop,
}

/// One labeled query with its ground-truth judgments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldenQuery {
    /// Stable identifier.
    pub id: String,
    /// The query text handed to the retriever.
    pub query: String,
    /// The query's intent type.
    pub query_type: QueryType,
    /// Graded relevance: `doc_id -> grade` (0 non-relevant .. 3 highly relevant).
    /// Docs absent from the map are non-relevant.
    pub relevant: HashMap<String, u8>,
    /// Docs the answer to this query should cite, if the query has a citation
    /// expectation. Empty ⇒ the query is skipped by the citation-correctness
    /// metric.
    #[serde(default)]
    pub expected_citations: HashSet<String>,
    /// A contradiction annotation, if this query probes stale-vs-current handling.
    #[serde(default)]
    pub contradiction: Option<ContradictionPair>,
}

/// A complete labeled corpus: documents + queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldenSet {
    /// A human-readable name for the corpus (e.g. `"ardur-architect-vault-v1"`).
    pub name: String,
    /// The searchable documents.
    pub docs: Vec<EvalDoc>,
    /// The labeled queries.
    pub queries: Vec<GoldenQuery>,
}

impl GoldenSet {
    /// Parse a golden set from a JSON string.
    ///
    /// # Errors
    ///
    /// Returns the underlying `serde_json` error if the JSON is malformed or does
    /// not match the schema.
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Load a golden set from a JSON file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the JSON does not parse.
    pub fn from_json_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path.as_ref())
            .map_err(|e| anyhow::anyhow!("reading golden set {}: {e}", path.as_ref().display()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// The set of `doc_id`s marked stale — the denominator input for the
    /// stale-memory rate.
    pub fn stale_doc_ids(&self) -> HashSet<String> {
        self.docs
            .iter()
            .filter(|d| d.stale)
            .map(|d| d.id.clone())
            .collect()
    }

    /// Validate referential integrity: every judged / cited / contradiction
    /// `doc_id` must exist in `docs`. Catches fixture typos that would otherwise
    /// silently deflate the metrics.
    ///
    /// # Errors
    ///
    /// Returns a list of every dangling reference found.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let ids: HashSet<&str> = self.docs.iter().map(|d| d.id.as_str()).collect();
        let mut errors = Vec::new();
        let mut check = |ctx: &str, id: &str| {
            if !ids.contains(id) {
                errors.push(format!("{ctx} references unknown doc_id `{id}`"));
            }
        };
        for q in &self.queries {
            for id in q.relevant.keys() {
                check(&format!("query `{}` relevant", q.id), id);
            }
            for id in &q.expected_citations {
                check(&format!("query `{}` expected_citations", q.id), id);
            }
            if let Some(c) = &q.contradiction {
                check(
                    &format!("query `{}` contradiction.current", q.id),
                    &c.current,
                );
                check(
                    &format!("query `{}` contradiction.superseded", q.id),
                    &c.superseded,
                );
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "name": "tiny",
      "docs": [
        {"id": "d1", "text": "the server binds port 8080", "kind": "note"},
        {"id": "d2", "text": "old: the server binds port 9090", "kind": "note", "stale": true}
      ],
      "queries": [
        {
          "id": "q1",
          "query": "what port does the server bind",
          "query_type": "factoid",
          "relevant": {"d1": 3},
          "expected_citations": ["d1"],
          "contradiction": {"current": "d1", "superseded": "d2"}
        }
      ]
    }"#;

    #[test]
    fn parses_and_validates() {
        let set = GoldenSet::from_json_str(SAMPLE).expect("parses");
        assert_eq!(set.docs.len(), 2);
        assert_eq!(set.queries.len(), 1);
        assert!(set.docs[1].stale);
        assert_eq!(
            set.stale_doc_ids(),
            ["d2".to_string()].into_iter().collect()
        );
        assert!(set.validate().is_ok());
    }

    #[test]
    fn optional_fields_default() {
        // A query with no citations / contradiction still parses.
        let json = r#"{"name":"t","docs":[{"id":"d1","text":"x","kind":"code"}],
          "queries":[{"id":"q","query":"x","query_type":"file_path","relevant":{"d1":1}}]}"#;
        let set = GoldenSet::from_json_str(json).expect("parses");
        assert!(set.queries[0].expected_citations.is_empty());
        assert!(set.queries[0].contradiction.is_none());
        assert!(!set.docs[0].stale);
    }

    #[test]
    fn validate_catches_dangling_reference() {
        let json = r#"{"name":"t","docs":[{"id":"d1","text":"x","kind":"note"}],
          "queries":[{"id":"q","query":"x","query_type":"factoid","relevant":{"ghost":1}}]}"#;
        let set = GoldenSet::from_json_str(json).expect("parses");
        let errs = set.validate().expect_err("dangling ref");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("ghost"));
    }
}
