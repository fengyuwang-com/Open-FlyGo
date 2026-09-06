// /api/syncview — sync network view ("the brain").
// Aggregates the local Syncthing REST API (127.0.0.1:8384) into one
// network-wide picture: which devices are online, which folders are shared
// where (replica count = number of devices per folder), per-folder file
// counts and sync completion percent.
//
// Endpoints:
//   GET /api/syncview/overview -> self + devices + folders (with db status)
//   GET /api/syncview/devices  -> self + devices (connection state only)
//
// All Syncthing calls are blocking ureq requests against the local GUI API
// and run inside spawn_blocking, so the tokio workers are never blocked.
// The API key is read from Syncthing's config.xml (same source as /api/sync).
//
// Notes on Syncthing v2 REST responses (verified against v2.1.3):
//   /rest/system/connections  -> {connections: {<deviceID>: {connected,...}}}
//   /rest/system/config       -> {devices:[{deviceID,name,...}], folders:[
//                                {id,label,path,devices:[{deviceID},...]}]}
//   /rest/system/status       -> {myID, uptime, ...} (myName absent in v2)
//   /rest/db/status?folder=X  -> {state, localFiles, globalFiles, ...,
//                                stateChanged}; no lastScan field in v2, so
//                                stateChanged is the freshness timestamp.
//                                Empty folder id returns 404 -> skipped.

use crate::ApiResult;
use axum::Json;
use serde::Serialize;

/// Syncthing REST base + timeout come from the config file
/// ([sync].syncthing_addr / timeout_secs, settings.rs) so sync.rs,
/// syncview.rs and onboarding.rs share one source.
fn st_base() -> String {
    crate::settings::get().syncthing_addr()
}

#[derive(Serialize)]
pub struct SyncDevice {
    pub device_id: String,
    pub short_id: String,
    pub name: String,
    pub online: bool,
    pub address: String,
}

#[derive(Serialize)]
pub struct SyncFolder {
    pub id: String,
    pub label: String,
    pub path: String,
    /// Number of devices this folder is shared with (including self) —
    /// the "replica count" view of the sync network.
    pub replica_count: usize,
    /// Names of the sharing devices (empty name falls back to short id).
    pub devices: Vec<String>,
    pub state: String,
    pub local_files: u64,
    pub global_files: u64,
    pub local_bytes: u64,
    pub global_bytes: u64,
    pub need_files: u64,
    pub need_bytes: u64,
    /// 0..100, rounded to one decimal; 100 when nothing is missing.
    pub completion_pct: f64,
    /// stateChanged timestamp from db/status (RFC3339 local time).
    pub last_change: String,
}

#[derive(Serialize)]
pub struct SyncOverview {
    pub ok: bool,
    pub error: Option<String>,
    pub syncthing_reachable: bool,
    pub my_id: Option<String>,
    pub my_name: Option<String>,
    pub devices: Vec<SyncDevice>,
    pub folders: Vec<SyncFolder>,
}

#[derive(Serialize)]
pub struct SyncDeviceList {
    pub ok: bool,
    pub error: Option<String>,
    pub syncthing_reachable: bool,
    pub my_id: Option<String>,
    pub my_name: Option<String>,
    pub devices: Vec<SyncDevice>,
}

/// Read the <apikey> element from Syncthing's config.xml. Path resolution:
/// FENGPAGE_SYNCTHING_HOME env override, else the platform-standard
/// Syncthing home (%LOCALAPPDATA%\Syncthing, fallback %APPDATA%\Syncthing),
/// so any Windows PC works without a hardcoded install path.
fn read_apikey() -> Option<String> {
    // Candidate homes in priority order; the first one whose config.xml
    // actually yields an <apikey> wins. Note LOCALAPPDATA/APPDATA always
    // exist on Windows, so a missing Syncthing subdir must fall through
    // instead of stopping the chain (that bug masked the panel home).
    let mut homes: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(v) = std::env::var("FENGPAGE_SYNCTHING_HOME") {
        if !v.trim().is_empty() {
            homes.push(std::path::PathBuf::from(v));
        }
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        homes.push(std::path::PathBuf::from(la).join("Syncthing"));
    }
    if let Some(ra) = std::env::var_os("APPDATA") {
        homes.push(std::path::PathBuf::from(ra).join("Syncthing"));
    }
    // Same source as the daemon launcher (T-09): when the panel started
    // syncthing itself, config.xml lives at the resolved panel home.
    if let Some(h) = crate::config::get().syncthing_home.clone() {
        homes.push(h);
    }
    for h in homes {
        let cfg = match std::fs::read_to_string(h.join("config.xml")) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let start = cfg.find("<apikey>")? + "<apikey>".len();
        let end = cfg[start..].find("</apikey>")? + start;
        return Some(cfg[start..end].to_string());
    }
    None
}

