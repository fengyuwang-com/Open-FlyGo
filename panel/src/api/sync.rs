// api/sync.rs - sync view endpoints: status aggregation + link wizard backing.
//
// Two generations live side by side:
//   GET  /api/sync             -> legacy status payload (kept for API compat;
//                                 stateless since the sync view moved to
//                                 /api/sync/overview)
//   GET  /api/sync/overview    -> one aggregated document for the sync view:
//                                 tray mirror-timer snapshot + last robocopy
//                                 mirror result + Syncthing connections and
//                                 folders. Syncthing being down is NOT an
//                                 error: that block degrades to null.
//   POST /api/sync/link        -> create one sync link (3-step wizard):
//                                   mirror    -> recorded in data/sync-links.json
//                                                (local copy on another disk)
//                                   syncthing -> folder created through the
//                                                local Syncthing REST API with
//                                                engine defaults (send-receive +
//                                                staggered versioning); those
//                                                parameters never reach the UI
//                                   manual    -> recorded only; the UI shows
//                                                a static guide
//   GET  /api/sync/links       -> list recorded links
//   DELETE /api/sync/links/:id -> drop one link (a Syncthing folder is removed
//                                 best-effort; files are never deleted)
//   POST /api/sync/folders/:id/share -> share an existing Syncthing folder
//                                 with paired devices (phone-to-phone relay)
//
// The new handlers own a small SyncState (a tray snapshot closure + the links
// file path) instead of the binary's AppState, so the integration test binary
// can drive the exact same handlers over real HTTP. Blocking ureq calls and
// file writes run inside spawn_blocking; tokio workers are never blocked.
//
// The Syncthing REST base honors FENGPAGE_SYNCTHING_ADDR (live env override,
// used by tests) over the frozen [sync].syncthing_addr setting.
//
// All code comments are pure ASCII; JSON payloads are UTF-8.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// shared state (decoupled from main.rs AppState on purpose)
// ---------------------------------------------------------------------------

/// Point-in-time tray snapshot feeding the overview status card (mirror
/// timer, syncthing child process, external drive). A closure keeps this
/// module free of daemon.rs so the test binary can include it standalone.
#[derive(Serialize, Clone, Default, Debug)]
pub struct TraySnapshot {
    pub timer_enabled: bool,
    pub timer_last_run: Option<String>,
    pub syncthing_running: bool,
    pub syncthing_pid: u32,
    pub usb_present: bool,
    pub usb_letters: Vec<String>,
}

/// State carried by the /api/sync/* sub-router.
pub struct SyncState {
    /// Live tray snapshot (built from the daemon in main.rs).
    pub tray: Box<dyn Fn() -> TraySnapshot + Send + Sync>,
    /// data/sync-links.json location (config::get().sync_links).
    pub links_path: PathBuf,
}

fn ok_json(v: Value) -> Json<Value> {
    Json(v)
}

fn fail_json(msg: String) -> Json<Value> {
    // Both keys on purpose: "error" matches ApiError, "message" matches tray.
    Json(json!({ "ok": false, "error": msg, "message": msg }))
}

// ---------------------------------------------------------------------------
// Syncthing REST helpers (blocking; call inside spawn_blocking)
// ---------------------------------------------------------------------------

/// Syncthing REST base: FENGPAGE_SYNCTHING_ADDR env (live, test hook) >
/// [sync].syncthing_addr from the frozen settings.
fn st_base() -> String {
    match std::env::var("FENGPAGE_SYNCTHING_ADDR") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => crate::settings::get().syncthing_addr(),
    }
}

/// Per-request REST timeout ([sync].sync_timeout_secs, default 2s).
fn st_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(
            crate::settings::get().sync_timeout_secs(),
        ))
        .build()
}

/// Read the <apikey> element from Syncthing's config.xml (never logged).
fn read_apikey() -> Option<String> {
    let home = crate::config::get().syncthing_home.clone()?;
    let cfg = std::fs::read_to_string(home.join("config.xml")).ok()?;
    let start = cfg.find("<apikey>")? + "<apikey>".len();
    let end = cfg[start..].find("</apikey>")? + start;
    Some(cfg[start..end].to_string())
}

fn st_call(agent: &ureq::Agent, apikey: &str, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let req = agent
        .request(method, &format!("{}{}", st_base(), path))
        .set("X-API-Key", apikey);
    let res = match body {
        Some(b) => req.send_json(b.clone()).map_err(|e| format!("{method} {path}: {e}"))?,
        None => req.call().map_err(|e| format!("{method} {path}: {e}"))?,
    };
    let status = res.status();
    if status != 200 {
        let text = res.into_string().unwrap_or_default();
        let short: String = text.chars().take(200).collect();
        return Err(format!("{method} {path}: HTTP {status} {short}"));
    }
    res.into_json::<Value>()
        .map_err(|e| format!("{method} {path}: bad json: {e}"))
}

fn short_id(id: &str) -> String {
    id.chars().take(7).collect()
}

/// Syncthing device IDs are base32 strings (optionally shown with dashes).
/// Every device id that reaches a REST URL segment or a config key must pass
/// this charset gate first, so a crafted value can never traverse to other
/// engine endpoints (same rationale as `valid_folder_id`).
fn valid_device_id(id: &str) -> bool {
    (8..=64).contains(&id.chars().count())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Syncthing folder IDs are user-visible labels used as a REST URL segment
/// AND (for freshly accepted folders without an explicit path) as a directory
/// name under <root>/sync/. Reject quotes-free traversal: only alphanumerics
/// and -_. are allowed, and "." / ".." / leading-dot names are refused so the
/// value can never escape the sync root or alter the endpoint path.
fn valid_folder_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= 64
        && !id.starts_with('.')
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// u64 out of a JSON number (int or float form).
fn u64_of(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))
        .unwrap_or(0)
}

