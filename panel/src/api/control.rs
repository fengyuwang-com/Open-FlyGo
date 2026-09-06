// /api/control - FlyGo control-plane client (panel side, "computer" node).
//
// The panel talks to the FlyGo control plane (API contract in
// ../../control-plane/API-CONTRACT.md, relative to this file) in three flows:
//   register  -> binds a stable hardware id, receives an 8-char code
//   activate  -> code + hardware id -> device JWT + subscription
//   heartbeat -> periodic POST keeping the node online and refreshing the
//                local subscription cache
// All node state (hardware id, activation code, JWT, subscription) is
// persisted atomically in <data>/control-state.json (tmp + rename, same
// pattern as api/components.rs) so the panel survives restarts without
// re-registering. The JWT never enters flygo.toml (secrets stay out of the
// config file, matching the project policy).
//
// Endpoints:
//   POST /api/control/register  -> {ok, activation_code, hardware_id}
//   POST /api/control/activate  -> {ok, token, subscription} (body {code})
//   GET  /api/control/status    -> local cache (+ live=1: real GET /api/status)
//   POST /api/control/settings  -> partial [control] section update of
//                                  flygo.toml (body {enabled?, base_url?,
//                                  device_name?, heartbeat_secs?, upload_max_mb?})
//   POST /api/control/heartbeat -> run one heartbeat now (UI "retry now");
//                                  serialized against the background loop
//
// Settings are re-read from the config file on every call (settings.rs
// Settings::load()), so [control] changes apply without a panel restart.
// Heartbeat failures are silent and unpenalized (node keeps its local
// state; the next tick retries) per the control-plane contract.
//
// All code comments are pure ASCII.

use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Serializes heartbeats (background loop + the manual "retry now" button)
/// so two POST /api/heartbeat can never overlap. The call itself is
/// idempotent, but a single in-flight attempt keeps the retry state
/// (fail count, next-retry estimate) truthful.
static HEARTBEAT_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// CREATE_NO_WINDOW: spawned console helpers (reg.exe) must not flash.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// REST timeout for control-plane calls (register/activate can be slow).
const PLANE_TIMEOUT_SECS: u64 = 8;

// ---------------------------------------------------------------------------
// persisted node state
// ---------------------------------------------------------------------------

/// Subscription as shown to the user (state normalized for the UI).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Subscription {
    /// trial | active (cancelled stays active tier for display).
    pub tier: String,
    /// unactivated | trial | active | expiring | expired | cancelled.
    pub state: String,
    /// Unix epoch seconds (UTC); None = no expiry (not activated).
    pub expires_at: Option<i64>,
}

/// Everything the panel knows about its control-plane identity. Persisted as
/// JSON in <data>/control-state.json.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ControlState {
    pub hardware_id: String,
    pub device_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<Subscription>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_ok: Option<bool>,
    /// Consecutive heartbeat failures (reset to 0 on the first success).
    /// Drives the exponential backoff and the "plane unreachable" UI state.
    #[serde(default)]
    pub heartbeat_fail_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Serializes state writes (heartbeat task + API handlers may race).
static STATE_WRITE_LOCK: Mutex<()> = Mutex::new(());

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn state_path() -> PathBuf {
    crate::config::get().control_state.clone()
}

fn load_state() -> ControlState {
    let path = state_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => ControlState::default(),
    }
}

