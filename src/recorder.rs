//! Flight Recorder: out-of-band forensic trail. While the LLM payloads stay
//! pruned and cheap, the full trail (prompts, raw responses, routing
//! decisions) is written to a local content-addressable store for human
//! debugging.
//!
//! Blobs are SHA-256 content-addressed (Git-plumbing style) so repeated
//! contexts deduplicate to a single object on disk.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// One flight-recorder entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub task_id: String,
    /// decision | prompt | response | error | verify
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub blob_sha: String,
    pub ts: DateTime<Utc>,
}

/// Recorder writes trace events under a base directory.
pub struct Recorder {
    base: PathBuf,
}

/// Canonical trace directory: $TOKENOS_TRACES or ~/.local/state/tokenos/traces.
pub fn default_dir() -> PathBuf {
    if let Ok(p) = std::env::var("TOKENOS_TRACES") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    match dirs::home_dir() {
        Some(home) => home.join(".local").join("state").join("tokenos").join("traces"),
        None => PathBuf::from(".tokenos-traces"),
    }
}

impl Recorder {
    /// Create a Recorder rooted at dir (None = default_dir()).
    pub fn new(dir: Option<&Path>) -> Result<Self> {
        let base = match dir {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => default_dir(),
        };
        fs::create_dir_all(base.join("objects"))?;
        Ok(Self { base })
    }

    /// Store content-addressed payload bytes, returning the SHA-256 hex.
    /// Identical payloads write once (deduplication).
    fn put_blob(&self, data: &[u8]) -> Result<String> {
        let sha = hex::encode(Sha256::digest(data));
        let dir = self.base.join("objects").join(&sha[..2]);
        let path = dir.join(&sha[2..]);
        if path.exists() {
            return Ok(sha); // already stored
        }
        fs::create_dir_all(&dir)?;
        fs::write(&path, data)?;
        Ok(sha)
    }

    /// Write a full payload blob plus an index line into the per-task
    /// journal (NDJSON, append-only). Returns the blob SHA (empty if no payload).
    pub fn record(&self, task_id: &str, kind: &str, summary: &str, payload: &[u8]) -> Result<String> {
        let sha = if payload.is_empty() {
            String::new()
        } else {
            self.put_blob(payload)?
        };
        let ev = Event {
            task_id: task_id.to_string(),
            kind: kind.to_string(),
            summary: summary.to_string(),
            blob_sha: sha.clone(),
            ts: Utc::now(),
        };
        let line = serde_json::to_string(&ev)?;
        let journal = self.base.join(format!("{}.ndjson", sanitize(task_id)));
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&journal)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(sha)
    }

    /// Replay the journal for a task.
    pub fn events(&self, task_id: &str) -> Result<Vec<Event>> {
        let journal = self.base.join(format!("{}.ndjson", sanitize(task_id)));
        let data = match fs::read_to_string(&journal) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for line in data.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<Event>(line) {
                out.push(ev);
            }
        }
        Ok(out)
    }

    /// Fetch a stored payload by SHA.
    pub fn blob(&self, sha: &str) -> Result<Vec<u8>> {
        if sha.len() < 3 {
            return Err(anyhow!("invalid sha"));
        }
        Ok(fs::read(self.base.join("objects").join(&sha[..2]).join(&sha[2..]))?)
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_recorder() -> (Recorder, tempdir::TempDirGuard) {
        let dir = tempdir::make();
        let r = Recorder::new(Some(dir.path())).unwrap();
        (r, dir)
    }

    // Minimal self-contained temp-dir helper (avoids extra dev-dependency).
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct TempDirGuard(PathBuf);
        impl TempDirGuard {
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        pub fn make() -> TempDirGuard {
            let p = std::env::temp_dir().join(format!(
                "tokenos-rec-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDirGuard(p)
        }
    }

    #[test]
    fn record_and_replay() {
        let (r, _g) = temp_recorder();
        let sha = r.record("task-1", "prompt", "the prompt", b"PAYLOAD BYTES").unwrap();
        assert_eq!(sha.len(), 64);
        r.record("task-1", "response", "the answer", b"RESPONSE").unwrap();
        let evs = r.events("task-1").unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].kind, "prompt");
        assert_eq!(evs[1].kind, "response");
        let blob = r.blob(&sha).unwrap();
        assert_eq!(blob, b"PAYLOAD BYTES");
    }

    #[test]
    fn blob_deduplication() {
        let (r, _g) = temp_recorder();
        let a = r.record("t", "prompt", "", b"same content").unwrap();
        let b = r.record("t", "prompt", "", b"same content").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn empty_payload_has_no_blob() {
        let (r, _g) = temp_recorder();
        let sha = r.record("t", "decision", "route=DIRECT", b"").unwrap();
        assert!(sha.is_empty());
        let evs = r.events("t").unwrap();
        assert_eq!(evs.len(), 1);
        assert!(evs[0].blob_sha.is_empty());
    }

    #[test]
    fn missing_journal_returns_empty() {
        let (r, _g) = temp_recorder();
        assert!(r.events("nope").unwrap().is_empty());
    }

    #[test]
    fn sanitize_path_traversal() {
        assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize("task-42_ok"), "task-42_ok");
    }

    #[test]
    fn invalid_sha_rejected() {
        let (r, _g) = temp_recorder();
        assert!(r.blob("ab").is_err());
    }
}
