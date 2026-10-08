//! Model gateway: a reverse proxy for LLM APIs.
//!
//! Agents call `/llm/{session}/{provider}/...` exactly as they would call the
//! provider, using their session token as the "API key". agentcore
//!
//! 1. authenticates the token against the session,
//! 2. checks the session is live, under its `max_model_calls` limit, and that
//!    the requested model is allowed for the provider,
//! 3. swaps in the real API key (decrypted from PostgreSQL),
//! 4. streams the upstream response back, aborting it if the session is
//!    stopped, and
//! 5. records the call: a `model_call` event in the hash-chained audit log
//!    (with SHA-256 of request and response) and the full bodies in the
//!    `model_calls` table.
//!
//! While the response streams, the generated text, reasoning and tool calls
//! are also sent to the session's live view as they arrive.
//!
//! Real provider keys never enter the sandbox, and sandboxes need no
//! internet access: only a route to agentcore.

use std::sync::Arc;
use std::time::Instant;

use agentcore_core::{
    EventKind, LiveFrame, ModelCallOutcome, ModelDeltaKind, ProviderKind, SessionId,
};
use agentcore_runtime::Session;
use agentcore_store::ModelCallRecord;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures::StreamExt;
use globset::{Glob, GlobSetBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::AppState;

/// Buffer kept for usage parsing of non-streaming responses.
const PARSE_LIMIT: usize = 4 * 1024 * 1024;

fn error_response(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    // Shape understood by both the Anthropic and OpenAI SDKs.
    let body = json!({
        "type": "error",
        "error": { "type": kind, "message": message.into() },
    });
    (status, axum::Json(body)).into_response()
}

/// Header that carries the session token when the client's own API-key
/// header must hold something else (opencode sends `Bearer public` to Zen).
pub const TOKEN_HEADER: &str = "x-agentcore-token";

fn presented_token(headers: &HeaderMap) -> Option<&str> {
    for name in [TOKEN_HEADER, "x-api-key"] {
        if let Some(key) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return Some(key.trim());
        }
    }
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

fn forward_request_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        let drop = HOP_BY_HOP.contains(&n)
            || matches!(
                n,
                "host"
                    | "authorization"
                    | "x-api-key"
                    | TOKEN_HEADER
                    | "cookie"
                    | "accept-encoding"
                    | "forwarded"
            )
            || n.starts_with("x-forwarded-");
        if !drop {
            out.append(name.clone(), value.clone());
        }
    }
    // Uncompressed responses, so they can be logged and inspected.
    out.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    out
}

fn forward_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if !HOP_BY_HOP.contains(&name.as_str()) && name != header::CONTENT_ENCODING {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

fn model_allowed(patterns: &[String], model: Option<&str>, method: &Method) -> Result<(), String> {
    if patterns.is_empty() {
        return Ok(());
    }
    let Some(model) = model else {
        // Listing models and similar metadata reads are harmless.
        return if method == Method::GET {
            Ok(())
        } else {
            Err("this provider only allows specific models, but the request names none".into())
        };
    };
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = Glob::new(pattern) {
            builder.add(glob);
        }
    }
    match builder.build() {
        Ok(set) if set.is_match(model) => Ok(()),
        _ => Err(format!("model `{model}` is not allowed for this provider")),
    }
}

/// Best-effort token accounting across Anthropic and OpenAI response shapes,
/// both plain JSON and server-sent events.
#[derive(Default)]
struct Usage {
    input: Option<u64>,
    output: Option<u64>,
    model: Option<String>,
    line: Vec<u8>,
    line_overflow: bool,
}

