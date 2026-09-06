// /api/plainapp - phone node registry + TCP reachability probing.
//
// Phones run the FlyGo app (PlainApp-based web file service). The panel never
// proxies file traffic: the browser talks to the phone directly (control
// plane vs data plane, docs/plainapp-integration.md section 3.1). This module
// only stores registered addresses and probes them.
//
// Endpoints:
//   GET    /api/plainapp/register        -> {ok, phones:[{name,host,port,note}]}
//   POST   /api/plainapp/register        -> register/update {name,host,port?,note?}
//                                           (name is the key; same name updates)
//   DELETE /api/plainapp/register?name=N -> remove one entry
//   GET    /api/plainapp/status          -> probe every phone concurrently
//                                           {ok, phones:[{name,host,port,note,
//                                            reachable,latency_ms,reason,
//                                            last_check}]}
//
// Persistence: data/phones.json (FENGPAGE_PHONES overrides), atomic
// tmp + rename write (same pattern as api/control.rs). A corrupt or missing
// file simply yields an empty list on the next read.
//
// Offline is a NORMAL state for phones (on-demand online, "launch" must be
// turned on manually): an unreachable probe is reported as reachability
// data, never as an error. Color thresholds match api/health.rs
// (<200ms green, <1000ms yellow, timeout/refused red) but with a shorter
// 1.5s timeout because phone links are LAN/Tailscale only.
//
// All code comments are pure ASCII.

use crate::ApiResult;
use axum::{extract::Query, Json};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;

/// Phone probe timeout: LAN/Tailscale hops only, 1.5s is plenty; a shorter
/// cap keeps the overview refresh snappy when a phone is off-network.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Default port of the phone web service when the register call omits it.
const DEFAULT_PORT: u16 = 8080;

// ---------------------------------------------------------------------------
// persisted registry
// ---------------------------------------------------------------------------

/// One registered phone. `name` is the user-facing key (unique).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PhoneEntry {
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// File shape of data/phones.json (object wrapper for forward compatibility).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct PhonesFile {
    #[serde(default)]
    phones: Vec<PhoneEntry>,
}

/// Serializes registry writes (API handlers may race).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn phones_path() -> PathBuf {
    crate::config::get().phones_path.clone()
}

/// Read the registry from disk (cheap; no cache so every request is fresh).
pub fn load_phones() -> Vec<PhoneEntry> {
    match std::fs::read_to_string(phones_path()) {
        Ok(text) => serde_json::from_str::<PhonesFile>(&text)
            .unwrap_or_default()
            .phones,
        Err(_) => Vec::new(),
    }
}

/// Atomic write: tmp file + rename (mirrors api/control.rs).
fn save_phones(list: &[PhoneEntry]) -> Result<(), String> {
    let path = phones_path();
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {} failed: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&PhonesFile { phones: list.to_vec() })
        .map_err(|e| format!("serialize phones failed: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace {} failed: {e}", path.display())
    })
}

// ---------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------

/// Host must be a bare IP or hostname: no scheme, no path, no spaces.
fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.contains("://")
        && !h.contains('/')
        && !h.chars().any(char::is_whitespace)
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// payloads
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct RegisterPayload {
    ok: bool,
    count: usize,
    phones: Vec<PhoneEntry>,
}

#[derive(Deserialize)]
pub struct RegisterReq {
    pub name: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub note: Option<String>,
}

#[derive(Deserialize)]
pub struct DelQuery {
    pub name: Option<String>,
}

#[derive(Serialize)]
pub struct PhoneStatus {
    name: String,
    host: String,
    port: u16,
    #[serde(skip_serializing_if = "String::is_empty")]
    note: String,
    /// TCP connect succeeded within PROBE_TIMEOUT.
    reachable: bool,
    /// Connect latency in ms; ~PROBE_TIMEOUT for timeouts.
    latency_ms: u64,
    /// ok / refused / timeout (refused also covers DNS failure).
    reason: String,
    /// Unix epoch seconds (UTC) of this probe.
    last_check: i64,
}

