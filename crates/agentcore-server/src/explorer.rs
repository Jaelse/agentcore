//! The organisation's file explorer: read-only browsing of each department
//! agent's workspace on the data volume.
//!
//! Department files and business data have their own APIs; this adds the
//! workspaces, which live as directories on the data volume every node
//! shares. Only `workspace/` is served (never the agent's `home/`, which
//! holds its credentials), `.git` is hidden, and nothing outside the
//! workspace is reachable, through `..` or through a symlink.

use std::path::{Component, Path as FsPath, PathBuf};

use agentcore_core::OrgAgentId;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;

type ApiResult<T> = Result<T, ApiError>;

/// Most entries listed for one directory.
const MAX_ENTRIES: usize = 2000;
/// Most bytes of a file shown.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Bytes looked at to tell text from binary.
const SNIFF_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

fn not_found(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, message)
}

fn io_error(e: std::io::Error) -> ApiError {
    match e.kind() {
        std::io::ErrorKind::NotFound => not_found("no such file or directory"),
        _ => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// A path inside the workspace, as plain names (no `..`, no root, no `.git`).
fn relative(path: &str) -> ApiResult<PathBuf> {
    let mut out = PathBuf::new();
    for component in FsPath::new(path.trim_matches('/')).components() {
        match component {
            Component::Normal(name) if name == ".git" => {
                return Err(bad("`.git` is not shown"));
            }
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            _ => return Err(bad(format!("`{path}` is not a path inside the workspace"))),
        }
    }
    Ok(out)
}

/// The workspace root of an agent and the resolved target inside it. The
/// target is canonicalised, so a symlink pointing outside is refused.
async fn resolve(state: &AppState, agent: OrgAgentId, path: &str) -> ApiResult<(PathBuf, PathBuf)> {
    let agent = state.store.get_agent(agent).await?;
    let root = state
        .agent_dir(agent.department_id, agent.id)
        .join("workspace");
    let rel = relative(path)?;
    let root = match tokio::fs::canonicalize(&root).await {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(not_found("this agent has no workspace yet"));
        }
        Err(e) => return Err(io_error(e)),
    };
    let target = tokio::fs::canonicalize(root.join(&rel))
        .await
        .map_err(io_error)?;
    let Ok(inside) = target.strip_prefix(&root) else {
        return Err(bad(format!("`{path}` leads outside the workspace")));
    };
    // A symlink may also lead into `.git`.
    relative(&inside.to_string_lossy())?;
    Ok((root, target))
}

fn modified(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.modified().ok().map(DateTime::<Utc>::from)
}

/// `GET /org/agents/{id}/workspace?path=dir`: one directory of an agent's
/// workspace, folders first.
pub async fn list(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<OrgAgentId>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    let (_, dir) = resolve(&state, id, &q.path).await?;
    let mut read = tokio::fs::read_dir(&dir).await.map_err(io_error)?;
    let mut entries = Vec::new();
    let mut truncated = false;
    while let Some(item) = read.next_entry().await.map_err(io_error)? {
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        if entries.len() == MAX_ENTRIES {
            truncated = true;
            break;
        }
        // Not followed: a symlink is shown as one, whatever it points at.
        let Ok(meta) = item.metadata().await else {
            continue;
        };
        let kind = if meta.is_symlink() {
            EntryKind::Symlink
        } else if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        entries.push(Entry {
            size: if kind == EntryKind::File {
                meta.len()
            } else {
                0
            },
            modified: modified(&meta),
            name,
            kind,
        });
    }
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Dir)
            .cmp(&(b.kind != EntryKind::Dir))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(Json(json!({
        "path": relative(&q.path)?.to_string_lossy(),
        "entries": entries,
        "truncated": truncated,
    })))
}

/// `GET /org/agents/{id}/workspace/file?path=file`: the text of one file
/// (the first MiB), or only its size when it is binary.
pub async fn read(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<OrgAgentId>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    use tokio::io::AsyncReadExt;

    let (_, file) = resolve(&state, id, &q.path).await?;
    let meta = tokio::fs::metadata(&file).await.map_err(io_error)?;
    if !meta.is_file() {
        return Err(bad(format!("`{}` is not a file", q.path)));
    }
    let mut bytes = Vec::new();
    tokio::fs::File::open(&file)
        .await
        .map_err(io_error)?
        .take(MAX_FILE_BYTES)
        .read_to_end(&mut bytes)
        .await
        .map_err(io_error)?;
    let binary = is_binary(&bytes);
    Ok(Json(json!({
        "path": relative(&q.path)?.to_string_lossy(),
        "size": meta.len(),
        "modified": modified(&meta),
        "binary": binary,
        "truncated": meta.len() > MAX_FILE_BYTES,
        "content": if binary { None } else { Some(String::from_utf8_lossy(&bytes)) },
    })))
}

/// Text unless there is a NUL byte, or the start is not UTF-8 (allowing a
/// character cut at the end of the sample).
fn is_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(SNIFF_BYTES)];
    if sample.contains(&0) {
        return true;
    }
    match std::str::from_utf8(sample) {
        Ok(_) => false,
        Err(e) => e.error_len().is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_stay_inside() {
        assert_eq!(relative("").unwrap(), PathBuf::new());
        assert_eq!(
            relative("/src/main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(relative("./a/./b").unwrap(), PathBuf::from("a/b"));
        assert!(relative("../etc/passwd").is_err());
        assert!(relative("a/../../b").is_err());
        assert!(relative(".git/config").is_err());
        assert!(relative("sub/.git").is_err());
    }

    #[test]
    fn tells_text_from_binary() {
        assert!(!is_binary(b"hello\nworld"));
        assert!(!is_binary("héllo".as_bytes()));
        assert!(is_binary(b"\x7fELF\x00\x01"));
        assert!(is_binary(&[0xff, 0xfe, 0x41]));
        // A multi-byte character cut by the sample is still text.
        let mut text = "a".repeat(SNIFF_BYTES - 1).into_bytes();
        text.extend("é".as_bytes());
        assert!(!is_binary(&text));
    }
}