impl Usage {
    fn feed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                if !self.line_overflow {
                    let line = std::mem::take(&mut self.line);
                    self.line(&line);
                }
                self.line.clear();
                self.line_overflow = false;
            } else if self.line.len() < 1024 * 1024 {
                self.line.push(byte);
            } else {
                self.line_overflow = true;
            }
        }
    }

    fn line(&mut self, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        if let Some(data) = line.trim().strip_prefix("data:")
            && let Ok(value) = serde_json::from_str::<Value>(data.trim())
        {
            self.absorb(&value);
        }
    }

    fn finish(&mut self, whole_body: Option<&[u8]>) {
        let rest = std::mem::take(&mut self.line);
        self.line(&rest);
        if self.input.is_none()
            && self.output.is_none()
            && let Some(body) = whole_body
            && let Ok(value) = serde_json::from_slice::<Value>(body)
        {
            self.absorb(&value);
        }
    }

    fn absorb(&mut self, value: &Value) {
        for scope in [value, &value["message"], &value["response"]] {
            if let Some(model) = scope.get("model").and_then(Value::as_str) {
                self.model.get_or_insert_with(|| model.to_string());
            }
            let usage = &scope["usage"];
            let pick = |keys: &[&str]| keys.iter().find_map(|k| usage.get(*k)?.as_u64());
            if let Some(n) = pick(&["input_tokens", "prompt_tokens"]) {
                self.input = Some(self.input.map_or(n, |m| m.max(n)));
            }
            if let Some(n) = pick(&["output_tokens", "completion_tokens"]) {
                self.output = Some(self.output.map_or(n, |m| m.max(n)));
            }
        }
    }
}

/// Extracts what the model is generating from streamed (SSE) or plain JSON
/// responses, for the live view: Anthropic Messages, OpenAI Chat Completions
/// and OpenAI Responses.
#[derive(Default)]
struct Deltas {
    line: Vec<u8>,
    seen: bool,
}

type Delta = (ModelDeltaKind, String);

impl Deltas {
    fn feed(&mut self, chunk: &[u8]) -> Vec<Delta> {
        let mut out = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.line(&line, &mut out);
            } else if self.line.len() < 1024 * 1024 {
                self.line.push(byte);
            }
        }
        coalesce(out)
    }

    fn line(&mut self, line: &[u8], out: &mut Vec<Delta>) {
        let line = String::from_utf8_lossy(line);
        if let Some(data) = line.trim().strip_prefix("data:")
            && let Ok(value) = serde_json::from_str::<Value>(data.trim())
        {
            let before = out.len();
            stream_deltas(&value, out);
            self.seen |= out.len() > before;
        }
    }

    /// For non-streaming responses: the whole answer at once.
    fn finish(&mut self, whole_body: Option<&[u8]>) -> Vec<Delta> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.line);
        self.line(&rest, &mut out);
        if !self.seen
            && let Some(body) = whole_body
            && let Ok(value) = serde_json::from_slice::<Value>(body)
        {
            message_deltas(&value, &mut out);
        }
        coalesce(out)
    }
}

/// Merge consecutive deltas of the same kind (fewer, larger frames).
fn coalesce(deltas: Vec<Delta>) -> Vec<Delta> {
    let mut out: Vec<Delta> = Vec::with_capacity(deltas.len());
    for (kind, text) in deltas {
        if text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((k, t)) if *k == kind && kind != ModelDeltaKind::ToolName => t.push_str(&text),
            _ => out.push((kind, text)),
        }
    }
    out
}

