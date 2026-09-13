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

/// Resolve `.` and `..` in a path textually, without touching the filesystem.
///
/// Used to judge a symlink's target, which may not exist yet and therefore
/// cannot be canonicalized. Purely lexical normalisation is sound here because
/// the result is only compared against an already-canonical root: a target that
/// normalises outside the root cannot be brought back inside it by a link the
/// check has not yet followed, and any intermediate link on the *existing*
/// portion of the path is caught by the canonicalize pass that follows.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Popping is correct for a lexical view; `/a/../b` is `/b`.
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the deepest existing ancestor of `path` and re-attach the rest.
///
/// A symlink's stored target may name a root by a non-canonical spelling — on
/// macOS `/var/folders/...` is really `/private/var/folders/...` — so comparing
/// it against an already-canonical root would reject links that never leave the
/// tree. Canonicalizing what exists resolves that, while the lexically
/// normalised tail keeps the comparison meaningful for a target that does not
/// exist yet.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut probe: &Path = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (probe.file_name(), probe.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                probe = parent;
            }
            // Nothing on this path exists; the lexical form is the best answer.
            _ => return path.to_path_buf(),
        }
    }
}

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
    //
    // A *dangling* symlink is the subtle case. `canonicalize` resolves the link
    // and then fails because its target does not exist, which is
    // indistinguishable here from "this path simply does not exist yet". Walking
    // up to the parent would then approve the link's in-vault directory, and the
    // subsequent open would follow the link and create the target outside the
    // root. So check for a symlink explicitly before falling back: a path that
    // *is* a link must resolve inside the root, whether or not its target
    // currently exists.
    let mut probe: &Path = &joined;
    loop {
        // `symlink_metadata` does not follow the link, so this is true exactly
        // when `probe` is itself a symlink — including a dangling one.
        if probe
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            let target = probe.read_link().map_err(|e| {
                ToolError::ExecutionFailed(format!("cannot read symlink `{rel}`: {e}"))
            })?;
            // A relative link resolves against the link's own directory.
            let resolved = if target.is_absolute() {
                target
            } else {
                probe.parent().unwrap_or(&canonical_root).join(target)
            };
            // The target may not exist, so normalise `.` and `..` lexically
            // rather than canonicalizing. The link's *stored* target can be
            // written against a non-canonical spelling of the root (on macOS a
            // vault under `/var/...` is really `/private/var/...`), so
            // canonicalize the deepest existing ancestor of the target and
            // rebuild the remainder on top of it before comparing.
            let normalized = canonicalize_existing_prefix(&normalize_lexically(&resolved));
            if !normalized.starts_with(&canonical_root) {
                return Err(ToolError::Denied {
                    reason: format!("path resolves outside the tool root via a symlink: `{rel}`"),
                });
            }
        }

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
    /// gh#413: where prior content goes before a write destroys it. `None`
    /// keeps the pre-existing behaviour, so a deployment that has not opted in
    /// writes exactly as before.
    snapshots: Option<crate::snapshot::SnapshotStore>,
    /// gh#414: checkers run over the content after a write. Advisory — the
    /// bytes are already on disk, so problems are reported, never raised as a
    /// failed call.
    diagnostics: Option<crate::diagnostics::SyntaxCheckers>,
}

impl WriteFileTool {
    /// The id [`WriteFileTool`] registers under.
    pub const ID: &'static str = "file.write";

    /// Run `checkers` over the content after each write (gh#414).
    ///
    /// Advisory by construction: a checker runs after the bytes are on disk,
    /// so returning an error would report a failed write that succeeded — and
    /// a model retrying on that error would write the content twice.
    #[must_use]
    pub fn with_diagnostics(mut self, checkers: crate::diagnostics::SyntaxCheckers) -> Self {
        self.diagnostics = Some(checkers);
        self
    }