/// Atomic write: tmp file + rename (mirrors api/components.rs). Corrupt or
/// missing state simply falls back to defaults on the next read.
fn save_state(st: &ControlState) -> Result<(), String> {
    let path = state_path();
    let _guard = STATE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {} failed: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(st)
        .map_err(|e| format!("serialize control state failed: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("replace {} failed: {e}", path.display()) })
}

// ---------------------------------------------------------------------------
// heartbeat retry state (fail count + backoff + next-retry estimate)
// ---------------------------------------------------------------------------

/// Exponential backoff for heartbeat retries: base * 2^fail_count, capped
/// just below the control-plane offline window (15 min) so the node is never
/// flagged offline by its own cadence. The 10s floor matches the settings
/// validation for [control].heartbeat_secs.
pub fn heartbeat_backoff(fail_count: u64, base_secs: u64) -> u64 {
    let cap = crate::settings::CONTROL_OFFLINE_WINDOW_SECS - 1;
    let shift = fail_count.min(20); // 2^20 already far beyond the cap
    base_secs.saturating_mul(1u64 << shift).min(cap).max(10)
}

/// Seconds until the next automatic heartbeat attempt, given the last attempt
/// timestamp and the consecutive-failure count. None when not in a failure
/// streak (the normal cadence applies; no countdown to show).
pub fn next_retry_in_secs(
    fail_count: u64,
    last_attempt_at: Option<i64>,
    base_secs: u64,
    now_secs: i64,
) -> Option<i64> {
    if fail_count == 0 {
        return None;
    }
    let iv = heartbeat_backoff(fail_count, base_secs) as i64;
    let since = last_attempt_at.map(|t| now_secs.saturating_sub(t)).unwrap_or(0);
    Some(iv.saturating_sub(since).max(0))
}

// ---------------------------------------------------------------------------
// hardware id (stable, persisted)
// ---------------------------------------------------------------------------

/// Stable hardware fingerprint: Windows MachineGuid from the registry,
/// prefixed to stay within the contract's 4-128 char rule. Stable across
/// panel reinstalls; falls back to a random id when the registry is
/// unreadable. Blocking (reg.exe) - call inside spawn_blocking.
pub fn ensure_hardware_id(st: &mut ControlState) -> String {
    if !st.hardware_id.trim().is_empty() {
        return st.hardware_id.clone();
    }
    let guid = machine_guid().filter(|g| g.len() >= 8).unwrap_or_else(random_hardware_id);
    st.hardware_id = format!("PANEL-{guid}");
    st.hardware_id.clone()
}

fn machine_guid() -> Option<String> {
    let out = Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains("MachineGuid"))?;
    let guid = line.split_whitespace().last()?.trim().trim_matches('"').to_string();
    if guid.len() >= 8 { Some(guid) } else { None }
}

fn random_hardware_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    nanos.hash(&mut h);
    std::process::id().hash(&mut h);
    format!("{:016x}", h.finish())
}

// ---------------------------------------------------------------------------
// subscription state machine (normalization for the UI)
// ---------------------------------------------------------------------------

/// Normalize a control-plane subscription into (state, tier) for display:
///  - server "pending" or absent -> "unactivated" (registered, not activated)
///  - local expiry always wins: past expires_at -> "expired"
///  - active with <= 30 days left -> "expiring" (same rule as the plane)
pub fn derive_subscription(
    server_state: Option<&str>,
    expires_at: Option<i64>,
    now_secs: i64,
) -> (String, String) {
    let expired = expires_at.is_some_and(|e| now_secs >= e);
    let near = expires_at.is_some_and(|e| e - now_secs <= crate::settings::CONTROL_EXPIRING_WINDOW_SECS);
    let st = server_state.unwrap_or("pending");
    let (state, tier) = match st {
        "cancelled" => ("cancelled", "active"),
        "pending" => ("unactivated", "trial"),
        "trial" => {
            if expired { ("expired", "trial") } else { ("trial", "trial") }
        }
        "active" => {
            if expired { ("expired", "active") } else if near { ("expiring", "active") } else { ("active", "active") }
        }
        "expiring" => {
            if expired { ("expired", "active") } else { ("expiring", "active") }
        }
        "expired" => ("expired", "active"),
        _ => ("unactivated", "trial"),
    };
    (state.to_string(), tier.to_string())
}

