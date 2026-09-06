// api/device.rs - device-facing endpoints plus admin subscription ops.
//
//   POST /api/register  {device_name, node_kind, hardware_id}  -> activation code
//   POST /api/activate  {activation_code, hardware_id, public_key?} -> JWT token
//     Accepts both code families (same 8-char alphabet):
//       - the device-bound code returned by /api/register (trial terms);
//       - a standalone admin-issued code (/api/admin/codes, trial or pro).
//   GET  /api/status    (device JWT)                           -> device + subscription
//   POST /api/heartbeat (device JWT) {disk_free_bytes, online, version}
//   POST /api/renew     (admin) {hardware_id, days?, plan?}
//   POST /api/cancel    (admin) {hardware_id}

use crate::api::{client_key, ApiError, ApiResult, JsonBody, LIMIT_ACTIVATE, LIMIT_REGISTER};
use crate::auth::{AuthAdmin, AuthDevice};
use crate::db;
use crate::AppState;
use axum::extract::{ConnectInfo, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RegisterReq {
    pub device_name: String,
    pub node_kind: String,
    pub hardware_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ActivateReq {
    pub activation_code: String,
    pub hardware_id: String,
    /// Reserved: node identity key (future pair/migration flows). The MVP
    /// accepts and ignores it; kept so the API contract stays stable.
    #[allow(dead_code)]
    pub public_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HeartbeatReq {
    pub disk_free_bytes: Option<i64>,
    /// Accepted for API stability; "online" is derived server-side from
    /// heartbeat recency (an offline node cannot report at all).
    #[allow(dead_code)]
    pub online: bool,
    pub version: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RenewReq {
    pub hardware_id: String,
    #[serde(default = "default_renew_days")]
    pub days: i64,
    #[serde(default)]
    pub plan: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CancelReq {
    pub hardware_id: String,
}

fn default_renew_days() -> i64 {
    db::RENEW_DAYS
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_register(req: &RegisterReq) -> ApiResult<()> {
    let name = req.device_name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::bad_request("device_name is required (1-64 chars)"));
    }
    match req.node_kind.as_str() {
        "computer" | "phone" | "drive" => {}
        _ => {
            return Err(ApiError::bad_request(
                "node_kind must be one of: computer, phone, drive",
            ))
        }
    }
    if req.hardware_id.trim().len() < 4 || req.hardware_id.trim().len() > 128 {
        return Err(ApiError::bad_request("hardware_id is required (4-128 chars)"));
    }
    Ok(())
}

fn device_id_from_claims(claims: &crate::auth::Claims) -> ApiResult<i64> {
    claims
        .sub
        .parse::<i64>()
        .map_err(|_| ApiError::unauthorized("malformed token subject"))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Register a device: creates a pending device bound to the hardware
/// fingerprint and issues an 8-char activation code. Re-registering the
/// same hardware while pending returns the same code (idempotent).
pub async fn register(
    State(state): State<AppState>,
    addr: Option<ConnectInfo<SocketAddr>>,
    JsonBody(req): JsonBody<RegisterReq>,
) -> ApiResult<Json<Value>> {
    state
        .limiter
        .check(&format!("register:{}", client_key(&addr)), LIMIT_REGISTER.0, LIMIT_REGISTER.1, state.now())?;
    validate_register(&req)?;
    let now = state.now();
    let hid = req.hardware_id.trim();
    let conn = state.db.lock().unwrap();

    if let Some(dev) = db::find_by_hardware_id(&conn, hid) {
        if dev.activated_at.is_some() {
            return Err(ApiError::conflict(
                "hardware already activated, one activation per device",
            ));
        }
        return Ok(Json(json!({ "ok": true, "device": dev.to_json(now, true) })));
    }

    // New device: allocate a unique activation code (retry on collision
    // against both devices.activation_code and the standalone codes table).
    for _ in 0..5 {
        let code = db::gen_activation_code();
        if db::find_by_activation_code(&conn, &code).is_some()
            || db::find_code(&conn, &code).is_some()
        {
            continue;
        }
        match db::insert_device(&conn, hid, req.device_name.trim(), &req.node_kind, &code, now) {
            Ok(dev) => return Ok(Json(json!({ "ok": true, "device": dev.to_json(now, true) }))),
            Err(e) => return Err(ApiError::internal(format!("db error: {e}"))),
        }
    }
    Err(ApiError::internal("could not allocate an activation code"))
}

/// Activation terms for one redemption: (days, plan, quota).
type Terms = (i64, &'static str, i64);

const TRIAL_TERMS: Terms = (db::TRIAL_DAYS, db::PLAN_TRIAL, db::TRIAL_QUOTA_BYTES);
const PRO_TERMS: Terms = (db::PRO_DAYS, db::PLAN_ACTIVE, db::ACTIVE_QUOTA_BYTES);

/// Activate: binds the device and returns the device JWT. The code may be
/// the device-bound registration code (default trial terms) or a standalone
/// admin-issued code (trial or pro terms).
pub async fn activate(
    State(state): State<AppState>,
    addr: Option<ConnectInfo<SocketAddr>>,
    JsonBody(req): JsonBody<ActivateReq>,
) -> ApiResult<Json<Value>> {
    state
        .limiter
        .check(&format!("activate:{}", client_key(&addr)), LIMIT_ACTIVATE.0, LIMIT_ACTIVATE.1, state.now())?;
    let now = state.now();
    let code = req.activation_code.trim().to_uppercase();
    if code.len() != db::CODE_LEN {
        return Err(ApiError::bad_request("invalid activation code"));
    }

    let conn = state.db.lock().unwrap();

    // Resolve the code against both families, then share one activation tail.
    let (dev, terms) = if let Some(dev) = db::find_by_activation_code(&conn, &code) {
        // Family 1: device-bound registration code.
        if dev.hardware_id != req.hardware_id {
            return Err(ApiError::bad_request(
                "activation code does not match this hardware",
            ));
        }
        if dev.activated_at.is_some() {
            return Err(ApiError::conflict("activation code already used"));
        }
        (dev, TRIAL_TERMS)
    } else if let Some(ac) = db::find_code(&conn, &code) {
        // Family 2: standalone admin-issued code.
        match ac.status.as_str() {
            db::CODE_USED => {
                return Err(ApiError::conflict("activation code already used"))
            }
            db::CODE_REVOKED => {
                return Err(ApiError::bad_request("activation code has been revoked"))
            }
            _ => {}
        }
        let dev = db::find_by_hardware_id(&conn, &req.hardware_id)
            .ok_or_else(|| ApiError::bad_request("unknown hardware id, register the device first"))?;
        if dev.activated_at.is_some() {
            return Err(ApiError::conflict("device already activated"));
        }
        let terms = if ac.kind == db::CODE_KIND_PRO { PRO_TERMS } else { TRIAL_TERMS };
        // Single redemption: burn the code as part of the same transaction
        // scope (one connection behind one mutex).
        db::mark_code_used(&conn, ac.id, dev.id, now)
            .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
        (dev, terms)
    } else {
        return Err(ApiError::bad_request("invalid activation code"));
    };

    let (days, plan, quota) = terms;
    db::activate_plan(&conn, dev.id, days, plan, quota, now)
        .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    let token = state.auth.issue_device_token(dev.id, &dev.hardware_id, now)?;
    let dev = db::find_by_id(&conn, dev.id)
        .ok_or_else(|| ApiError::internal("device vanished after activation"))?;

    Ok(Json(json!({
        "ok": true,
        "token": token,
        "device": dev.to_json(now, false),
        "subscription_state": dev.subscription_state(now),
        "expires_at": dev.expires_at
    })))
}

/// Current device info + subscription state (JWT protected).
pub async fn status(
    State(state): State<AppState>,
    AuthDevice(claims): AuthDevice,
) -> ApiResult<Json<Value>> {
    let now = state.now();
    let conn = state.db.lock().unwrap();
    let id = device_id_from_claims(&claims)?;
    let dev = db::find_by_id(&conn, id)
        .ok_or_else(|| ApiError::not_found("device not found"))?;
    Ok(Json(json!({
        "ok": true,
        "server_time": now,
        "device": dev.to_json(now, false)
    })))
}

/// Heartbeat: nodes report every few minutes. The response carries the
/// current subscription state so the node can react to expiry locally.
pub async fn heartbeat(
    State(state): State<AppState>,
    AuthDevice(claims): AuthDevice,
    JsonBody(req): JsonBody<HeartbeatReq>,
) -> ApiResult<Json<Value>> {
    if let Some(free) = req.disk_free_bytes {
        if free < 0 {
            return Err(ApiError::bad_request("disk_free_bytes must be >= 0"));
        }
    }
    let now = state.now();
    let conn = state.db.lock().unwrap();
    let id = device_id_from_claims(&claims)?;
    // Existence check only: the heartbeat is what matters here.
    db::find_by_id(&conn, id)
        .ok_or_else(|| ApiError::not_found("device not found"))?;
    db::touch_heartbeat(&conn, id, req.disk_free_bytes, req.version.as_deref(), now)
        .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    let dev = db::find_by_id(&conn, id)
        .ok_or_else(|| ApiError::internal("device vanished after heartbeat"))?;
    Ok(Json(json!({
        "ok": true,
        "server_time": now,
        "subscription_state": dev.subscription_state(now),
        "expires_at": dev.expires_at
    })))
}

/// Admin renew: extend the subscription. Defaults: +365 days, plan = active.
pub async fn renew(
    State(state): State<AppState>,
    _admin: AuthAdmin,
    JsonBody(req): JsonBody<RenewReq>,
) -> ApiResult<Json<Value>> {
    if !(1..=3650).contains(&req.days) {
        return Err(ApiError::bad_request("days must be between 1 and 3650"));
    }
    let plan = req.plan.as_deref().unwrap_or(db::PLAN_ACTIVE);
    if plan != db::PLAN_ACTIVE && plan != db::PLAN_TRIAL {
        return Err(ApiError::bad_request("plan must be 'active' or 'trial'"));
    }
    let now = state.now();
    let conn = state.db.lock().unwrap();
    let dev = db::find_by_hardware_id(&conn, &req.hardware_id)
        .ok_or_else(|| ApiError::not_found("device not found"))?;
    let quota = if plan == db::PLAN_ACTIVE {
        db::ACTIVE_QUOTA_BYTES
    } else {
        db::TRIAL_QUOTA_BYTES
    };
    db::renew(&conn, dev.id, req.days, plan, quota, now)
        .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    let dev = db::find_by_id(&conn, dev.id)
        .ok_or_else(|| ApiError::internal("device vanished after renew"))?;
    Ok(Json(json!({ "ok": true, "device": dev.to_json(now, true) })))
}

/// Admin cancel: terminate the subscription immediately (terminal state).
/// Per the product plan this only stops inter-backup / Pro services;
/// basic local data access is untouched by design (client-side policy).
pub async fn cancel(
    State(state): State<AppState>,
    _admin: AuthAdmin,
    JsonBody(req): JsonBody<CancelReq>,
) -> ApiResult<Json<Value>> {
    let now = state.now();
    let conn = state.db.lock().unwrap();
    let dev = db::find_by_hardware_id(&conn, &req.hardware_id)
        .ok_or_else(|| ApiError::not_found("device not found"))?;
    db::cancel(&conn, dev.id, now).map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    let dev = db::find_by_id(&conn, dev.id)
        .ok_or_else(|| ApiError::internal("device vanished after cancel"))?;
    Ok(Json(json!({ "ok": true, "device": dev.to_json(now, true) })))
}