/// One blocking GET against the Syncthing GUI API (timeout from the config
/// file, default 3s).
fn st_get(apikey: &str, path: &str) -> Result<serde_json::Value, String> {
    let timeout_secs = crate::settings::get().syncview_timeout_secs();
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build();
    agent
        .get(&format!("{}{path}", st_base()))
        .set("X-API-Key", apikey)
        .call()
        .map_err(|e| format!("GET {path}: {e}"))?
        .into_json::<serde_json::Value>()
        .map_err(|e| format!("GET {path}: bad json: {e}"))
}

/// u64 out of a JSON number (int or float form).
fn u64_of(v: &serde_json::Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))
        .unwrap_or(0)
}

/// First 7 characters of a Syncthing device id (stable short code).
fn short_id(id: &str) -> String {
    id.chars().take(7).collect()
}

/// deviceID -> name map from the config's top-level device list.
fn device_name_map(config: &serde_json::Value) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    if let Some(devs) = config.get("devices").and_then(|d| d.as_array()) {
        for d in devs {
            if let Some(id) = d.get("deviceID").and_then(|v| v.as_str()) {
                let name = d
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| short_id(id));
                m.insert(id.to_string(), name);
            }
        }
    }
    m
}

/// Devices with live connection state. Returns (my_id, my_name, devices).
fn collect_devices(apikey: &str) -> Result<(String, String, Vec<SyncDevice>), String> {
    let conns = st_get(apikey, "/rest/system/connections")?;
    let config = st_get(apikey, "/rest/system/config")?;
    let status = st_get(apikey, "/rest/system/status")?;

    let my_id = status
        .get("myID")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let names = device_name_map(&config);
    let my_name = names
        .get(&my_id)
        .cloned()
        .unwrap_or_else(|| short_id(&my_id));

    let conn_map = conns
        .get("connections")
        .and_then(|c| c.as_object())
        .cloned()
        .unwrap_or_default();

    let mut devices = Vec::new();
    if let Some(devs) = config.get("devices").and_then(|d| d.as_array()) {
        for d in devs {
            let Some(id) = d.get("deviceID").and_then(|v| v.as_str()) else {
                continue;
            };
            let name = names.get(id).cloned().unwrap_or_else(|| short_id(id));
            let conn = conn_map.get(id);
            let mut online = conn
                .and_then(|c| c.get("connected"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut address = conn
                .and_then(|c| c.get("address"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // The local node is never listed as "connected" in
            // /rest/system/connections, but we just talked to it, so it is
            // online by definition.
            if id == my_id {
                online = true;
                if address.is_empty() {
                    address = "local".to_string();
                }
            }
            devices.push(SyncDevice {
                device_id: id.to_string(),
                short_id: short_id(id),
                name,
                online,
                address,
            });
        }
    }
    Ok((my_id, my_name, devices))
}

/// Per-folder db status merged with config info. A failed db/status call for
/// one folder degrades that folder to an "unknown" row instead of failing
/// the whole view.
fn collect_folders(apikey: &str) -> Vec<SyncFolder> {
    let Ok(config) = st_get(apikey, "/rest/system/config") else {
        return Vec::new();
    };
    let names = device_name_map(&config);
    let Some(folders) = config.get("folders").and_then(|f| f.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in folders {
        let id = f.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let label = f
            .get("label")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| id.clone());
        let path = f.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let mut share_names = Vec::new();
        if let Some(fdevs) = f.get("devices").and_then(|d| d.as_array()) {
            for fd in fdevs {
                if let Some(did) = fd.get("deviceID").and_then(|v| v.as_str()) {
                    share_names.push(
                        names
                            .get(did)
                            .cloned()
                            .unwrap_or_else(|| short_id(did)),
                    );
                }
            }
        }
        let replica_count = share_names.len();

        // db/status needs a non-empty folder id (empty id -> 404).
        let mut st = SyncFolder {
            id: id.clone(),
            label,
            path,
            replica_count,
            devices: share_names,
            state: "unknown".to_string(),
            local_files: 0,
            global_files: 0,
            local_bytes: 0,
            global_bytes: 0,
            need_files: 0,
            need_bytes: 0,
            completion_pct: 0.0,
            last_change: String::new(),
        };
        if !id.is_empty() {
            if let Ok(d) = st_get(apikey, &format!("/rest/db/status?folder={id}")) {
                st.state = d.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
                st.local_files = d.get("localFiles").map(u64_of).unwrap_or(0);
                st.global_files = d.get("globalFiles").map(u64_of).unwrap_or(0);
                st.local_bytes = d.get("localBytes").map(u64_of).unwrap_or(0);
                st.global_bytes = d.get("globalBytes").map(u64_of).unwrap_or(0);
                st.need_files = d.get("needFiles").map(u64_of).unwrap_or(0);
                st.need_bytes = d.get("needBytes").map(u64_of).unwrap_or(0);
                st.last_change = d
                    .get("stateChanged")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                st.completion_pct = if st.global_bytes > 0 {
                    let done = st.global_bytes.saturating_sub(st.need_bytes) as f64
                        / st.global_bytes as f64
                        * 100.0;
                    (done.clamp(0.0, 100.0) * 10.0).round() / 10.0
                } else {
                    100.0
                };
            }
        }
        out.push(st);
    }
    out
}

fn collect_overview(apikey: &str) -> Result<SyncOverview, String> {
    let (my_id, my_name, devices) = collect_devices(apikey)?;
    let folders = collect_folders(apikey);
    Ok(SyncOverview {
        ok: true,
        error: None,
        syncthing_reachable: true,
        my_id: Some(my_id),
        my_name: Some(my_name),
        devices,
        folders,
    })
}

/// Failure fallback: reachable=false + error message, so the frontend can
/// tell "Syncthing down / no api key" apart from an empty network.
fn overview_err(e: String) -> SyncOverview {
    SyncOverview {
        ok: true,
        error: Some(e),
        syncthing_reachable: false,
        my_id: None,
        my_name: None,
        devices: Vec::new(),
        folders: Vec::new(),
    }
}

pub async fn overview() -> ApiResult<Json<SyncOverview>> {
    let apikey = read_apikey();
    let r = tokio::task::spawn_blocking(move || match apikey.as_deref() {
        Some(k) => collect_overview(k),
        None => Err("Syncthing API key not found in config.xml".to_string()),
    })
    .await
    .unwrap_or_else(|e| Err(format!("syncview task panicked: {e}")));
    Ok(Json(match r {
        Ok(ov) => ov,
        Err(e) => overview_err(e),
    }))
}

pub async fn devices() -> ApiResult<Json<SyncDeviceList>> {
    let apikey = read_apikey();
    let r = tokio::task::spawn_blocking(move || match apikey.as_deref() {
        Some(k) => collect_devices(k).map(|(my_id, my_name, devices)| SyncDeviceList {
            ok: true,
            error: None,
            syncthing_reachable: true,
            my_id: Some(my_id),
            my_name: Some(my_name),
            devices,
        }),
        None => Err("Syncthing API key not found in config.xml".to_string()),
    })
    .await
    .unwrap_or_else(|e| Err(format!("syncview task panicked: {e}")));
    Ok(Json(match r {
        Ok(l) => l,
        Err(e) => SyncDeviceList {
            ok: true,
            error: Some(e),
            syncthing_reachable: false,
            my_id: None,
            my_name: None,
            devices: Vec::new(),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True when the local Syncthing GUI API is accepting connections
    /// (probes the default port; a custom [services].ports.syncthing needs a
    /// running daemon to test live).
    fn syncthing_up() -> bool {
        let addr = format!("127.0.0.1:{}", crate::settings::DEFAULT_SYNCTHING_PORT);
        std::net::TcpStream::connect_timeout(
            &addr.parse().unwrap(),
            std::time::Duration::from_millis(500),
        )
        .is_ok()
    }

    /// Live aggregation smoke test: requires a running Syncthing on
    /// 127.0.0.1:8384 with a readable config.xml; skipped when it is down.
    /// Prints the full overview when it runs.
    #[test]
    fn live_overview_collect() {
        crate::config::init(); // freezes settings + config (read_apikey needs config::get)
        if !syncthing_up() {
            eprintln!("SKIP live_overview_collect: local Syncthing not running");
            return;
        }
        // The engine is up, but its config.xml may live outside the
        // platform-default homes this test can see (e.g. a FengNAS or
        // winget-managed install). No readable apikey -> skip, same as down.
        let Some(key) = read_apikey() else {
            eprintln!("SKIP live_overview_collect: config.xml/apikey not readable");
            return;
        };
        let ov = collect_overview(&key).expect("overview collected");
        eprintln!("LIVE OVERVIEW:\n{}", serde_json::to_string_pretty(&ov).unwrap());
        assert!(ov.syncthing_reachable, "syncthing must be reachable");
        assert!(ov.devices.len() >= 1, "at least one device");
        assert!(
            ov.my_id.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
            "my id must be present"
        );
    }

    /// Route smoke test: serve both syncview routes on an ephemeral port and
    /// hit them over real HTTP (curl-equivalent, in-process). Multi-thread
    /// runtime: ureq is a blocking client and must not starve the spawned
    /// axum server on the only worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn routes_serve_http() {
        crate::config::init(); // freezes settings + config (read_apikey needs config::get)
        use axum::routing::get;
        let app = axum::Router::new()
            .route(
                "/",
                get(|| async {
                    (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        crate::INDEX_HTML,
                    )
                }),
            )
            .route("/api/syncview/overview", get(overview))
            .route("/api/syncview/devices", get(devices));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(8))
            .build();
        // Reachable in the route sense = engine up AND apikey resolvable;
        // the route returns degraded data when either is missing.
        let up = syncthing_up() && read_apikey().is_some();
        // Readiness poll: give the spawned server task a few tries.
        let mut ok = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if agent
                .get(&format!("http://{addr}/api/syncview/overview"))
                .call()
                .is_ok()
            {
                ok = true;
                break;
            }
        }
        assert!(ok, "test server did not come up on {addr}");

        for path in ["/api/syncview/overview", "/api/syncview/devices"] {
            let body = agent
                .get(&format!("http://{addr}{path}"))
                .call()
                .unwrap_or_else(|e| panic!("GET {path} failed: {e}"))
                .into_string()
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&body).expect("valid json body");
            assert_eq!(v["ok"], true, "{path}: ok flag");
            // The reachable flag must match the live Syncthing state: true
            // when the engine runs (full data), false when it is down.
            assert_eq!(v["syncthing_reachable"], up, "{path}: reachable flag");
            assert!(v["devices"].is_array(), "{path}: devices array");
            eprintln!("HTTP {path} -> ok devices={} folders={}",
                v["devices"].as_array().map(|a| a.len()).unwrap_or(0),
                v["folders"].as_array().map(|a| a.len()).unwrap_or(0));
        }
        // Manual verification hook: with SYNCVIEW_HOLD set, keep serving so a
        // real curl from the shell can hit the ephemeral port (printed above).
        if let Ok(hold) = std::env::var("SYNCVIEW_HOLD") {
            let secs: u64 = hold.parse().unwrap_or(60);
            eprintln!("[hold] serving on {addr} for {secs}s (curl http://{addr}/api/syncview/overview)");
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
        server.abort();
    }
}