/// Extract (state, expires_at) from a plane response, tolerating both the
/// flat shape ({subscription_state, expires_at}) and the nested one
/// ({subscription: {state, expires_at}}).
fn sub_from_value(v: &Value) -> (Option<String>, Option<i64>) {
    let flat_state = v.get("subscription_state").and_then(|s| s.as_str());
    let flat_exp = v.get("expires_at").and_then(|e| e.as_i64());
    let nested_state = v
        .get("subscription")
        .and_then(|s| s.get("state"))
        .and_then(|s| s.as_str());
    let nested_exp = v
        .get("subscription")
        .and_then(|s| s.get("expires_at"))
        .and_then(|e| e.as_i64());
    (
        flat_state.or(nested_state).map(|s| s.to_string()),
        flat_exp.or(nested_exp),
    )
}

// ---------------------------------------------------------------------------
// control-plane HTTP client (ureq, blocking; call inside spawn_blocking)
// ---------------------------------------------------------------------------

/// One control-plane REST call. Returns the parsed JSON body; errors are
/// strings with the server message when available ({"ok":false,"message":..}
/// bodies, or HTTP status codes).
fn call_plane(
    base_url: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> Result<Value, String> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), path.trim_start_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(PLANE_TIMEOUT_SECS))
        .build();
    let req = match method {
        "GET" => agent.get(&url),
        _ => agent.post(&url),
    };
    let req = match token {
        Some(t) => req.set("Authorization", &format!("Bearer {t}")),
        None => req,
    };
    let resp = match body {
        Some(b) => req.set("Content-Type", "application/json").send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => r
            .into_json::<Value>()
            .map_err(|e| format!("control plane returned bad JSON ({url}): {e}")),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| format!("HTTP {code}"));
            Err(format!("{path}: {code} {msg}"))
        }
        Err(e) => Err(format!("control plane unreachable ({url}): {e}")),
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// POST /api/control/register - bind this panel as a "computer" node and
/// receive (or reuse) the 8-char activation code. Idempotent: a cached code
/// is returned without a network call.
pub async fn register() -> Json<Value> {
    let s = crate::settings::Settings::load();
    let base = s.control_base_url();
    let name = s.control_device_name();
    let r = tokio::task::spawn_blocking(move || register_blocking(&base, &name))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "message": "register task panicked"}));
    Json(r)
}

fn register_blocking(base_url: &str, device_name: &str) -> Value {
    let mut st = load_state();
    if let Some(code) = &st.activation_code {
        if !code.trim().is_empty() {
            return json!({
                "ok": true,
                "activation_code": code,
                "hardware_id": st.hardware_id,
                "cached": true,
            });
        }
    }
    let hw = ensure_hardware_id(&mut st);
    let body = json!({
        "device_name": device_name,
        "node_kind": "computer",
        "hardware_id": hw,
    });
    match call_plane(base_url, "POST", "/api/register", Some(&body), None) {
        Ok(v) if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) => {
            crate::api::stats::record_control(true);
            let code = v
                .get("device")
                .and_then(|d| d.get("activation_code"))
                .and_then(|c| c.as_str())
                .or_else(|| v.get("activation_code").and_then(|c| c.as_str()))
                .unwrap_or("")
                .trim()
                .to_string();
            if code.is_empty() {
                // Business "ok" but no code - count as a failed call.
                crate::api::stats::record_control(false);
                return json!({"ok": false, "message": "control plane returned no activation code"});
            }
            st.hardware_id = hw.clone();
            st.device_name = device_name.to_string();
            st.activation_code = Some(code.clone());
            st.registered_at = Some(now());
            st.last_error = None;
            if let Err(e) = save_state(&st) {
                return json!({"ok": false, "message": e});
            }
            json!({
                "ok": true,
                "activation_code": code,
                "hardware_id": hw,
                "device_name": st.device_name,
                "cached": false,
            })
        }
        Ok(v) => {
            crate::api::stats::record_control(false);
            json!({
                "ok": false,
                "message": v.get("message").and_then(|m| m.as_str()).unwrap_or("register failed").to_string(),
            })
        }
        Err(e) => {
            crate::api::stats::record_control(false);
            json!({"ok": false, "message": e})
        }
    }
}

