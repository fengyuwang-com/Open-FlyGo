// /api/onboarding - phone onboarding flow: scan-to-pair Syncthing, keep-alive
// guide. Hides Syncthing pairing complexity from the user (product philosophy:
// scan a QR code, three steps max).
//
// Endpoints:
//   GET /api/onboarding/syncthing-qr  -> local device ID + QR SVG (pairing)
//   GET /api/onboarding/status        -> paired / connected device state
//   GET /api/onboarding/plainapp      -> PlainApp/Termux guide slot (reserved)
//
// Syncthing facts used here:
//   - local device ID: /rest/system/status -> myID (authoritative while the
//     Syncthing daemon answers); best-effort config.xml fallback otherwise.
//   - paired devices : /rest/config/devices minus this node itself.
//   - connectivity   : /rest/system/connections (connected device IDs).
//   - QR content is the RAW device ID string: Syncthing-Fork's QR scanner
//     stores the scanned text verbatim as the device ID (no URI prefix).
use crate::ApiResult;
use axum::{extract::Query, Json};
use qrcode::{render::svg, EcLevel, QrCode};
use serde::{Deserialize, Serialize};

/// Syncthing REST base from the config file ([sync].syncthing_addr,
/// settings.rs; shared with sync.rs and syncview.rs).
fn sync_rest() -> String {
    crate::settings::get().syncthing_addr()
}
// /rest/system/status can be slow when Syncthing is busy rescanning a folder
// (observed 3-4s), so allow more slack than /api/sync's default 2s: timeout
// from [sync].onboarding_timeout_secs (default 5s). The three calls run in
// parallel threads below, so worst case is one slow call, not a sum.
fn rest_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(crate::settings::get().onboarding_timeout_secs())
}

// ---------------------------------------------------------------------------
// payloads
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct QrPayload {
    pub ok: bool,
    pub error: Option<String>,
    pub device_id: Option<String>,
    pub device_name: Option<String>,
    /// "rest" when myID came from the live daemon, "config" when parsed from
    /// config.xml (Syncthing was down - device may be a guess).
    pub source: Option<String>,
    /// Inline SVG markup; embed with innerHTML inside a white rounded box.
    pub qr_svg: Option<String>,
}

#[derive(Deserialize)]
pub struct QrQuery {
    /// Optional: encode arbitrary text instead of the device ID
    /// (e.g. the Syncthing-Fork download URL for the on-screen QR).
    pub text: Option<String>,
}

#[derive(Serialize)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub connected: bool,
}

#[derive(Serialize)]
pub struct StatusPayload {
    pub ok: bool,
    pub syncthing_up: bool,
    pub my_id: Option<String>,
    pub devices: Vec<DeviceRow>,
    pub paired_count: usize,
    pub connected_count: usize,
    pub phone_connected: bool,
}

#[derive(Serialize)]
pub struct PlainAppPayload {
    pub ok: bool,
    /// Reserved slot: filled when docs/plainapp-report.md lands.
    pub available: bool,
    pub note: String,
    pub items: Vec<serde_json::Value>,
    /// Download page for the phone app (also the on-screen QR text).
    pub download_url: String,
    /// Phones already registered in the panel (data/phones.json).
    pub registered: Vec<crate::api::plainapp::PhoneEntry>,
}

// ---------------------------------------------------------------------------
// Syncthing plumbing (self-contained; intentionally does not touch api/sync.rs
// so the two modules stay independent)
// ---------------------------------------------------------------------------

/// Read the Syncthing API key from config.xml (same source as /api/sync;
/// path resolved by config.rs so the panel stays device-agnostic).
fn read_apikey() -> Option<String> {
    let home = crate::config::get().syncthing_home.clone()?;
    let cfg = std::fs::read_to_string(home.join("config.xml")).ok()?;
    let start = cfg.find("<apikey>")? + "<apikey>".len();
    let end = cfg[start..].find("</apikey>")? + start;
    Some(cfg[start..end].to_string())
}

/// Snapshot of the local Syncthing state (all fields best-effort).
struct StSnapshot {
    up: bool,
    my_id: Option<String>,
    my_name: Option<String>,
    devices: Vec<(String, String)>, // (id, name) of every configured device
    connected: Vec<String>,          // ids currently connected
}

