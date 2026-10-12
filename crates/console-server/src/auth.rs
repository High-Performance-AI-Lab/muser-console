//! Browser -> console bearer auth. Shapes are byte-identical to the engine's
//! `valid_bearer` / `auth_required` / `error_json` so the dashboard's error
//! paths behave identically behind the console.

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::state::{AppState, MutationAuthError};

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

pub fn valid_bearer(expected: &[u8], headers: &HeaderMap) -> bool {
    let Some(candidate) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    constant_time_eq(candidate.as_bytes(), expected)
}

pub fn error_json(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"type": kind, "message": message}})),
    )
        .into_response()
}

pub fn auth_required() -> Response {
    let mut response = error_json(
        StatusCode::UNAUTHORIZED,
        "authentication_required",
        "a valid bearer API key is required",
    );
    response
        .headers_mut()
        .insert("www-authenticate", HeaderValue::from_static("Bearer"));
    response
}

pub fn read_rejection(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if state.authorized_read(headers) {
        None
    } else {
        Some(auth_required())
    }
}

pub fn mutation_rejection(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    match state.authorized_mutation(headers) {
        Ok(()) => None,
        Err(MutationAuthError::Unauthorized) => Some(auth_required()),
        Err(MutationAuthError::Csrf) => Some(error_json(
            StatusCode::FORBIDDEN,
            "csrf_required",
            "a valid CSRF token is required for this dashboard session",
        )),
    }
}
