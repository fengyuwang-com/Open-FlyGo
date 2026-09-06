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
        Some(k) => collect_syncthing(k).map_err(|e| e),
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
}

/// Build the Syncthing folder document for the wizard. Engine defaults are
/// fixed here and never exposed to the frontend:
///   type = send-receive (bidirectional)
///   versioning = staggered, maxAge 7 days (staggered retention has no
///   max-versions knob; the age window is what bounds history, covering the
///   "10 versions / 7 days" product default).
fn st_folder_doc(folder: &str, label: &str, id: &str, self_id: &str, peer: Option<&str>) -> Value {
    let mut devices = vec![json!({ "deviceID": self_id })];
    if let Some(p) = peer {
        devices.push(json!({ "deviceID": p }));
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
                if p.len() > 128 {
                    return fail_json("target_id（设备 ID）格式不正确".into());
                }
            }
            let apikey = match read_apikey() {
                Some(k) => k,
                None => return fail_json("Syncthing API key 不可读（config.xml 缺失）".into()),
            };
            let folder_arg = folder.clone();
            let label = folder_label(&folder);
            let peer = target.clone();
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
                let doc = st_folder_doc(&folder_arg, &label, &id, &self_id, peer.as_deref());
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
    let target_final = target.clone();
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