/// Collect the Syncthing block for /api/sync/overview: device list with live
/// connection state plus per-folder state. All calls share the configured
/// per-request timeout; one db/status failure degrades that folder to
/// "unknown" instead of failing the whole block.
fn collect_syncthing(apikey: &str) -> Result<Value, String> {
    let agent = st_agent();
    let conns = st_call(&agent, apikey, "GET", "/rest/system/connections", None)?;
    let devices_cfg = st_call(&agent, apikey, "GET", "/rest/config/devices", None)
        .unwrap_or_else(|_| json!([]));
    let folders_cfg = st_call(&agent, apikey, "GET", "/rest/config/folders", None)?;
    let status = st_call(&agent, apikey, "GET", "/rest/system/status", None)?;

    let my_id = status
        .get("myID")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // deviceID -> display name (falls back to the short id).
    let mut names = std::collections::HashMap::new();
    if let Some(arr) = devices_cfg.as_array() {
        for d in arr {
            if let Some(id) = d.get("deviceID").and_then(|v| v.as_str()) {
                let name = d
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| short_id(id));
                names.insert(id.to_string(), name);
            }
        }
    }

    // Devices = config devices (paired, may be offline) + any extra entry
    // that only shows up in the live connections map.
    let conn_map = conns
        .get("connections")
        .and_then(|c| c.as_object())
        .cloned()
        .unwrap_or_default();
    let mut dev_ids: Vec<String> = devices_cfg
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d.get("deviceID").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    for id in conn_map.keys() {
        if !dev_ids.contains(id) {
            dev_ids.push(id.clone());
        }
    }
    let devices: Vec<Value> = dev_ids
        .iter()
        .map(|id| {
            let conn = conn_map.get(id);
            let connected = conn
                .and_then(|c| c.get("connected"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let address = conn
                .and_then(|c| c.get("address"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            json!({
                "id": id,
                "short_id": short_id(id),
                "name": names.get(id).cloned().unwrap_or_else(|| short_id(id)),
                "connected": connected || id == &my_id,
                "address": address,
                "self": id == &my_id,
            })
        })
        .collect();

    let mut folders: Vec<Value> = Vec::new();
    if let Some(arr) = folders_cfg.as_array() {
        for f in arr {
            let id = f.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let label = f
                .get("label")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| id.clone());
            let path = f
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut state = "unknown".to_string();
            let mut completion_pct = 100.0f64;
            let mut need_files = 0u64;
            if !id.is_empty() {
                if let Ok(d) = st_call(
                    &agent,
                    apikey,
                    "GET",
                    &format!("/rest/db/status?folder={id}"),
                    None,
                ) {
                    state = d.get("state").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                    need_files = d.get("needFiles").map(u64_of).unwrap_or(0);
                    let (global, need) = (
                        d.get("globalBytes").map(u64_of).unwrap_or(0),
                        d.get("needBytes").map(u64_of).unwrap_or(0),
                    );
                    completion_pct = if global > 0 {
                        let done = global.saturating_sub(need) as f64 / global as f64 * 100.0;
                        (done.clamp(0.0, 100.0) * 10.0).round() / 10.0
                    } else {
                        100.0
                    };
                }
            }
            folders.push(json!({
                "id": id, "label": label, "path": path,
                "state": state, "completion_pct": completion_pct,
                "need_files": need_files,
            }));
        }
    }

    Ok(json!({ "devices": devices, "folders": folders }))
}

// ---------------------------------------------------------------------------
// mirror block (robocopy log; read-only)
// ---------------------------------------------------------------------------

/// Mirror status for the overview card: null when the deployment has no
/// mirror configured; otherwise the log's mtime as the last-run stamp.
fn mirror_block() -> Value {
    let c = crate::config::get();
    if c.mirror_log.is_empty() && c.mirror_src.is_empty() && c.mirror_dst.is_empty() {
        return Value::Null;
    }
    let log_exists = !c.mirror_log.is_empty() && Path::new(&c.mirror_log).exists();
    let last_run_unix = if log_exists {
        std::fs::metadata(&c.mirror_log)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    } else {
        None
    };
    json!({
        "src": c.mirror_src,
        "dst": c.mirror_dst,
        "log_exists": log_exists,
        "last_run_unix": last_run_unix,
    })
}

/// Common local folders offered as quick picks in wizard step 1 (paths only;
/// labels are frontend i18n). Mirror source joins when configured.
fn quick_dirs() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(home) = std::env::var("USERPROFILE") {
        let h = PathBuf::from(&home);
        for sub in ["Desktop", "Documents", "Pictures", "Downloads", "Videos"] {
            let d = h.join(sub);
            if d.is_dir() {
                out.push(d.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let src = &crate::config::get().mirror_src;
    if !src.is_empty() && Path::new(src).is_dir() {
        out.push(src.replace('\\', "/"));
    }
    out
}

// ---------------------------------------------------------------------------
// GET /api/sync/overview
// ---------------------------------------------------------------------------

pub async fn overview(State(st): State<Arc<SyncState>>) -> Json<Value> {
    let tray = (st.tray)();
    let apikey = read_apikey();
    let syncthing = tokio::task::spawn_blocking(move || match apikey.as_deref() {
        Some(k) => collect_syncthing(k),
        None => Err("Syncthing API key not found in config.xml".to_string()),
    })
    .await
    .unwrap_or_else(|e| Err(format!("overview task panicked: {e}")));

    let (syncthing_block, syncthing_error) = match syncthing {
        Ok(v) => (v, Value::Null),
        Err(e) => (Value::Null, json!(e)),
    };
    let reachable = !syncthing_block.is_null();

    ok_json(json!({
        "ok": true,
        "tray": tray,
        "mirror": mirror_block(),
        "syncthing": syncthing_block,
        "syncthing_reachable": reachable,
        "syncthing_error": syncthing_error,
        "quick_dirs": quick_dirs(),
    }))
}

// ---------------------------------------------------------------------------
// link registry (data/sync-links.json, atomic tmp+rename writes)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SyncLink {
    pub id: String,
    /// mirror | syncthing | manual
    pub kind: String,
    pub folder_path: String,
    pub folder_label: String,
    /// mirror: destination path; syncthing: peer device id; manual: null
    pub target_id: Option<String>,
    /// Syncthing folder id (syncthing kind only), used for engine cleanup.
    pub st_folder_id: Option<String>,
    pub created_at_unix: u64,
    /// Last runner outcome {state: ok|warn|err, detail}; null = not yet run.
    pub last_result: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct LinksFile {
    version: u32,
    links: Vec<SyncLink>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// FNV-1a 32-bit hash (no extra dependency; only used for stable slugs).
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

fn read_links_file(path: &Path) -> Vec<SyncLink> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<LinksFile>(&text) {
            Ok(f) => f.links,
            Err(e) => {
                // Tolerate a corrupt registry: start empty rather than
                // failing every request (the next write replaces the file).
                eprintln!("[sync] links file unreadable ({}): {e}", path.display());
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// Atomic write: serialize to <file>.json.tmp, then rename over the target.
fn write_links_file(path: &Path, links: &[SyncLink]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let doc = LinksFile { version: 1, links: links.to_vec() };
    let text = serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// path helpers
// ---------------------------------------------------------------------------

/// Normalize a user-entered path: backslashes to forward slashes, trailing
/// separator dropped (drive roots like "E:/" keep theirs).
fn normalize_path(p: &str) -> String {
    let mut s = p.trim().replace('\\', "/");
    while s.len() > 3 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// Absolute Windows path: drive-letter form or UNC (after normalization).
fn is_abs_path(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() >= 3 {
        let drive = b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\');
        if drive {
            return true;
        }
    }
    p.starts_with("//") // normalized UNC
}

fn folder_label(path: &str) -> String {
    path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(path).to_string()
}

// ---------------------------------------------------------------------------
// POST /api/sync/link
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
pub struct LinkReq {
    pub folder_path: String,
    pub target_kind: String,
    pub target_id: Option<String>,
    /// Optional engine folder type for the peer share: sendreceive (default) |
    /// sendonly | receiveonly. Only applied to the peer entry; this node
    /// always keeps full sendreceive.
    pub folder_type: Option<String>,
    /// Optional pending phone device to accept and share with in one step
    /// ("phone_target"): the device is PUT into the engine config before the
    /// folder is created, so no manual device-ID entry is needed.
    pub pending_device_id: Option<String>,
}

/// Normalize the requested folder type; None = engine default sendreceive.
fn folder_type_of(req: &Option<String>) -> Result<String, String> {
    match req.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
        None => Ok("sendreceive".into()),
        Some(t) if t.is_empty() || t == "sendreceive" => Ok("sendreceive".into()),
        Some(t) if t == "sendonly" || t == "receiveonly" => Ok(t),
        Some(t) => Err(format!(
            "folder_type 须为 sendreceive / sendonly / receiveonly（收到 {t}）"
        )),
    }
}

/// Build the Syncthing folder document for the wizard. Engine defaults are
/// fixed here and never exposed to the frontend:
///   type = send-receive (bidirectional)
///   versioning = staggered, maxAge 7 days (staggered retention has no
///   max-versions knob; the age window is what bounds history, covering the
///   "10 versions / 7 days" product default).
fn st_folder_doc(folder: &str, label: &str, id: &str, self_id: &str, peer: Option<(&str, &str)>) -> Value {
    let mut devices = vec![json!({ "deviceID": self_id })];
    if let Some((p, ptype)) = peer {
        let mut entry = json!({ "deviceID": p });
        // Only carry an explicit type when it deviates from the engine default
        // (sendreceive); Syncthing fills the rest with its own defaults.
        if ptype != "sendreceive" {
            entry["type"] = json!(ptype);
        }
        devices.push(entry);
    }
    json!({
        "id": id,
        "label": label,
        "path": folder,
        "type": "sendreceive",
        "rescanIntervalS": 3600,
        "fsWatcherEnabled": true,
        "versioning": {
            "type": "staggered",
            "params": { "maxAge": "604800", "cleanoutDays": "7" }
        },
        "devices": devices,
    })
}

/// Generate a unique Syncthing folder id for a path ("flygo-<hash8>"),
/// avoiding collisions with existing folder ids.
fn st_folder_id(folder: &str, existing: &[Value]) -> String {
    let mut id = format!("flygo-{:08x}", fnv1a(folder));
    let taken = |cand: &str| {
        existing
            .iter()
            .any(|f| f.get("id").and_then(|v| v.as_str()) == Some(cand))
    };
    let mut n = 1u32;
    while taken(&id) {
        id = format!("flygo-{:08x}-{n}", fnv1a(folder));
        n += 1;
    }
    id
}

pub async fn create_link(State(st): State<Arc<SyncState>>, Json(body): Json<LinkReq>) -> Json<Value> {
    let folder = normalize_path(&body.folder_path);
    let kind = body.target_kind.trim().to_ascii_lowercase();
    let target = body
        .target_id
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // ---- shared validation ----
    if !is_abs_path(&folder) {
        return fail_json("folder_path 需为绝对路径（例如 D:\\Photos，以本机实际盘符为准）".into());
    }
    if folder.chars().count() > 400 {
        return fail_json("folder_path 过长（>400 字符）".into());
    }
    if !Path::new(&folder).is_dir() {
        return fail_json(format!("目录不存在：{folder}"));
    }

    let mut st_folder: Option<String> = None;
    // Device actually shared with in the syncthing arm: explicit target_id or
    // the one-step pending phone. Persisted as the link's target_id so the
    // links list shows a paired peer instead of "仅本机登记".
    let mut st_peer: Option<String> = None;
    match kind.as_str() {
        "mirror" => {
            let Some(dest_raw) = &target else {
                return fail_json("镜像目标不能为空（例如 D:\\Mirror\\Photos，以本机实际盘符为准）".into());
            };
            let dest = normalize_path(dest_raw);
            if !is_abs_path(&dest) {
                return fail_json("镜像目标需为绝对路径（例如 D:\\Mirror\\Photos，以本机实际盘符为准）".into());
            }
            if dest.eq_ignore_ascii_case(&folder) {
                return fail_json("镜像目标不能与源目录相同".into());
            }
            // Create the destination so the mirror always has somewhere to
            // land (robocopy would create it on first run anyway).
            if let Err(e) = std::fs::create_dir_all(&dest) {
                return fail_json(format!("无法创建镜像目标目录 {dest}: {e}"));
            }
        }
        "syncthing" => {
            if let Some(p) = &target {
                if !valid_device_id(p) {
                    return fail_json("target_id（设备 ID）格式不正确".into());
                }
            }
            // "phone_target" semantics: accept a pending phone device (panel
            // UI picked it from /api/sync/pending) and share with it in one
            // step, so no device-ID typing is needed on either side.
            let pending_dev = body
                .pending_device_id
                .as_deref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if let Some(p) = &pending_dev {
                if !valid_device_id(p) {
                    return fail_json("pending_device_id 设备 ID 格式不正确".into());
                }
            }
            let ftype = match folder_type_of(&body.folder_type) {
                Ok(t) => t,
                Err(e) => return fail_json(e),
            };
            let apikey = match read_apikey() {
                Some(k) => k,
                None => return fail_json("Syncthing API key 不可读（config.xml 缺失）".into()),
            };
            let folder_arg = folder.clone();
            let label = folder_label(&folder);
            let peer = target.clone().or_else(|| pending_dev.clone());
            st_peer = peer.clone();
            let accept_dev = pending_dev.clone();
            let created = tokio::task::spawn_blocking(move || -> Result<(String, String), String> {
                let agent = st_agent();
                let status = st_call(&agent, &apikey, "GET", "/rest/system/status", None)?;
                let self_id = status
                    .get("myID")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if self_id.is_empty() {
                    return Err("Syncthing 未返回本机设备 ID".into());
                }
                // One-step pending-device accept (idempotent: an already
                // paired device is left as-is by the engine PUT).
                if let Some(dev) = &accept_dev {
                    if dev != &self_id {
                        let doc = json!({
                            "deviceID": dev,
                            "name": "",
                            "addresses": ["dynamic"],
                            "compression": "metadata",
                            "introducer": false,
                            "paused": false,
                        });
                        st_call(
                            &agent,
                            &apikey,
                            "PUT",
                            &format!("/rest/config/devices/{dev}"),
                            Some(&doc),
                        )?;
                    }
                }
                let folders = st_call(&agent, &apikey, "GET", "/rest/config/folders", None)?;
                let existing = folders.as_array().cloned().unwrap_or_default();
                // Reject a duplicate registration for the same path.
                if let Some(f) = existing.iter().find(|f| {
                    f.get("path")
                        .and_then(|v| v.as_str())
                        .map(|p| normalize_path(p).eq_ignore_ascii_case(&folder_arg))
                        .unwrap_or(false)
                }) {
                    return Err(format!(
                        "该目录已在 Syncthing 中（文件夹 {}）",
                        f.get("id").and_then(|v| v.as_str()).unwrap_or("?")
                    ));
                }
                let id = st_folder_id(&folder_arg, &existing);
                let doc = st_folder_doc(
                    &folder_arg,
                    &label,
                    &id,
                    &self_id,
                    peer.as_deref().map(|p| (p, ftype.as_str())),
                );
                st_call(
                    &agent,
                    &apikey,
                    "PUT",
                    &format!("/rest/config/folders/{id}"),
                    Some(&doc),
                )?;
                Ok((id, label))
            })
            .await
            .unwrap_or_else(|e| Err(format!("link task panicked: {e}")));
            match created {
                Ok((id, _label)) => st_folder = Some(id),
                Err(e) => {
                    return fail_json(format!("Syncthing 创建失败：{e}（引擎未运行时可在「总览 → 管家组件」启动后重试）"));
                }
            }
        }
        "manual" => {}
        _ => return fail_json("target_kind 须为 mirror / syncthing / manual".into()),
    }

    // ---- persist (blocking file IO) ----
    let links_path = st.links_path.clone();
    let label = folder_label(&folder);
    let target_final = target.clone().or_else(|| st_peer.clone());
    let written = tokio::task::spawn_blocking(move || -> Result<SyncLink, String> {
        let mut links = read_links_file(&links_path);
        let link = SyncLink {
            id: format!("lnk-{:08x}", fnv1a(&format!(
                "{}|{}|{}|{}",
                kind, folder, target_final.as_deref().unwrap_or(""), now_unix()
            ))),
            kind: kind.clone(),
            folder_path: folder.clone(),
            folder_label: label,
            target_id: target_final,
            st_folder_id: st_folder,
            created_at_unix: now_unix(),
            last_result: None,
        };
        links.push(link.clone());
        write_links_file(&links_path, &links)?;
        Ok(link)
    })
    .await
    .unwrap_or_else(|e| Err(format!("link task panicked: {e}")));

    match written {
        Ok(link) => ok_json(json!({
            "ok": true,
            "link": link,
            "message": match link.kind.as_str() {
                "mirror" => format!("镜像链路已登记：{} → {}", link.folder_path, link.target_id.as_deref().unwrap_or("")),
                "syncthing" => format!("已在 Syncthing 创建文件夹 {}（双向同步 + 自动版本保留）", link.st_folder_id.as_deref().unwrap_or("")),
                _ => "手动链路已登记".to_string(),
            },
        })),
        Err(e) => fail_json(format!("写入 sync-links.json 失败：{e}")),
    }
}

// ---------------------------------------------------------------------------
// GET /api/sync/links
// ---------------------------------------------------------------------------

pub async fn list_links(State(st): State<Arc<SyncState>>) -> Json<Value> {
    let path = st.links_path.clone();
    let links = tokio::task::spawn_blocking(move || read_links_file(&path))
        .await
        .unwrap_or_default();
    ok_json(json!({ "ok": true, "links": links }))
}

// ---------------------------------------------------------------------------
// DELETE /api/sync/links/:id
// ---------------------------------------------------------------------------

pub async fn delete_link(
    State(st): State<Arc<SyncState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Json<Value> {
    let links_path = st.links_path.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<(SyncLink, Option<String>), String> {
        let mut links = read_links_file(&links_path);
        let Some(pos) = links.iter().position(|l| l.id == id) else {
            return Err(format!("链路不存在：{id}"));
        };
        let link = links.remove(pos);
        // Engine cleanup for syncthing links: remove the folder from the
        // Syncthing config best-effort (never touches files on disk).
        let mut engine_note = None;
        if link.kind == "syncthing" {
            if let (Some(fid), Some(key)) = (&link.st_folder_id, read_apikey()) {
                let agent = st_agent();
                match st_call(&agent, &key, "DELETE", &format!("/rest/config/folders/{fid}"), None) {
                    Ok(_) => engine_note = Some(format!("已从 Syncthing 移除文件夹 {fid}"),
                    ),
                    Err(e) => {
                        engine_note = Some(format!("Syncthing 文件夹移除失败（{e}），仅移除了面板记录"));
                    }
                }
            }
        }
        write_links_file(&links_path, &links)?;
        Ok((link, engine_note))
    })
    .await
    .unwrap_or_else(|e| Err(format!("delete task panicked: {e}")));

    match result {
        Ok((link, note)) => ok_json(json!({
            "ok": true,
            "message": format!("已删除链路 {}", link.id) + "。" + &note.unwrap_or_default(),
        })),
        Err(e) => fail_json(e),
    }
}

// ---------------------------------------------------------------------------
// pending devices / folders (phone pairing closure)
//
// When a phone running Syncthing-Fork scans the panel QR, it announces itself
// to this node; Syncthing parks the offer in /rest/config/pending/*. These
// handlers let the UI poll and resolve those offers without any device-ID
// typing. All engine access is the same blocking st_call pattern as above.
// ---------------------------------------------------------------------------

/// Pull one pending entry out of an object-keyed or array-shaped REST reply.
/// Syncthing returns object maps keyed by id (folder id / device id); both
/// shapes are tolerated to stay robust across engine versions.
fn pending_entries(v: &Value, id_key: &str) -> Vec<(String, Value)> {
    let entry_id = |body: &Value| -> Option<String> {
        body.get(id_key)
            .and_then(|x| x.as_str())
            .map(String::from)
            .filter(|s| !s.is_empty())
    };
    match v {
        Value::Object(map) => map
            .iter()
            .map(|(k, body)| (entry_id(body).unwrap_or_else(|| k.clone()), body.clone()))
            .collect(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|body| entry_id(body).map(|id| (id, body.clone())))
            .collect(),
        _ => Vec::new(),
    }
}

/// GET /api/sync/pending -> phones waiting to be accepted + folders shared to
/// us by already-paired devices. Engine down degrades to empty lists (200),
/// matching the overview convention.
pub async fn pending() -> Json<Value> {
    let apikey = read_apikey();
    let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let agent = st_agent();
        let key = apikey.ok_or("Syncthing API key 不可读（config.xml 缺失）")?;
        let dev_raw = st_call(&agent, &key, "GET", "/rest/config/pending/devices", None)
            .unwrap_or_else(|_| json!({}));
        let fold_raw = st_call(&agent, &key, "GET", "/rest/config/pending/folders", None)
            .unwrap_or_else(|_| json!({}));

        let devices: Vec<Value> = pending_entries(&dev_raw, "deviceID")
            .into_iter()
            .map(|(id, body)| {
                let name = body
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| short_id(&id));
                json!({ "id": id, "short_id": short_id(&id), "name": name })
            })
            .collect();

        let folders: Vec<Value> = pending_entries(&fold_raw, "id")
            .into_iter()
            .map(|(id, body)| {
                // offeredBy: map of deviceID -> {label, receiveEncrypted, ...}
                let offered = body
                    .get("offeredBy")
                    .and_then(|v| v.as_object())
                    .map(|m| {
                        m.iter()
                            .map(|(dev, info)| {
                                json!({
                                    "device": dev,
                                    "label": info.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                                })
                            })
                            .collect::<Vec<Value>>()
                    })
                    .unwrap_or_default();
                let label = offered
                    .first()
                    .and_then(|o| o.get("label"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| id.clone());
                json!({
                    "id": id,
                    "label": label,
                    "offered_by": offered,
                })
            })
            .collect();

        Ok(json!({ "ok": true, "devices": devices, "folders": folders }))
    })
    .await
    .unwrap_or_else(|e| Err(format!("pending task panicked: {e}")));

    match res {
        Ok(v) => ok_json(v),
        Err(e) => ok_json(json!({ "ok": true, "devices": [], "folders": [], "error": e })),
    }
}

/// POST /api/sync/pending/devices/:id/accept -> add the phone to this node's
/// engine config (dynamic addresses, metadata compression), clearing the
/// pending entry on the engine side.
pub async fn accept_pending_device(axum::extract::Path(id): axum::extract::Path<String>) -> Json<Value> {
    let id = id.trim().to_string();
    if !valid_device_id(&id) {
        return fail_json("设备 ID 格式不正确".into());
    }
    let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let agent = st_agent();
        let key = read_apikey().ok_or("Syncthing API key 不可读（config.xml 缺失）")?;
        // Carry over the name the phone announced, if still available.
        let pend = st_call(&agent, &key, "GET", "/rest/config/pending/devices", None)
            .unwrap_or_else(|_| json!({}));
        let name = pending_entries(&pend, "deviceID")
            .into_iter()
            .find(|(pid, _)| *pid == id)
            .and_then(|(_, body)| {
                body.get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_default();
        let doc = json!({
            "deviceID": id,
            "name": name,
            "addresses": ["dynamic"],
            "compression": "metadata",
            "introducer": false,
            "paused": false,
        });
        st_call(&agent, &key, "PUT", &format!("/rest/config/devices/{id}"), Some(&doc))?;
        Ok(ok_json(json!({
            "ok": true,
            "message": format!("已接受设备 {}（{}）", short_id(&id), if name.is_empty() { "手机" } else { &name }),
        }))
        .0)
    })
    .await
    .unwrap_or_else(|e| Err(format!("accept device task panicked: {e}")));

    match res {
        Ok(v) => ok_json(v),
        Err(e) => fail_json(format!("接受设备失败：{e}")),
    }
}

/// DELETE /api/sync/pending/devices/:id/dismiss -> drop the pairing offer.
pub async fn dismiss_pending_device(axum::extract::Path(id): axum::extract::Path<String>) -> Json<Value> {
    let id = id.trim().to_string();
    if !valid_device_id(&id) {
        return fail_json("设备 ID 格式不正确".into());
    }
    let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let agent = st_agent();
        let key = read_apikey().ok_or("Syncthing API key 不可读（config.xml 缺失）")?;
        st_call(&agent, &key, "DELETE", &format!("/rest/config/pending/devices/{id}"), None)?;
        Ok(ok_json(json!({
            "ok": true,
            "message": format!("已忽略设备 {}", short_id(&id)),
        }))
        .0)
    })
    .await
    .unwrap_or_else(|e| Err(format!("dismiss device task panicked: {e}")));

    match res {
        Ok(v) => ok_json(v),
        Err(e) => fail_json(format!("忽略设备失败：{e}")),
    }
}

#[derive(Deserialize, Debug, Default)]
pub struct PendingFolderReq {
    /// Local path for a not-yet-configured folder; falls back to
    /// <install root>/sync/<folder id>.
    pub path: Option<String>,
}

/// POST /api/sync/pending/folders/:folder/:device/accept -> accept a folder
/// shared by an already-paired device. Existing local folder gains the device
/// in its share list; a new folder is created unpaused (sendreceive +
/// staggered versioning, same engine defaults as the link wizard).
pub async fn accept_pending_folder(
    axum::extract::Path((folder, device)): axum::extract::Path<(String, String)>,
    body: Option<Json<PendingFolderReq>>,
) -> Json<Value> {
    let folder = folder.trim().to_string();
    let device = device.trim().to_string();
    if !valid_folder_id(&folder) {
        return fail_json("文件夹 ID 格式不正确".into());
    }
    if !valid_device_id(&device) {
        return fail_json("设备 ID 格式不正确".into());
    }
    let custom_path = body
        .as_ref()
        .and_then(|Json(b)| b.path.as_deref())
        .map(normalize_path)
        .filter(|p| !p.is_empty());

    let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let agent = st_agent();
        let key = read_apikey().ok_or("Syncthing API key 不可读（config.xml 缺失）")?;
        let status = st_call(&agent, &key, "GET", "/rest/system/status", None)?;
        let self_id = status.get("myID").and_then(|v| v.as_str()).unwrap_or("");
        if self_id.is_empty() {
            return Err("Syncthing 未返回本机设备 ID".into());
        }

        // Pending entry for label info (best-effort; offer may be gone).
        let pend = st_call(&agent, &key, "GET", "/rest/config/pending/folders", None)
            .unwrap_or_else(|_| json!({}));
        let pending_entry = pending_entries(&pend, "id")
            .into_iter()
            .find(|(fid, _)| *fid == folder)
            .map(|(_, body)| body);
        let offer_label = pending_entry
            .as_ref()
            .and_then(|p| p.get("offeredBy"))
            .and_then(|o| o.get(&device).or_else(|| o.get(self_id)))
            .and_then(|v| v.get("label"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let folders = st_call(&agent, &key, "GET", "/rest/config/folders", None)?;
        let existing = folders.as_array().cloned().unwrap_or_default();
        let doc = match existing
            .iter()
            .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(folder.as_str()))
        {
            // Folder already configured here: just share it with the device.
            Some(f) => {
                let mut doc = f.clone();
                let mut devs = doc
                    .get("devices")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if !devs.iter().any(|d| d.get("deviceID").and_then(|v| v.as_str()) == Some(&device)) {
                    devs.push(json!({ "deviceID": device, "introducedBy": "" }));
                    doc["devices"] = Value::Array(devs);
                }
                doc["paused"] = json!(false);
                doc
            }
            // New folder: engine defaults mirroring st_folder_doc, unpaused.
            None => {
                let path = match &custom_path {
                    Some(p) if is_abs_path(p) => p.clone(),
                    Some(p) => return Err(format!("path 需为绝对路径（收到 {p}）")),
                    None => {
                        let root = crate::config::get()
                            .sync_links
                            .parent()
                            .and_then(|d| d.parent())
                            .map(|d| d.to_path_buf())
                            .unwrap_or_else(|| PathBuf::from("."));
                        let p = root.join("sync").join(&folder);
                        std::fs::create_dir_all(&p).map_err(|e| format!("创建目录 {} 失败: {e}", p.display()))?;
                        p.to_string_lossy().replace('\\', "/")
                    }
                };
                let label = offer_label.unwrap_or_else(|| folder.clone());
                let mut doc = st_folder_doc(
                    &path,
                    &label,
                    &folder,
                    self_id,
                    Some((device.as_str(), "sendreceive")),
                );
                doc["paused"] = json!(false);
                doc
            }
        };
        st_call(&agent, &key, "PUT", &format!("/rest/config/folders/{folder}"), Some(&doc))?;
        Ok(ok_json(json!({
            "ok": true,
            "message": format!("已接受共享文件夹 {folder}（来自设备 {}）", short_id(&device)),
        }))
        .0)
    })
    .await
    .unwrap_or_else(|e| Err(format!("accept folder task panicked: {e}")));

    match res {
        Ok(v) => ok_json(v),
        Err(e) => fail_json(format!("接受共享文件夹失败：{e}")),
    }
}

#[derive(Deserialize, Debug, Default)]
pub struct ShareFolderReq {
    /// Device IDs to share the folder with (already-paired phones/PCs).
    pub device_ids: Vec<String>,
}

/// POST /api/sync/folders/:id/share -> share an existing Syncthing folder with
/// one or more already-paired devices. This is the missing half of the
/// phone-to-phone relay (D6): the PC shares the SAME folder with phone A and
/// phone B; once both phones accept the share in Syncthing-Fork, the two
/// phones sync with each other through the PC as the relay node.
///
/// Any device id that is not yet in the engine config is PUT in first (same
/// doc as accept_pending_device), so the share works for a phone that paired
/// moments ago; the PUT is idempotent for already-paired devices.
pub async fn share_folder(
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ShareFolderReq>,
) -> Json<Value> {
    let id = id.trim().to_string();
    if !valid_folder_id(&id) {
        return fail_json("文件夹 ID 格式不正确".into());
    }
    let mut devs: Vec<String> = Vec::new();
    for s in &body.device_ids {
        let d = s.trim();
        if d.is_empty() {
            continue;
        }
        if !valid_device_id(d) {
            return fail_json("device_ids 含非法设备 ID".into());
        }
        devs.push(d.to_string());
    }
    devs.dedup();
    if devs.is_empty() {
        return fail_json("device_ids 不能为空（至少一台设备）".into());
    }

    let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let agent = st_agent();
        let key = read_apikey().ok_or("Syncthing API key 不可读（config.xml 缺失）")?;
        let folders = st_call(&agent, &key, "GET", "/rest/config/folders", None)?;
        let existing = folders.as_array().cloned().unwrap_or_default();
        let mut doc = existing
            .iter()
            .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .cloned()
            .ok_or_else(|| format!("文件夹不存在：{id}"))?;
        let mut share_list = doc
            .get("devices")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let is_shared = |list: &[Value], dev: &str| {
            list.iter()
                .any(|d| d.get("deviceID").and_then(|v| v.as_str()) == Some(dev))
        };
        // Ensure every requested device exists in the engine config (idempotent
        // for already-paired devices), then append it to the folder share list.
        for dev in &devs {
            if !is_shared(&share_list, dev) {
                let ddoc = json!({
                    "deviceID": dev,
                    "name": "",
                    "addresses": ["dynamic"],
                    "compression": "metadata",
                    "introducer": false,
                    "paused": false,
                });
                st_call(&agent, &key, "PUT", &format!("/rest/config/devices/{dev}"), Some(&ddoc))?;
                share_list.push(json!({ "deviceID": dev, "introducedBy": "" }));
            }
        }
        doc["devices"] = Value::Array(share_list);
        st_call(&agent, &key, "PUT", &format!("/rest/config/folders/{id}"), Some(&doc))?;
        Ok(ok_json(json!({
            "ok": true,
            "message": format!("已把文件夹 {id} 共享给 {} 台设备（手机在 Syncthing-Fork 中接受后即开始互相同步）", devs.len()),
        }))
        .0)
    })
    .await
    .unwrap_or_else(|e| Err(format!("share folder task panicked: {e}")));

    match res {
        Ok(v) => ok_json(v),
        Err(e) => fail_json(format!("共享文件夹失败：{e}")),
    }
}

/// DELETE /api/sync/pending/folders/:folder/:device/dismiss -> drop the
/// folder share offer.
pub async fn dismiss_pending_folder(
    axum::extract::Path((folder, device)): axum::extract::Path<(String, String)>,
) -> Json<Value> {
    let folder = folder.trim().to_string();
    let device = device.trim().to_string();
    if !valid_folder_id(&folder) || !valid_device_id(&device) {
        return fail_json("文件夹/设备 ID 格式不正确".into());
    }
    let res = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let agent = st_agent();
        let key = read_apikey().ok_or("Syncthing API key 不可读（config.xml 缺失）")?;
        st_call(
            &agent,
            &key,
            "DELETE",
            &format!("/rest/config/pending/folders/{folder}?device={device}"),
            None,
        )?;
        Ok(ok_json(json!({ "ok": true, "message": format!("已忽略共享文件夹 {folder}") })).0)
    })
    .await
    .unwrap_or_else(|e| Err(format!("dismiss folder task panicked: {e}")));

    match res {
        Ok(v) => ok_json(v),
        Err(e) => fail_json(format!("忽略共享文件夹失败：{e}")),
    }
}