    /// Capture prior file content into `store` before each write (gh#413).
    ///
    /// Opt-in: without it the tool behaves exactly as before, so enabling
    /// snapshots is a deployment decision rather than a silent change in what
    /// the agent writes to disk.
    #[must_use]
    pub fn with_snapshots(mut self, store: crate::snapshot::SnapshotStore) -> Self {
        self.snapshots = Some(store);
        self
    }

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
            snapshots: None,
            diagnostics: None,
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

        // gh#413: capture what is about to be lost, BEFORE the write. Taking
        // it afterwards would capture the new content, which is exactly the
        // thing that is not worth keeping.
        //
        // A capture failure fails the write. The alternative — proceed and
        // return a warning — destroys unrecoverable content in the one case
        // the snapshot exists to protect, and the caller asked for snapshots
        // by opting in.
        let snapshot = match &self.snapshots {
            Some(store) => Some(store.capture(&path).await.map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "could not snapshot the prior content of `{}`, so the write \
                     was not attempted: {e}",
                    args.path
                ))
            })?),
            None => None,
        };

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

        let mut out = output(json!({
            "bytes_written": bytes.len(),
            "path_written": path.display().to_string(),
        }));

        // The snapshot id rides in `receipt_data`. NOTE: the runtime currently
        // builds its `ToolCallReceipt` unconditionally and never reads this
        // field, so the snapshot is recorded here but is NOT yet linked into
        // the receipt chain. Populating it now means the link becomes real the
        // moment the runtime consumes it; claiming the link exists today would
        // be false.
        if let Some(snapshot) = &snapshot {
            // MERGE, do not replace: `output` already put bytes_written and
            // path_written here, and a consumer that gets only a content hash
            // cannot say which file the snapshot belongs to — which is the
            // audit question.
            let snapshot_json = json!({
                "snapshot": match snapshot {
                    crate::snapshot::Snapshot::Captured(id) => json!({
                        "prior_content": id.as_str(),
                    }),
                    crate::snapshot::Snapshot::NothingToCapture => json!({
                        "prior_content": serde_json::Value::Null,
                        "note": "the file did not exist; undoing this write means removing it",
                    }),
                },
            });
            if let (Some(base), Some(extra)) =
                (out.receipt_data.as_object_mut(), snapshot_json.as_object())
            {
                for (k, v) in extra {
                    base.insert(k.clone(), v.clone());
                }
            } else {
                out.receipt_data = snapshot_json;
            }
        }
        // gh#414: check what was just written. The bytes are already on disk,
        // so this NEVER fails the call — problems ride along in the output.
        if let Some(checkers) = &self.diagnostics {
            // Check what is ON DISK, not what was passed in. For an overwrite
            // those are the same string, but for an append `args.content` is
            // only the added chunk — checking that in isolation reports
            // syntax errors for perfectly good appends (a fragment rarely
            // parses alone) and misses breakage the append actually caused.
            let to_check = if append {
                match tokio::fs::metadata(&path).await {
                    Ok(m) if m.len() <= crate::diagnostics::MAX_CHECK_BYTES => {
                        tokio::fs::read_to_string(&path).await.ok()
                    }
                    // Too large, or unstattable: skip rather than buffer it.
                    _ => None,
                }
            } else {
                Some(args.content.clone())
            };

            let report = to_check
                .as_deref()
                .map(|c| checkers.run(&path, c))
                .unwrap_or_else(crate::diagnostics::DiagnosticReport::not_checked);
            if let Some(obj) = out.content.as_object_mut() {
                // `checked` distinguishes "nothing understood this file" from
                // "checked and clean". Collapsing them would let an unchecked
                // file read as validated.
                obj.insert("checked".to_string(), json!(report.was_checked()));
                if !report.is_empty() {
                    obj.insert("diagnostics".to_string(), report.to_json());
                    if report.truncated() > 0 {
                        obj.insert(
                            "diagnostics_truncated".to_string(),
                            json!(report.truncated()),
                        );
                    }
                }
            }
        }

        Ok(out)
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
