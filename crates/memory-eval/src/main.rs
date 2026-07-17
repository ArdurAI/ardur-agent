//! `ardur-memory-eval` — CLI for the memory retrieval-quality harness.
//!
//! Loads a JSON golden set, builds the BM25 / dense / hybrid retrievers over its
//! documents, scores all three, and prints the comparison table (or JSON). The
//! dense half is the deterministic `MockEmbedder` by default; pass `--live`
//! (built with `--features live-embed`) for the real BGE-M3 baseline.
#![forbid(unsafe_code)]

use std::sync::Arc;

use anyhow::Context as _;
use clap::{Parser, ValueEnum};

use ardur_embeddings::MockEmbedder;
use ardur_memory_eval::{
    Bm25Retriever, DenseRetriever, EvalConfig, GoldenSet, HybridRetriever, Retriever, evaluate_all,
};

/// Output format for the report.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    /// A compact human-readable table.
    Table,
    /// Pretty JSON (for diffing / CI artifacts).
    Json,
}

/// Measure retrieval quality (Recall@K / nDCG@K / MRR@K / citation / stale /
/// contradiction) of BM25, dense, and hybrid retrievers over a golden set.
#[derive(Debug, Parser)]
#[command(name = "ardur-memory-eval", version)]
struct Cli {
    /// Path to the JSON golden set.
    #[arg(long)]
    golden: std::path::PathBuf,
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    format: Format,
    /// The cutoff the single-value metrics and release gate report at.
    #[arg(long, default_value_t = 5)]
    primary_k: usize,
    /// Embedding dimension for the deterministic MockEmbedder dense retriever.
    #[arg(long, default_value_t = 384)]
    dim: usize,
    /// Use the real fastembed BGE-M3 model instead of MockEmbedder (requires the
    /// crate to be built with `--features live-embed`).
    #[arg(long, default_value_t = false)]
    live: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let golden = GoldenSet::from_json_file(&cli.golden)
        .with_context(|| format!("loading golden set {}", cli.golden.display()))?;
    if let Err(errs) = golden.validate() {
        anyhow::bail!(
            "golden set has dangling references:\n  {}",
            errs.join("\n  ")
        );
    }

    let bm25: Arc<dyn Retriever> = Arc::new(Bm25Retriever::index(&golden.docs).await?);
    let dense: Arc<dyn Retriever> = build_dense(&golden, cli.dim, cli.live).await?;
    let hybrid = HybridRetriever::new(dense.clone(), bm25.clone());

    let config = EvalConfig {
        primary_k: cli.primary_k,
        ..EvalConfig::default()
    };
    let report = evaluate_all(
        &[&*bm25, &*dense, &hybrid as &dyn Retriever],
        &golden,
        &config,
    )
    .await?;

    match cli.format {
        Format::Table => print!("{}", report.to_table()),
        Format::Json => println!("{}", report.to_json()?),
    }
    Ok(())
}

/// Build the dense retriever — deterministic MockEmbedder, or the real fastembed
/// model behind `--live` + the `live-embed` feature.
async fn build_dense(
    golden: &GoldenSet,
    dim: usize,
    live: bool,
) -> anyhow::Result<Arc<dyn Retriever>> {
    if live {
        #[cfg(feature = "live-embed")]
        {
            let embedder = ardur_embeddings::FastEmbedEmbedder::from_env()
                .context("loading fastembed model for --live")?;
            return Ok(Arc::new(
                DenseRetriever::index(embedder, &golden.docs).await?,
            ));
        }
        #[cfg(not(feature = "live-embed"))]
        {
            let _ = golden;
            anyhow::bail!("--live requires building with `--features live-embed`");
        }
    }
    Ok(Arc::new(
        DenseRetriever::index(MockEmbedder::new(dim), &golden.docs).await?,
    ))
}