/// POST /api/control/activate - body {"code": "XXXX-XXXX"}; binds the code
/// to this hardware, stores the device JWT + subscription locally.
pub async fn activate(body: String) -> Json<Value> {
    let code = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_string))
        .map(|c| c.trim().to_uppercase())
        .unwrap_or_default();
    if code.is_empty() {
        return Json(json!({"ok": false, "message": "activation code is required (8 chars, e.g. ABCD-EFGH)"}));
    }
    let s = crate::settings::Settings::load();
    let base = s.control_base_url();
    let r = tokio::task::spawn_blocking(move || activate_blocking(&base, &code))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "message": "activate task panicked"}));
    Json(r)
}

fn activate_blocking(base_url: &str, code: &str) -> Value {
    let mut st = load_state();
    // Idempotent activation: this exact code already bound this node. Serve
    // the cached identity without a second plane call - repeated clicks (or a
    // retried request) must never double-bind, and the cache works offline.
    if st.activation_code.as_deref() == Some(code) && st.token.is_some() {
        return json!({
            "ok": true,
            "token": st.token.clone().unwrap_or_default(),
            "subscription": st.subscription.clone(),
            "hardware_id": st.hardware_id.clone(),
            "cached": true,
        });
    }
    let hw = ensure_hardware_id(&mut st);
    let body = json!({
        "activation_code": code,
        "hardware_id": hw,
    });
    match call_plane(base_url, "POST", "/api/activate", Some(&body), None) {
        Ok(v) if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) => {
            crate::api::stats::record_control(true);
            let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
            if token.is_empty() {
                return json!({"ok": false, "message": "control plane returned no token"});
            }
            let (server_state, expires_at) = sub_from_value(&v);
            let (state, tier) = derive_subscription(server_state.as_deref(), expires_at, now());
            let sub = Subscription { tier, state, expires_at };
            st.hardware_id = hw.clone();
            st.token = Some(token.clone());
            st.subscription = Some(sub.clone());
            st.activated_at = Some(now());
            st.last_error = None;
            if let Err(e) = save_state(&st) {
                return json!({"ok": false, "message": e});
            }
            json!({"ok": true, "token": token, "subscription": sub, "hardware_id": hw})
        }
        Ok(v) => {
            crate::api::stats::record_control(false);
            json!({
                "ok": false,
                "message": v.get("message").and_then(|m| m.as_str()).unwrap_or("activate failed").to_string(),
            })
        }
        Err(e) => {
            crate::api::stats::record_control(false);
            json!({"ok": false, "message": e})
        }
    }
}

/// GET /api/control/status - local cached state; ?live=1 additionally does a
/// real GET /api/status with the stored JWT and refreshes the cache.
pub async fn status(params: axum::extract::Query<Value>) -> Json<Value> {
    let st = load_state();
    let s = crate::settings::Settings::load();
    let enabled = s.control_enabled();
    let base_url = s.control_base_url();
    let device_name = s.control_device_name();
    let live = params
        .get("live")
        .and_then(|v| v.as_str())
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    let base_secs = s.control_heartbeat_secs()
        .clamp(10, crate::settings::CONTROL_OFFLINE_WINDOW_SECS - 1);
    let mut out = status_json(&st, enabled, &base_url, &device_name, base_secs);
    if live {
        if let Some(token) = st.token.clone() {
            let base = base_url.clone();
            let r = tokio::task::spawn_blocking(move || {
                call_plane(&base, "GET", "/api/status", None, Some(&token))
            })
            .await
            .unwrap_or_else(|_| Err("status task panicked".into()));
            match r {
                Ok(v) => {
                    crate::api::stats::record_control(true);
                    out["live"] = v.clone();
                    let (server_state, expires_at) = sub_from_value(&v);
                    if let Some(exp) = expires_at {
                        let (state, tier) = derive_subscription(server_state.as_deref(), Some(exp), now());
                        let mut st2 = load_state();
                        let changed = st2
                            .subscription
                            .as_ref()
                            .map(|s| s.state != state || s.expires_at != Some(exp))
                            .unwrap_or(true);
                        if changed {
                            let sub = Subscription { tier, state, expires_at: Some(exp) };
                            out["subscription"] =
                                serde_json::to_value(&sub).unwrap_or(Value::Null);
                            st2.subscription = Some(sub);
                            st2.last_error = None;
                            let _ = save_state(&st2);
                        }
                    }
                }
                Err(e) => {
                    crate::api::stats::record_control(false);
                    out["live_error"] = json!(e);
                }
            }
        } else {
            out["live_error"] = json!("not activated (no token)");
        }
    }
    Json(out)
}