#[derive(Serialize)]
pub struct StatusPayload {
    ok: bool,
    count: usize,
    phones: Vec<PhoneStatus>,
}

// ---------------------------------------------------------------------------
// probing
// ---------------------------------------------------------------------------

/// Single TCP connect probe against an arbitrary host (the probe twin of
/// api/health.rs probe_tcp, which is hardcoded to 127.0.0.1).
async fn probe_tcp(host: &str, port: u16) -> (bool, u64, &'static str) {
    let start = Instant::now();
    let t = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((host, port))).await;
    let latency = start.elapsed().as_millis() as u64;
    match t {
        Ok(Ok(_)) => (true, latency, "ok"),
        Ok(Err(_)) => (false, latency, "refused"),
        Err(_) => (false, latency, "timeout"),
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// GET /api/plainapp/register
pub async fn register_list() -> ApiResult<Json<RegisterPayload>> {
    let phones = load_phones();
    Ok(Json(RegisterPayload { ok: true, count: phones.len(), phones }))
}

/// POST /api/plainapp/register  body {name, host, port?, note?}
/// Registers or updates (same name = update in place).
pub async fn register_add(Json(req): Json<RegisterReq>) -> ApiResult<Json<RegisterPayload>> {
    let name = req.name.unwrap_or_default().trim().to_string();
    let host = req.host.unwrap_or_default().trim().to_string();
    let note = req.note.unwrap_or_default().trim().to_string();
    let port = req.port.unwrap_or(DEFAULT_PORT);
    if name.is_empty() || name.len() > 64 {
        return Err(crate::ApiError("name must be 1-64 chars".into()));
    }
    if !valid_host(&host) {
        return Err(crate::ApiError(
            "host must be a bare IP or hostname (no scheme, no path)".into(),
        ));
    }
    if port == 0 {
        return Err(crate::ApiError("port must be 1-65535".into()));
    }
    if note.len() > 256 {
        return Err(crate::ApiError("note too long (max 256 chars)".into()));
    }

    let entry = PhoneEntry { name, host, port, note };
    let mut phones = load_phones();
    match phones.iter().position(|p| p.name == entry.name) {
        Some(i) => phones[i] = entry, // update in place, keep ordering stable
        None => phones.push(entry),
    }
    save_phones(&phones).map_err(crate::ApiError)?;
    Ok(Json(RegisterPayload { ok: true, count: phones.len(), phones }))
}

/// DELETE /api/plainapp/register?name=N
pub async fn register_del(Query(q): Query<DelQuery>) -> ApiResult<Json<RegisterPayload>> {
    let name = q.name.unwrap_or_default().trim().to_string();
    if name.is_empty() {
        return Err(crate::ApiError("query param name is required".into()));
    }
    let mut phones = load_phones();
    let before = phones.len();
    phones.retain(|p| p.name != name);
    if phones.len() == before {
        return Err(crate::ApiError(format!("unknown phone: {name}")));
    }
    save_phones(&phones).map_err(crate::ApiError)?;
    Ok(Json(RegisterPayload { ok: true, count: phones.len(), phones }))
}

/// GET /api/plainapp/status
/// Concurrent TCP probe of every registered phone. Unreachable phones are a
/// normal state (on-demand online) and still return HTTP 200.
pub async fn status() -> ApiResult<Json<StatusPayload>> {
    let phones = load_phones();
    let probed = futures::future::join_all(phones.iter().map(|p| {
        let p = p.clone();
        async move {
            let (reachable, latency_ms, reason) = probe_tcp(&p.host, p.port).await;
            PhoneStatus {
                name: p.name,
                host: p.host,
                port: p.port,
                note: p.note,
                reachable,
                latency_ms,
                reason: reason.to_string(),
                last_check: now_epoch(),
            }
        }
    }))
    .await;
    Ok(Json(StatusPayload { ok: true, count: probed.len(), phones: probed }))
}
