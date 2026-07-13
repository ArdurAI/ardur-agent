//! Marketplace / skill + plugin registry CLI surface.
//!
//! Eight lifecycle verbs over locally-signed capability manifests: `browse`,
//! `search`, `install`, `inspect`, `update`, `audit`, `uninstall`, `publish`
//! (plus the pre-existing `validate` / `verify` helpers). Every verb operates
//! on the same on-disk state: one JSON [`SkillRecord`] per installed
//! skill/plugin under `${ArdurHome}/skills/`, and — for `kind = "skill"`
//! manifests that bundle a `SKILL.md` artifact — a mirrored copy under
//! `${ArdurHome}/skills_catalog/<id>/SKILL.md` where
//! [`ardur_tool_registry`]'s filesystem loader picks it up.
//!
//! There is no remote catalog fetch yet (installs read a local manifest
//! file); `install`/`update` are signature-verified by default (ES256) and
//! refuse to proceed on an unsigned manifest unless the caller passes
//! `--allow-unsigned` and accepts the printed warning. Manifest, capability,
//! artifact, and runtime-claim counts are all bounded so a malicious manifest
//! cannot exhaust memory or disk during validation.

use std::path::{Component, Path, PathBuf};

use ardur_cli::CliError;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use clap::{Args, Subcommand};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::Signer, signature::Verifier};
use p256::pkcs8::{DecodePrivateKey, DecodePublicKey};
use sha2::{Digest, Sha256};

use crate::StateDirs;

/// Manifest byte-size ceiling (256 KiB) — refused before parsing.
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// Maximum declared capability strings per manifest.
const MAX_CAPABILITIES: usize = 32;
/// Maximum bundled artifacts per manifest.
const MAX_ARTIFACTS: usize = 64;
/// Per-artifact byte-size ceiling (10 MiB).
const MAX_ARTIFACT_BYTES: u64 = 10 * 1024 * 1024;
/// Maximum declared runtime claims per plugin manifest.
const MAX_RUNTIME_CLAIMS: usize = 16;
/// Capability names (post `cap.` strip) whose grant is treated as high-risk
/// in `audit` output — mirrors [`ardur_tool_registry::Capability`]'s
/// dangerous variants.
const HIGH_RISK_CAPABILITIES: &[&str] = &["shell_exec", "process_spawn", "network_out", "fs_write"];

/// Arguments to `ardur marketplace`.
#[derive(Args)]
pub struct MarketplaceArgs {
    #[command(subcommand)]
    pub action: MarketplaceAction,
}

