//! Marketplace / skill registry CLI surface.

use std::path::{Component, Path, PathBuf};

use ardur_cli::CliError;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use clap::{Args, Subcommand};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use p256::pkcs8::DecodePublicKey;
use sha2::{Digest, Sha256};

use crate::StateDirs;
use crate::state_id::sanitize_state_id;

/// Arguments to `ardur marketplace`.
#[derive(Args)]
pub struct MarketplaceArgs {
    #[command(subcommand)]
    pub action: MarketplaceAction,
}

/// Subcommands for `ardur marketplace`.
#[derive(Subcommand)]
pub enum MarketplaceAction {
    /// List installed skills.
    List,
    /// Search the marketplace index for skills.
    Search {
        /// Query string.
        query: String,
    },
    /// Install a skill from a local path or remote URL.
    Install {
        /// Skill URL, manifest path, or local artifact path.
        source: String,
    },
    /// Validate a signed skill/plugin capability manifest.
    Validate {
        /// Manifest JSON path.
        manifest: PathBuf,
        /// P-256 public key PEM used to verify the ES256 manifest signature.
        #[arg(long, value_name = "PUBLIC_KEY_PEM")]
        key: PathBuf,
    },
    /// Show details of an installed skill.
    Show {
        /// Skill id.
        id: String,
    },
    /// Remove an installed skill.
    Remove {
        /// Skill id.
        id: String,
    },
    /// Verify signatures of installed skills.
    Verify {
        /// Optional P-256 public key PEM for installed local manifests.
        #[arg(long, value_name = "PUBLIC_KEY_PEM")]
        key: Option<PathBuf>,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SkillRecord {
    skill_id: String,
    name: String,
    version: String,
    source: String,
    installed_at: u64,
    #[serde(default = "default_skill_kind")]
    kind: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    signature: String,
}

#[derive(Debug, serde::Deserialize)]
struct CapabilityManifest {
    schema_version: u32,
    kind: String,
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    artifacts: Vec<ManifestArtifact>,
    signature: ManifestSignature,
}

#[derive(Debug, serde::Deserialize)]
struct ManifestArtifact {
    path: String,
    sha256: String,
}

#[derive(Debug, serde::Deserialize)]
struct ManifestSignature {
    alg: String,
    value: String,
}

fn default_skill_kind() -> String {
    "skill".to_string()
}

fn skills_dir(root: &Path) -> PathBuf {
    root.join("skills")
}

fn read_skills(root: &Path) -> Result<Vec<SkillRecord>, CliError> {
    let dir = skills_dir(root);
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(v) = serde_json::from_str::<SkillRecord>(&content) {
                        records.push(v);
                    }
                }
            }
        }
    }
    Ok(records)
}

fn read_manifest(path: &Path) -> Result<CapabilityManifest, CliError> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(|e| {
        CliError::State(format!(
            "invalid marketplace manifest {}: {e}",
            path.display()
        ))
    })
}

fn canonical_manifest_payload(manifest: &CapabilityManifest) -> Result<Vec<u8>, CliError> {
    let lines = vec![
        (
            "schema_version",
            serde_json::to_string(&manifest.schema_version),
        ),
        ("kind", serde_json::to_string(&manifest.kind)),
        ("id", serde_json::to_string(&manifest.id)),
        ("name", serde_json::to_string(&manifest.name)),
        ("version", serde_json::to_string(&manifest.version)),
        (
            "capabilities",
            serde_json::to_string(&manifest.capabilities),
        ),
        (
            "artifacts",
            serde_json::to_string(&manifest.artifacts_as_json()),
        ),
    ];

    let mut out = Vec::new();
    for (name, value) in lines {
        let value = value.map_err(|e| CliError::State(e.to_string()))?;
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b":");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\n");
    }
    Ok(out)
}

impl CapabilityManifest {
    fn artifacts_as_json(&self) -> Vec<serde_json::Value> {
        self.artifacts
            .iter()
            .map(|artifact| {
                serde_json::json!({
                    "path": artifact.path,
                    "sha256": artifact.sha256,
                })
            })
            .collect()
    }
}

