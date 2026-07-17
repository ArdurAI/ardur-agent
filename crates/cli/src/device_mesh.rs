//! Device mesh and companion app control-plane CLI.

use std::path::{Path, PathBuf};

use ardur_cli::{CliError, read_string_no_follow, write_private_file_atomic_no_follow};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::StateDirs;

const DEFAULT_STALE_AFTER_SECS: u64 = 300;

/// Arguments to `ardur nodes`.
#[derive(Args)]
pub struct NodesArgs {
    #[command(subcommand)]
    pub action: NodesAction,
}

/// Subcommands for paired companion devices and mesh routing.
#[derive(Subcommand)]
pub enum NodesAction {
    /// Print control-plane status as JSON.
    Status,
    /// Pair a device identity as a pending principal.
    Pair {
        /// Stable device id from the companion app.
        device_id: String,
        /// Platform label, e.g. macos, ios, android, browser.
        #[arg(long)]
        platform: String,
        /// Capability grant. Repeat for multiple capabilities.
        #[arg(long = "cap")]
        capabilities: Vec<String>,
        /// Trust tier for this companion.
        #[arg(long, default_value = "companion")]
        trust_tier: String,
        /// Pairing token TTL in seconds.
        #[arg(long, default_value_t = 3600)]
        ttl_seconds: u64,
    },
    /// Approve a pending/known device.
    Approve {
        /// Device id.
        device_id: String,
    },
    /// Revoke a paired device.
    Revoke {
        /// Device id.
        device_id: String,
    },
    /// Update last-seen heartbeat for an approved node.
    Heartbeat {
        /// Device id.
        device_id: String,
    },
    /// Route a tool request to a paired device and write a receipt.
    RouteTool {
        /// Device id.
        device_id: String,
        /// Tool/action name to route.
        tool: String,
        /// Capability required for the route.
        #[arg(long)]
        capability: String,
        /// Receipt output path.
        #[arg(long)]
        receipt: PathBuf,
        /// Allow stale/offline companions to produce an offline-fallback receipt.
        #[arg(long)]
        offline_ok: bool,
        /// Node staleness threshold.
        #[arg(long, default_value_t = DEFAULT_STALE_AFTER_SECS)]
        stale_after_secs: u64,
    },
    /// Toggle emergency stop for all mesh routing.
    EmergencyStop {
        /// Enable emergency stop.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Disable emergency stop.
        #[arg(long)]
        disable: bool,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceMeshState {
    #[serde(default)]
    emergency_stop: bool,
    #[serde(default)]
    devices: Vec<DeviceIdentity>,
    #[serde(default)]
    sessions: Vec<MeshSession>,
    #[serde(default)]
    receipts: Vec<RouteReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceIdentity {
    id: String,
    platform: String,
    capabilities: Vec<String>,
    trust_tier: String,
    status: DeviceStatus,
    paired_at: u64,
    approved_at: Option<u64>,
    revoked_at: Option<u64>,
    last_seen_at: Option<u64>,
    token_expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DeviceStatus {
    Pending,
    Approved,
    Revoked,
}

#[derive(Debug, Serialize, Deserialize)]
struct MeshSession {
    id: String,
    device_id: String,
    started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouteReceipt {
    id: String,
    device_id: String,
    tool: String,
    capability: String,
    status: RouteStatus,
    reason: Option<String>,
    routed_at: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RouteStatus {
    Routed,
    OfflineFallback,
}

fn mesh_path(root: &Path) -> PathBuf {
    root.join("device-mesh.json")
}

fn load_state(path: &Path) -> Result<DeviceMeshState, CliError> {
    let contents = match read_string_no_follow(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeviceMeshState::default());
        }
        Err(e) => return Err(CliError::Io(e)),
    };
    serde_json::from_str(&contents)
        .map_err(|e| CliError::State(format!("invalid device mesh state {}: {e}", path.display())))
}

fn save_state(path: &Path, state: &DeviceMeshState) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_file_atomic_no_follow(
        path,
        serde_json::to_string_pretty(state)
            .map_err(|e| CliError::State(e.to_string()))?
            .as_bytes(),
    )?;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn validate_nonempty(field: &str, value: &str) -> Result<(), CliError> {
    if value.trim().is_empty() {
        return Err(CliError::State(format!("{field} must not be empty")));
    }
    Ok(())
}

fn device_mut<'a>(
    state: &'a mut DeviceMeshState,
    device_id: &str,
) -> Result<&'a mut DeviceIdentity, CliError> {
    state
        .devices
        .iter_mut()
        .find(|device| device.id == device_id)
        .ok_or_else(|| CliError::State(format!("device `{device_id}` not found")))
}

fn device<'a>(state: &'a DeviceMeshState, device_id: &str) -> Result<&'a DeviceIdentity, CliError> {
    state
        .devices
        .iter()
        .find(|device| device.id == device_id)
        .ok_or_else(|| CliError::State(format!("device `{device_id}` not found")))
}