// ---------------------------------------------------------------------------
// legacy GET /api/sync (stateless rework of the original handler)
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct LegacySync {
    pub syncthing_reachable: bool,
    pub syncthing_connections: Vec<String>,
    pub syncthing_syncing: bool,
    pub mirror_last_log: Option<String>,
    pub mirror_src: String,
    pub mirror_dst: String,
}

fn con_syncthing(apikey: &str) -> (bool, Vec<String>, bool) {
    let agent = st_agent();
    match st_call(&agent, apikey, "GET", "/rest/system/connections", None) {
        Ok(v) => {
            let connections = v
                .get("connections")
                .and_then(|c| c.as_object())
                .map(|obj| obj.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let syncing = !connections.is_empty();
            (true, connections, syncing)
        }
        Err(_) => (false, Vec::new(), false),
    }
}

fn mirror_last_log(path: &str) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let t = meta.modified().ok()?;
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format_unix(secs))
}

fn format_unix(secs: u64) -> String {
    // Simplified display (UTC); the overview endpoint carries the exact unix
    // stamp and lets the frontend format it in local time.
    let days = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    format!("{h:02}:{m:02} ({days} days ago)")
}

pub async fn sync() -> Json<LegacySync> {
    let apikey = read_apikey();
    let st = tokio::task::spawn_blocking(move || match apikey.as_deref() {
        Some(k) => con_syncthing(k),
        None => (false, Vec::new(), false),
    })
    .await
    .unwrap_or((false, Vec::new(), false));

    let c = crate::config::get();
    Json(LegacySync {
        syncthing_reachable: st.0,
        syncthing_connections: st.1,
        syncthing_syncing: st.2,
        mirror_last_log: mirror_last_log(&c.mirror_log),
        mirror_src: c.mirror_src.clone(),
        mirror_dst: c.mirror_dst.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_charset_gate() {
        // Real/base32-with-dashes ids pass.
        assert!(valid_device_id("ABC2345ABCDEFGHIJKLMNOPQRSTUVWXYZ23456"));
        assert!(valid_device_id("XXXXXXX-YYYYZZZ-AAABBBCCC"));
        assert!(valid_device_id("PHONEA1CEID000000000000000000000000000000000000000000"));
        // URL-traversal / injection shapes refuse (review P2: only length was checked).
        assert!(!valid_device_id("SELF/../../admin"));
        assert!(!valid_device_id("dev?foo=bar"));
        assert!(!valid_device_id("dev%2f.."));
        assert!(!valid_device_id("a-b c"));
        assert!(!valid_device_id("short"));
        assert!(!valid_device_id(&"A".repeat(65)));
    }

    #[test]
    fn folder_id_charset_gate() {
        assert!(valid_folder_id("flygo-photos"));
        assert!(valid_folder_id("photos"));
        assert!(valid_folder_id("my.folder_1"));
        // Must never escape the sync root or alter the REST endpoint path.
        assert!(!valid_folder_id(".."));
        assert!(!valid_folder_id("../evil"));
        assert!(!valid_folder_id("a/../b"));
        assert!(!valid_folder_id(".hidden"));
        assert!(!valid_folder_id("a/b"));
        assert!(!valid_folder_id(r"a\b"));
        assert!(!valid_folder_id(""));
        assert!(!valid_folder_id(&"p".repeat(65)));
    }
}