fn rest_get_json(agent: &ureq::Agent, key: &str, path: &str) -> Option<serde_json::Value> {
    agent
        .get(&format!("{}{path}", sync_rest()))
        .set("X-API-Key", key)
        .call()
        .ok()?
        .into_json::<serde_json::Value>()
        .ok()
}

fn rest_myid(key: &str) -> Option<String> {
    let agent = ureq::AgentBuilder::new().timeout(rest_timeout()).build();
    rest_get_json(&agent, key, "/rest/system/status")
        .and_then(|v| v.get("myID").and_then(|m| m.as_str()).map(|s| s.to_string()))
}

fn rest_devices(key: &str) -> Vec<(String, String)> {
    let agent = ureq::AgentBuilder::new().timeout(rest_timeout()).build();
    rest_get_json(&agent, key, "/rest/config/devices")
        .and_then(|v| v.get("devices").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|d| {
            let id = d.get("deviceID")?.as_str()?.to_string();
            let name = d.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            Some((id, name))
        })
        .collect()
}

fn rest_connected(key: &str) -> Vec<String> {
    let agent = ureq::AgentBuilder::new().timeout(rest_timeout()).build();
    rest_get_json(&agent, key, "/rest/system/connections")
        .and_then(|v| v.get("connections").and_then(|c| c.as_object()).cloned())
        .map(|obj| {
            obj.iter()
                .filter(|(_, c)| c.get("connected").and_then(|b| b.as_bool()).unwrap_or(false))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn snapshot_syncthing() -> StSnapshot {
    let Some(key) = read_apikey() else {
        return StSnapshot { up: false, my_id: None, my_name: None, devices: vec![], connected: vec![] };
    };
    // Three independent REST calls in parallel (status is occasionally slow).
    let (my_id, devices, connected) = std::thread::scope(|s| {
        let a = s.spawn(|| rest_myid(&key));
        let b = s.spawn(|| rest_devices(&key));
        let c = s.spawn(|| rest_connected(&key));
        (
            a.join().unwrap_or(None),
            b.join().unwrap_or_default(),
            c.join().unwrap_or_default(),
        )
    });

    let my_name = my_id
        .as_ref()
        .and_then(|id| devices.iter().find(|(d, _)| d == id).map(|(_, n)| n.clone()));

    StSnapshot { up: my_id.is_some(), my_id, my_name, devices, connected }
}

/// Best-effort local device ID from config.xml (used only when the daemon is
/// down). The local entry is not guaranteed to be first, so when several
/// named devices exist the answer may be wrong - callers must warn the user
/// to start Syncthing for a reliable pairing QR.
fn config_myid() -> Option<(String, String)> {
    let home = crate::config::get().syncthing_home.clone()?;
    let cfg = std::fs::read_to_string(home.join("config.xml")).ok()?;
    let mut rest = &cfg[..];
    let mut first: Option<(String, String)> = None;
    while let Some(pos) = rest.find("<device id=\"") {
        let tag_start = pos + "<device id=\"".len();
        let id_end = rest[tag_start..].find('"')? + tag_start;
        let id = rest[tag_start..id_end].to_string();
        let tag_end = rest[id_end..].find('>')? + id_end;
        let tag = &rest[id_end..tag_end];
        let name = tag
            .find("name=\"")
            .and_then(|p| {
                let s = p + "name=\"".len();
                tag[s..].find('"').map(|e| tag[s..s + e].to_string())
            })
            .unwrap_or_default();
        if first.is_none() && !id.is_empty() {
            first = Some((id.clone(), name.clone()));
        }
        rest = &rest[tag_end..];
    }
    first
}

/// Render any text as a QR SVG (white background, black modules, 4px module
/// size so the image stays crisp on high-DPI phone screens).
fn render_qr_svg(text: &str) -> String {
    let code = match QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M) {
        Ok(c) => c,
        Err(_) => QrCode::new(text.as_bytes()).expect("qr encode"),
    };
    code.render::<svg::Color>()
        .min_dimensions(4, 4)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build()
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// GET /api/onboarding/syncthing-qr[?text=...]
/// Default: pairing QR for this node's Syncthing device ID.
/// With ?text=: QR of the given string (used for app download links).
pub async fn syncthing_qr(Query(q): Query<QrQuery>) -> ApiResult<Json<QrPayload>> {
    let st = tokio::task::spawn_blocking(snapshot_syncthing)
        .await
        .unwrap_or_else(|_| StSnapshot { up: false, my_id: None, my_name: None, devices: vec![], connected: vec![] });

    if let Some(text) = q.text {
        if text.trim().is_empty() {
            return Ok(Json(QrPayload {
                ok: false,
                error: Some("text must not be empty".into()),
                device_id: None,
                device_name: None,
                source: None,
                qr_svg: None,
            }));
        }
        return Ok(Json(QrPayload {
            ok: true,
            error: None,
            device_id: None,
            device_name: None,
            source: Some("custom".into()),
            qr_svg: Some(render_qr_svg(&text)),
        }));
    }

    let (device_id, device_name, source) = match st.my_id {
        Some(id) => (id, st.my_name.unwrap_or_default(), "rest".to_string()),
        None => match config_myid() {
            Some((id, name)) => (id, name, "config".to_string()),
            None => {
                return Ok(Json(QrPayload {
                    ok: false,
                    error: Some("cannot determine Syncthing device ID (daemon down, config.xml unreadable)".into()),
                    device_id: None,
                    device_name: None,
                    source: None,
                    qr_svg: None,
                }));
            }
        },
    };

    Ok(Json(QrPayload {
        ok: true,
        error: None,
        device_id: Some(device_id.clone()),
        device_name: Some(device_name),
        source: Some(source),
        qr_svg: Some(render_qr_svg(&device_id)),
    }))
}

/// GET /api/onboarding/status
pub async fn status() -> ApiResult<Json<StatusPayload>> {
    let st = tokio::task::spawn_blocking(snapshot_syncthing)
        .await
        .unwrap_or_else(|_| StSnapshot { up: false, my_id: None, my_name: None, devices: vec![], connected: vec![] });

    let paired: Vec<DeviceRow> = st
        .devices
        .iter()
        .filter(|(id, _)| st.my_id.as_ref().map(|m| id != m).unwrap_or(true))
        .map(|(id, name)| DeviceRow {
            id: id.clone(),
            name: name.clone(),
            connected: st.connected.contains(id),
        })
        .collect();
    let paired_count = paired.len();
    let connected_count = st.connected.len();
    let phone_connected = st
        .my_id
        .as_ref()
        .map(|m| st.connected.iter().any(|c| c != m))
        .unwrap_or(false);

    Ok(Json(StatusPayload {
        ok: true,
        syncthing_up: st.up,
        my_id: st.my_id,
        devices: paired,
        paired_count,
        connected_count,
        phone_connected,
    }))
}

/// GET /api/onboarding/plainapp
/// Upgraded from the reserved slot to real guide data (plainapp-integration.md
/// section 4.3): download link (QR text), three-step guide, keep-alive items
/// and the phones already registered via /api/plainapp/register. The
/// {ok, available, note, items} shape is kept; new fields are additive.
pub async fn plainapp() -> ApiResult<Json<PlainAppPayload>> {
    // Releases page of the FlyGo app (PlainApp-based phone web service).
    const DOWNLOAD_URL: &str = "https://github.com/fengyuwang-com/FlyGo/releases";
    let registered = crate::api::plainapp::load_phones();
    Ok(Json(PlainAppPayload {
        ok: true,
        available: true,
        note: "install the FlyGo app, launch its web service, then register the phone address in the panel".into(),
        items: vec![
            serde_json::json!({
                "key": "download",
                "title": "下载安装 FlyGo App",
                "url": DOWNLOAD_URL,
                "qr_text": DOWNLOAD_URL,
            }),
            serde_json::json!({
                "key": "launch",
                "title": "打开发射",
                "text": "打开 App → 「发射」→ 启动 Web 服务，手机侧会显示访问地址",
            }),
            serde_json::json!({
                "key": "register",
                "title": "登记地址",
                "text": "在面板「总览 → 手机节点」登记手机地址（Tailscale IP 或局域网 IP），端口默认 8080",
            }),
            serde_json::json!({
                "key": "keepalive",
                "title": "保活清单",
                "text": "电池优化豁免 / 允许自启动 / 后台运行 / 最近任务锁定；按需在线即可，无需 7x24 常驻",
            }),
        ],
        download_url: DOWNLOAD_URL.to_string(),
        registered,
    }))
}