fn status_json(
    st: &ControlState,
    enabled: bool,
    base_url: &str,
    device_name: &str,
    base_secs: u64,
) -> Value {
    let unreachable = st.token.is_some() && st.last_heartbeat_ok == Some(false);
    let next_retry = if unreachable {
        next_retry_in_secs(st.heartbeat_fail_count, st.last_heartbeat_at, base_secs, now())
    } else {
        None
    };
    json!({
        "ok": true,
        "enabled": enabled,
        "base_url": base_url,
        "device_name": if st.device_name.is_empty() { device_name } else { &st.device_name },
        "hardware_id": st.hardware_id,
        "registered": st.activation_code.is_some(),
        "activation_code": st.activation_code,
        "activated": st.token.is_some(),
        "subscription": st.subscription,
        "registered_at": st.registered_at,
        "activated_at": st.activated_at,
        "last_heartbeat_at": st.last_heartbeat_at,
        "last_heartbeat_ok": st.last_heartbeat_ok,
        "heartbeat_fail_count": st.heartbeat_fail_count,
        "plane_unreachable": unreachable,
        "next_retry_in": next_retry,
        "last_error": st.last_error,
    })
}

/// POST /api/control/settings - partial update of the [control] section in
/// flygo.toml. Only the present fields are changed; the rest of the file
/// (comments included) is preserved. Atomic tmp + rename write.
pub async fn settings(body: String) -> Json<Value> {
    let patch: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return Json(json!({"ok": false, "message": format!("invalid JSON: {e}")})),
    };
    if patch.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Json(json!({"ok": false, "message": "no fields to update"}));
    }

    let target = crate::settings::config_target();
    let existing = std::fs::read_to_string(&target).unwrap_or_default();
    // Merge: existing file values > defaults, then the patch on top.
    let mut cfg: crate::settings::FileConfig =
        toml::from_str(&existing).unwrap_or_default();
    let c = &mut cfg.control;
    if let Some(v) = patch.get("enabled") {
        match v.as_bool() {
            Some(b) => c.enabled = b,
            None => return Json(json!({"ok": false, "message": "enabled must be a boolean"})),
        }
    }
    if let Some(v) = patch.get("base_url") {
        match v.as_str() {
            Some(s) => c.base_url = s.trim().to_string(),
            None => return Json(json!({"ok": false, "message": "base_url must be a string"})),
        }
    }
    if let Some(v) = patch.get("device_name") {
        match v.as_str() {
            Some(s) => c.device_name = s.trim().to_string(),
            None => return Json(json!({"ok": false, "message": "device_name must be a string"})),
        }
    }
    if let Some(v) = patch.get("heartbeat_secs") {
        match v.as_u64() {
            Some(n) => c.heartbeat_secs = n,
            None => return Json(json!({"ok": false, "message": "heartbeat_secs must be an integer"})),
        }
    }
    if let Some(v) = patch.get("upload_max_mb") {
        match v.as_u64() {
            Some(n) => c.upload_max_mb = n,
            None => return Json(json!({"ok": false, "message": "upload_max_mb must be an integer"})),
        }
    }
    let errs = crate::settings::validate(&cfg);
    if !errs.is_empty() {
        return Json(json!({"ok": false, "errors": errs}));
    }

    let text = upsert_control_section(&existing, &cfg.control);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Json(json!({"ok": false, "message": format!("create dir {} failed: {e}", parent.display())}));
            }
        }
    }
    let tmp = target.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, text) {
        return Json(json!({"ok": false, "message": format!("write {} failed: {e}", tmp.display())}));
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Json(json!({"ok": false, "message": format!("replace {} failed: {e}", target.display())}));
    }
    Json(json!({
        "ok": true,
        "file": target.to_string_lossy(),
        "note": "applies to the heartbeat on the next tick (no restart needed)",
    }))
}

