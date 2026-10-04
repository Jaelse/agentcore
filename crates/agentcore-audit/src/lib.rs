//! Tamper-evident audit log.
//!
//! Every [`Event`] of a session is appended to a JSON Lines file. Each record
//! carries the SHA-256 hash of the previous record, forming a hash chain:
//! modifying, inserting, reordering or deleting any record (other than
//! truncating the tail) breaks verification. To also detect truncation, ship
//! the latest hash (see [`AuditLog::head`]) to an external system, e.g. a WORM
//! bucket or a transparency log.
//!
//! This is the automatic record-keeping required for high-risk AI systems by
//! EU AI Act Art. 12 and the log retention duties of Art. 19 / Art. 26(6).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agentcore_core::Event;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("audit log {path} line {line}: {reason}")]
    Tampered {
        path: PathBuf,
        line: usize,
        reason: String,
    },
    #[error("failed to serialise audit event: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Serialize)]
struct RecordOut<'a> {
    seq: u64,
    recorded_at: &'a str,
    prev_hash: &'a str,
    event: &'a RawValue,
    hash: &'a str,
}

#[derive(Deserialize)]
struct RecordIn<'a> {
    seq: u64,
    recorded_at: &'a str,
    prev_hash: &'a str,
    #[serde(borrow)]
    event: &'a RawValue,
    hash: &'a str,
}

fn record_hash(seq: u64, recorded_at: &str, prev_hash: &str, event_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seq.to_be_bytes());
    for part in [recorded_at, prev_hash, event_json] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// The head of a chain: number of records and hash of the last one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainHead {
    pub records: u64,
    pub hash: String,
}

struct Inner {
    file: File,
    head: ChainHead,
}

/// Append-only, hash-chained writer for one audit file.
pub struct AuditLog {
    path: PathBuf,
    sync: bool,
    inner: Mutex<Inner>,
}

