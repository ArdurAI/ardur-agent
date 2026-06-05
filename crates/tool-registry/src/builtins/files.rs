//! Root-confined filesystem tools: [`ReadFileTool`], [`WriteFileTool`], and
//! [`ListDirTool`].
//!
//! Every tool here is constructed with a root directory via `with_root` and may
//! only touch paths *inside* that root. There is deliberately no constructor
//! that grants the whole filesystem — a path is resolved relative to the root,
//! absolute paths and `..` traversal are refused, and the resolved path's
//! nearest existing ancestor is canonicalized and checked to still sit under the
//! (canonicalized) root, which also catches symlink escapes.
//!
//! The containment check has an inherent TOCTOU gap: a symlink swapped between
//! the check and the subsequent open could still redirect outside the root.
//! These tools are a convenience boundary, not a security sandbox; pair them
//! with the §11 capability + Cedar layers when the prompt is untrusted.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// Default read ceiling: 64 KiB.
const DEFAULT_MAX_BYTES: usize = 64 * 1024;
/// Default directory-listing ceiling.
const DEFAULT_MAX_ENTRIES: usize = 100;

/// Resolve `rel` against `root` and confirm the result stays inside `root`.
///
/// Refuses absolute inputs and any `..` (or other non-`Normal`/`CurDir`)
/// component up front, then canonicalizes `root` and walks up from the joined
/// path to its nearest existing ancestor, verifying that ancestor still lies
/// under the canonical root. The returned path is `canonical_root / rel`, safe
/// to hand to a filesystem call.
fn contained_path(root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let rel_path = Path::new(rel);

    if rel_path.is_absolute() {
        return Err(ToolError::Denied {
            reason: format!("absolute paths are not permitted: `{rel}`"),
        });
    }
    for component in rel_path.components() {
        match component {
            // The only components that keep a path inside the root.
            Component::Normal(_) | Component::CurDir => {}
            // ParentDir, RootDir, and Prefix can all walk out.
            _ => {
                return Err(ToolError::Denied {
                    reason: format!("path escapes the tool root: `{rel}`"),
                });
            }
        }
    }

    let canonical_root = root.canonicalize().map_err(|e| {
        ToolError::ExecutionFailed(format!(
            "tool root `{}` is unavailable: {e}",
            root.display()
        ))
    })?;
    let joined = canonical_root.join(rel_path);

    // Defence in depth against symlink escapes: canonicalize the nearest
    // existing ancestor and confirm it is still under the root. (The target
    // itself may not exist yet — e.g. a file about to be written.)
    let mut probe: &Path = &joined;
    loop {
        match probe.canonicalize() {
            Ok(real) => {
                if !real.starts_with(&canonical_root) {
                    return Err(ToolError::Denied {
                        reason: format!("path resolves outside the tool root: `{rel}`"),
                    });
                }
                break;
            }
            Err(_) => match probe.parent() {
                Some(parent) => probe = parent,
                None => break,
            },
        }
    }

    Ok(joined)
}

/// Build the standard `{ content, cost, receipt_data }` output where the receipt
/// mirrors the content.
fn output(content: serde_json::Value) -> ToolOutput {
    ToolOutput {
        content: content.clone(),
        cost: CostTuple::default(),
        receipt_data: content,
    }
}

// ── file.read ──────────────────────────────────────────────────────────────

/// Arguments to a `file.read` invocation.
#[derive(Deserialize)]
struct ReadArgs {
    /// Root-relative path to read.
    path: String,
    /// Byte ceiling; the content is truncated past it.
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
}

fn default_max_bytes() -> usize {
    DEFAULT_MAX_BYTES
}

/// Reads a file inside the tool root, returning `{ content, bytes_read,
/// truncated }`.
pub struct ReadFileTool {
    schema: ToolSchema,
    root: PathBuf,
    caps: Vec<Capability>,
}

impl ReadFileTool {
    /// The id [`ReadFileTool`] registers under.
    pub const ID: &'static str = "file.read";

    /// A [`ReadFileTool`] confined to `root`. Paths are resolved relative to it
    /// and may not escape it.
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        let schema = ToolSchema {
            description: "Read a file, relative to the tool root. Returns its content (UTF-8 \
                          lossy), bytes read, and whether it was truncated."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Root-relative file path." },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Read ceiling in bytes (default 65536).",
                        "minimum": 0
                    }
                },
                "required": ["path"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "bytes_read": { "type": "integer" },
                    "truncated": { "type": "boolean" }
                },
                "required": ["content", "bytes_read", "truncated"]
            }),
            examples: vec![],
        };
        Self {
            schema,
            root,
            caps: vec![Capability::FsRead],
        }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn id(&self) -> ToolId {
        ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let path = contained_path(&self.root, &args.path)?;

        let data = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("read `{}`: {e}", args.path)))?;
        let truncated = data.len() > args.max_bytes;
        let slice = &data[..data.len().min(args.max_bytes)];

        Ok(output(json!({
            "content": String::from_utf8_lossy(slice),
            "bytes_read": slice.len(),
            "truncated": truncated,
        })))
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}

// ── file.write ─────────────────────────────────────────────────────────────