/// Replace or append the [control] section of a flygo.toml text, keeping all
/// other sections and comments untouched. All five fields are written (the
/// caller passes the merged values), so the section is always complete.
fn upsert_control_section(text: &str, c: &crate::settings::ControlCfg) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_ctl = false;
    let mut found = false;
    for line in text.lines() {
        let t = line.trim();
        if t == "[control]" {
            in_ctl = true;
            found = true;
            continue; // replaced below
        }
        if in_ctl && t.starts_with('[') && t.ends_with(']') {
            in_ctl = false; // next section header ends the block
        }
        if !in_ctl {
            out.push(line);
        }
    }
    let block = format!(
        "[control]\n\
         enabled = {}\n\
         base_url = \"{}\"\n\
         device_name = \"{}\"\n\
         heartbeat_secs = {}\n\
         upload_max_mb = {}\n",
        c.enabled,
        toml_escape(&c.base_url),
        toml_escape(&c.device_name),
        c.heartbeat_secs,
        c.upload_max_mb,
    );
    let mut joined = out.join("\n");
    if !joined.is_empty() && !joined.ends_with('\n') {
        joined.push('\n');
    }
    if found && !joined.ends_with("\n\n") {
        joined.push('\n');
    }
    joined.push_str(&block);
    joined
}

/// Minimal TOML string escaping (backslash + double quote).
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// heartbeat task
// ---------------------------------------------------------------------------

/// Background heartbeat loop. Interval is re-read from [control] each cycle
/// so config edits apply without a restart; skipped entirely while disabled
/// or not activated. Failures are logged and retried on the next tick
/// (no penalty, per the control-plane contract).
pub async fn heartbeat_task() {
    loop {
        let s = crate::settings::Settings::load();
        let base_secs = s.control_heartbeat_secs()
            .clamp(10, crate::settings::CONTROL_OFFLINE_WINDOW_SECS - 1);
        // Back off after consecutive failures (2x per miss, capped below the
        // offline window); the UI shows the countdown via next_retry_in.
        let fail_count = load_state().heartbeat_fail_count;
        let secs = heartbeat_backoff(fail_count, base_secs);
        tokio::time::sleep(Duration::from_secs(secs)).await;
        if !s.control_enabled() {
            continue;
        }
        let base = s.control_base_url();
        // Serialized against the manual "retry now" endpoint: at most one
        // in-flight heartbeat, so the retry state stays truthful.
        let _guard = HEARTBEAT_LOCK.lock().await;
        match heartbeat_once(&base).await {
            Ok(()) => eprintln!("[control] heartbeat ok"),
            Err(e) => eprintln!("[control] heartbeat failed: {e} (no penalty; backoff next tick)"),
        }
    }
}

/// One heartbeat: POST /api/heartbeat with disk free / online / version,
/// then refresh the local subscription cache from the response.
pub async fn heartbeat_once(base_url: &str) -> Result<(), String> {
    let st = load_state();
    let Some(token) = st.token.clone() else {
        return Err("not activated yet (no token)".into());
    };
    let disk = tokio::task::spawn_blocking(disk_free_bytes)
        .await
        .unwrap_or(0);
    let version = read_version();
    let body = json!({
        "disk_free_bytes": disk,
        "online": true,
        "version": version,
    });
    let base = base_url.to_string();
    let r = tokio::task::spawn_blocking(move || {
        call_plane(&base, "POST", "/api/heartbeat", Some(&body), Some(&token))
    })
    .await
    .unwrap_or_else(|_| Err("heartbeat task panicked".into()));

    let mut st2 = load_state();
    match r {
        Ok(v) => {
            let (server_state, expires_at) = sub_from_value(&v);
            let (state, tier) = derive_subscription(server_state.as_deref(), expires_at, now());
            let sub = Subscription { tier, state, expires_at };
            st2.subscription = Some(sub);
            st2.last_heartbeat_at = Some(now());
            st2.last_heartbeat_ok = Some(true);
            st2.heartbeat_fail_count = 0;
            st2.last_error = None;
            // Persist every cycle: the heartbeat timestamps must survive even
            // when the subscription itself is unchanged.
            let _ = save_state(&st2);
            crate::api::stats::record_control(true);
            Ok(())
        }
        Err(e) => {
            st2.last_heartbeat_at = Some(now());
            st2.last_heartbeat_ok = Some(false);
            st2.heartbeat_fail_count = st2.heartbeat_fail_count.saturating_add(1);
            st2.last_error = Some(e.clone());
            let _ = save_state(&st2);
            crate::api::stats::record_control(false);
            Err(e)
        }
    }
}