fn ensure_route_allowed(
    state: &DeviceMeshState,
    candidate: &DeviceIdentity,
    capability: &str,
    stale_after_secs: u64,
    offline_ok: bool,
) -> Result<RouteStatus, CliError> {
    if state.emergency_stop {
        return Err(CliError::State(
            "device mesh emergency stop is active".into(),
        ));
    }
    if candidate.status == DeviceStatus::Revoked {
        return Err(CliError::State(format!(
            "device `{}` is revoked",
            candidate.id
        )));
    }
    if candidate.status != DeviceStatus::Approved {
        return Err(CliError::State(format!(
            "device `{}` is not approved",
            candidate.id
        )));
    }
    let now = now_secs();
    if candidate.token_expires_at <= now {
        return Err(CliError::State(format!(
            "device `{}` pairing token expired",
            candidate.id
        )));
    }
    if !candidate
        .capabilities
        .iter()
        .any(|grant| grant == capability || grant == "*")
    {
        return Err(CliError::State(format!(
            "device `{}` lacks capability `{capability}`",
            candidate.id
        )));
    }
    let last_seen = candidate.last_seen_at.unwrap_or(candidate.paired_at);
    if now.saturating_sub(last_seen) >= stale_after_secs {
        if offline_ok {
            return Ok(RouteStatus::OfflineFallback);
        }
        return Err(CliError::State(format!(
            "device `{}` is stale/offline",
            candidate.id
        )));
    }
    Ok(RouteStatus::Routed)
}

fn write_receipt(path: &Path, receipt: &RouteReceipt) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(receipt).map_err(|e| CliError::State(e.to_string()))?,
    )?;
    Ok(())
}

/// Run `ardur nodes` subcommands.
pub fn run_nodes(args: NodesArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let path = mesh_path(&root);
    let mut state = load_state(&path)?;

    match args.action {
        NodesAction::Status => {
            println!(
                "{}",
                serde_json::to_string_pretty(&state).map_err(|e| CliError::State(e.to_string()))?
            );
        }
        NodesAction::Pair {
            device_id,
            platform,
            capabilities,
            trust_tier,
            ttl_seconds,
        } => {
            validate_nonempty("device_id", &device_id)?;
            validate_nonempty("platform", &platform)?;
            validate_nonempty("trust_tier", &trust_tier)?;
            if capabilities.is_empty() {
                return Err(CliError::State(
                    "at least one --cap grant is required".into(),
                ));
            }
            for capability in &capabilities {
                validate_nonempty("capability", capability)?;
            }
            let now = now_secs();
            let identity = DeviceIdentity {
                id: device_id.clone(),
                platform,
                capabilities,
                trust_tier,
                status: DeviceStatus::Pending,
                paired_at: now,
                approved_at: None,
                revoked_at: None,
                last_seen_at: Some(now),
                token_expires_at: now.saturating_add(ttl_seconds),
            };
            match state
                .devices
                .iter_mut()
                .find(|device| device.id == device_id)
            {
                Some(existing) => *existing = identity,
                None => state.devices.push(identity),
            }
            save_state(&path, &state)?;
            println!("paired device {device_id} pending approval");
        }
        NodesAction::Approve { device_id } => {
            let device = device_mut(&mut state, &device_id)?;
            if device.status == DeviceStatus::Revoked {
                return Err(CliError::State(format!(
                    "device `{device_id}` is revoked and must be paired again"
                )));
            }
            let now = now_secs();
            device.status = DeviceStatus::Approved;
            device.approved_at = Some(now);
            device.last_seen_at = Some(now);
            state.sessions.push(MeshSession {
                id: uuid::Uuid::now_v7().to_string(),
                device_id: device_id.clone(),
                started_at: now,
            });
            save_state(&path, &state)?;
            println!("approved device {device_id}");
        }
        NodesAction::Revoke { device_id } => {
            let device = device_mut(&mut state, &device_id)?;
            device.status = DeviceStatus::Revoked;
            device.revoked_at = Some(now_secs());
            save_state(&path, &state)?;
            println!("revoked device {device_id}");
        }
        NodesAction::Heartbeat { device_id } => {
            let device = device_mut(&mut state, &device_id)?;
            if device.status != DeviceStatus::Approved {
                return Err(CliError::State(format!(
                    "device `{device_id}` is not approved"
                )));
            }
            device.last_seen_at = Some(now_secs());
            save_state(&path, &state)?;
            println!("heartbeat accepted for {device_id}");
        }
        NodesAction::RouteTool {
            device_id,
            tool,
            capability,
            receipt,
            offline_ok,
            stale_after_secs,
        } => {
            validate_nonempty("tool", &tool)?;
            validate_nonempty("capability", &capability)?;
            let candidate = device(&state, &device_id)?;
            let status =
                ensure_route_allowed(&state, candidate, &capability, stale_after_secs, offline_ok)?;
            let route_receipt = RouteReceipt {
                id: uuid::Uuid::now_v7().to_string(),
                device_id: device_id.clone(),
                tool,
                capability,
                status,
                reason: match status {
                    RouteStatus::Routed => None,
                    RouteStatus::OfflineFallback => {
                        Some("device stale; offline fallback selected".into())
                    }
                },
                routed_at: now_secs(),
            };
            write_receipt(&receipt, &route_receipt)?;
            state.receipts.push(route_receipt.clone());
            save_state(&path, &state)?;
            println!("route receipt {}", route_receipt.id);
        }
        NodesAction::EmergencyStop { enable, disable } => {
            if !enable && !disable {
                return Err(CliError::State("pass --enable or --disable".into()));
            }
            state.emergency_stop = enable && !disable;
            save_state(&path, &state)?;
            println!("emergency_stop={}", state.emergency_stop);
        }
    }

    Ok(())
}
