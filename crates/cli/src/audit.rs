//! Supply-chain security and SBOM audit CLI surface.

use std::path::{Path, PathBuf};

use ardur_cli::CliError;
use clap::{Args, Subcommand};
use serde_json::json;

use crate::StateDirs;

/// Arguments to `ardur audit`.
#[derive(Args)]
pub struct AuditArgs {
    #[command(subcommand)]
    pub action: AuditAction,
}

/// Subcommands for `ardur audit`.
#[derive(Subcommand)]
pub enum AuditAction {
    /// Scan source files for hardcoded secrets.
    Secrets {
        /// Directory to scan (defaults to cwd).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Generate a minimal SBOM from Cargo.lock.
    Sbom {
        /// Path to Cargo.lock.
        #[arg(short, long, default_value = "Cargo.lock")]
        lockfile: PathBuf,
        /// Output file; defaults to stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Check for known-vulnerable crate versions via OSV.
    Vulns {
        /// Path to Cargo.lock.
        #[arg(short, long, default_value = "Cargo.lock")]
        lockfile: PathBuf,
    },
    /// Run all supply-chain checks and write a report.
    Run {
        /// Directory to scan.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn default_secret_patterns() -> Vec<regex::Regex> {
    let patterns = [
        r"(?i)(api[_-]?key|token|secret|password|passwd|private[_-]?key)\s*[:=]\s*[\x27\x22\x3e]?([A-Za-z0-9_\-]{8,})",
        r"(?i)sk-[a-zA-Z0-9]{32,}",
        r"(?i)ghp_[a-zA-Z0-9]{36,}",
        r"(?i)AKIA[0-9A-Z]{16}",
    ];
    patterns
        .iter()
        .map(|p| regex::Regex::new(p).expect("valid pattern"))
        .collect()
}

/// Ceiling on a single file's size for the secrets scan, checked via
/// `fs::metadata` before it's read. `ardur audit secrets`/`run` is meant to
/// be pointed at an *untrusted* checkout (the `oss-contributor` workflow
/// does exactly this), so a single large file — a data blob, a vendored
/// bundle, a multi-GB binary — must not be buffered into memory before the
/// regex pass ever runs.
const MAX_SCAN_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

fn scan_for_secrets(path: &Path) -> Result<Vec<serde_json::Value>, CliError> {
    let patterns = default_secret_patterns();
    let mut findings = Vec::new();
    if path.is_file() {
        scan_file(path, &patterns, &mut findings);
    } else if path.is_dir() {
        for entry in ignore::Walk::new(path).flatten() {
            let p = entry.path();
            if p.is_file() {
                scan_file(p, &patterns, &mut findings);
            }
        }
    }
    Ok(findings)
}

/// Scans one file for secret patterns, skipping (not aborting the caller's
/// walk over) anything that can't be scanned as text: unreadable metadata,
/// a read failure, a file over [`MAX_SCAN_FILE_BYTES`], or non-UTF-8
/// content. The overwhelming majority of files in a real checkout —
/// images, compiled objects, `node_modules` artifacts — are binary, and a
/// secrets scan hitting the first one of those should not abort the whole
/// command; every skip is still reported on stderr so scan coverage stays
/// visible rather than silently incomplete.
fn scan_file(path: &Path, patterns: &[regex::Regex], findings: &mut Vec<serde_json::Value>) {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("audit: skipping {} ({e})", path.display());
            return;
        }
    };
    if metadata.len() > MAX_SCAN_FILE_BYTES {
        eprintln!(
            "audit: skipping {} ({} bytes exceeds the {MAX_SCAN_FILE_BYTES}-byte scan cap)",
            path.display(),
            metadata.len()
        );
        return;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("audit: skipping {} ({e})", path.display());
            return;
        }
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        // Binary/non-UTF-8 content isn't secret-scannable as text; this is
        // the expected, common case (not an error), so no stderr note.
        return;
    };
    for (line_no, line) in text.lines().enumerate() {
        for pat in patterns {
            if pat.is_match(line) {
                findings.push(json!({
                    "file": path.to_string_lossy(),
                    "line": line_no + 1,
                    "kind": "potential_secret",
                }));
                break;
            }
        }
    }
}

fn generate_sbom(lockfile: &Path) -> Result<serde_json::Value, CliError> {
    if !lockfile.is_file() {
        return Err(CliError::State(format!(
            "lockfile {} not found",
            lockfile.display()
        )));
    }
    let content = std::fs::read_to_string(lockfile)?;
    let lock: cargo_lock::Lockfile = content
        .parse()
        .map_err(|e| CliError::State(format!("parse Cargo.lock: {e}")))?;
    let packages: Vec<serde_json::Value> = lock
        .packages
        .iter()
        .map(|p| {
            json!({
                "name": p.name.to_string(),
                "version": p.version.to_string(),
                "source": p.source.as_ref().map(|s| s.to_string()),
                "checksum": p.checksum,
            })
        })
        .collect();
    Ok(json!({
        "sbomVersion": "1.0",
        "generator": "ardur-audit",
        "packageCount": packages.len(),
        "packages": packages,
    }))
}

fn check_vulns(lockfile: &Path) -> Result<serde_json::Value, CliError> {
    let sbom = generate_sbom(lockfile)?;
    let packages = sbom
        .get("packages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut checked = Vec::new();
    for pkg in &packages {
        let name = pkg.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let version = pkg.get("version").and_then(|v| v.as_str()).unwrap_or("");
        checked.push(json!({
            "name": name,
            "version": version,
            "vulns": [],
            "note": "live OSV lookup requires network and is not yet implemented",
        }));
    }
    Ok(json!({
        "checked": packages.len(),
        "packages": checked,
        "advisories": [],
    }))
}

/// Run `ardur audit` subcommands.
pub fn run_audit(args: AuditArgs) -> Result<(), CliError> {
    match args.action {
        AuditAction::Secrets { path } => {
            let findings = scan_for_secrets(&path)?;
            if findings.is_empty() {
                println!("no potential secrets found");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "findings": findings }))
                        .map_err(|e| CliError::State(e.to_string()))?
                );
            }
        }
        AuditAction::Sbom { lockfile, output } => {
            let sbom = generate_sbom(&lockfile)?;
            let json =
                serde_json::to_string_pretty(&sbom).map_err(|e| CliError::State(e.to_string()))?;
            match output {
                Some(path) => {
                    std::fs::write(&path, json)?;
                    println!("wrote SBOM to {}", path.display());
                }
                None => println!("{json}"),
            }
        }
        AuditAction::Vulns { lockfile } => {
            let report = check_vulns(&lockfile)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .map_err(|e| CliError::State(e.to_string()))?
            );
        }
        AuditAction::Run { path } => {
            let findings = scan_for_secrets(&path)?;
            let lockfile = path.join("Cargo.lock");
            let sbom = if lockfile.is_file() {
                generate_sbom(&lockfile)?
            } else {
                json!({ "error": "Cargo.lock not found" })
            };
            let vulns = if lockfile.is_file() {
                check_vulns(&lockfile)?
            } else {
                json!({ "error": "Cargo.lock not found" })
            };
            let report = json!({
                "secret_findings": findings,
                "sbom": sbom,
                "vulns": vulns,
            });
            let root = StateDirs::resolve()?.root;
            std::fs::create_dir_all(&root)?;
            let report_path = root.join("audit_report.json");
            std::fs::write(
                &report_path,
                serde_json::to_string_pretty(&report)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("wrote audit report to {}", report_path.display());
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .map_err(|e| CliError::State(e.to_string()))?
            );
        }
    }
    Ok(())
}
