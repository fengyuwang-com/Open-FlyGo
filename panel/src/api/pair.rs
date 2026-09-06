// /api/pair - mutual-backup orchestration (tech plan 4.2; the heart of the
// 499 CNY dual-node SKU). One-click pairing between two FlyGo nodes:
//
//   POST /api/pair/create   -> local Syncthing device ID (live, 2s timeout)
//                              -> control-plane invite -> 6-digit code
//   POST /api/pair/join     -> control-plane accept (peer id back)
//                              -> both sides add each other in Syncthing
//   GET  /api/pair/status   -> control-plane matched poll + local peer
//                              health (/rest/system/connections, 60s cache)
//   POST /api/pair/folders  -> per selected path PUT /rest/config/folders:
//                              send-receive + FORCED staggered versioning
//   GET  /api/pair/recover  -> per paired folder /rest/db/status progress
//                              (needBytes/globalBytes -> recovery percent)
//
// Honest three-state payloads (no fake errors): create/join are user actions
// and fail hard with HTTP 400 + {"ok":false,"error":"<reason>"} when either
// Syncthing or the control plane is down; status/recover always answer 200
// with a `degraded` part list so the page renders the real state plus a
// what-to-do hint (gather.rs convention).
//
// The control-plane JWT comes from data/control-state.json (written by
// api/control.rs); this module stays self-contained (only settings.rs +
// config.rs dependencies, same independence rule as gather.rs /
// onboarding.rs). The Syncthing API key is read from config.xml and used
// only in the X-API-Key header - never logged, never serialized.
//
// All code comments are pure ASCII.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Spec timeout for the local Syncthing probe (fast honest failure; a hung
/// daemon must not freeze the pairing wizard). Comes from
/// [sync].sync_timeout_secs (default 2s) so operators can tune it.
fn st_timeout() -> Duration {
    Duration::from_secs(crate::settings::Settings::load().sync_timeout_secs().max(1))
}

/// Control-plane call timeout (same class as api/control.rs).
const PLANE_TIMEOUT: Duration = Duration::from_secs(8);

/// Peer health cache TTL (plan 4.2: poll /rest/system/connections every
/// 60s; the panel status endpoint serves the cached snapshot in between).
const HEALTH_CACHE_SECS: u64 = 60;

/// Staggered versioning window: versions older than 7 days age out of the
/// retention schedule (maxAge=604800s) and expired versions are swept on a
/// 7-day cleanup cadence (cleanoutDays=7). Together this is the "undo
/// window" that stops a mistaken delete from propagating irrecoverably
/// (top risk #2 in the tech plan).
const STAGGERED_MAX_AGE_SECS: u64 = 604_800;
const STAGGERED_CLEANOUT_DAYS: u64 = 7;

// ---------------------------------------------------------------------------
// persisted pair state (data/pair-state.json, atomic tmp + rename)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PairState {
    /// This node's Syncthing device ID as seen at the last live probe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub my_syncthing_id: Option<String>,
    /// The peer's Syncthing device ID once a pairing succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_syncthing_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired_at: Option<i64>,
    /// Outstanding invite as inviter (display + poll hint for the UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite: Option<InviteInfo>,
    /// Folder paths provisioned for mutual backup (recovery page input).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InviteInfo {
    pub code: String,
    pub expires_at: i64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn state_path() -> PathBuf {
    crate::config::get().pair_state.clone()
}

fn load_state() -> PairState {
    match std::fs::read_to_string(state_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => PairState::default(),
    }
}

/// Atomic write: tmp + rename (components.rs / control.rs pattern).
fn save_state(st: &PairState) -> Result<(), String> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {} failed: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(st)
        .map_err(|e| format!("serialize pair state failed: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace {} failed: {e}", path.display())
    })
}

