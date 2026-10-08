//! Live view frames: what a supervisor sees "over the agent's shoulder".
//!
//! Frames are ephemeral and high-volume (terminal bytes, token deltas), so
//! they are not individual audit events. What matters for the record is still
//! audited: the terminal is recorded to an asciicast file whose SHA-256 is
//! audited, command results and model calls have their own events.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OutputStream;

/// Terminal size agents run with (and viewers render).
pub const TERMINAL_COLS: u16 = 120;
pub const TERMINAL_ROWS: u16 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDeltaKind {
    /// Visible answer text.
    Text,
    /// Reasoning ("thinking") text, when the model exposes it.
    Thinking,
    /// The name of a tool the model starts calling.
    ToolName,
    /// Arguments of the tool call, as they are generated (partial JSON).
    ToolInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    #[serde(rename = "created")]
    Created,
    #[serde(rename = "modified")]
    Modified,
    #[serde(rename = "removed")]
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Path relative to the workspace.
    pub path: String,
    pub kind: FileChangeKind,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub elapsed: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum LiveFrame {
    /// Raw terminal output of the agent (ANSI sequences included).
    Terminal {
        data: String,
    },
    /// Sent first to a new viewer: recent terminal output to rebuild the screen.
    TerminalReset {
        data: String,
        cols: u16,
        rows: u16,
    },
    /// Live output of a command the agent asked agentcore to run.
    ToolOutput {
        action_id: Uuid,
        stream: OutputStream,
        data: String,
    },
    ModelStart {
        call_id: Uuid,
        provider: String,
        model: Option<String>,
    },
    ModelDelta {
        call_id: Uuid,
        kind: ModelDeltaKind,
        text: String,
    },
    ModelEnd {
        call_id: Uuid,
    },
    /// Files created, changed or removed in the workspace.
    Files {
        changes: Vec<FileChange>,
    },
    /// Processes running in the sandbox (sent while someone is watching).
    Processes {
        processes: Vec<ProcessInfo>,
    },
}
