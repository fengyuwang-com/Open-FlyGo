// api/mod.rs - JSON error envelope + router assembly.
//
// Error convention matches the FlyGo panel:
//   success: {"ok": true, ...}
//   failure: {"ok": false, "message": "..."}

pub mod admin;
pub mod device;
pub mod pair;

use crate::AppState;
use axum::async_trait;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::net::SocketAddr;

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.into() }
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::UNAUTHORIZED, message: msg.into() }
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::FORBIDDEN, message: msg.into() }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: msg.into() }
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::CONFLICT, message: msg.into() }
    }
    pub fn too_many_requests(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::TOO_MANY_REQUESTS, message: msg.into() }
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::SERVICE_UNAVAILABLE, message: msg.into() }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: msg.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "ok": false, "message": self.message }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Json body extractor that maps body errors (missing fields, bad JSON)
/// onto the standard error envelope {"ok": false, "message": ...} with
/// status 400, instead of axum's default 422 rejection body.
pub struct JsonBody<T>(pub T);

impl<T> std::ops::Deref for JsonBody<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

#[async_trait]
impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(v) = Json::<T>::from_request(req, state)
            .await
            .map_err(|e: JsonRejection| ApiError::bad_request(format!("bad request body: {e}")))?;
        Ok(JsonBody(v))
    }
}

// ---------------------------------------------------------------------------
// Rate limit policy (requests per 60s window, per client IP)
// ---------------------------------------------------------------------------

pub const LIMIT_REGISTER: (u32, i64) = (10, 60);
pub const LIMIT_ACTIVATE: (u32, i64) = (10, 60);
pub const LIMIT_ADMIN_LOGIN: (u32, i64) = (5, 60);
/// Mutual-backup pairing signaling (invite / accept), same budget class as
/// register/activate: 10 requests per 60s window per client IP.
pub const LIMIT_PAIR: (u32, i64) = (10, 60);

/// Client IP used as the rate-limit key. Falls back to "unknown" when
/// ConnectInfo is missing (unit tests without a socket connection).
pub fn client_key(info: &Option<ConnectInfo<SocketAddr>>) -> String {
    match info {
        Some(i) => i.0.ip().to_string(),
        None => "unknown".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the full router with state applied. Also used by the tests.
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/", get(admin::page))
        .route("/api/register", post(device::register))
        .route("/api/activate", post(device::activate))
        .route("/api/status", get(device::status))
        .route("/api/heartbeat", post(device::heartbeat))
        .route("/api/renew", post(device::renew))
        .route("/api/cancel", post(device::cancel))
        .route("/api/admin/login", post(admin::login))
        .route("/api/admin/devices", get(admin::devices))
        .route("/api/admin/codes", get(admin::codes_list).post(admin::codes_issue))
        .route("/api/admin/codes/revoke", post(admin::codes_revoke))
        .route("/api/pair/invite", post(pair::invite))
        .route("/api/pair/accept", post(pair::accept))
        .route("/api/pair/poll", get(pair::poll))
        .layer(cors())
        .fallback(|| async { ApiError::not_found("no such endpoint") })
        .with_state(state)
}

/// Permissive CORS. The MVP admin page is same-origin, so this is only a
/// convenience for future client apps; tighten it once real clients exist.
fn cors() -> tower_http::cors::CorsLayer {
    tower_http::cors::CorsLayer::permissive()
}