/// Subcommands for `ardur marketplace`.
#[derive(Subcommand)]
pub enum MarketplaceAction {
    /// Browse installed skills and plugins.
    #[command(alias = "list")]
    Browse,
    /// Search installed skills/plugins by name, id, or capability.
    Search {
        /// Query string.
        query: String,
    },
    /// Install a skill or plugin from a local signed manifest.
    Install {
        /// Path to a manifest JSON file. Remote/URL sources are not yet
        /// implemented — publish or copy the manifest locally first.
        source: String,
        /// P-256 public key PEM verifying the manifest's ES256 signature.
        /// Required unless `--allow-unsigned` is passed.
        #[arg(long, value_name = "PUBLIC_KEY_PEM")]
        key: Option<PathBuf>,
        /// Explicitly accept an unsigned or unverifiable manifest. Refused by
        /// default — installs are signature-verified unless you opt out.
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// Show an installed skill/plugin's manifest, signature state, and
    /// declared capabilities/claims.
    #[command(alias = "show")]
    Inspect {
        /// Skill or plugin id.
        id: String,
    },
    /// Update an installed skill/plugin to a new manifest version.
    Update {
        /// Skill or plugin id (must match the new manifest's `id`).
        id: String,
        /// Path to the new manifest JSON file.
        manifest: PathBuf,
        /// P-256 public key PEM verifying the new manifest's signature.
        #[arg(long, value_name = "PUBLIC_KEY_PEM")]
        key: Option<PathBuf>,
        /// Explicitly accept an unsigned or unverifiable manifest.
        #[arg(long)]
        allow_unsigned: bool,
        /// Allow a same-version reinstall or a version downgrade.
        #[arg(long)]
        force: bool,
    },
    /// Audit one (or, with no id, every) installed skill/plugin for
    /// unsigned installs, high-risk capabilities, and source-manifest drift.
    Audit {
        /// Skill or plugin id; audits every installed entry if omitted.
        id: Option<String>,
    },
    /// Remove an installed skill/plugin.
    #[command(alias = "remove")]
    Uninstall {
        /// Skill or plugin id.
        id: String,
    },
    /// Sign a local skill directory's manifest, producing an installable
    /// signed manifest bundle.
    Publish {
        /// Directory containing the skill's `SKILL.md`.
        skill_dir: PathBuf,
        /// Manifest id (e.g. `skill.my-helper`).
        id: String,
        /// Human-readable name.
        name: String,
        /// Version string (e.g. `0.1.0`).
        version: String,
        /// Declared capability string; repeatable.
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        /// Declared runtime claim as `<name>:<tool|channel|provider>`;
        /// repeatable. Only meaningful for plugin manifests (`--kind plugin`).
        #[arg(long = "claim")]
        claims: Vec<String>,
        /// Manifest kind: `skill` (default) or `plugin`.
        #[arg(long, default_value = "skill")]
        kind: String,
        /// P-256 private key PEM (PKCS#8) to sign the manifest with.
        #[arg(long, value_name = "PRIVATE_KEY_PEM")]
        key: PathBuf,
        /// Output path for the signed manifest JSON.
        #[arg(long, default_value = "manifest.json")]
        out: PathBuf,
    },
    /// Validate a signed skill/plugin capability manifest.
    Validate {
        /// Manifest JSON path.
        manifest: PathBuf,
        /// P-256 public key PEM used to verify the ES256 manifest signature.
        #[arg(long, value_name = "PUBLIC_KEY_PEM")]
        key: PathBuf,
    },
    /// Verify signatures of installed skills/plugins.
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
    /// Whether the signature was cryptographically checked at install/update
    /// time (`--key` was supplied). Records from before this field existed
    /// default to `false` — audit surfaces them as unverified, which is
    /// correct: their signature (if any) was never actually checked.
    #[serde(default)]
    verified: bool,
    /// Declared plugin runtime claims (empty for `kind = "skill"`).
    #[serde(default)]
    runtime_claims: Vec<RuntimeClaimRecord>,
}

/// A plugin's declared intent to extend one `Tool`/`Channel`/`Provider`
/// trait-family registration once activated by `ardur-plugin-runtime`.
/// Declaring a claim here does not activate it — activation happens at
/// process boot, outside this CLI's scope.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RuntimeClaimRecord {
    name: String,
    family: String,
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
    #[serde(default)]
    runtime_claims: Vec<RuntimeClaimRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

/// The directory the filesystem `SKILL.md` loader (`ardur_tool_registry`)
/// scans: one subdirectory per installed skill, each containing `SKILL.md`.
fn skill_catalog_dir(root: &Path) -> PathBuf {
    root.join("skills_catalog")
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
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_MANIFEST_BYTES {
        return Err(CliError::State(format!(
            "manifest {} is {} bytes, exceeding the {MAX_MANIFEST_BYTES}-byte ceiling",
            path.display(),
            meta.len()
        )));
    }
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
        (
            "runtime_claims",
            serde_json::to_string(&manifest.runtime_claims_as_json()),
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

    fn runtime_claims_as_json(&self) -> Vec<serde_json::Value> {
        self.runtime_claims
            .iter()
            .map(|claim| {
                serde_json::json!({
                    "name": claim.name,
                    "family": claim.family,
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
    validate_capabilities_bound(manifest)?;
    validate_artifacts_bound(manifest)?;
    validate_runtime_claims(manifest)?;
    Ok(())
}

fn validate_capabilities_bound(manifest: &CapabilityManifest) -> Result<(), CliError> {
    if manifest.capabilities.len() > MAX_CAPABILITIES {
        return Err(CliError::State(format!(
            "manifest declares {} capabilities, exceeding the {MAX_CAPABILITIES} ceiling",
            manifest.capabilities.len()
        )));
    }
    if let Some(bad) = manifest
        .capabilities
        .iter()
        .find(|c| c.trim().is_empty() || c.len() > 128)
    {
        return Err(CliError::State(format!(
            "capability `{bad}` must be 1-128 non-blank characters"
        )));
    }
    Ok(())
}

fn validate_artifacts_bound(manifest: &CapabilityManifest) -> Result<(), CliError> {
    if manifest.artifacts.len() > MAX_ARTIFACTS {
        return Err(CliError::State(format!(
            "manifest declares {} artifacts, exceeding the {MAX_ARTIFACTS} ceiling",
            manifest.artifacts.len()
        )));
    }
    Ok(())
}

/// Closure-invariant check over a plugin manifest's declared runtime claims:
/// bounded count, known trait family, bounded/charset-safe claim name, and no
/// duplicate `(family, name)` pair. Non-plugin manifests must declare none.
fn validate_runtime_claims(manifest: &CapabilityManifest) -> Result<(), CliError> {
    if manifest.kind != "plugin" {
        if !manifest.runtime_claims.is_empty() {
            return Err(CliError::State(
                "runtime_claims are only valid on kind=\"plugin\" manifests".to_string(),
            ));
        }
        return Ok(());
    }
    if manifest.runtime_claims.len() > MAX_RUNTIME_CLAIMS {
        return Err(CliError::State(format!(
            "manifest declares {} runtime claims, exceeding the {MAX_RUNTIME_CLAIMS} ceiling",
            manifest.runtime_claims.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for claim in &manifest.runtime_claims {
        if !matches!(claim.family.as_str(), "tool" | "channel" | "provider") {
            return Err(CliError::State(format!(
                "runtime claim `{}` names unknown trait family `{}` (expected tool, channel, or provider)",
                claim.name, claim.family
            )));
        }
        let name_ok = !claim.name.is_empty()
            && claim.name.len() <= 64
            && claim
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !name_ok {
            return Err(CliError::State(format!(
                "runtime claim name `{}` must be 1-64 characters of [a-zA-Z0-9_-]",
                claim.name
            )));
        }
        if !seen.insert((claim.family.clone(), claim.name.clone())) {
            return Err(CliError::State(format!(
                "duplicate runtime claim `{}` in family `{}`",
                claim.name, claim.family
            )));
        }
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
        let meta = std::fs::metadata(&path)
            .map_err(|e| CliError::State(format!("reading artifact {}: {e}", path.display())))?;
        if meta.len() > MAX_ARTIFACT_BYTES {
            return Err(CliError::State(format!(
                "artifact `{}` is {} bytes, exceeding the {MAX_ARTIFACT_BYTES}-byte ceiling",
                artifact.path,
                meta.len()
            )));
        }
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

/// Full verification: shape + bounds, artifact digests, then signature. Used
/// by `validate`, and by `install`/`update` whenever `--key` is supplied.
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

/// Shape + bounds + artifact-digest checks only, no signature check. Used by
/// `install --allow-unsigned` / `update --allow-unsigned` — the caller has
/// explicitly accepted the risk of an unverified manifest.
fn validate_manifest_unsigned(manifest_path: &Path) -> Result<CapabilityManifest, CliError> {
    let manifest = read_manifest(manifest_path)?;
    validate_manifest_shape(&manifest)?;
    verify_artifacts(manifest_path, &manifest)?;
    Ok(manifest)
}

/// Resolve a manifest per the install/update `--key`/`--allow-unsigned`
/// contract: signature-verified by default; unsigned only on explicit opt-in.
fn resolve_install_manifest(
    path: &Path,
    key: Option<&PathBuf>,
    allow_unsigned: bool,
) -> Result<(CapabilityManifest, bool), CliError> {
    if !path.is_file() {
        return Err(CliError::State(format!(
            "cannot install from `{}`: remote/URL sources are not implemented; \
             install from a local signed manifest file (see `ardur marketplace publish`)",
            path.display()
        )));
    }
    match key {
        Some(key_path) => Ok((validate_manifest(path, key_path)?, true)),
        None if allow_unsigned => {
            let manifest = validate_manifest_unsigned(path)?;
            eprintln!(
                "warning: installing `{}` without signature verification (--allow-unsigned); \
                 this manifest is NOT cryptographically trusted",
                manifest.id
            );
            Ok((manifest, false))
        }
        None => Err(CliError::State(
            "refusing to install an unsigned manifest by default; pass `--key <public-key.pem>` \
             to verify its signature, or `--allow-unsigned` to explicitly accept the risk"
                .to_string(),
        )),
    }
}

/// If `manifest` is a `kind = "skill"` manifest bundling a `SKILL.md`
/// artifact, copy the (already digest-verified) artifact into the local
/// skill catalog directory so the filesystem `SKILL.md` loader picks it up.
/// Returns the destination path when a copy happened.
fn sync_skill_markdown(
    root: &Path,
    manifest_path: &Path,
    manifest: &CapabilityManifest,
) -> Result<Option<PathBuf>, CliError> {
    if manifest.kind != "skill" {
        return Ok(None);
    }
    let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let Some(artifact) = manifest
        .artifacts
        .iter()
        .find(|a| Path::new(&a.path).file_name().and_then(|f| f.to_str()) == Some("SKILL.md"))
    else {
        return Ok(None);
    };
    let src = safe_artifact_path(base, &artifact.path)?;
    let dest_dir = skill_catalog_dir(root).join(&manifest.id);
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("SKILL.md");
    std::fs::copy(&src, &dest)?;
    Ok(Some(dest))
}

fn remove_skill_markdown(root: &Path, id: &str) -> Result<(), CliError> {
    let dest_dir = skill_catalog_dir(root).join(id);
    if dest_dir.is_dir() {
        std::fs::remove_dir_all(&dest_dir)?;
    }
    Ok(())
}

/// `"cap.shell_exec"` → `"shell_exec"`; bare strings pass through unchanged.
fn strip_capability_prefix(capability: &str) -> &str {
    capability.strip_prefix("cap.").unwrap_or(capability)
}

fn capability_risk(capability: &str) -> &'static str {
    if HIGH_RISK_CAPABILITIES.contains(&strip_capability_prefix(capability)) {
        "high-risk"
    } else {
        "standard"
    }
}

/// Parse a `major.minor.patch` (patch/minor optional, default 0) numeric
/// version for ordering. Non-numeric versions return `None` — the caller
/// falls back to a simple inequality check when this fails.
fn parse_numeric_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map(str::parse).transpose().ok()??;
    let patch = parts.next().map(str::parse).transpose().ok()??;
    Some((major, minor, patch))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn record_from_manifest(
    manifest: &CapabilityManifest,
    source: &str,
    verified: bool,
) -> SkillRecord {
    SkillRecord {
        skill_id: manifest.id.clone(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        source: source.to_string(),
        installed_at: now_secs(),
        kind: manifest.kind.clone(),
        capabilities: manifest.capabilities.clone(),
        signature: manifest.signature.value.clone(),
        verified,
        runtime_claims: manifest.runtime_claims.clone(),
    }
}

fn write_record(dir: &Path, record: &SkillRecord) -> Result<(), CliError> {
    std::fs::write(
        dir.join(format!("{}.json", record.skill_id)),
        serde_json::to_string_pretty(record).map_err(|e| CliError::State(e.to_string()))?,
    )?;
    Ok(())
}

/// Run `ardur marketplace` subcommands.
pub fn run_marketplace(args: MarketplaceArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let dir = skills_dir(&root);
    std::fs::create_dir_all(&dir)?;

    match args.action {
        MarketplaceAction::Browse => {
            let records = read_skills(&root)?;
            if records.is_empty() {
                println!("no skills or plugins installed");
            } else {
                println!("ID                  KIND      NAME                VERSION  SIGNED");
                for r in &records {
                    println!(
                        "{: <20} {: <8} {: <20} {: <8} {}",
                        r.skill_id,
                        r.kind,
                        r.name,
                        r.version,
                        if r.verified { "yes" } else { "no" }
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
                println!("no installed skills/plugins match '{query}'");
                println!("note: remote marketplace search is a Phase 2 wiring task");
            } else {
                for r in filtered {
                    println!("{} {} {}", r.skill_id, r.name, r.version);
                }
            }
        }
        MarketplaceAction::Install {
            source,
            key,
            allow_unsigned,
        } => {
            let path = Path::new(&source);
            let (manifest, verified) =
                resolve_install_manifest(path, key.as_ref(), allow_unsigned)?;
            let record = record_from_manifest(&manifest, &source, verified);
            write_record(&dir, &record)?;
            let synced = sync_skill_markdown(&root, path, &manifest)?;

            println!(
                "installed {} {} version {} ({})",
                record.kind,
                record.skill_id,
                record.version,
                if verified {
                    "signature verified"
                } else {
                    "UNVERIFIED"
                }
            );
            match synced {
                Some(p) => println!("skill catalog updated: {}", p.display()),
                None if manifest.kind == "skill" => println!(
                    "note: no SKILL.md artifact found; skill not wired into the local tool loader"
                ),
                None => {}
            }
            if !manifest.runtime_claims.is_empty() {
                println!(
                    "{} runtime claim(s) declared; activation happens at process boot via ardur-plugin-runtime",
                    manifest.runtime_claims.len()
                );
            }
        }
        MarketplaceAction::Inspect { id } => {
            let records = read_skills(&root)?;
            let r = records
                .iter()
                .find(|r| r.skill_id == id)
                .ok_or_else(|| CliError::State(format!("skill `{id}` not found")))?;
            println!("id:        {}", r.skill_id);
            println!("kind:      {}", r.kind);
            println!("name:      {}", r.name);
            println!("version:   {}", r.version);
            println!("source:    {}", r.source);
            println!(
                "signature: {}",
                if r.signature.is_empty() {
                    "absent".to_string()
                } else if r.verified {
                    "present, verified at install/update time".to_string()
                } else {
                    "present, NOT verified (installed with --allow-unsigned)".to_string()
                }
            );
            println!("capabilities ({}):", r.capabilities.len());
            for c in &r.capabilities {
                println!("  - {c} [{}]", capability_risk(c));
            }
            if !r.runtime_claims.is_empty() {
                println!("runtime claims ({}):", r.runtime_claims.len());
                for c in &r.runtime_claims {
                    println!("  - {} ({})", c.name, c.family);
                }
            }
            let catalog_path = skill_catalog_dir(&root).join(&r.skill_id).join("SKILL.md");
            println!("loaded into local tool catalog: {}", catalog_path.is_file());
        }
        MarketplaceAction::Update {
            id,
            manifest,
            key,
            allow_unsigned,
            force,
        } => {
            let existing_path = dir.join(format!("{id}.json"));
            let existing: SkillRecord = serde_json::from_str(
                &std::fs::read_to_string(&existing_path)
                    .map_err(|_| CliError::State(format!("skill `{id}` not found")))?,
            )
            .map_err(|e| CliError::State(e.to_string()))?;

            let (new_manifest, verified) =
                resolve_install_manifest(&manifest, key.as_ref(), allow_unsigned)?;

            if new_manifest.id != existing.skill_id {
                return Err(CliError::State(format!(
                    "manifest id `{}` does not match installed skill `{}`; uninstall and install fresh instead",
                    new_manifest.id, existing.skill_id
                )));
            }
            if new_manifest.version == existing.version && !force {
                return Err(CliError::State(format!(
                    "manifest version `{}` is unchanged; pass --force to reinstall the same version",
                    new_manifest.version
                )));
            }
            if let (Some(old_v), Some(new_v)) = (
                parse_numeric_version(&existing.version),
                parse_numeric_version(&new_manifest.version),
            ) {
                if new_v < old_v && !force {
                    return Err(CliError::State(format!(
                        "refusing downgrade from {} to {}; pass --force to override",
                        existing.version, new_manifest.version
                    )));
                }
            }

            let added: Vec<&String> = new_manifest
                .capabilities
                .iter()
                .filter(|c| !existing.capabilities.contains(c))
                .collect();
            let removed: Vec<&String> = existing
                .capabilities
                .iter()
                .filter(|c| !new_manifest.capabilities.contains(c))
                .collect();

            let record = record_from_manifest(&new_manifest, &manifest.to_string_lossy(), verified);
            write_record(&dir, &record)?;
            sync_skill_markdown(&root, &manifest, &new_manifest)?;

            println!(
                "updated {} {} -> {} ({})",
                id,
                existing.version,
                new_manifest.version,
                if verified {
                    "signature verified"
                } else {
                    "UNVERIFIED"
                }
            );
            if !added.is_empty() {
                println!("capabilities added: {added:?}");
            }
            if !removed.is_empty() {
                println!("capabilities removed: {removed:?}");
            }
        }
        MarketplaceAction::Audit { id } => {
            let records = read_skills(&root)?;
            let targets: Vec<&SkillRecord> = match &id {
                Some(id) => records.iter().filter(|r| &r.skill_id == id).collect(),
                None => records.iter().collect(),
            };
            if targets.is_empty() {
                return Err(CliError::State(match &id {
                    Some(id) => format!("skill `{id}` not found"),
                    None => "no skills or plugins installed".to_string(),
                }));
            }

            let mut unverified_count = 0;
            let mut high_risk_count = 0;
            for r in &targets {
                println!("== {} ({}) v{} ==", r.skill_id, r.kind, r.version);
                if !r.verified {
                    println!("  [flag] not signature-verified at install/update time");
                    unverified_count += 1;
                }
                let high_risk: Vec<&String> = r
                    .capabilities
                    .iter()
                    .filter(|c| capability_risk(c) == "high-risk")
                    .collect();
                if !high_risk.is_empty() {
                    println!("  [flag] high-risk capabilities: {high_risk:?}");
                    high_risk_count += 1;
                }
                let source_path = Path::new(&r.source);
                if source_path.is_file() {
                    match read_manifest(source_path) {
                        Ok(current) if current.signature.value != r.signature => {
                            println!(
                                "  [flag] source manifest signature changed since install; re-run `update`"
                            );
                        }
                        Err(e) => println!("  [flag] source manifest no longer parses: {e}"),
                        _ => {}
                    }
                } else {
                    println!(
                        "  [note] source manifest no longer present on disk at {}",
                        r.source
                    );
                }
            }
            println!(
                "audit summary: {} checked, {unverified_count} unverified, {high_risk_count} with high-risk capabilities",
                targets.len()
            );
        }
        MarketplaceAction::Uninstall { id } => {
            let path = dir.join(format!("{id}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("skill `{id}` not found")));
            }
            std::fs::remove_file(&path)?;
            remove_skill_markdown(&root, &id)?;
            println!("removed skill {id}");
        }
        MarketplaceAction::Publish {
            skill_dir,
            id,
            name,
            version,
            capabilities,
            claims,
            kind,
            key,
            out,
        } => {
            if !matches!(kind.as_str(), "skill" | "plugin") {
                return Err(CliError::State(format!(
                    "unsupported --kind `{kind}` (expected skill or plugin)"
                )));
            }
            if capabilities.len() > MAX_CAPABILITIES {
                return Err(CliError::State(format!(
                    "{} capabilities exceeds the {MAX_CAPABILITIES} ceiling",
                    capabilities.len()
                )));
            }
            let runtime_claims = parse_claim_args(&claims, &kind)?;

            let skill_md = skill_dir.join("SKILL.md");
            let bytes = std::fs::read(&skill_md)
                .map_err(|e| CliError::State(format!("reading {}: {e}", skill_md.display())))?;
            if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
                return Err(CliError::State(format!(
                    "{} is {} bytes, exceeding the {MAX_ARTIFACT_BYTES}-byte ceiling",
                    skill_md.display(),
                    bytes.len()
                )));
            }
            let digest = hex::encode(Sha256::digest(&bytes));
            let artifacts = vec![ManifestArtifact {
                path: "SKILL.md".to_string(),
                sha256: digest,
            }];

            let unsigned = CapabilityManifest {
                schema_version: 1,
                kind: kind.clone(),
                id: id.clone(),
                name: name.clone(),
                version: version.clone(),
                capabilities: capabilities.clone(),
                artifacts: artifacts.clone(),
                signature: ManifestSignature {
                    alg: "ES256".to_string(),
                    value: String::new(),
                },
                runtime_claims: runtime_claims.clone(),
            };
            validate_manifest_shape_for_publish(&unsigned)?;

            let payload = canonical_manifest_payload(&unsigned)?;
            let key_pem = std::fs::read_to_string(&key)?;
            let signing_key = SigningKey::from_pkcs8_pem(&key_pem).map_err(|e| {
                CliError::State(format!("loading P-256 private key {}: {e}", key.display()))
            })?;
            let signature: Signature = signing_key.sign(&payload);

            let manifest_json = serde_json::json!({
                "schema_version": 1,
                "kind": kind,
                "id": id,
                "name": name,
                "version": version,
                "capabilities": capabilities,
                "artifacts": artifacts.iter().map(|a| serde_json::json!({"path": a.path, "sha256": a.sha256})).collect::<Vec<_>>(),
                "runtime_claims": runtime_claims.iter().map(|c| serde_json::json!({"name": c.name, "family": c.family})).collect::<Vec<_>>(),
                "signature": {
                    "alg": "ES256",
                    "value": URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes()),
                },
            });

            if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &out,
                serde_json::to_string_pretty(&manifest_json)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            let out_dir = out.parent().filter(|p| !p.as_os_str().is_empty());
            if let Some(out_dir) = out_dir {
                std::fs::copy(&skill_md, out_dir.join("SKILL.md"))?;
            }
            println!("published manifest written to {}", out.display());
            println!(
                "hand this file (and its sibling SKILL.md) plus your public key to an installer: \
                 `ardur marketplace install {} --key <public-key.pem>`",
                out.display()
            );
        }
        MarketplaceAction::Validate { manifest, key } => {
            let manifest = validate_manifest(&manifest, &key)?;
            println!(
                "manifest {} ({}) version {} verified",
                manifest.id, manifest.kind, manifest.version
            );
            println!(
                "{} capabilities, {} artifacts, {} runtime claims",
                manifest.capabilities.len(),
                manifest.artifacts.len(),
                manifest.runtime_claims.len()
            );
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

/// `validate_manifest_shape` minus the signature-value-non-empty check (the
/// manifest being built for `publish` has no signature yet).
fn validate_manifest_shape_for_publish(manifest: &CapabilityManifest) -> Result<(), CliError> {
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
    validate_capabilities_bound(manifest)?;
    validate_artifacts_bound(manifest)?;
    validate_runtime_claims(manifest)?;
    Ok(())
}

/// Parse `--claim <name>:<family>` arguments into [`RuntimeClaimRecord`]s,
/// running the same closure-invariant check `validate_runtime_claims` applies
/// at install time so a publisher cannot sign a manifest their own installer
/// would refuse.
fn parse_claim_args(claims: &[String], kind: &str) -> Result<Vec<RuntimeClaimRecord>, CliError> {
    if !claims.is_empty() && kind != "plugin" {
        return Err(CliError::State(
            "--claim requires --kind plugin".to_string(),
        ));
    }
    let mut parsed = Vec::with_capacity(claims.len());
    for raw in claims {
        let (name, family) = raw
            .split_once(':')
            .ok_or_else(|| CliError::State(format!("--claim `{raw}` must be `<name>:<family>`")))?;
        parsed.push(RuntimeClaimRecord {
            name: name.to_string(),
            family: family.to_string(),
        });
    }
    let probe = CapabilityManifest {
        schema_version: 1,
        kind: kind.to_string(),
        id: "probe".to_string(),
        name: "probe".to_string(),
        version: "0.0.0".to_string(),
        capabilities: vec![],
        artifacts: vec![],
        signature: ManifestSignature {
            alg: "ES256".to_string(),
            value: "x".to_string(),
        },
        runtime_claims: parsed.clone(),
    };
    validate_runtime_claims(&probe)?;
    Ok(parsed)
}