/// POST /api/control/heartbeat - run one heartbeat immediately (the UI's
/// "retry now"). Returns ok:false (no state change) when reporting is
/// disabled. Safe to repeat: POST /api/heartbeat is idempotent, and the call
/// is serialized against the background loop (HEARTBEAT_LOCK).
pub async fn heartbeat_now() -> Json<Value> {
    let s = crate::settings::Settings::load();
    if !s.control_enabled() {
        return Json(json!({"ok": false, "message": "control reporting is disabled - enable it first"}));
    }
    let base = s.control_base_url();
    let _guard = HEARTBEAT_LOCK.lock().await;
    match heartbeat_once(&base).await {
        Ok(()) => Json(json!({"ok": true, "message": "heartbeat ok"})),
        Err(e) => Json(json!({"ok": false, "message": e})),
    }
}

/// Total free space across the local fixed disks (sysinfo, same data source
/// as /api/health). Blocking - call inside spawn_blocking.
fn disk_free_bytes() -> u64 {
    use sysinfo::{DiskKind, Disks};
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| d.kind() != DiskKind::Unknown(-1))
        .map(|d| d.available_space())
        .sum()
}

/// Panel version from the install-root Version file (contract field
/// "version"); empty string when unreadable.
fn read_version() -> String {
    let path = crate::config::get().root.join("Version");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// unit tests (pure logic; no disk / network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_subscription_covers_the_state_machine() {
        let now = 1_000_000_000i64;
        // pending / absent -> unactivated
        assert_eq!(derive_subscription(None, None, now), ("unactivated".into(), "trial".into()));
        assert_eq!(derive_subscription(Some("pending"), None, now), ("unactivated".into(), "trial".into()));
        // trial within window
        assert_eq!(
            derive_subscription(Some("trial"), Some(now + 100), now),
            ("trial".into(), "trial".into())
        );
        // active -> expiring within 30 days
        assert_eq!(
            derive_subscription(Some("active"), Some(now + 10 * 86400), now),
            ("expiring".into(), "active".into())
        );
        assert_eq!(
            derive_subscription(Some("active"), Some(now + 60 * 86400), now),
            ("active".into(), "active".into())
        );
        // expiry always wins
        assert_eq!(
            derive_subscription(Some("active"), Some(now - 1), now),
            ("expired".into(), "active".into())
        );
        assert_eq!(
            derive_subscription(Some("trial"), Some(now - 1), now),
            ("expired".into(), "trial".into())
        );
        // expiring / expired / cancelled pass through
        assert_eq!(
            derive_subscription(Some("expiring"), Some(now + 100), now),
            ("expiring".into(), "active".into())
        );
        assert_eq!(
            derive_subscription(Some("expired"), Some(now - 5), now),
            ("expired".into(), "active".into())
        );
        assert_eq!(
            derive_subscription(Some("cancelled"), Some(now + 100), now),
            ("cancelled".into(), "active".into())
        );
    }

    #[test]
    fn sub_from_value_tolerates_flat_and_nested_shapes() {
        let flat = json!({"subscription_state": "active", "expires_at": 123});
        assert_eq!(sub_from_value(&flat), (Some("active".into()), Some(123)));
        let nested = json!({"subscription": {"state": "trial", "expires_at": 456}});
        assert_eq!(sub_from_value(&nested), (Some("trial".into()), Some(456)));
        let none = json!({"ok": true});
        assert_eq!(sub_from_value(&none), (None, None));
    }

    #[test]
    fn upsert_control_preserves_other_sections_and_comments() {
        let text = "# top comment\n[server]\naddr = \"0.0.0.0:8388\"\n\n# sync note\n[sync]\ninterval_secs = 600\n";
        let c = crate::settings::ControlCfg::default();
        let out = upsert_control_section(text, &c);
        assert!(out.contains("# top comment"));
        assert!(out.contains("[server]\naddr = \"0.0.0.0:8388\""));
        assert!(out.contains("# sync note"));
        assert!(out.contains("[control]\nenabled = false\nbase_url = \"http://127.0.0.1:9001\""));
        // Appending when the section is missing must not duplicate it.
        let out2 = upsert_control_section(&out, &c);
        assert_eq!(out2.matches("[control]").count(), 1);
    }

    #[test]
    fn upsert_control_replaces_existing_section() {
        let text = "[control]\nenabled = true\nbase_url = \"http://old:1\"\nheartbeat_secs = 60\nupload_max_mb = 100\ndevice_name = \"x\"\n[server]\naddr = \"0.0.0.0:8388\"\n";
        let mut c = crate::settings::ControlCfg::default();
        c.enabled = true;
        let out = upsert_control_section(text, &c);
        assert_eq!(out.matches("[control]").count(), 1);
        assert!(out.contains("base_url = \"http://127.0.0.1:9001\""));
        assert!(out.contains("heartbeat_secs = 300"));
        assert!(out.contains("[server]"));
        assert!(!out.contains("http://old"));
    }

    #[test]
    fn toml_escape_quotes() {
        assert_eq!(toml_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn heartbeat_backoff_doubles_and_caps() {
        // Doubling per consecutive failure.
        assert_eq!(heartbeat_backoff(0, 300), 300);
        assert_eq!(heartbeat_backoff(1, 300), 600);
        // 2^2 * 300 = 1200 already crosses the 900s offline window, so the
        // cap (window - 1) applies from the second miss onward.
        assert_eq!(heartbeat_backoff(2, 300), 899);
        assert_eq!(heartbeat_backoff(3, 300), 899);
        assert_eq!(heartbeat_backoff(50, 300), 899);
        // 10s floor for tiny cadences.
        assert_eq!(heartbeat_backoff(0, 10), 10);
    }

    #[test]
    fn next_retry_in_counts_down_from_the_backoff_slot() {
        // Two consecutive misses at base 300 -> the backoff slot is capped at
        // 899s (offline window - 1); 5s elapsed -> 894s left.
        let n = next_retry_in_secs(2, Some(1000), 300, 1005);
        assert_eq!(n, Some(894));
        // A single miss at base 60 -> 120s slot; 10s elapsed -> 110s left.
        let n = next_retry_in_secs(1, Some(1000), 60, 1010);
        assert_eq!(n, Some(110));
        // Slot already elapsed -> the retry is due immediately.
        let n = next_retry_in_secs(2, Some(1000), 300, 2500);
        assert_eq!(n, Some(0));
        // No failure streak -> no countdown to show.
        let n = next_retry_in_secs(0, Some(1000), 300, 1005);
        assert_eq!(n, None);
    }

    #[test]
    fn heartbeat_fail_count_defaults_to_zero() {
        // State files written before M15 lack the field; it must deserialize
        // to 0 (no failure streak) instead of erroring. device_name is a
        // mandatory field of every real state file (written at register).
        let st: ControlState =
            serde_json::from_str(r#"{"hardware_id":"x","device_name":"flygo-panel"}"#).unwrap();
        assert_eq!(st.heartbeat_fail_count, 0);
        let st2: ControlState = serde_json::from_str(
            r#"{"hardware_id":"x","device_name":"flygo-panel","heartbeat_fail_count":3}"#,
        )
        .unwrap();
        assert_eq!(st2.heartbeat_fail_count, 3);
    }
}