/// The control-plane device JWT written by api/control.rs (activate step).
/// None = this node never activated against the control plane.
fn stored_plane_token() -> Option<String> {
    let text = std::fs::read_to_string(crate::config::get().control_state.clone()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("token")
        .and_then(|t| t.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Syncthing REST plumbing (self-contained; convention of sync/gather modules)
// ---------------------------------------------------------------------------

fn sync_rest() -> String {
    crate::settings::Settings::load().syncthing_addr()
}

/// Read the Syncthing API key from config.xml (request header only).
fn read_apikey() -> Option<String> {
    let home = crate::config::get().syncthing_home.clone()?;
    let cfg = std::fs::read_to_string(home.join("config.xml")).ok()?;
    let start = cfg.find("<apikey>")? + "<apikey>".len();
    let end = cfg[start..].find("</apikey>")? + start;
    Some(cfg[start..end].to_string())
}

/// One blocking JSON call against the local Syncthing GUI API.
fn st_json(method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let Some(key) = read_apikey() else {
        return Err(format!(
            "Syncthing config.xml not found at {} (is Syncthing set up?)",
            crate::config::get()
                .syncthing_home
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "?".into())
        ));
    };
    let agent = ureq::AgentBuilder::new().timeout(st_timeout()).build();
    let url = format!("{}{path}", sync_rest());
    let req = agent.request(method, &url).set("X-API-Key", &key);
    let resp = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => r
            .into_json::<Value>()
            .map_err(|e| format!("GET {path}: bad json: {e}")),
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "{path}: HTTP {code} {}",
            r.into_string().unwrap_or_default().trim()
        )),
        Err(e) => Err(format!(
            "Syncthing not reachable at {} ({e}); start it in Overview -> Managed services",
            sync_rest()
        )),
    }
}

/// Live local device ID (/rest/system/status -> myID).
fn my_syncthing_id_live() -> Result<String, String> {
    let v = st_json("GET", "/rest/system/status", None)?;
    v.get("myID")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Syncthing answered without myID".to_string())
}

/// Add (or refresh) a device entry in the local Syncthing config.
/// PUT /rest/config/devices upserts by deviceID, so sending just the peer
/// never clobbers the existing device list.
fn add_peer_device(peer_id: &str) -> Result<(), String> {
    let body = json!([{ "deviceID": peer_id, "name": "FlyGo mutual backup peer" }]);
    st_json("PUT", "/rest/config/devices", Some(&body)).map(|_| ())
}

// ---------------------------------------------------------------------------
// control-plane client (ureq, blocking; only codes + device IDs are relayed)
// ---------------------------------------------------------------------------

