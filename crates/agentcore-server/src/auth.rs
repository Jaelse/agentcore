//! Bearer-token authentication for operators.

use agentcore_core::Principal;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::config::Role;
use crate::error::ApiError;

/// An authenticated human caller.
#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    pub role: Role,
}

impl Caller {
    pub fn principal(&self) -> Principal {
        Principal::human(self.name.clone())
    }

    pub fn require_operator(&self) -> Result<(), ApiError> {
        match self.role {
            Role::Operator => Ok(()),
            Role::Viewer => Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "this action requires the operator role",
            )),
        }
    }
}

pub fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let operators = &state.config.server.operators;
        if operators.is_empty() {
            // Only reachable on loopback; see `Config::validate`.
            return Ok(Self {
                name: "local".into(),
                role: Role::Operator,
            });
        }
        let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid token");
        let presented = hash_token(bearer(parts).ok_or_else(unauthorized)?);
        operators
            .iter()
            .find(|op| constant_time_eq(op.token_sha256.as_bytes(), presented.as_bytes()))
            .map(|op| Self {
                name: op.name.clone(),
                role: op.role,
            })
            .ok_or_else(unauthorized)
    }
}