fn stream_deltas(v: &Value, out: &mut Vec<Delta>) {
    let s = |v: &Value| v.as_str().map(str::to_string);
    let mut push = |kind, text: Option<String>| {
        if let Some(text) = text {
            out.push((kind, text));
        }
    };
    match v["type"].as_str() {
        // Anthropic Messages.
        Some("content_block_start") => {
            let block = &v["content_block"];
            if matches!(block["type"].as_str(), Some("tool_use" | "server_tool_use")) {
                push(ModelDeltaKind::ToolName, s(&block["name"]));
            }
        }
        Some("content_block_delta") => {
            let d = &v["delta"];
            match d["type"].as_str() {
                Some("text_delta") => push(ModelDeltaKind::Text, s(&d["text"])),
                Some("thinking_delta") => push(ModelDeltaKind::Thinking, s(&d["thinking"])),
                Some("input_json_delta") => push(ModelDeltaKind::ToolInput, s(&d["partial_json"])),
                _ => {}
            }
        }
        // OpenAI Responses.
        Some("response.output_text.delta") => push(ModelDeltaKind::Text, s(&v["delta"])),
        Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
            push(ModelDeltaKind::Thinking, s(&v["delta"]))
        }
        Some("response.function_call_arguments.delta") => {
            push(ModelDeltaKind::ToolInput, s(&v["delta"]))
        }
        Some("response.output_item.added") if v["item"]["type"] == "function_call" => {
            push(ModelDeltaKind::ToolName, s(&v["item"]["name"]))
        }
        _ => {
            // OpenAI Chat Completions chunks.
            let d = &v["choices"][0]["delta"];
            push(ModelDeltaKind::Thinking, s(&d["reasoning_content"]));
            push(ModelDeltaKind::Thinking, s(&d["reasoning"]));
            push(ModelDeltaKind::Text, s(&d["content"]));
            if let Some(calls) = d["tool_calls"].as_array() {
                for call in calls {
                    push(ModelDeltaKind::ToolName, s(&call["function"]["name"]));
                    push(ModelDeltaKind::ToolInput, s(&call["function"]["arguments"]));
                }
            }
        }
    }
}

fn message_deltas(v: &Value, out: &mut Vec<Delta>) {
    // Anthropic: {"content": [{type: text|thinking|tool_use}]}.
    if let Some(blocks) = v["content"].as_array() {
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => out.push((
                    ModelDeltaKind::Text,
                    b["text"].as_str().unwrap_or_default().into(),
                )),
                Some("thinking") => out.push((
                    ModelDeltaKind::Thinking,
                    b["thinking"].as_str().unwrap_or_default().into(),
                )),
                Some("tool_use") => {
                    out.push((
                        ModelDeltaKind::ToolName,
                        b["name"].as_str().unwrap_or_default().into(),
                    ));
                    out.push((ModelDeltaKind::ToolInput, b["input"].to_string()));
                }
                _ => {}
            }
        }
    }
    // OpenAI Chat Completions: {"choices": [{"message": {...}}]}.
    let m = &v["choices"][0]["message"];
    for key in ["reasoning_content", "reasoning"] {
        if let Some(t) = m[key].as_str() {
            out.push((ModelDeltaKind::Thinking, t.into()));
        }
    }
    if let Some(t) = m["content"].as_str() {
        out.push((ModelDeltaKind::Text, t.into()));
    }
    if let Some(calls) = m["tool_calls"].as_array() {
        for call in calls {
            let f = &call["function"];
            out.push((
                ModelDeltaKind::ToolName,
                f["name"].as_str().unwrap_or_default().into(),
            ));
            out.push((
                ModelDeltaKind::ToolInput,
                f["arguments"].as_str().unwrap_or_default().into(),
            ));
        }
    }
}

/// Everything known about a call; turned into the audit event and DB row.
struct Call {
    state: AppState,
    session: Arc<Session>,
    record: ModelCallRecord,
    clock: Instant,
}