fn call_plane(
    base_url: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> Result<Value, String> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), path.trim_start_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout(PLANE_TIMEOUT).build();
    let req = agent.request(method, &url);
    let req = match token {
        Some(t) => req.set("Authorization", &format!("Bearer {t}")),
        None => req,
    };
    let resp = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => r
            .into_json::<Value>()
            .map_err(|e| format!("control plane returned bad JSON: {e}")),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| format!("HTTP {code}"));
            Err(format!("{path}: {code} {msg}"))
        }
        Err(e) => Err(format!("control plane unreachable at {url}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// hard-fail envelope for user actions (real HTTP 400 + reason)
// ---------------------------------------------------------------------------

fn bad_request(reason: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({ "ok": false, "error": reason }).to_string(),
    )
        .into_response()
}

fn ok_json(v: Value) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// peer health (60s cache + consecutive-failure counter)
// ---------------------------------------------------------------------------

struct HealthCache {
    at: std::time::Instant,
    /// Cache key: entries are only reused for the same peer (a re-pairing
    /// must never serve the previous peer's health).
    peer_id: String,
    payload: Value,
}

static HEALTH: Mutex<Option<HealthCache>> = Mutex::new(None);
/// Consecutive failed Syncthing probes since the last success (error count
/// surfaced on the peer card; reset by any successful probe).
static PROBE_FAILURES: Mutex<u64> = Mutex::new(0);

fn peer_health(peer_id: Option<&str>) -> (Value, bool, u64) {
    // (health JSON, syncthing_up, probe_failures)
    let peer_key = peer_id.unwrap_or("").to_string();
    let mut cache = HEALTH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = cache.as_ref() {
        if c.peer_id == peer_key && c.at.elapsed().as_secs() < HEALTH_CACHE_SECS {
            let up = c.payload.get("syncthing_up").and_then(|b| b.as_bool()).unwrap_or(false);
            let fails = *PROBE_FAILURES.lock().unwrap_or_else(|e| e.into_inner());
            return (c.payload.clone(), up, fails);
        }
    }
    let (payload, up) = probe_peer_health(peer_id);
    {
        let mut fails = PROBE_FAILURES.lock().unwrap_or_else(|e| e.into_inner());
        *fails = if up { 0 } else { *fails + 1 };
    }
    let fails = *PROBE_FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    *cache = Some(HealthCache {
        at: std::time::Instant::now(),
        peer_id: peer_key,
        payload: payload.clone(),
    });
    (payload, up, fails)
}

/// One /rest/system/connections probe folded down to the peer's row.
fn probe_peer_health(peer_id: Option<&str>) -> (Value, bool) {
    let peer = match peer_id {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return (json!({ "syncthing_up": false, "peer_present": false }), false),
    };
    match st_json("GET", "/rest/system/connections", None) {
        Ok(v) => {
            let entry = v
                .get("connections")
                .and_then(|c| c.get(&peer))
                .cloned()
                .unwrap_or(Value::Null);
            let connected = entry.get("connected").and_then(|b| b.as_bool()).unwrap_or(false);
            let at = entry.get("at").and_then(|t| t.as_f64()).unwrap_or(0.0) as i64;
            let last_seen_secs = if at > 0 { (now() - at).max(0) } else { -1 };
            (
                json!({
                    "syncthing_up": true,
                    "peer_present": !entry.is_null(),
                    "online": connected,
                    "last_seen_secs": last_seen_secs,
                    "address": entry.get("address").and_then(|a| a.as_str()).unwrap_or(""),
                    "client_version": entry.get("clientVersion").and_then(|c| c.as_str()).unwrap_or(""),
                    "in_bytes": entry.get("inBytesTotal").and_then(|b| b.as_u64()).unwrap_or(0),
                    "out_bytes": entry.get("outBytesTotal").and_then(|b| b.as_u64()).unwrap_or(0),
                    "cached": false,
                }),
                true,
            )
        }
        Err(e) => (
            json!({ "syncthing_up": false, "peer_present": false, "error": e, "cached": false }),
            false,
        ),
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// POST /api/pair/create - inviter side: live local device ID, then a
/// 6-digit control-plane invite code (10-minute TTL).
pub async fn create() -> Response {
    let token = tokio::task::spawn_blocking(stored_plane_token)
        .await
        .unwrap_or(None);
    let Some(token) = token else {
        return bad_request(
            "control plane not activated yet - register and activate in the Control plane card first".into(),
        );
    };
    let base = crate::settings::Settings::load().control_base_url();
    let r = tokio::task::spawn_blocking(move || create_blocking(&base, &token))
        .await
        .unwrap_or_else(|_| Err("create task panicked".into()));
    match r {
        Ok(v) => ok_json(v),
        Err(e) => bad_request(e),
    }
}

fn create_blocking(base_url: &str, token: &str) -> Result<Value, String> {
    let my = my_syncthing_id_live()?;
    let body = json!({ "syncthing_id": my });
    let v = call_plane(base_url, "POST", "/api/pair/invite", Some(&body), Some(token))?;
    let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
    let expires_at = v.get("expires_at").and_then(|e| e.as_i64()).unwrap_or(0);
    if code.is_empty() {
        return Err(format!("control plane returned no invite code: {v}"));
    }
    let mut st = load_state();
    st.my_syncthing_id = Some(my.clone());
    st.invite = Some(InviteInfo { code: code.clone(), expires_at });
    save_state(&st)?;
    Ok(json!({
        "ok": true,
        "code": code,
        "expires_at": expires_at,
        "my_syncthing_id": my,
    }))
}

/// POST /api/pair/join {code} - accepter side: redeem the inviter's code,
/// learn the peer ID, add it to the local Syncthing config.
pub async fn join(body: String) -> Response {
    let code = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(|s| s.trim().to_string()))
        .unwrap_or_default();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return bad_request("invite code must be exactly 6 digits".into());
    }
    let token = tokio::task::spawn_blocking(stored_plane_token)
        .await
        .unwrap_or(None);
    let Some(token) = token else {
        return bad_request(
            "control plane not activated yet - register and activate in the Control plane card first".into(),
        );
    };
    let base = crate::settings::Settings::load().control_base_url();
    let r = tokio::task::spawn_blocking(move || join_blocking(&base, &token, &code))
        .await
        .unwrap_or_else(|_| Err("join task panicked".into()));
    match r {
        Ok(v) => ok_json(v),
        Err(e) => bad_request(e),
    }
}

fn join_blocking(base_url: &str, token: &str, code: &str) -> Result<Value, String> {
    let my = my_syncthing_id_live()?;
    let body = json!({ "code": code, "accepter_syncthing_id": my });
    let v = call_plane(base_url, "POST", "/api/pair/accept", Some(&body), Some(token))?;
    let peer = v
        .get("inviter_syncthing_id")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if peer.is_empty() {
        return Err(format!("control plane returned no inviter device id: {v}"));
    }
    if peer == my {
        return Err("control plane paired this node with itself - aborting".into());
    }
    // Both sides add each other; the inviter side does its PUT via /api/pair/status.
    add_peer_device(&peer)?;
    let mut st = load_state();
    st.my_syncthing_id = Some(my.clone());
    st.peer_syncthing_id = Some(peer.clone());
    st.paired_at = Some(now());
    st.invite = None;
    save_state(&st)?;
    Ok(json!({ "ok": true, "peer_syncthing_id": peer, "my_syncthing_id": my }))
}

/// GET /api/pair/status - inviter poll (auto-completes the pairing when the
/// accepter redeemed the code) + cached peer health. Always HTTP 200.
pub async fn status() -> Response {
    let r = tokio::task::spawn_blocking(status_blocking)
        .await
        .unwrap_or_else(|_| {
            json!({ "ok": true, "degraded": "internal", "error": "status task panicked" })
        });
    ok_json(r)
}

fn status_blocking() -> Value {
    let mut st = load_state();
    let mut degraded: Vec<&str> = Vec::new();

    // Live local ID (best-effort; falls back to the stored one).
    let syncthing_up = my_syncthing_id_live().map(|id| {
        st.my_syncthing_id = Some(id);
        true
    });
    if syncthing_up.is_err() {
        degraded.push("syncthing");
    }

    // Inviter flow: poll the control plane while an invite is outstanding
    // and no peer is known yet. On matched: add the peer locally + persist.
    let mut plane: Value = Value::Null;
    let outstanding = st.invite.is_some() && st.peer_syncthing_id.is_none();
    if outstanding {
        let token = stored_plane_token();
        let base = crate::settings::Settings::load().control_base_url();
        match token.map(|t| call_plane(&base, "GET", "/api/pair/poll", None, Some(&t))) {
            Some(Ok(v)) => {
                plane = v.clone();
                if v.get("status").and_then(|s| s.as_str()) == Some("matched") {
                    if let Some(peer) =
                        v.get("accepter_syncthing_id").and_then(|c| c.as_str()).map(|s| s.to_string())
                    {
                        if !peer.is_empty() {
                            if add_peer_device(&peer).is_ok() {
                                st.peer_syncthing_id = Some(peer);
                                st.paired_at = Some(now());
                                st.invite = None;
                                let _ = save_state(&st);
                            }
                        }
                    }
                }
            }
            Some(Err(e)) => {
                degraded.push("control");
                plane = json!({ "error": e });
            }
            None => degraded.push("control"), // not activated
        }
    } else if st.invite.is_some() {
        let inv = st.invite.as_ref().unwrap();
        if now() > inv.expires_at {
            st.invite = None;
            let _ = save_state(&st);
        }
    }

    let peer = st.peer_syncthing_id.clone();
    let (health, _, probe_failures) = peer_health(peer.as_deref());

    json!({
        "ok": true,
        "paired": peer.is_some(),
        "my_syncthing_id": st.my_syncthing_id,
        "peer_syncthing_id": peer,
        "paired_at": st.paired_at,
        "invite": st.invite,
        "folders": st.folders,
        "syncthing_up": syncthing_up.is_ok(),
        "plane": plane,
        "peer_health": health,
        "probe_failures": probe_failures,
        "degraded": if degraded.is_empty() { Value::Null } else { json!(degraded.join(",")) },
    })
}

// ---------------------------------------------------------------------------
// folder provisioning
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FoldersReq {
    pub paths: Vec<String>,
}

/// Stable folder id for a path: "flygo-" + 16 hex of a simple hash, so a
/// re-provision of the same path upserts the same folder instead of
/// duplicating it.
fn folder_id_for(path: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325; // FNV-1a
    for b in path.to_lowercase().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("flygo-{h:016x}")
}

/// POST /api/pair/folders {paths:[...]} - provision each existing directory
/// as a send-receive mutual-backup folder shared with the peer, with FORCED
/// staggered versioning (7-day undo window; see constants above). This is
/// additive - existing folders are upserted by id, never deleted.
pub async fn folders(body: String) -> Response {
    let req: FoldersReq = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return bad_request(format!("invalid JSON body: {e}")),
    };
    if req.paths.is_empty() {
        return bad_request("paths[] must contain at least one directory".into());
    }
    if req.paths.len() > 64 {
        return bad_request("paths[] is limited to 64 entries".into());
    }
    let paths: Vec<String> = req
        .paths
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let r = tokio::task::spawn_blocking(move || folders_blocking(&paths))
        .await
        .unwrap_or_else(|_| Err("folders task panicked".into()));
    match r {
        Ok(v) => ok_json(v),
        Err(e) => bad_request(e),
    }
}

