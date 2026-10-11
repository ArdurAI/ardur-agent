//! Synced, redacted JSON lines. A truncated final line is recoverable after a crash.

use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

/// An append-only transcript created without replacing existing evidence.
pub struct Transcript {
    file: File,
}
impl Transcript {
    /// Create a new transcript. Random file names keep scenario ids out of paths.
    pub fn create(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file =
            options.open(dir.join(format!("{}.jsonl", home_client::fresh_client_nonce())))?;
        // Persist the directory entry before any remote work can start.
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(Self { file })
    }
    /// Redact fields before serialization, append a complete line, and sync it.
    pub fn append(&mut self, entry: Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(&home_client::redact(entry))?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()
    }
    /// Read durable entries, ignoring only an unterminated final line.
    /// A request entry without admission can be replayed with its original nonce
    /// and source scenario; an admission can be resumed with the existing wait command.
    pub fn recover(path: &Path) -> io::Result<Vec<Value>> {
        let mut input = BufReader::new(File::open(path)?);
        let mut entries = Vec::new();
        loop {
            let mut line = Vec::new();
            if input.read_until(b'\n', &mut line)? == 0 || !line.ends_with(b"\n") {
                break;
            }
            entries.push(home_client::redact(serde_json::from_slice(&line)?));
        }
        Ok(entries)
    }
}
