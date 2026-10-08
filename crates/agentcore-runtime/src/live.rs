//! The live view: everything a supervisor needs to watch an agent work as if
//! it were sharing its screen.
//!
//! [`LiveHub`] fans out [`LiveFrame`]s (terminal bytes, command output, model
//! token streams, file changes, processes) to viewers. Frames are ephemeral;
//! the terminal is additionally recorded to an asciicast v2 file whose SHA-256
//! goes into the audit log when the recording is closed.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use agentcore_core::live::{TERMINAL_COLS, TERMINAL_ROWS};
use agentcore_core::{FileChange, FileChangeKind, LiveFrame, ProcessInfo};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

/// Terminal output kept for viewers that join late.
const BACKLOG_BYTES: usize = 256 * 1024;
/// File changes kept for viewers that join late.
const RECENT_FILES: usize = 200;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct Screen {
    backlog: VecDeque<u8>,
    files: VecDeque<FileChange>,
    processes: Vec<ProcessInfo>,
}

/// What a closed recording looked like, for the audit log.
#[derive(Debug, Clone)]
pub struct RecordingSummary {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
}

struct Recorder {
    path: PathBuf,
    file: std::io::BufWriter<std::fs::File>,
    hasher: Sha256,
    bytes: u64,
    started: Instant,
}

impl Recorder {
    fn create(path: &Path, title: &str) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::File::create(path)?;
        let mut recorder = Self {
            path: path.to_path_buf(),
            file: std::io::BufWriter::new(file),
            hasher: Sha256::new(),
            bytes: 0,
            started: Instant::now(),
        };
        let header = serde_json::json!({
            "version": 2,
            "width": TERMINAL_COLS,
            "height": TERMINAL_ROWS,
            "timestamp": chrono::Utc::now().timestamp(),
            "title": title,
            "env": { "TERM": "xterm-256color" },
        });
        recorder.line(&header.to_string())?;
        Ok(recorder)
    }

    fn line(&mut self, line: &str) -> std::io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.hasher.update(line.as_bytes());
        self.hasher.update(b"\n");
        self.bytes += line.len() as u64 + 1;
        Ok(())
    }

    fn event(&mut self, code: &str, data: &str) -> std::io::Result<()> {
        let t = self.started.elapsed().as_secs_f64();
        let line = serde_json::to_string(&(((t * 1000.0).round() / 1000.0), code, data))
            .unwrap_or_default();
        self.line(&line)?;
        // Flush often: a recording must survive a crash as far as possible.
        self.file.flush()
    }

    fn finish(mut self) -> std::io::Result<RecordingSummary> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(RecordingSummary {
            file: self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            bytes: self.bytes,
            sha256: hex::encode(self.hasher.finalize()),
        })
    }
}

pub struct LiveHub {
    frames: broadcast::Sender<LiveFrame>,
    screen: Mutex<Screen>,
    recorder: Mutex<Option<Recorder>>,
}

impl Default for LiveHub {
    fn default() -> Self {
        Self {
            frames: broadcast::channel(4096).0,
            screen: Mutex::new(Screen {
                backlog: VecDeque::new(),
                files: VecDeque::new(),
                processes: Vec::new(),
            }),
            recorder: Mutex::new(None),
        }
    }
}

impl LiveHub {
    /// Frames that rebuild the current picture, plus a receiver for what
    /// follows. Taken atomically so a viewer neither misses nor repeats
    /// terminal output.
    pub fn subscribe(&self) -> (Vec<LiveFrame>, broadcast::Receiver<LiveFrame>) {
        let screen = lock(&self.screen);
        let (a, b) = screen.backlog.as_slices();
        let mut bytes = Vec::with_capacity(a.len() + b.len());
        bytes.extend_from_slice(a);
        bytes.extend_from_slice(b);
        let mut initial = vec![LiveFrame::TerminalReset {
            data: String::from_utf8_lossy(&bytes).into_owned(),
            cols: TERMINAL_COLS,
            rows: TERMINAL_ROWS,
        }];
        if !screen.files.is_empty() {
            initial.push(LiveFrame::Files {
                changes: screen.files.iter().cloned().collect(),
            });
        }
        initial.push(LiveFrame::Processes {
            processes: screen.processes.clone(),
        });
        (initial, self.frames.subscribe())
    }

    /// Number of connected viewers.
    pub fn viewers(&self) -> usize {
        self.frames.receiver_count()
    }

    pub fn sender(&self) -> broadcast::Sender<LiveFrame> {
        self.frames.clone()
    }

    pub fn send(&self, frame: LiveFrame) {
        let _ = self.frames.send(frame);
    }