impl Call {
    fn new(
        state: &AppState,
        session: Arc<Session>,
        provider: &str,
        method: &Method,
        path: &str,
        body: &Bytes,
    ) -> Self {
        let model = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v.get("model").and_then(Value::as_str).map(String::from));
        let max = state.config.model_gateway.max_logged_body_bytes;
        let log = state.config.model_gateway.log_bodies;
        Self {
            record: ModelCallRecord {
                id: Uuid::now_v7(),
                session_id: session.id(),
                provider: provider.to_string(),
                model,
                method: method.to_string(),
                path: path.to_string(),
                http_status: None,
                outcome: String::new(),
                detail: None,
                input_tokens: None,
                output_tokens: None,
                started_at: Utc::now(),
                duration_ms: 0,
                request_body: log
                    .then(|| String::from_utf8_lossy(&body[..body.len().min(max)]).into_owned()),
                response_body: None,
                bodies_truncated: log && body.len() > max,
                request_sha256: hex::encode(Sha256::digest(body)),
                response_sha256: None,
            },
            state: state.clone(),
            session,
            clock: Instant::now(),
        }
    }

    async fn finish(mut self, outcome: ModelCallOutcome, detail: Option<String>) {
        self.record.set_outcome(outcome);
        self.record.detail = detail;
        self.record.duration_ms = self.clock.elapsed().as_millis() as i64;
        let r = &self.record;
        let event = EventKind::ModelCall {
            call_id: r.id,
            provider: r.provider.clone(),
            model: r.model.clone(),
            path: r.path.clone(),
            http_status: r.http_status.map(|s| s as u16),
            outcome,
            input_tokens: r.input_tokens.map(|n| n as u64),
            output_tokens: r.output_tokens.map(|n| n as u64),
            duration_ms: r.duration_ms as u64,
            request_sha256: r.request_sha256.clone(),
            response_sha256: r.response_sha256.clone(),
            detail: r.detail.clone(),
        };
        if let Err(err) = self.session.record_model_call(event) {
            tracing::error!(session = %r.session_id, error = %err, "failed to audit model call");
        }
        // The session row must exist before the call row references it.
        self.state.persist(&self.session).await;
        if let Err(err) = self.state.store.insert_model_call(&self.record).await {
            tracing::error!(session = %r.session_id, error = %err, "failed to store model call");
        }
    }

    async fn reject(self, status: StatusCode, kind: &str, message: String) -> Response {
        let response = error_response(status, kind, message.clone());
        let mut call = self;
        call.record.http_status = Some(status.as_u16() as i32);
        call.finish(ModelCallOutcome::Rejected, Some(message)).await;
        response
    }
}

