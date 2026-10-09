//! Append-only JSONL transcript log.
//!
//! Every conversation message and harness event is serialised as one JSON
//! line and appended to the session transcript file.  This gives a complete
//! audit trail that can be replayed or inspected after the fact.
//!
//! # File location
//!
//! By default each session gets its own file under a configurable directory:
//! `<dir>/<session_id>.jsonl`.  The caller supplies both the directory and a
//! session id.  The directory is created automatically on first write.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::events::HarnessEvent;
use crate::memory::Message;

// ── TranscriptEntry ───────────────────────────────────────────────────────────

/// One line written to the transcript.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// A message appended to conversation memory.
    Message {
        #[serde(flatten)]
        message: Message,
    },
    /// A harness event emitted to the frontend.
    Event {
        #[serde(flatten)]
        event: HarnessEvent,
    },
}

// ── Transcript ────────────────────────────────────────────────────────────────

/// An append-only JSONL transcript for one session.
///
/// Cheap to clone — the inner writer is wrapped in an `Arc<Mutex<…>>`.
/// Call [`Transcript::log_message`] or [`Transcript::log_event`] from
/// anywhere in the loop; the file is flushed after each entry.
pub struct Transcript {
    path: PathBuf,
    writer: tokio::sync::Mutex<BufWriter<File>>,
}

impl Transcript {
    /// Open (or create) the transcript file at `path`.
    ///
    /// The parent directory must exist; call [`Transcript::open_in`] to
    /// create it automatically.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("could not open transcript at {}", path.display()))?;
        Ok(Self {
            path,
            writer: tokio::sync::Mutex::new(BufWriter::new(file)),
        })
    }

    /// Create the directory at `dir` if needed, then open
    /// `<dir>/<session_id>.jsonl`.
    pub async fn open_in(dir: impl AsRef<Path>, session_id: &str) -> Result<Self> {
        let dir = dir.as_ref();
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("could not create transcript dir {}", dir.display()))?;
        let path = dir.join(format!("{session_id}.jsonl"));
        Self::open(path).await
    }

    /// Return the path of the underlying file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a message to the transcript.
    pub async fn log_message(&self, message: &Message) -> Result<()> {
        let entry = TranscriptEntry::Message {
            message: message.clone(),
        };
        self.write_entry(&entry).await
    }

    /// Append a harness event to the transcript.
    pub async fn log_event(&self, event: &HarnessEvent) -> Result<()> {
        let entry = TranscriptEntry::Event {
            event: event.clone(),
        };
        self.write_entry(&entry).await
    }

    /// Serialise `entry` as a single JSON line, terminated by `\n`, and
    /// flush to disk.
    async fn write_entry(&self, entry: &TranscriptEntry) -> Result<()> {
        let line = serde_json::to_string(entry).context("could not serialise transcript entry")?;
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes())
            .await
            .context("transcript write failed")?;
        w.write_all(b"\n")
            .await
            .context("transcript newline write failed")?;
        w.flush().await.context("transcript flush failed")?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::HarnessEvent;
    use crate::memory::user_message;
    use tempfile::TempDir;

    #[tokio::test]
    async fn writes_readable_jsonl() {
        let dir = TempDir::new().unwrap();
        let t = Transcript::open_in(dir.path(), "sess-1").await.unwrap();

        t.log_message(&user_message("hello")).await.unwrap();
        t.log_event(&HarnessEvent::TextDelta { delta: "hi".into() })
            .await
            .unwrap();

        let raw = tokio::fs::read_to_string(t.path()).await.unwrap();
        let lines: Vec<&str> = raw.trim().lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 JSONL lines");

        // First line must be valid JSON containing the message kind.
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["kind"], "message");

        let v2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v2["kind"], "event");
    }

    #[tokio::test]
    async fn creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("deep").join("nested");
        // Should not exist yet.
        assert!(!nested.exists());
        let t = Transcript::open_in(&nested, "test").await.unwrap();
        assert!(t.path().exists());
    }

    #[tokio::test]
    async fn appends_across_reopens() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sess.jsonl");

        {
            let t = Transcript::open(&path).await.unwrap();
            t.log_message(&user_message("first")).await.unwrap();
        }
        {
            let t = Transcript::open(&path).await.unwrap();
            t.log_message(&user_message("second")).await.unwrap();
        }

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(raw.lines().count(), 2);
    }
}
