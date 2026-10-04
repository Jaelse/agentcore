//! MCP tool gateway (Streamable HTTP transport, JSON responses).
//!
//! Each session has its own endpoint, `/mcp/{session_id}`, protected by a
//! per-session bearer token injected into the sandbox. Every tool call is
//! turned into an [`Action`] and goes through policy, human approval and the
//! audit log before it runs inside the sandbox.

use agentcore_core::{Action, ActionOutcome, SessionId};
use agentcore_runtime::RuntimeError;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::AppState;

const PROTOCOL_VERSION: &str = "2025-06-18";

const INSTRUCTIONS: &str = "You are running inside an agentcore sandbox. Every side effect \
must go through these tools. Each call is checked against a policy written by humans; some \
calls wait for a human to approve them and may be rejected. If a call is denied, do not try \
to work around the denial: explain what you needed and why, then continue with another approach. \
Everything you do is recorded in an audit log.";

fn tools() -> Value {
    json!([
        {
            "name": "run_command",
            "description": "Run a program inside the sandbox and return its exit code, stdout and stderr. \
                            No shell is involved: pass the program and its arguments separately. To use \
                            shell features, run `sh` with `-c` (this usually requires human approval).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Program to run, e.g. `cargo`." },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string", "description": "Working directory, relative to /workspace." }
                },
                "required": ["command"]
            }
        },
        {
            "name": "read_file",
            "description": "Read a text file from the workspace.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        },
        {
            "name": "write_file",
            "description": "Create or overwrite a file in the workspace with the given content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "list_files",
            "description": "List the entries of a directory in the workspace.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "default": "." } }
            }
        }
    ])
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn tool_text(text: impl Into<String>, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": is_error })
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument `{key}`"))
}

/// Translate a tool call into an action (plus file contents for writes).
fn to_action(name: &str, args: &Value) -> Result<(Action, Option<Vec<u8>>), String> {
    Ok(match name {
        "run_command" => {
            let command = str_arg(args, "command")?.to_string();
            let args_list = match args.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|v| v.as_str().map(String::from).ok_or("`args` must be strings"))
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err("`args` must be an array of strings".into()),
            };
            let cwd = args.get("cwd").and_then(Value::as_str).map(String::from);
            (
                Action::Exec {
                    command,
                    args: args_list,
                    cwd,
                },
                None,
            )
        }
        "read_file" => (
            Action::FileRead {
                path: str_arg(args, "path")?.into(),
            },
            None,
        ),
        "write_file" => {
            let content = str_arg(args, "content")?.as_bytes().to_vec();
            let action = Action::FileWrite {
                path: str_arg(args, "path")?.into(),
                bytes: content.len(),
            };
            (action, Some(content))
        }
        "list_files" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            let action = Action::Exec {
                command: "ls".into(),
                args: vec!["-la".into(), "--".into(), path.into()],
                cwd: None,
            };
            (action, None)
        }
        other => return Err(format!("unknown tool `{other}`")),
    })
}

fn render_outcome(outcome: ActionOutcome) -> Value {
    match outcome {
        ActionOutcome::Succeeded { output } => {
            if let Some(content) = output.get("content").and_then(Value::as_str) {
                let mut text = content.to_string();
                if output["truncated"] == true {
                    text.push_str("\n[output truncated]");
                }
                tool_text(text, false)
            } else if output.get("stdout").is_some() {
                let failed = output["exit_code"] != 0;
                tool_text(
                    serde_json::to_string_pretty(&output).unwrap_or_default(),
                    failed,
                )
            } else {
                tool_text(output.to_string(), false)
            }
        }
        ActionOutcome::Failed { error } => tool_text(format!("error: {error}"), true),
        ActionOutcome::Denied { reason } => {
            tool_text(format!("DENIED: {reason}. Do not retry this action."), true)
        }
    }
}

pub async fn handle(
    State(state): State<AppState>,
    Path(id): Path<SessionId>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // Same response for unknown sessions and bad tokens.
    let session = match (state.manager.get(id), token) {
        (Ok(session), Some(token)) if session.check_gateway_token(token.trim()) => session,
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Json(rpc_error(&Value::Null, -32600, "invalid request")).into_response();
    };
    let Some(id) = request.get("id").cloned() else {
        // Notifications (e.g. `notifications/initialized`) get no response body.
        return StatusCode::ACCEPTED.into_response();
    };
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let response = match method {
        "initialize" => rpc_result(
            &id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "agentcore", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }),
        ),
        "ping" => rpc_result(&id, json!({})),
        "tools/list" => rpc_result(&id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match to_action(name, &args) {
                Err(message) => rpc_result(&id, tool_text(message, true)),
                Ok((action, contents)) => match session.request_action(action, contents).await {
                    Ok(outcome) => rpc_result(&id, render_outcome(outcome)),
                    Err(RuntimeError::NotRunning) => {
                        rpc_result(&id, tool_text("the session has been stopped", true))
                    }
                    Err(err) => rpc_error(&id, -32603, err.to_string()),
                },
            }
        }
        other => rpc_error(&id, -32601, format!("method `{other}` not found")),
    };
    Json(response).into_response()
}

/// Streamable HTTP lets servers decline the optional GET stream.
pub async fn method_not_allowed() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_tools_to_actions() {
        let (action, _) = to_action(
            "run_command",
            &json!({"command": "cargo", "args": ["test"]}),
        )
        .unwrap();
        assert_eq!(action.command_line().as_deref(), Some("cargo test"));
        let (action, contents) =
            to_action("write_file", &json!({"path": "a.txt", "content": "hey"})).unwrap();
        assert_eq!(
            action,
            Action::FileWrite {
                path: "a.txt".into(),
                bytes: 3
            }
        );
        assert_eq!(contents.as_deref(), Some(&b"hey"[..]));
        assert!(to_action("run_command", &json!({"command": "x", "args": "y"})).is_err());
        assert!(to_action("nope", &json!({})).is_err());
    }
}