fn folders_blocking(paths: &[String]) -> Result<Value, String> {
    let mut st = load_state();
    let Some(peer) = st.peer_syncthing_id.clone().filter(|p| !p.is_empty()) else {
        return Err("pair the peer node first (create or join a pairing)".into());
    };
    let my = my_syncthing_id_live()?;

    let mut results: Vec<Value> = Vec::new();
    let mut ok_paths: Vec<String> = Vec::new();
    for path in paths {
        // Only real, existing directories (never invent folders on a guess).
        match std::fs::metadata(path) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                results.push(json!({ "path": path, "ok": false, "error": "not a directory" }));
                continue;
            }
            Err(e) => {
                results.push(json!({ "path": path, "ok": false, "error": format!("not accessible: {e}") }));
                continue;
            }
        }
        let id = folder_id_for(path);
        let label = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id.clone());
        // send-receive + staggered versioning are the whole point of mutual
        // backup: both nodes converge, and deletions stay reversible for the
        // 7-day window (STAGGERED_* constants).
        let folder = json!({
            "id": id,
            "label": format!("FlyGo backup - {label}"),
            "path": path,
            "type": "sendreceive",
            "rescanIntervalS": 3600,
            "fsWatcherEnabled": true,
            "devices": [
                { "deviceID": my },
                { "deviceID": peer },
            ],
            "versioning": {
                "type": "staggered",
                "params": {
                    "maxAge": STAGGERED_MAX_AGE_SECS.to_string(),
                    "cleanoutDays": STAGGERED_CLEANOUT_DAYS.to_string(),
                }
            },
        });
        match st_json("PUT", "/rest/config/folders", Some(&json!([folder]))) {
            Ok(_) => {
                results.push(json!({ "path": path, "ok": true, "folder_id": id }));
                ok_paths.push(path.clone());
            }
            Err(e) => results.push(json!({ "path": path, "ok": false, "error": e })),
        }
    }

    // Keep the successfully provisioned set (union with earlier selections).
    for p in ok_paths {
        if !st.folders.contains(&p) {
            st.folders.push(p);
        }
    }
    save_state(&st)?;
    let all_ok = results.iter().all(|r| r.get("ok").and_then(|b| b.as_bool()).unwrap_or(false));
    Ok(json!({ "ok": all_ok, "results": results }))
}