pub async fn proxy(
    State(state): State<AppState>,
    Path((session_id, provider, rest)): Path<(SessionId, String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let session = match (state.manager.get(session_id), presented_token(&headers)) {
        (Ok(session), Some(token)) if session.check_gateway_token(token) => session,
        _ => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "invalid agentcore session token",
            );
        }
    };
    // A paused session's model calls wait (the agent is frozen anyway; this
    // holds requests that were already in flight).
    if session.is_paused() && !session.wait_unpaused().await {
        return error_response(
            StatusCode::FORBIDDEN,
            "permission_error",
            "the session was stopped",
        );
    }
    let call = Call::new(&state, session.clone(), &provider, &method, &rest, &body);

    if let Err(message) = session.admit_model_call() {
        let status = if session.status().is_terminal() || session.cancellation().is_cancelled() {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        return call.reject(status, "permission_error", message).await;
    }
    if rest.split('/').any(|seg| seg == ".." || seg == ".") {
        return call
            .reject(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid path".into(),
            )
            .await;
    }
    let (provider_info, api_key) = match state.store.provider_credentials(&provider).await {
        Ok(Some(found)) if found.0.enabled => found,
        Ok(Some(_)) => {
            let msg = format!("model provider `{provider}` is disabled");
            return call
                .reject(StatusCode::FORBIDDEN, "permission_error", msg)
                .await;
        }
        Ok(None) => {
            let msg = format!("model provider `{provider}` is not configured in agentcore");
            return call
                .reject(StatusCode::NOT_FOUND, "not_found_error", msg)
                .await;
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to load model provider");
            let msg = "agentcore could not load the provider credentials".to_string();
            return call.reject(StatusCode::BAD_GATEWAY, "api_error", msg).await;
        }
    };
    if let Err(message) = model_allowed(
        &provider_info.allowed_models,
        call.record.model.as_deref(),
        &method,
    ) {
        return call
            .reject(StatusCode::FORBIDDEN, "permission_error", message)
            .await;
    }

    // A pausing budget that is used up stops the spending at once.
    if let Some(message) = crate::budgets::blocked(&state, session.context().department_id).await {
        return call
            .reject(StatusCode::FORBIDDEN, "permission_error", message)
            .await;
    }

    let mut url = format!("{}/{}", provider_info.base_url, rest);
    if let Some(query) = uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let mut upstream_headers = forward_request_headers(&headers);
    let auth = match provider_info.kind {
        ProviderKind::Anthropic => (
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(&api_key),
        ),
        ProviderKind::Openai | ProviderKind::OpencodeZen => (
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}")),
        ),
    };
    match auth {
        (name, Ok(mut value)) => {
            value.set_sensitive(true);
            upstream_headers.insert(name, value);
        }
        (_, Err(_)) => {
            let msg = "stored API key contains invalid characters".to_string();
            return call.reject(StatusCode::BAD_GATEWAY, "api_error", msg).await;
        }
    }

    let cancel = session.cancellation();
    let request = state
        .http
        .request(method, &url)
        .headers(upstream_headers)
        .body(body)
        .send();
    let upstream = tokio::select! {
        result = request => result,
        () = cancel.cancelled() => {
            let response = error_response(StatusCode::FORBIDDEN, "permission_error", "the session was stopped");
            call.finish(ModelCallOutcome::Aborted, Some("session stopped before the upstream answered".into())).await;
            return response;
        }
    };
    let upstream = match upstream {
        Ok(response) => response,
        Err(err) => {
            let detail = format!("upstream request failed: {err}");
            let response = error_response(StatusCode::BAD_GATEWAY, "api_error", detail.clone());
            call.finish(ModelCallOutcome::UpstreamError, Some(detail))
                .await;
            return response;
        }
    };

    let status = upstream.status();
    let response_headers = forward_response_headers(upstream.headers());
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let mut call = call;
    call.record.http_status = Some(status.as_u16() as i32);
    let log_limit = if state.config.model_gateway.log_bodies {
        state.config.model_gateway.max_logged_body_bytes
    } else {
        0
    };

    let call_id = call.record.id;
    let live = session.live().sender();
    let _ = live.send(LiveFrame::ModelStart {
        call_id,
        provider: provider.clone(),
        model: call.record.model.clone(),
    });
    let send_deltas = move |deltas: Vec<Delta>| {
        for (kind, text) in deltas {
            let _ = live.send(LiveFrame::ModelDelta {
                call_id,
                kind,
                text,
            });
        }
    };

    tokio::spawn(async move {
        let mut stream = upstream.bytes_stream();
        let mut deltas = Deltas::default();
        let mut hasher = Sha256::new();
        let mut captured: Vec<u8> = Vec::new();
        let mut total = 0usize;
        let mut usage = Usage::default();
        let (outcome, detail) = loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    let _ = tx.send(Err(std::io::Error::other("session stopped"))).await;
                    break (ModelCallOutcome::Aborted, Some("session stopped while streaming".to_string()));
                }
                chunk = stream.next() => match chunk {
                    None => break (ModelCallOutcome::Completed, None),
                    Some(Err(err)) => {
                        let _ = tx.send(Err(std::io::Error::other(err.to_string()))).await;
                        break (ModelCallOutcome::UpstreamError, Some(format!("upstream stream failed: {err}")));
                    }
                    Some(Ok(bytes)) => {
                        hasher.update(&bytes);
                        usage.feed(&bytes);
                        send_deltas(deltas.feed(&bytes));
                        total += bytes.len();
                        let keep = log_limit.max(PARSE_LIMIT).saturating_sub(captured.len());
                        captured.extend_from_slice(&bytes[..bytes.len().min(keep)]);
                        if tx.send(Ok(bytes)).await.is_err() {
                            break (ModelCallOutcome::Aborted, Some("agent disconnected".to_string()));
                        }
                    }
                }
            }
        };
        drop(tx);
        let whole = (total <= captured.len()).then_some(&captured[..]);
        send_deltas(deltas.finish(whole));
        let _ = call
            .session
            .live()
            .sender()
            .send(LiveFrame::ModelEnd { call_id });
        usage.finish(whole);
        let r = &mut call.record;
        r.response_sha256 = Some(hex::encode(hasher.finalize()));
        r.input_tokens = usage.input.map(|n| n as i64);
        r.output_tokens = usage.output.map(|n| n as i64);
        if r.model.is_none() {
            r.model = usage.model.take();
        }
        if log_limit > 0 {
            r.response_body = Some(
                String::from_utf8_lossy(&captured[..captured.len().min(log_limit)]).into_owned(),
            );
            r.bodies_truncated |= total > log_limit;
        }
        call.finish(outcome, detail).await;
    });

    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_anthropic_stream() {
        let mut usage = Usage::default();
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-x\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":15}}\n\n",
        );
        // Split mid-line to exercise buffering.
        let (a, b) = stream.split_at(40);
        usage.feed(a.as_bytes());
        usage.feed(b.as_bytes());
        usage.finish(None);
        assert_eq!((usage.input, usage.output), (Some(25), Some(15)));
        assert_eq!(usage.model.as_deref(), Some("claude-x"));
    }

    #[test]
    fn deltas_from_anthropic_stream() {
        let stream = concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me \"}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"look.\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Reading it\"}}\n",
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"name\":\"read_file\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n",
        );
        let mut d = Deltas::default();
        let (a, b) = stream.split_at(30);
        let mut out = d.feed(a.as_bytes());
        out.extend(d.feed(b.as_bytes()));
        out.extend(d.finish(None));
        assert_eq!(
            coalesce(out),
            vec![
                (ModelDeltaKind::Thinking, "Let me look.".into()),
                (ModelDeltaKind::Text, "Reading it".into()),
                (ModelDeltaKind::ToolName, "read_file".into()),
                (ModelDeltaKind::ToolInput, "{\"path\":".into()),
            ]
        );
    }

    #[test]
    fn deltas_from_openai_chunks_and_plain_responses() {
        let mut d = Deltas::default();
        let out = d.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\
              data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"run_command\",\"arguments\":\"{}\"}}]}}]}\n\
              data: [DONE]\n",
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[1], (ModelDeltaKind::ToolName, "run_command".into()));

        let body = br#"{"content":[{"type":"text","text":"Done."}]}"#;
        let mut d = Deltas::default();
        d.feed(body);
        assert_eq!(
            d.finish(Some(body)),
            vec![(ModelDeltaKind::Text, "Done.".into())]
        );
    }

    #[test]
    fn usage_from_openai_json() {
        let body = br#"{"model":"gpt-x","usage":{"prompt_tokens":7,"completion_tokens":3}}"#;
        let mut usage = Usage::default();
        usage.feed(body);
        usage.finish(Some(body));
        assert_eq!((usage.input, usage.output), (Some(7), Some(3)));
    }

    #[test]
    fn model_allow_list() {
        let patterns = vec!["claude-*".to_string()];
        assert!(model_allowed(&patterns, Some("claude-sonnet"), &Method::POST).is_ok());
        assert!(model_allowed(&patterns, Some("gpt-5"), &Method::POST).is_err());
        assert!(model_allowed(&patterns, None, &Method::GET).is_ok());
        assert!(model_allowed(&patterns, None, &Method::POST).is_err());
        assert!(model_allowed(&[], Some("anything"), &Method::POST).is_ok());
    }

    #[test]
    fn credentials_are_never_forwarded() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("session-token"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer session-token"),
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert(header::HOST, HeaderValue::from_static("agentcore:8080"));
        headers.insert(TOKEN_HEADER, HeaderValue::from_static("session-token"));
        assert_eq!(presented_token(&headers), Some("session-token"));
        let out = forward_request_headers(&headers);
        assert!(out.get(TOKEN_HEADER).is_none());
        assert!(out.get("x-api-key").is_none());
        assert!(out.get(header::AUTHORIZATION).is_none());
        assert!(out.get(header::HOST).is_none());
        assert_eq!(out["anthropic-version"], "2023-06-01");
        assert_eq!(out[header::ACCEPT_ENCODING], "identity");
    }
}