fn validate_manifest_shape(manifest: &CapabilityManifest) -> Result<(), CliError> {
    if manifest.schema_version != 1 {
        return Err(CliError::State(format!(
            "unsupported manifest schema_version {} (expected 1)",
            manifest.schema_version
        )));
    }
    if !matches!(manifest.kind.as_str(), "skill" | "plugin") {
        return Err(CliError::State(format!(
            "unsupported manifest kind `{}` (expected skill or plugin)",
            manifest.kind
        )));
    }
    for (field, value) in [
        ("id", manifest.id.as_str()),
        ("name", manifest.name.as_str()),
        ("version", manifest.version.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(CliError::State(format!(
                "manifest field `{field}` must not be empty"
            )));
        }
    }
    if manifest.signature.alg != "ES256" {
        return Err(CliError::State(format!(
            "unsupported signature alg `{}` (expected ES256)",
            manifest.signature.alg
        )));
    }
    if manifest.signature.value.trim().is_empty() {
        return Err(CliError::State("manifest signature is empty".to_string()));
    }
    Ok(())
}

fn safe_artifact_path(base: &Path, raw: &str) -> Result<PathBuf, CliError> {
    let rel = Path::new(raw);
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CliError::State(format!(
            "artifact path `{raw}` must be relative and stay inside the manifest directory"
        )));
    }
    Ok(base.join(rel))
}

fn verify_artifacts(manifest_path: &Path, manifest: &CapabilityManifest) -> Result<(), CliError> {
    let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    for artifact in &manifest.artifacts {
        let path = safe_artifact_path(base, &artifact.path)?;
        let bytes = std::fs::read(&path)
            .map_err(|e| CliError::State(format!("reading artifact {}: {e}", path.display())))?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(CliError::State(format!(
                "artifact `{}` sha256 mismatch: expected {}, got {actual}",
                artifact.path, artifact.sha256
            )));
        }
    }
    Ok(())
}

fn decode_signature(raw: &str) -> Result<Signature, CliError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .or_else(|_| STANDARD.decode(raw))
        .map_err(|e| CliError::State(format!("decoding manifest signature: {e}")))?;
    Signature::from_der(&bytes)
        .or_else(|_| Signature::try_from(bytes.as_slice()))
        .map_err(|e| CliError::State(format!("parsing ES256 signature: {e}")))
}

fn verify_manifest_signature(
    manifest: &CapabilityManifest,
    public_key_path: &Path,
) -> Result<(), CliError> {
    let key_pem = std::fs::read_to_string(public_key_path)?;
    let verifying_key = VerifyingKey::from_public_key_pem(&key_pem).map_err(|e| {
        CliError::State(format!(
            "loading P-256 public key {}: {e}",
            public_key_path.display()
        ))
    })?;
    let payload = canonical_manifest_payload(manifest)?;
    let signature = decode_signature(&manifest.signature.value)?;
    verifying_key
        .verify(&payload, &signature)
        .map_err(|e| CliError::State(format!("manifest signature verification failed: {e}")))
}

fn validate_manifest(
    manifest_path: &Path,
    key_path: &Path,
) -> Result<CapabilityManifest, CliError> {
    let manifest = read_manifest(manifest_path)?;
    validate_manifest_shape(&manifest)?;
    verify_artifacts(manifest_path, &manifest)?;
    verify_manifest_signature(&manifest, key_path)?;
    Ok(manifest)
}