// ---------------------------------------------------------------------------
// recovery progress
// ---------------------------------------------------------------------------

/// GET /api/pair/recover - per paired folder /rest/db/status -> needBytes /
/// globalBytes progress. Folders = locally configured folders shared with
/// the peer (live truth; also survives a lost pair-state.json). Always 200;
/// degrades to a syncthing_up=false snapshot when the daemon is down.
pub async fn recover() -> Response {
    let r = tokio::task::spawn_blocking(recover_blocking)
        .await
        .unwrap_or_else(|_| json!({ "ok": true, "syncthing_up": false, "degraded": "syncthing,error:recover task panicked" }));
    ok_json(r)
}

fn recover_blocking() -> Value {
    let Some(key_peer) = load_state().peer_syncthing_id.filter(|p| !p.is_empty()) else {
        return json!({
            "ok": true,
            "paired": false,
            "folders": [],
            "total_need_bytes": 0,
            "total_global_bytes": 0,
        });
    };
    let cfg = match st_json("GET", "/rest/config/folders", None) {
        Ok(v) => v,
        Err(e) => {
            return json!({
                "ok": true,
                "paired": true,
                "syncthing_up": false,
                "degraded": "syncthing",
                "error": e,
                "folders": [],
            })
        }
    };
    // Folders shared with the peer = the mutual-backup set.
    let mut paired_folders: Vec<(String, String)> = Vec::new(); // (id, label)
    if let Some(arr) = cfg.get("folders").and_then(|f| f.as_array()) {
        for f in arr {
            let shared = f
                .get("devices")
                .and_then(|d| d.as_array())
                .map(|devs| {
                    devs.iter().any(|d| {
                        d.get("deviceID").and_then(|x| x.as_str()) == Some(key_peer.as_str())
                    })
                })
                .unwrap_or(false);
            if !shared {
                continue;
            }
            let id = f.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let label = f
                .get("label")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| id.clone());
            if !id.is_empty() {
                paired_folders.push((id, label));
            }
        }
    }

    // Per-folder db/status in parallel; one failed call degrades only its row.
    let rows: Vec<Value> = std::thread::scope(|s| {
        let handles: Vec<_> = paired_folders
            .iter()
            .map(|(id, label)| {
                let (id, label) = (id.clone(), label.clone());
                s.spawn(move || {
                    let st = st_json("GET", &format!("/rest/db/status?folder={id}"), None);
                    match st {
                        Ok(v) => {
                            let need = v.get("needBytes").and_then(|b| b.as_u64()).unwrap_or(0);
                            let global =
                                v.get("globalBytes").and_then(|b| b.as_u64()).unwrap_or(0);
                            let pct = if global == 0 {
                                100.0
                            } else {
                                ((global - need.min(global)) as f64 / global as f64) * 100.0
                            };
                            json!({
                                "folder_id": id,
                                "label": label,
                                "state": v.get("state").and_then(|x| x.as_str()).unwrap_or("unknown"),
                                "need_bytes": need,
                                "global_bytes": global,
                                "completion_pct": (pct * 10.0).round() / 10.0,
                                "error": Value::Null,
                            })
                        }
                        Err(e) => json!({
                            "folder_id": id,
                            "label": label,
                            "state": "unknown",
                            "need_bytes": 0,
                            "global_bytes": 0,
                            "completion_pct": 0.0,
                            "error": e,
                        }),
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(json!({ "folder_id": "", "label": "", "state": "unknown", "need_bytes": 0, "global_bytes": 0, "completion_pct": 0.0, "error": "db/status thread panicked" })))
            .collect()
    });

    let total_need: u64 = rows.iter().filter_map(|r| r["need_bytes"].as_u64()).sum();
    let total_global: u64 = rows.iter().filter_map(|r| r["global_bytes"].as_u64()).sum();
    let total_pct = if total_global == 0 {
        100.0
    } else {
        ((total_global - total_need.min(total_global)) as f64 / total_global as f64) * 100.0
    };
    json!({
        "ok": true,
        "paired": true,
        "syncthing_up": true,
        "folders": rows,
        "total_need_bytes": total_need,
        "total_global_bytes": total_global,
        "total_completion_pct": (total_pct * 10.0).round() / 10.0,
    })
}

// ---------------------------------------------------------------------------
// unit tests (pure logic; no disk / network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_id_is_stable_and_sanitized() {
        let a = folder_id_for("E:/Data/Photos");
        let b = folder_id_for("e:/data/photos");
        let c = folder_id_for("E:/Data/Other");
        assert_eq!(a, b, "case-insensitive path -> same folder id (same volume)");
        assert_ne!(a, c);
        assert!(a.starts_with("flygo-") && a.len() == "flygo-".len() + 16);
    }

    #[test]
    fn staggered_window_is_seven_days() {
        // The undo window is a product guarantee (top risk #2 mitigation):
        // maxAge 604800s = 7 days, cleanup cadence 7 days.
        assert_eq!(STAGGERED_MAX_AGE_SECS, 7 * 86400);
        assert_eq!(STAGGERED_CLEANOUT_DAYS, 7);
    }

    #[test]
    fn pair_state_roundtrip_keeps_defaults() {
        // Default state serializes without null noise; invite/folders only
        // when present (serde skip attributes above).
        let st = PairState::default();
        let text = serde_json::to_string(&st).unwrap();
        assert_eq!(text, "{}");
        let back: PairState = serde_json::from_str(&text).unwrap();
        assert!(back.peer_syncthing_id.is_none());
    }
}