    /// Start recording the terminal to an asciicast v2 file.
    pub fn start_recording(&self, path: &Path, title: &str) -> std::io::Result<()> {
        let recorder = Recorder::create(path, title)?;
        *lock(&self.recorder) = Some(recorder);
        Ok(())
    }

    /// Close the recording; `None` when nothing was being recorded.
    pub fn finish_recording(&self) -> Option<std::io::Result<RecordingSummary>> {
        lock(&self.recorder).take().map(Recorder::finish)
    }

    /// A marker in the recording (shown as a chapter by players).
    pub fn marker(&self, label: &str) {
        if let Some(rec) = lock(&self.recorder).as_mut() {
            let _ = rec.event("m", label);
        }
    }

    /// Terminal output (UTF-8, ANSI sequences included).
    pub fn terminal(&self, data: &str) {
        if data.is_empty() {
            return;
        }
        {
            let mut screen = lock(&self.screen);
            screen.backlog.extend(data.as_bytes());
            let excess = screen.backlog.len().saturating_sub(BACKLOG_BYTES);
            if excess > 0 {
                screen.backlog.drain(..excess);
                // Don't start the backlog in the middle of a UTF-8 character.
                while screen
                    .backlog
                    .front()
                    .is_some_and(|b| (0x80..0xC0).contains(b))
                {
                    screen.backlog.pop_front();
                }
            }
            self.send(LiveFrame::Terminal {
                data: data.to_string(),
            });
        }
        if let Some(rec) = lock(&self.recorder).as_mut()
            && let Err(err) = rec.event("o", data)
        {
            tracing::error!(error = %err, "terminal recording failed");
        }
    }

    pub fn files(&self, changes: Vec<FileChange>) {
        if changes.is_empty() {
            return;
        }
        let mut screen = lock(&self.screen);
        for change in &changes {
            screen.files.retain(|c| c.path != change.path);
            screen.files.push_back(change.clone());
        }
        while screen.files.len() > RECENT_FILES {
            screen.files.pop_front();
        }
        self.send(LiveFrame::Files { changes });
    }

    pub fn processes(&self, processes: Vec<ProcessInfo>) {
        let mut screen = lock(&self.screen);
        if screen.processes == processes {
            return;
        }
        screen.processes = processes.clone();
        self.send(LiveFrame::Processes { processes });
    }
}

/// Decodes a byte stream as UTF-8 across chunk boundaries.
#[derive(Default)]
pub(crate) struct Utf8Stream {
    carry: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.carry) {
            Ok(_) => self.carry.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            // Invalid bytes (not just an incomplete tail): decode lossily.
            Err(_) => self.carry.len(),
        };
        let out = String::from_utf8_lossy(&self.carry[..valid]).into_owned();
        self.carry.drain(..valid);
        out
    }
}

/// Turns terminal output into plain text lines for the audit log: ANSI escape
/// sequences are removed, a bare carriage return starts the line over
/// (progress bars and spinners keep only their final state).
#[derive(Default)]
pub(crate) struct PlainLines {
    line: String,
    escape: Escape,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Escape {
    #[default]
    None,
    Esc,
    Csi,
    Osc,
    OscEsc,
    CarriageReturn,
}

impl PlainLines {
    pub fn push(&mut self, text: &str) -> Vec<String> {
        let mut lines = Vec::new();
        for c in text.chars() {
            match self.escape {
                Escape::Esc => {
                    self.escape = match c {
                        '[' => Escape::Csi,
                        ']' => Escape::Osc,
                        _ => Escape::None,
                    };
                    continue;
                }
                Escape::Csi => {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        self.escape = Escape::None;
                    }
                    continue;
                }
                Escape::Osc => {
                    match c {
                        '\u{7}' => self.escape = Escape::None,
                        '\u{1b}' => self.escape = Escape::OscEsc,
                        _ => {}
                    }
                    continue;
                }
                Escape::OscEsc => {
                    self.escape = Escape::None;
                    continue;
                }
                Escape::CarriageReturn => {
                    self.escape = Escape::None;
                    if c != '\n' {
                        self.line.clear();
                    }
                }
                Escape::None => {}
            }
            match c {
                '\u{1b}' => self.escape = Escape::Esc,
                '\r' => self.escape = Escape::CarriageReturn,
                '\n' => lines.push(std::mem::take(&mut self.line)),
                '\t' => self.line.push(c),
                '\u{8}' => {
                    self.line.pop();
                }
                c if c.is_control() => {}
                c => self.line.push(c),
            }
        }
        lines
    }

    /// Whatever is left after the stream ended.
    pub fn finish(&mut self) -> Option<String> {
        let line = std::mem::take(&mut self.line);
        (!line.is_empty()).then_some(line)
    }
}