fn install_record(source: &str) -> SkillRecord {
    let source_path = Path::new(source);
    if source_path.is_file() {
        if let Ok(manifest) = read_manifest(source_path) {
            return SkillRecord {
                skill_id: manifest.id,
                name: manifest.name,
                version: manifest.version,
                source: source.to_string(),
                installed_at: now_secs(),
                kind: manifest.kind,
                capabilities: manifest.capabilities,
                signature: manifest.signature.value,
            };
        }
    }

    SkillRecord {
        skill_id: uuid::Uuid::new_v4().to_string(),
        name: "installed-skill".to_string(),
        version: "0.0.1".to_string(),
        source: source.to_string(),
        installed_at: now_secs(),
        kind: "skill".to_string(),
        capabilities: Vec::new(),
        signature: String::new(),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run `ardur marketplace` subcommands.
pub fn run_marketplace(args: MarketplaceArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let dir = skills_dir(&root);
    std::fs::create_dir_all(&dir)?;

    match args.action {
        MarketplaceAction::List => {
            let records = read_skills(&root)?;
            if records.is_empty() {
                println!("no skills installed");
            } else {
                println!("ID                  KIND      NAME                VERSION");
                for r in &records {
                    println!(
                        "{: <20} {: <8} {: <20} {}",
                        r.skill_id, r.kind, r.name, r.version
                    );
                }
            }
        }
        MarketplaceAction::Search { query } => {
            let query = query.to_lowercase();
            let records = read_skills(&root)?;
            let filtered: Vec<&SkillRecord> = records
                .iter()
                .filter(|r| {
                    r.name.to_lowercase().contains(&query)
                        || r.skill_id.to_lowercase().contains(&query)
                        || r.capabilities
                            .iter()
                            .any(|capability| capability.to_lowercase().contains(&query))
                })
                .collect();
            if filtered.is_empty() {
                println!("no installed skills match '{query}'");
                println!("note: remote marketplace search is a Phase 2 wiring task");
            } else {
                for r in filtered {
                    println!("{} {} {}", r.skill_id, r.name, r.version);
                }
            }
        }
        MarketplaceAction::Install { source } => {
            let record = install_record(&source);
            let id = record.skill_id.clone();
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string_pretty(&record)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("installed {} {id} from {source}", record.kind);
            if record.signature.is_empty() {
                println!("warning: installed record has no manifest signature");
            } else {
                println!(
                    "signature recorded; run `ardur marketplace validate {source} --key <public-key.pem>` before trusting it"
                );
            }
        }
        MarketplaceAction::Validate { manifest, key } => {
            let manifest = validate_manifest(&manifest, &key)?;
            println!(
                "manifest {} ({}) version {} verified",
                manifest.id, manifest.kind, manifest.version
            );
            println!(
                "{} capabilities, {} artifacts",
                manifest.capabilities.len(),
                manifest.artifacts.len()
            );
        }
        MarketplaceAction::Show { id } => {
            let records = read_skills(&root)?;
            let found = records.iter().find(|r| r.skill_id == id);
            match found {
                Some(r) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(r)
                            .map_err(|e| CliError::State(e.to_string()))?
                    );
                }
                None => {
                    return Err(CliError::State(format!("skill `{id}` not found")));
                }
            }
        }
        MarketplaceAction::Remove { id } => {
            sanitize_state_id(&id)?;
            let path = dir.join(format!("{id}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("skill `{id}` not found")));
            }
            std::fs::remove_file(&path)?;
            println!("removed skill {id}");
        }
        MarketplaceAction::Verify { key } => {
            let records = read_skills(&root)?;
            let mut ok = 0;
            let mut failed = 0;
            for r in &records {
                if let Some(key_path) = key.as_ref() {
                    let source = Path::new(&r.source);
                    if source.is_file() && validate_manifest(source, key_path).is_ok() {
                        println!("{}: verified", r.skill_id);
                        ok += 1;
                        continue;
                    }
                }
                if r.signature.is_empty() {
                    println!("{}: missing manifest signature", r.skill_id);
                    failed += 1;
                } else if key.is_none() {
                    println!("{}: signature present (supply --key to verify)", r.skill_id);
                    failed += 1;
                } else {
                    println!("{}: invalid signature or artifact digest", r.skill_id);
                    failed += 1;
                }
            }
            println!("{ok} verified, {failed} failed out of {}", records.len());
        }
    }
    Ok(())
}
