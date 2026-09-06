// api/pair.rs - mutual-backup pairing signaling (tech plan 4.2).
//
//   POST /api/pair/invite  (device JWT) {syncthing_id}    -> 6-digit code, 10 min TTL
//   POST /api/pair/accept  (device JWT) {code, accepter_syncthing_id}
//                                                        -> inviter_syncthing_id
//   GET  /api/pair/poll    (device JWT)                  -> matched? + peer id
//
// The control plane relays ONLY invite codes and Syncthing device IDs - it
// never touches the data plane (files flow peer-to-peer via Tailscale /
// Syncthing) and never learns folder paths or contents.
//
// Eligibility gate (plan 4.1: expired subscriptions stop mutual backup but
// never local operation): pairing requires an activated, non-expired device
// JWT. Unactivated (pending) or expired/cancelled devices get 403 - the node
// keeps every local feature either way.

use crate::api::{client_key, ApiError, ApiResult, JsonBody, LIMIT_PAIR};
use crate::auth::AuthDevice;
use crate::db;
use crate::AppState;
use axum::extract::{ConnectInfo, State};
use axum::Json;
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct InviteReq {
    pub syncthing_id: String,
}

#[derive(Debug, Deserialize)]
pub struct AcceptReq {
    pub code: String,
    pub accepter_syncthing_id: String,
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn device_id_from_claims(claims: &crate::auth::Claims) -> ApiResult<i64> {
    claims
        .sub
        .parse::<i64>()
        .map_err(|_| ApiError::unauthorized("malformed token subject"))
}

/// Syncthing device IDs are relayed verbatim; accept a loose-but-bounded
/// shape (base32 groups joined by dashes) so garbage cannot be stored.
/// Normalize: trim + uppercase.
fn normalize_syncthing_id(raw: &str) -> ApiResult<String> {
    let id = raw.trim().to_uppercase();
    let len = id.len();
    if !(40..=128).contains(&len) {
        return Err(ApiError::bad_request(
            "syncthing_id must be 40-128 chars (e.g. ABCDEFG-1234567-...)",
        ));
    }
    if !id.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-') {
        return Err(ApiError::bad_request(
            "syncthing_id may only contain A-Z, 0-9 and dashes",
        ));
    }
    Ok(id)
}

/// Pairing requires an activated device whose subscription has not expired
/// (trial / active / expiring are all fine). pending -> 403 with a "activate
/// first" hint; expired/cancelled -> 403 with the plan's honest message.
fn require_pairing_eligible(conn: &rusqlite::Connection, id: i64, now: i64) -> ApiResult<db::Device> {
    let dev = db::find_by_id(conn, id)
        .ok_or_else(|| ApiError::not_found("device not found"))?;
    match dev.subscription_state(now) {
        "pending" => Err(ApiError::forbidden(
            "device not activated yet - activate before pairing",
        )),
        "expired" => Err(ApiError::forbidden(
            "subscription expired; mutual backup requires an active subscription (local data access is unaffected)",
        )),
        "cancelled" => Err(ApiError::forbidden(
            "subscription cancelled; mutual backup requires an active subscription (local data access is unaffected)",
        )),
        _ => Ok(dev),
    }
}

