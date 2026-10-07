//! Live view endpoints: watch the agent's screen, pause/resume it, replay
//! the terminal recording.

use std::convert::Infallible;
use std::time::Duration;

use agentcore_core::SessionId;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::response::sse::{self, KeepAlive, Sse};
use futures::{Stream, StreamExt, stream};
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::sessions::SessionRef;

type ApiResult<T> = Result<T, ApiError>;

/// `GET /sessions/{id}/live`: live frames of a running session. The first
/// frames rebuild the current screen; a viewer that falls behind gets
/// `lagged` and should reconnect.
pub async fn stream(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>> {
    let frames = match state.lookup(id).await? {
        SessionRef::Live(session) => {
            let (initial, rx) = session.live().subscribe();
            let live = BroadcastStream::new(rx).map(|r| r.map_err(|_| ()));
            stream::iter(initial.into_iter().map(Ok))
                .chain(live)
                .boxed()
        }
        SessionRef::Archived(_) => stream::empty().boxed(),
    };
    // `lagged`: reconnect (and get a fresh screen). `ended`: the session is over.
    let lagged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = lagged.clone();
    let events = frames
        .scan(false, move |stop, item| {
            if *stop {
                return futures::future::ready(None);
            }
            let out = match item {
                Ok(frame) => sse::Event::default()
                    .event("frame")
                    .json_data(&frame)
                    .unwrap_or_else(|_| sse::Event::default().event("error")),
                Err(()) => {
                    *stop = true;
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    sse::Event::default().event("lagged").data("{}")
                }
            };
            futures::future::ready(Some(out))
        })
        .chain(
            stream::once(async move {
                (!lagged.load(std::sync::atomic::Ordering::SeqCst))
                    .then(|| sse::Event::default().event("ended").data("{}"))
            })
            .filter_map(futures::future::ready),
        )
        .map(Ok);
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// `POST /sessions/{id}/pause`: freeze the agent and everything it runs.
pub async fn pause(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let session = state.lookup(id).await?.live()?;
    session.pause(caller.principal()).await?;
    Ok(Json(json!(session.info())))
}

/// `POST /sessions/{id}/resume`.
pub async fn resume(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<Json<Value>> {
    caller.require_operator()?;
    let session = state.lookup(id).await?.live()?;
    session.resume(caller.principal()).await?;
    Ok(Json(json!(session.info())))
}

/// `GET /sessions/{id}/recording`: the terminal recording (asciicast v2).
/// While the session runs this is the recording so far.
pub async fn recording(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<SessionId>,
) -> ApiResult<impl IntoResponse> {
    // Make sure the session exists (and the caller may see it).
    state.lookup(id).await?;
    let path = state
        .config
        .storage
        .data_dir
        .join("recordings")
        .join(format!("{id}.cast"));
    let body = match tokio::fs::read(&path).await {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "this session has no terminal recording",
            ));
        }
        Err(e) => {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
            ));
        }
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-asciicast".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"agentcore-{id}.cast\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    ))
}