/// Arguments to a `file.write` invocation.
#[derive(Deserialize)]
struct WriteArgs {
    /// Root-relative path to write.
    path: String,
    /// The bytes to write.
    content: String,
    /// `"overwrite"` (default) or `"append"`.
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_mode() -> String {
    "overwrite".to_string()
}

/// Writes a file inside the tool root, creating parent directories as needed.
/// Returns `{ bytes_written, path_written }`.
pub struct WriteFileTool {
    schema: ToolSchema,
    root: PathBuf,
    caps: Vec<Capability>,
}

impl WriteFileTool {
    /// The id [`WriteFileTool`] registers under.
    pub const ID: &'static str = "file.write";

    /// A [`WriteFileTool`] confined to `root`.
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        let schema = ToolSchema {
            description: "Write a file, relative to the tool root, creating parent directories. \
                          `mode` is \"overwrite\" (default) or \"append\"."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Root-relative file path." },
                    "content": { "type": "string", "description": "Bytes to write." },
                    "mode": {
                        "type": "string",
                        "enum": ["overwrite", "append"],
                        "description": "Write mode (default overwrite)."
                    }
                },
                "required": ["path", "content"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "bytes_written": { "type": "integer" },
                    "path_written": { "type": "string" }
                },
                "required": ["bytes_written", "path_written"]
            }),
            examples: vec![],
        };
        Self {
            schema,
            root,
            caps: vec![Capability::FsWrite],
        }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn id(&self) -> ToolId {
        ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: WriteArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let append = match args.mode.as_str() {
            "overwrite" => false,
            "append" => true,
            other => {
                return Err(ToolError::InvalidArgs(format!(
                    "`mode` must be \"overwrite\" or \"append\", got `{other}`"
                )));
            }
        };

        let path = contained_path(&self.root, &args.path)?;
        let bytes = args.content.as_bytes();

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("create parent of `{}`: {e}", args.path))
            })?;
        }

        if append {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("open `{}`: {e}", args.path)))?;
            file.write_all(bytes)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("append `{}`: {e}", args.path)))?;
            // `tokio::fs::File` buffers internally and does not guarantee a flush
            // on drop, so push the bytes through before returning — otherwise a
            // caller that immediately reads the file can miss the append.
            file.flush()
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("flush `{}`: {e}", args.path)))?;
        } else {
            tokio::fs::write(&path, bytes)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("write `{}`: {e}", args.path)))?;
        }

        Ok(output(json!({
            "bytes_written": bytes.len(),
            "path_written": path.display().to_string(),
        })))
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}

// ── file.list ──────────────────────────────────────────────────────────────

/// Arguments to a `file.list` invocation.
#[derive(Deserialize)]
struct ListArgs {
    /// Root-relative directory to list.
    path: String,
    /// Entry ceiling; the listing is truncated past it.
    #[serde(default = "default_max_entries")]
    max_entries: usize,
}

fn default_max_entries() -> usize {
    DEFAULT_MAX_ENTRIES
}

/// Lists a directory inside the tool root, returning `{ entries, truncated }`
/// where each entry is `{ name, is_dir, size_bytes }`.
pub struct ListDirTool {
    schema: ToolSchema,
    root: PathBuf,
    caps: Vec<Capability>,
}

impl ListDirTool {
    /// The id [`ListDirTool`] registers under.
    pub const ID: &'static str = "file.list";

    /// A [`ListDirTool`] confined to `root`.
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        let schema = ToolSchema {
            description: "List a directory, relative to the tool root. Returns its entries \
                          (name, is_dir, size_bytes) and whether the listing was truncated."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Root-relative directory path." },
                    "max_entries": {
                        "type": "integer",
                        "description": "Entry ceiling (default 100).",
                        "minimum": 0
                    }
                },
                "required": ["path"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "is_dir": { "type": "boolean" },
                                "size_bytes": { "type": "integer" }
                            },
                            "required": ["name", "is_dir", "size_bytes"]
                        }
                    },
                    "truncated": { "type": "boolean" }
                },
                "required": ["entries", "truncated"]
            }),
            examples: vec![],
        };
        Self {
            schema,
            root,
            caps: vec![Capability::FsRead],
        }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn id(&self) -> ToolId {
        ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: ListArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let path = contained_path(&self.root, &args.path)?;

        let mut read_dir = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("list `{}`: {e}", args.path)))?;

        let mut entries = Vec::new();
        let mut truncated = false;
        loop {
            let next = read_dir
                .next_entry()
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("list `{}`: {e}", args.path)))?;
            let Some(entry) = next else { break };
            if entries.len() >= args.max_entries {
                truncated = true;
                break;
            }
            // A metadata read can fail (e.g. a dangling symlink); fall back to
            // not-a-dir / zero-size rather than failing the whole listing.
            let metadata = entry.metadata().await.ok();
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "is_dir": metadata.as_ref().is_some_and(std::fs::Metadata::is_dir),
                "size_bytes": metadata.as_ref().map_or(0, std::fs::Metadata::len),
            }));
        }

        Ok(output(json!({
            "entries": entries,
            "truncated": truncated,
        })))
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}
