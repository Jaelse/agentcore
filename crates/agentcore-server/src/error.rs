use agentcore_runtime::RuntimeError;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    body: Option<serde_json::Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            body: None,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// An error with a structured JSON body (must contain `error`).
    pub fn with_body(status: StatusCode, body: serde_json::Value) -> Self {
        Self {
            status,
            message: body["error"].as_str().unwrap_or_default().to_string(),
            body: Some(body),
        }
    }
}

impl From<RuntimeError> for ApiError {
    fn from(err: RuntimeError) -> Self {
        let status = match &err {
            RuntimeError::SessionNotFound(_) | RuntimeError::ApprovalNotFound(_) => {
                StatusCode::NOT_FOUND
            }
            RuntimeError::UnknownAgent(_) | RuntimeError::UnknownPolicy(_) => {
                StatusCode::BAD_REQUEST
            }
            RuntimeError::NotRunning | RuntimeError::NotAwaitingInput => StatusCode::CONFLICT,
            RuntimeError::Workspace(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, err.to_string())
    }
}

impl From<agentcore_audit::AuditError> for ApiError {
    fn from(err: agentcore_audit::AuditError) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

impl From<agentcore_store::StoreError> for ApiError {
    fn from(err: agentcore_store::StoreError) -> Self {
        use agentcore_store::StoreError as E;
        let status = match &err {
            E::Invalid(_) => StatusCode::BAD_REQUEST,
            E::NotFound(_) => StatusCode::NOT_FOUND,
            E::Conflict(_) | E::Limit(_) => StatusCode::CONFLICT,
            E::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(status = %self.status, error = %self.message, "request failed");
        }
        let body = self
            .body
            .unwrap_or_else(|| serde_json::json!({ "error": self.message }));
        (self.status, Json(body)).into_response()
    }
}
