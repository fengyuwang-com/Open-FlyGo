// api/admin.rs - bootstrap login, device listing, standalone activation
// code management and the embedded admin page.
//
//   GET  /                       -> embedded single-file admin page
//   POST /api/admin/login        {token: FENGDOG_ADMIN_TOKEN} -> admin JWT

//   GET  /api/admin/devices     (admin JWT or raw admin token) -> device list
//   GET  /api/admin/codes       (admin) -> issued code list
//   POST /api/admin/codes       (admin) {kind: trial|pro} -> issue a code
//   POST /api/admin/codes/revoke (admin) {code} -> void a still-unused code

use crate::api::{client_key, ApiError, ApiResult, JsonBody, LIMIT_ADMIN_LOGIN};
use crate::auth::{ct_eq, AuthAdmin};
use crate::db;
use crate::AppState;
use axum::extract::{ConnectInfo, State};
use axum::response::Html;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;

/// Embedded admin page (same include_str! approach as the FlyGo panel).
const ADMIN_HTML: &str = include_str!("../static/admin.html");

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct IssueCodeReq {
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct RevokeCodeReq {
    pub code: String,
}

/// The admin page itself. All admin data calls still require the admin
/// token/JWT; keep the port behind the reverse proxy / firewall anyway.
pub async fn page() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

/// Exchange the bootstrap admin token (FENGDOG_ADMIN_TOKEN) for a 12h JWT.
pub async fn login(
    State(state): State<AppState>,
    addr: Option<ConnectInfo<SocketAddr>>,
    JsonBody(req): JsonBody<LoginReq>,
) -> ApiResult<Json<Value>> {
    state
        .limiter
        .check(
            &format!("admin_login:{}", client_key(&addr)),
            LIMIT_ADMIN_LOGIN.0,
            LIMIT_ADMIN_LOGIN.1,
            state.now(),
        )?;
    let expected = state
        .admin_token
        .as_ref()
        .ok_or_else(|| ApiError::unavailable("admin token not configured (set FENGDOG_ADMIN_TOKEN)"))?;
    if !ct_eq(&req.token, expected) {
        return Err(ApiError::unauthorized("wrong admin token"));
    }
    let token = state.auth.issue_admin_token(state.now())?;
    Ok(Json(json!({ "ok": true, "token": token, "expires_in_secs": 12 * 3600 })))
}

/// Full device list with heartbeat + subscription state (admin data source
/// for the management page).
pub async fn devices(State(state): State<AppState>, _admin: AuthAdmin) -> ApiResult<Json<Value>> {
    let now = state.now();
    let conn = state.db.lock().unwrap();
    let list = db::list_all(&conn)
        .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    let arr: Vec<Value> = list.iter().map(|d| d.to_json(now, true)).collect();
    Ok(Json(json!({
        "ok": true,
        "server_time": now,
        "count": arr.len(),
        "devices": arr
    })))
}

/// Issue a standalone activation code (kind = trial | pro). The code is
/// redeemed at /api/activate and carries the plan terms of its kind.
pub async fn codes_issue(
    State(state): State<AppState>,
    _admin: AuthAdmin,
    JsonBody(req): JsonBody<IssueCodeReq>,
) -> ApiResult<Json<Value>> {
    let kind = match req.kind.as_str() {
        db::CODE_KIND_TRIAL => db::CODE_KIND_TRIAL,
        db::CODE_KIND_PRO => db::CODE_KIND_PRO,
        _ => return Err(ApiError::bad_request("kind must be 'trial' or 'pro'")),
    };
    let now = state.now();
    let conn = state.db.lock().unwrap();
    // Allocate a unique code (retry on collision against both code tables).
    for _ in 0..5 {
        let code = db::gen_activation_code();
        if db::find_by_activation_code(&conn, &code).is_some()
            || db::find_code(&conn, &code).is_some()
        {
            continue;
        }
        db::insert_code(&conn, &code, kind, now)
            .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
        let ac = db::find_code(&conn, &code)
            .ok_or_else(|| ApiError::internal("code vanished after insert"))?;
        return Ok(Json(json!({ "ok": true, "code": ac.to_json() })));
    }
    Err(ApiError::internal("could not allocate an activation code"))
}

/// Issued codes, newest first (admin listing for the management page).
pub async fn codes_list(State(state): State<AppState>, _admin: AuthAdmin) -> ApiResult<Json<Value>> {
    let conn = state.db.lock().unwrap();
    let list = db::list_codes(&conn)
        .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    let arr: Vec<Value> = list.iter().map(|c| c.to_json()).collect();
    Ok(Json(json!({ "ok": true, "count": arr.len(), "codes": arr })))
}

/// Void a still-unused code (issued -> revoked; revoking again is a no-op).
/// A used code cannot be revoked - it is already burned into a subscription.
pub async fn codes_revoke(
    State(state): State<AppState>,
    _admin: AuthAdmin,
    JsonBody(req): JsonBody<RevokeCodeReq>,
) -> ApiResult<Json<Value>> {
    let code = req.code.trim().to_uppercase();
    let conn = state.db.lock().unwrap();
    let ac = db::find_code(&conn, &code)
        .ok_or_else(|| ApiError::not_found("code not found"))?;
    if ac.status == db::CODE_USED {
        return Err(ApiError::conflict("code already used, cannot revoke"));
    }
    if ac.status == db::CODE_ISSUED {
        db::revoke_code(&conn, ac.id)
            .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    }
    let ac = db::find_code(&conn, &code)
        .ok_or_else(|| ApiError::internal("code vanished after revoke"))?;
    Ok(Json(json!({ "ok": true, "code": ac.to_json() })))
}
