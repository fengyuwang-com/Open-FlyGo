// tests.rs - end-to-end API tests against a temp SQLite file.
//
// Covers: register -> activate -> status -> heartbeat -> admin list,
// plus the error paths (bad token 401, wrong activation code 400,
// wrong hardware 400, rate limit 429, re-register 409).

use crate::api;
use crate::auth;
use crate::db;
use crate::limit;
use crate::AppState;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const TEST_SECRET: &str = "test-secret-0123456789";
const TEST_ADMIN_TOKEN: &str = "test-admin-token-01";

fn test_state() -> AppState {
    let path = std::env::temp_dir().join(format!("cp-test-{}.db", rand::random::<u64>()));
    let _ = std::fs::remove_file(&path);
    let conn = db::open_db(path.to_str().unwrap()).expect("open test db");
    AppState {
        db: Arc::new(Mutex::new(conn)),
        auth: auth::Auth::new(TEST_SECRET),
        admin_token: Some(TEST_ADMIN_TOKEN.to_string()),
        limiter: limit::RateLimiter::new(),
    }
}

/// Run one HTTP request against the full router.
async fn call(
    state: AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        builder = builder.header("Authorization", format!("Bearer {t}"));
    }
    let body_str = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = builder
        .header("Content-Type", "application/json")
        .body(Body::from(body_str))
        .unwrap();
    // Simulate a real socket so rate limiting sees a client IP.
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5555))));

    let resp = api::build_app(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn full_chain_register_activate_heartbeat_admin() {
    let state = test_state();

    // --- register ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": "node-a", "node_kind": "computer", "hardware_id": "HW-ABC-123"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["ok"], true);
    let code = v["device"]["activation_code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 8);
    assert_eq!(v["device"]["subscription_state"], "pending");

    // --- re-register same hardware while pending: same code (idempotent) ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": "node-a", "node_kind": "computer", "hardware_id": "HW-ABC-123"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["device"]["activation_code"].as_str().unwrap(), code);

    // --- wrong activation code -> 400 ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": "XXXXXXXX", "hardware_id": "HW-ABC-123"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["ok"], false);

    // --- activation code for a different hardware -> 400 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": code, "hardware_id": "HW-OTHER-999"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // --- activate ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": code, "hardware_id": "HW-ABC-123"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let token = v["token"].as_str().unwrap().to_string();
    assert!(!token.is_empty());
    assert_eq!(v["subscription_state"], "trial");
    let expires_at = v["expires_at"].as_i64().unwrap();

    // --- activate twice -> 409 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": code, "hardware_id": "HW-ABC-123"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // --- status with the device token ---
    let (st, v) = call(state.clone(), "GET", "/api/status", None, Some(&token)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["device"]["hardware_id"], "HW-ABC-123");
    assert_eq!(v["device"]["subscription_state"], "trial");

    // --- status without / with garbage token -> 401 ---
    let (st, _) = call(state.clone(), "GET", "/api/status", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = call(state.clone(), "GET", "/api/status", None, Some("garbage.token.here")).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // --- heartbeat with token ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/heartbeat",
        Some(json!({"disk_free_bytes": 1234567890, "online": true, "version": "0.1.0"})),
        Some(&token),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["subscription_state"], "trial");

    // --- heartbeat without token -> 401 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/heartbeat",
        Some(json!({"disk_free_bytes": 1, "online": true, "version": "0.1.0"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // --- register again after activation -> 409 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": "node-a", "node_kind": "computer", "hardware_id": "HW-ABC-123"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // --- admin login: wrong token -> 401, right token -> JWT ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/admin/login",
        Some(json!({"token": "wrong-token"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/admin/login",
        Some(json!({"token": TEST_ADMIN_TOKEN})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let admin_jwt = v["token"].as_str().unwrap().to_string();

    // --- admin devices with admin JWT ---
    let (st, v) = call(state.clone(), "GET", "/api/admin/devices", None, Some(&admin_jwt)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["count"], 1);
    assert_eq!(v["devices"][0]["hardware_id"], "HW-ABC-123");
    assert_eq!(v["devices"][0]["online"], true);
    assert_eq!(v["devices"][0]["disk_free_bytes"], 1234567890);
    assert_eq!(v["devices"][0]["version"], "0.1.0");

    // --- admin devices with raw bootstrap admin token also works ---
    let (st, v) = call(state.clone(), "GET", "/api/admin/devices", None, Some(TEST_ADMIN_TOKEN)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["count"], 1);

    // --- admin devices with a device token -> 403 ---
    let (st, _) = call(state.clone(), "GET", "/api/admin/devices", None, Some(&token)).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // --- renew: +365 days, plan active ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/renew",
        Some(json!({"hardware_id": "HW-ABC-123", "days": 365})),
        Some(&admin_jwt),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["device"]["plan"], "active");
    assert_eq!(v["device"]["subscription_state"], "active");
    assert!(v["device"]["expires_at"].as_i64().unwrap() > expires_at + 300 * 86400);

    // --- cancel: terminal state ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/cancel",
        Some(json!({"hardware_id": "HW-ABC-123"})),
        Some(&admin_jwt),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["device"]["subscription_state"], "cancelled");

    // --- heartbeat still accepted after cancel (control plane never
    //     blocks the node; expiry policy is client-side) ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/heartbeat",
        Some(json!({"disk_free_bytes": 1, "online": true, "version": "0.1.0"})),
        Some(&token),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn rate_limit_blocks_abuse_on_register() {
    let state = test_state();
    let body = json!({"device_name": "n", "node_kind": "computer", "hardware_id": "HW-RL-001"});
    // Register limit is 10 per 60s window.
    for i in 0..10 {
        let (st, _) = call(state.clone(), "POST", "/api/register", Some(body.clone()), None).await;
        assert_eq!(st, StatusCode::OK, "call {i} should pass");
    }
    let (st, v) = call(state.clone(), "POST", "/api/register", Some(body.clone()), None).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(v["ok"], false);
    assert!(v["message"].is_string());
}

#[tokio::test]
async fn validation_errors_and_unknown_route() {
    let state = test_state();
    // Bad node_kind -> 400
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": "x", "node_kind": "toaster", "hardware_id": "HW-VAL-1"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Missing fields -> 400
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": ""})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Unknown route -> 404 with the JSON error envelope
    let (st, v) = call(state.clone(), "GET", "/api/nope", None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["ok"], false);
    assert!(v["message"].is_string());
    // --- Root serves the admin page
    let (st, v) = call(state.clone(), "GET", "/", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(v.is_null()); // HTML body, not JSON
}

// ---------------------------------------------------------------------------
// Mutual-backup pairing signaling (POST /api/pair/invite|accept, GET /api/pair/poll)
// ---------------------------------------------------------------------------

/// Register + activate a device and return (device_id, JWT).
async fn activated_device(state: &AppState, name: &str, hid: &str) -> (i64, String) {
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": name, "node_kind": "computer", "hardware_id": hid})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let code = v["device"]["activation_code"].as_str().unwrap().to_string();
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": code, "hardware_id": hid})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    (
        v["device"]["device_id"].as_i64().unwrap(),
        v["token"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn pair_signaling_roundtrip() {
    let state = test_state();
    let st_id_a = "PAIRAAAA-AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGG";
    let st_id_b = "PAIRBBBB-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHH";
    let (dev_a, tok_a) = activated_device(&state, "node-a", "HW-PAIR-A").await;
    let (_dev_b, tok_b) = activated_device(&state, "node-b", "HW-PAIR-B").await;

    // --- pairing without a token -> 401 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/pair/invite",
        Some(json!({"syncthing_id": st_id_a})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // --- bad syncthing_id -> 400 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/pair/invite",
        Some(json!({"syncthing_id": "short"})),
        Some(&tok_a),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // --- invite -> 6-digit code ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/pair/invite",
        Some(json!({"syncthing_id": st_id_a})),
        Some(&tok_a),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let code = v["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 6);
    assert!(code.chars().all(|c| c.is_ascii_digit()));
    assert!(v["expires_in_secs"].as_i64().unwrap() > 0);

    // --- poll before accept: pending ---
    let (st, v) = call(state.clone(), "GET", "/api/pair/poll", None, Some(&tok_a)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["status"], "pending");

    // --- accept with a wrong code -> 400 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/pair/accept",
        Some(json!({"code": "000000", "accepter_syncthing_id": st_id_b})),
        Some(&tok_b),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // --- a fresh invite supersedes the still-pending one; poll follows the newest ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/pair/invite",
        Some(json!({"syncthing_id": st_id_a})),
        Some(&tok_a),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let code2 = v["code"].as_str().unwrap().to_string();
    assert_ne!(code, code2, "a fresh invite must allocate a new code");
    let (_st, v) = call(state.clone(), "GET", "/api/pair/poll", None, Some(&tok_a)).await;
    assert_eq!(v["status"], "pending");
    // the superseded (pending->expired) code is no longer redeemable -> 400
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/pair/accept",
        Some(json!({"code": code, "accepter_syncthing_id": st_id_b})),
        Some(&tok_b),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // --- self-pairing (inviter accepts its own code) -> 409 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/pair/accept",
        Some(json!({"code": code2, "accepter_syncthing_id": st_id_b})),
        Some(&tok_a),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // --- accept -> hands back the inviter's Syncthing device ID ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/pair/accept",
        Some(json!({"code": code2, "accepter_syncthing_id": st_id_b})),
        Some(&tok_b),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["inviter_syncthing_id"], st_id_a);

    // --- poll now reports matched + the accepter's device ID ---
    let (st, v) = call(state.clone(), "GET", "/api/pair/poll", None, Some(&tok_a)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["status"], "matched");
    assert_eq!(v["accepter_syncthing_id"], st_id_b);

    // --- the code redeems exactly once -> 409 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/pair/accept",
        Some(json!({"code": code2, "accepter_syncthing_id": st_id_b})),
        Some(&tok_b),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let _ = dev_a;
}

#[tokio::test]
async fn pair_expiry_and_eligibility_gate() {
    let state = test_state();
    let st_id = "EXPIAAAA-AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGG";
    let (dev, tok) = activated_device(&state, "node-x", "HW-PAIR-X").await;

    // --- plant an already-expired pairing directly (deterministic expiry) ---
    {
        let conn = state.db.lock().unwrap();
        db::insert_pairing(&conn, "555444", dev, st_id, 1000).unwrap();
        // force expires_at into the past (insert_pairing adds the 10-min TTL)
        conn.execute("UPDATE pairings SET expires_at = 1500 WHERE code = '555444'", [])
            .unwrap();
    }

    // poll -> expired (lazy transition persisted)
    let (st, v) = call(state.clone(), "GET", "/api/pair/poll", None, Some(&tok)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["status"], "expired");

    // accept on the expired code -> 400 with the honest message
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/pair/accept",
        Some(json!({"code": "555444", "accepter_syncthing_id": "EXPIBBBB-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG"})),
        Some(&tok),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["message"].as_str().unwrap().contains("expired"));

    // Note: the "pending / not activated" branch of the eligibility gate is
    // unreachable over HTTP by construction (a device JWT only exists after
    // activation, which starts the trial) - it stays as a defensive branch.

    // --- expired subscription -> 403 (mutual backup gated, local access not) ---
    {
        let conn = state.db.lock().unwrap();
        let now = state.now();
        conn.execute(
            "UPDATE devices SET expires_at = ?1 WHERE id = ?2",
            rusqlite::params![now - 1, dev],
        )
        .unwrap();
    }
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/pair/invite",
        Some(json!({"syncthing_id": st_id})),
        Some(&tok),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(v["message"].as_str().unwrap().contains("mutual backup"));
}

// ---------------------------------------------------------------------------
// Admin UI backend (M13): login gate, devices shape, standalone codes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_login_disabled_without_admin_token() {
    let mut state = test_state();
    state.admin_token = None; // simulate FENGDOG_ADMIN_TOKEN unset

    // --- login -> 503 (admin feature disabled) ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/admin/login",
        Some(json!({"token": "whatever"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["ok"], false);
    assert!(v["message"].as_str().unwrap().contains("FENGDOG_ADMIN_TOKEN"));

    // --- admin endpoints reject the raw bootstrap token too ---
    let (st, _) = call(state.clone(), "GET", "/api/admin/devices", None, Some(TEST_ADMIN_TOKEN)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = call(state.clone(), "GET", "/api/admin/codes", None, Some(TEST_ADMIN_TOKEN)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // --- with the token configured, the same login works (roundtrip) ---
    state.admin_token = Some(TEST_ADMIN_TOKEN.to_string());
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/admin/login",
        Some(json!({"token": "wrong-token"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/admin/login",
        Some(json!({"token": TEST_ADMIN_TOKEN})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["expires_in_secs"], 12 * 3600);
}

#[tokio::test]
async fn admin_devices_list_shape() {
    let state = test_state();
    let (_, v) = call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": "shape-node", "node_kind": "phone", "hardware_id": "HW-SHAPE-1"})),
        None,
    )
    .await;
    let code = v["device"]["activation_code"].as_str().unwrap().to_string();
    let (_, v) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": code, "hardware_id": "HW-SHAPE-1"})),
        None,
    )
    .await;
    let dev_tok = v["token"].as_str().unwrap().to_string();
    call(
        state.clone(),
        "POST",
        "/api/heartbeat",
        Some(json!({"disk_free_bytes": 42, "online": true, "version": "9.9.9"})),
        Some(&dev_tok),
    )
    .await;

    let (_, v) = call(
        state.clone(),
        "POST",
        "/api/admin/login",
        Some(json!({"token": TEST_ADMIN_TOKEN})),
        None,
    )
    .await;
    let jwt = v["token"].as_str().unwrap().to_string();
    let (st, v) = call(state.clone(), "GET", "/api/admin/devices", None, Some(&jwt)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["ok"], true);
    assert!(v["server_time"].is_i64());
    assert_eq!(v["count"], 1);
    let d = &v["devices"][0];
    // Fields the admin page renders (name / kind / subscription / heartbeat /
    // online verdict), plus the admin-only activation code.
    assert_eq!(d["name"], "shape-node");
    assert_eq!(d["node_kind"], "phone");
    assert_eq!(d["subscription_state"], "trial");
    assert!(d["last_heartbeat_at"].is_i64());
    assert_eq!(d["online"], true);
    assert!(d["activation_code"].is_string());
    assert_eq!(d["hardware_id"], "HW-SHAPE-1");
    assert!(d["expires_at"].is_i64());
    assert!(d["plan"].is_string());
}

#[tokio::test]
async fn admin_codes_issue_revoke_roundtrip() {
    let state = test_state();

    // --- login for the admin JWT ---
    let (_, v) = call(
        state.clone(),
        "POST",
        "/api/admin/login",
        Some(json!({"token": TEST_ADMIN_TOKEN})),
        None,
    )
    .await;
    let jwt = v["token"].as_str().unwrap().to_string();

    // --- issue without auth -> 401; bad kind -> 400 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/admin/codes",
        Some(json!({"kind": "pro"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/admin/codes",
        Some(json!({"kind": "golden"})),
        Some(&jwt),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // --- issue a pro code ---
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/admin/codes",
        Some(json!({"kind": "pro"})),
        Some(&jwt),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let pro_code = v["code"]["code"].as_str().unwrap().to_string();
    assert_eq!(pro_code.len(), 8);
    assert_eq!(v["code"]["kind"], "pro");
    assert_eq!(v["code"]["status"], "issued");

    // --- the code list shows it ---
    let (st, v) = call(state.clone(), "GET", "/api/admin/codes", None, Some(&jwt)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["count"], 1);
    assert_eq!(v["codes"][0]["code"], pro_code);

    // --- a registered device redeems the pro code -> active plan, 1 year ---
    let (_, v) = call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": "pro-node", "node_kind": "computer", "hardware_id": "HW-CODE-A"})),
        None,
    )
    .await;
    assert!(v["device"]["activation_code"].as_str().unwrap() != pro_code);
    let before = state.now();
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": pro_code, "hardware_id": "HW-CODE-A"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["device"]["plan"], "active");
    assert_eq!(v["subscription_state"], "active");
    let exp = v["expires_at"].as_i64().unwrap();
    assert!(exp > before + 364 * 86400 && exp < before + 366 * 86400);

    // --- the code is burned: second device -> 409 ---
    call(
        state.clone(),
        "POST",
        "/api/register",
        Some(json!({"device_name": "pro-node-2", "node_kind": "computer", "hardware_id": "HW-CODE-B"})),
        None,
    )
    .await;
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": pro_code, "hardware_id": "HW-CODE-B"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // --- revoke: unknown -> 404; used -> 409 ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/admin/codes/revoke",
        Some(json!({"code": "ZZZZZZZZ"})),
        Some(&jwt),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/admin/codes/revoke",
        Some(json!({"code": pro_code})),
        Some(&jwt),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // --- issue + revoke a trial code; redemption is refused ---
    let (_, v) = call(
        state.clone(),
        "POST",
        "/api/admin/codes",
        Some(json!({"kind": "trial"})),
        Some(&jwt),
    )
    .await;
    let trial_code = v["code"]["code"].as_str().unwrap().to_string();
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/admin/codes/revoke",
        Some(json!({"code": trial_code})),
        Some(&jwt),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["code"]["status"], "revoked");
    // revoke again: idempotent
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/admin/codes/revoke",
        Some(json!({"code": trial_code})),
        Some(&jwt),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": trial_code, "hardware_id": "HW-CODE-B"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["message"].as_str().unwrap().contains("revoked"));

    // --- unregistered hardware cannot redeem a valid code ---
    let (_, v) = call(
        state.clone(),
        "POST",
        "/api/admin/codes",
        Some(json!({"kind": "trial"})),
        Some(&jwt),
    )
    .await;
    let fresh = v["code"]["code"].as_str().unwrap().to_string();
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": fresh, "hardware_id": "HW-NEVER-REGISTERED"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // --- an already activated device cannot burn a second code ---
    let (st, _) = call(
        state.clone(),
        "POST",
        "/api/activate",
        Some(json!({"activation_code": fresh, "hardware_id": "HW-CODE-A"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}