/// Watches the workspace on the host and reports changed files (debounced),
/// ignoring git's internals.
pub(crate) fn watch_files(
    root: PathBuf,
    on_change: impl Fn(Vec<FileChange>) + Send + 'static,
) -> notify::Result<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    tokio::spawn(async move {
        let mut pending: BTreeMap<String, FileChangeKind> = BTreeMap::new();
        // Flush after a short quiet period, but at least once a second while
        // files keep changing.
        let mut since = tokio::time::Instant::now();
        loop {
            let event = if pending.is_empty() {
                match rx.recv().await {
                    Some(e) => {
                        since = tokio::time::Instant::now();
                        Some(e)
                    }
                    None => break,
                }
            } else {
                let wait = Duration::from_millis(300)
                    .min(Duration::from_secs(1).saturating_sub(since.elapsed()));
                match tokio::time::timeout(wait, rx.recv()).await {
                    Ok(Some(e)) if since.elapsed() < Duration::from_secs(1) => Some(e),
                    Ok(Some(e)) => {
                        // Too long without a flush: deliver what we have
                        // first, then start a new batch with this event.
                        let now = chrono::Utc::now();
                        on_change(
                            std::mem::take(&mut pending)
                                .into_iter()
                                .map(|(path, kind)| FileChange {
                                    path,
                                    kind,
                                    at: now,
                                })
                                .collect(),
                        );
                        since = tokio::time::Instant::now();
                        Some(e)
                    }
                    Ok(None) => break,
                    Err(_) => None,
                }
            };
            let Some(event) = event else {
                let now = chrono::Utc::now();
                on_change(
                    std::mem::take(&mut pending)
                        .into_iter()
                        .map(|(path, kind)| FileChange {
                            path,
                            kind,
                            at: now,
                        })
                        .collect(),
                );
                continue;
            };
            let kind = match event.kind {
                EventKind::Create(_) => FileChangeKind::Created,
                EventKind::Modify(notify::event::ModifyKind::Metadata(_)) => continue,
                EventKind::Modify(_) => FileChangeKind::Modified,
                EventKind::Remove(_) => FileChangeKind::Removed,
                _ => continue,
            };
            for path in event.paths {
                let Ok(rel) = path.strip_prefix(&root) else {
                    continue;
                };
                if rel.as_os_str().is_empty()
                    || rel.components().any(|c| c.as_os_str() == ".git")
                    || path.is_dir()
                {
                    continue;
                }
                let rel = rel.to_string_lossy().into_owned();
                // A file created and then written in one burst stays "created".
                let merged = match (pending.get(&rel), &kind) {
                    (Some(FileChangeKind::Created), FileChangeKind::Modified) => {
                        FileChangeKind::Created
                    }
                    _ => kind.clone(),
                };
                pending.insert(rel, merged);
            }
        }
    });
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_strip_ansi_and_spinners() {
        let mut p = PlainLines::default();
        let lines = p.push(
            "\x1b[1m> build\x1b[0m · model\r\nworking |\rworking /\rdone\r\n\x1b]0;title\x07ok",
        );
        assert_eq!(lines, vec!["> build · model", "done"]);
        assert_eq!(p.finish().as_deref(), Some("ok"));
    }

    #[test]
    fn utf8_stream_handles_split_characters() {
        let mut s = Utf8Stream::default();
        let bytes = "größe ✓".as_bytes();
        let mut out = String::new();
        for b in bytes {
            out.push_str(&s.push(&[*b]));
        }
        assert_eq!(out, "größe ✓");
    }

    #[test]
    fn late_viewers_get_the_backlog_and_recording_is_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let hub = LiveHub::default();
        hub.start_recording(&dir.path().join("x.cast"), "t")
            .unwrap();
        hub.terminal("hello ");
        hub.marker("turn 2");
        hub.terminal("world");
        let (initial, _rx) = hub.subscribe();
        assert!(
            matches!(&initial[0], LiveFrame::TerminalReset { data, .. } if data == "hello world")
        );
        let summary = hub.finish_recording().unwrap().unwrap();
        let content = std::fs::read(dir.path().join("x.cast")).unwrap();
        assert_eq!(summary.bytes, content.len() as u64);
        assert_eq!(summary.sha256, hex::encode(Sha256::digest(&content)));
        let text = String::from_utf8(content).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("\"version\":2"));
        assert!(lines[1].contains("\"o\",\"hello \""));
        assert!(lines[2].contains("\"m\",\"turn 2\""));
    }

    #[tokio::test]
    async fn file_watcher_reports_changes_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _watcher = watch_files(dir.path().to_path_buf(), move |c| {
            let _ = tx.send(c);
        })
        .unwrap();
        std::fs::write(dir.path().join(".git/index"), "x").unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let changes = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].path, "a.txt");
        assert_eq!(changes[0].kind, FileChangeKind::Created);
    }
}