/// 6-digit invite code (numeric: easy to read out across the room).
fn gen_pair_code() -> String {
    let mut rng = rand::thread_rng();
    (0..db::PAIR_CODE_LEN)
        .map(|_| char::from(b'0' + rng.gen_range(0..10)))
        .collect()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Invite: create a 6-digit code bound to this device + its Syncthing device
/// ID. A fresh invite supersedes any still-pending one from this device, so
/// at most one code per inviter is redeemable at any time.
pub async fn invite(
    State(state): State<AppState>,
    addr: Option<ConnectInfo<SocketAddr>>,
    AuthDevice(claims): AuthDevice,
    JsonBody(req): JsonBody<InviteReq>,
) -> ApiResult<Json<Value>> {
    state
        .limiter
        .check(&format!("pair-invite:{}", client_key(&addr)), LIMIT_PAIR.0, LIMIT_PAIR.1, state.now())?;
    let syncthing_id = normalize_syncthing_id(&req.syncthing_id)?;
    let now = state.now();
    let conn = state.db.lock().unwrap();
    let id = device_id_from_claims(&claims)?;
    require_pairing_eligible(&conn, id, now)?;

    db::supersede_pending(&conn, id)
        .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    for _ in 0..10 {
        let code = gen_pair_code();
        if db::find_pairing_by_code(&conn, &code).is_some() {
            continue; // code still held by an old pairing (any status)
        }
        match db::insert_pairing(&conn, &code, id, &syncthing_id, now) {
            Ok(_) => {
                return Ok(Json(json!({
                    "ok": true,
                    "code": code,
                    "expires_at": now + db::PAIR_TTL_SECS,
                    "expires_in_secs": db::PAIR_TTL_SECS,
                })))
            }
            Err(e) => return Err(ApiError::internal(format!("db error: {e}"))),
        }
    }
    Err(ApiError::internal("could not allocate an invite code"))
}

/// Accept: validate the code (pending, not expired, not self), record the
/// accepter and hand back the INVITER's Syncthing device ID so the accepter
/// can add it locally. One code redeems exactly once.
pub async fn accept(
    State(state): State<AppState>,
    addr: Option<ConnectInfo<SocketAddr>>,
    AuthDevice(claims): AuthDevice,
    JsonBody(req): JsonBody<AcceptReq>,
) -> ApiResult<Json<Value>> {
    state
        .limiter
        .check(&format!("pair-accept:{}", client_key(&addr)), LIMIT_PAIR.0, LIMIT_PAIR.1, state.now())?;
    let accepter_syncthing_id = normalize_syncthing_id(&req.accepter_syncthing_id)?;
    let code = req.code.trim().to_string();
    if code.len() != db::PAIR_CODE_LEN || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(ApiError::bad_request("invalid invite code (6 digits)"));
    }
    let now = state.now();
    let conn = state.db.lock().unwrap();
    let id = device_id_from_claims(&claims)?;
    require_pairing_eligible(&conn, id, now)?;

    let pairing = db::find_pairing_by_code(&conn, &code)
        .ok_or_else(|| ApiError::bad_request("invalid invite code"))?;
    if pairing.status == db::PAIR_MATCHED {
        return Err(ApiError::conflict("invite code already used"));
    }
    if pairing.status != db::PAIR_PENDING || pairing.is_expired(now) {
        let _ = db::expire_pairing(&conn, pairing.id);
        return Err(ApiError::bad_request("invite code expired - generate a new one"));
    }
    if pairing.inviter_device_id == id {
        return Err(ApiError::conflict("cannot pair a device with itself"));
    }
    if pairing.inviter_syncthing_id == accepter_syncthing_id {
        return Err(ApiError::bad_request(
            "accepter Syncthing ID equals the inviter's - two distinct nodes are required",
        ));
    }

    db::match_pairing(&conn, pairing.id, id, &accepter_syncthing_id, now)
        .map_err(|e| ApiError::internal(format!("db error: {e}")))?;
    Ok(Json(json!({
        "ok": true,
        "inviter_syncthing_id": pairing.inviter_syncthing_id,
        "message": "paired - add the inviter's device ID locally",
    })))
}

/// Poll: the inviter checks whether its latest invite got matched. matched
/// responses carry the ACCEPTER's Syncthing device ID (the peer to add).
pub async fn poll(
    State(state): State<AppState>,
    AuthDevice(claims): AuthDevice,
) -> ApiResult<Json<Value>> {
    let now = state.now();
    let conn = state.db.lock().unwrap();
    let id = device_id_from_claims(&claims)?;
    db::find_by_id(&conn, id)
        .ok_or_else(|| ApiError::not_found("device not found"))?;

    let Some(pairing) = db::latest_pairing_for_inviter(&conn, id) else {
        return Ok(Json(json!({ "ok": true, "status": "none" })));
    };
    match pairing.status.as_str() {
        db::PAIR_MATCHED => Ok(Json(json!({
            "ok": true,
            "status": "matched",
            "accepter_syncthing_id": pairing.accepter_syncthing_id,
            "matched_at": pairing.matched_at,
        }))),
        db::PAIR_PENDING => {
            if pairing.is_expired(now) {
                let _ = db::expire_pairing(&conn, pairing.id);
                return Ok(Json(json!({
                    "ok": true,
                    "status": "expired",
                    "expires_at": pairing.expires_at,
                })));
            }
            Ok(Json(json!({
                "ok": true,
                "status": "pending",
                "expires_at": pairing.expires_at,
                "expires_in_secs": (pairing.expires_at - now).max(0),
            })))
        }
        // expired / anything else: the latest invite is no longer redeemable
        _ => Ok(Json(json!({
            "ok": true,
            "status": pairing.status,
            "expires_at": pairing.expires_at,
        }))),
    }
}