impl AuditLog {
    /// Open (or create) an audit file. An existing file is fully verified
    /// first; a broken chain is an error so we never extend a tampered log.
    ///
    /// With `sync = true` every record is `fsync`ed before `append` returns.
    pub fn open(path: impl Into<PathBuf>, sync: bool) -> Result<Self, AuditError> {
        let path = path.into();
        let io = |source| AuditError::Io {
            path: path.clone(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let head = if path.exists() {
            verify_file(&path)?
        } else {
            ChainHead {
                records: 0,
                hash: GENESIS_HASH.into(),
            }
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        Ok(Self {
            path,
            sync,
            inner: Mutex::new(Inner { file, head }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an event, returning the new chain head.
    pub fn append(&self, event: &Event) -> Result<ChainHead, AuditError> {
        let event_json = serde_json::to_string(event)?;
        let raw = RawValue::from_string(event_json)?;
        let recorded_at = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);

        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let seq = inner.head.records;
        let hash = record_hash(seq, &recorded_at, &inner.head.hash, raw.get());
        let mut line = serde_json::to_string(&RecordOut {
            seq,
            recorded_at: &recorded_at,
            prev_hash: &inner.head.hash,
            event: &raw,
            hash: &hash,
        })?;
        line.push('\n');

        let io = |source| AuditError::Io {
            path: self.path.clone(),
            source,
        };
        inner.file.write_all(line.as_bytes()).map_err(io)?;
        inner.file.flush().map_err(io)?;
        if self.sync {
            inner.file.sync_data().map_err(io)?;
        }
        inner.head = ChainHead {
            records: seq + 1,
            hash,
        };
        Ok(inner.head.clone())
    }

    pub fn head(&self) -> ChainHead {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .head
            .clone()
    }
}

/// A verified record, as returned by [`read_events`].
#[derive(Debug, Clone)]
pub struct VerifiedRecord {
    pub seq: u64,
    pub recorded_at: DateTime<Utc>,
    pub hash: String,
    pub event: Event,
}

/// Verify the whole chain of an audit file and return its head.
pub fn verify_file(path: &Path) -> Result<ChainHead, AuditError> {
    let mut head = ChainHead {
        records: 0,
        hash: GENESIS_HASH.into(),
    };
    walk(path, |_, record| {
        head = ChainHead {
            records: record.seq + 1,
            hash: record.hash.to_string(),
        };
        Ok(())
    })?;
    Ok(head)
}

/// Verify an audit file and return its decoded events.
pub fn read_events(path: &Path) -> Result<Vec<VerifiedRecord>, AuditError> {
    let mut out = Vec::new();
    walk(path, |line, record| {
        let tampered = |reason: String| AuditError::Tampered {
            path: path.to_path_buf(),
            line,
            reason,
        };
        out.push(VerifiedRecord {
            seq: record.seq,
            recorded_at: DateTime::parse_from_rfc3339(record.recorded_at)
                .map_err(|e| tampered(format!("bad timestamp: {e}")))?
                .with_timezone(&Utc),
            hash: record.hash.to_string(),
            event: serde_json::from_str(record.event.get())
                .map_err(|e| tampered(format!("bad event: {e}")))?,
        });
        Ok(())
    })?;
    Ok(out)
}

fn walk(
    path: &Path,
    mut visit: impl FnMut(usize, &RecordIn<'_>) -> Result<(), AuditError>,
) -> Result<(), AuditError> {
    let io = |source| AuditError::Io {
        path: path.to_path_buf(),
        source,
    };
    let reader = BufReader::new(File::open(path).map_err(io)?);
    let mut prev = GENESIS_HASH.to_string();
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let expected_seq = index as u64;
        let line = line.map_err(io)?;
        let tampered = |reason: String| AuditError::Tampered {
            path: path.to_path_buf(),
            line: line_no,
            reason,
        };
        let record: RecordIn<'_> =
            serde_json::from_str(&line).map_err(|e| tampered(format!("unparseable: {e}")))?;
        if record.seq != expected_seq {
            return Err(tampered(format!(
                "sequence gap: expected {expected_seq}, found {}",
                record.seq
            )));
        }
        if record.prev_hash != prev {
            return Err(tampered("prev_hash does not match previous record".into()));
        }
        let actual = record_hash(
            record.seq,
            record.recorded_at,
            record.prev_hash,
            record.event.get(),
        );
        if actual != record.hash {
            return Err(tampered("record hash mismatch (content modified)".into()));
        }
        visit(line_no, &record)?;
        prev = actual;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcore_core::{EventKind, OutputStream};
    use uuid::Uuid;

    fn event(seq: u64, line: &str) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            seq,
            timestamp: Utc::now(),
            kind: EventKind::Output {
                stream: OutputStream::Stdout,
                line: line.into(),
            },
        }
    }

    fn write_log(path: &Path, n: u64) -> ChainHead {
        let log = AuditLog::open(path, false).unwrap();
        for i in 0..n {
            log.append(&event(i, &format!("line {i}"))).unwrap();
        }
        log.head()
    }

    #[test]
    fn roundtrip_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let head = write_log(&path, 5);
        assert_eq!(head.records, 5);
        assert_eq!(verify_file(&path).unwrap(), head);
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 5);
        assert!(
            matches!(&events[2].event.kind, EventKind::Output { line, .. } if line == "line 2")
        );
    }

    #[test]
    fn reopening_continues_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write_log(&path, 2);
        let head = write_log(&path, 3);
        assert_eq!(head.records, 5);
        assert_eq!(verify_file(&path).unwrap(), head);
    }

    #[test]
    fn detects_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write_log(&path, 3);
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, content.replace("line 1", "line X")).unwrap();
        let err = verify_file(&path).unwrap_err();
        assert!(matches!(err, AuditError::Tampered { line: 2, .. }), "{err}");
        // A tampered log must not be extended.
        assert!(AuditLog::open(&path, false).is_err());
    }

    #[test]
    fn detects_deleted_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write_log(&path, 3);
        let content = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<_> = content
            .lines()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, l)| l)
            .collect();
        std::fs::write(&path, kept.join("\n") + "\n").unwrap();
        assert!(matches!(
            verify_file(&path),
            Err(AuditError::Tampered { line: 2, .. })
        ));
    }
}
