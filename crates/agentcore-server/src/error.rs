use agentcore_runtime::RuntimeError;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
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
            RuntimeError::NotRunning => StatusCode::CONFLICT,
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(status = %self.status, error = %self.message, "request failed");
        }
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}
