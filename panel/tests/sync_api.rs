// Integration test for the M06 sync view endpoints (/api/sync/*).
//
// Runs as its own test binary (like config_api.rs) so FENGPAGE_* env vars
// can be set without racing the unit-test binary. The real handlers from
// src/api/sync.rs run over real HTTP against an in-process axum server,
// backed by a mock Syncthing REST API. All state lives in a temp install
// root (fake syncthing config.xml included); the production install and
// C:/FengNAS are never touched.
//
// Phases (sequential in one test because the process env is shared):
//   1. overview with Syncthing unreachable -> 200 + degraded null block
//   2. mirror log appears -> last_run_unix populated
//   3. link roundtrip (validation error, mirror create/list/delete, manual)
//   4. mock Syncthing up -> overview parses devices/folders; syncthing link
//      create (engine folder PUT), duplicate-path rejection, delete (engine
//      folder DELETE best-effort)
//
// All code comments are pure ASCII.

#[path = "../src/settings.rs"]
mod settings;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/api/sync.rs"]
mod sync;

use axum::extract::Path as AxPath;
use axum::Json;
use axum::Router;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("flygo-sync-it-{}", std::process::id()))
}

/// In-process mock Syncthing REST API (only the endpoints sync.rs touches).
fn mock_syncthing(folders: Arc<Mutex<Vec<Value>>>) -> Router {
    async fn status() -> Json<Value> {
        Json(json!({ "myID": "SELFDEV1CEID0000000000000000000000000000000000000000" }))
    }
    let devs = || {
        json!([
            { "deviceID": "SELFDEV1CEID0000000000000000000000000000000000000000", "name": "nas" },
            { "deviceID": "PEERDEV1CEID0000000000000000000000000000000000000000", "name": "phone" },
        ])
    };
    let f_list = folders.clone();
    let f_put = folders.clone();
    let f_del = folders.clone();
    Router::new()
        .route("/rest/system/status", axum::routing::get(status))
        .route(
            "/rest/config/devices",
            axum::routing::get(move || async move { Json(devs()) }),
        )
        .route(
            "/rest/system/connections",
            axum::routing::get(|| async move {
                Json(json!({
                    "connections": {
                        "SELFDEV1CEID0000000000000000000000000000000000000000": { "connected": false },
                        "PEERDEV1CEID0000000000000000000000000000000000000000": { "connected": true, "address": "192.0.2.9:22000" }
                    }
                }))
            }),
        )
        .route(
            "/rest/config/folders",
            axum::routing::get(move || {
                let f = f_list.clone();
                async move { Json(Value::Array(f.lock().unwrap().clone())) }
            }),
        )
        .route(
            "/rest/config/folders/:id",
            axum::routing::put(move |AxPath(_id): AxPath<String>, Json(body): Json<Value>| {
                let f = f_put.clone();
                async move {
                    f.lock().unwrap().push(body);
                    Json(json!({ "ok": true }))
                }
            })
            .delete(move |AxPath(id): AxPath<String>| {
                let f = f_del.clone();
                async move {
                    let mut g = f.lock().unwrap();
                    g.retain(|x| x.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
                    Json(json!({ "ok": true }))
                }
            }),
        )
        .route(
            "/rest/db/status",
            axum::routing::get(|| async move {
                Json(json!({ "state": "idle", "needFiles": 0, "globalBytes": 10, "needBytes": 0 }))
            }),
        )
}

#[test]
fn sync_api_over_http() {
    let dir = temp_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("photos")).unwrap();
    std::fs::create_dir_all(dir.join("mirror")).unwrap();
    std::fs::create_dir_all(dir.join("syncthing-home")).unwrap();
    // Fake Syncthing home: config.xml with a throwaway API key (never printed).
    std::fs::write(
        dir.join("syncthing-home").join("config.xml"),
        "<configuration><gui><apikey>test-api-key-not-secret</apikey></gui></configuration>",
    )
    .unwrap();

    // Isolate every machine-specific resolution into the temp root.
    std::env::set_var("FENGPAGE_ROOT", &dir);
    std::env::set_var("FENGPAGE_SYNCTHING_HOME", dir.join("syncthing-home"));
    std::env::set_var("FENGPAGE_MIRROR_LOG", dir.join("mirror").join("sync.log"));
    // Phase 1: nothing listens on this port -> degraded overview.
    std::env::set_var("FENGPAGE_SYNCTHING_ADDR", "http://127.0.0.1:59999");
    config::init(); // also freezes settings::init()

    // Sync state exactly as main.rs builds it, with a fixed tray snapshot.
    let state = Arc::new(sync::SyncState {
        tray: Box::new(|| sync::TraySnapshot {
            timer_enabled: true,
            timer_last_run: Some("2026-08-17 01:23:45".into()),
            syncthing_running: false,
            syncthing_pid: 0,
            usb_present: true,
            usb_letters: vec!["E".into()],
        }),
        links_path: dir.join("data").join("sync-links.json"),
    });
    let app = Router::new()
        .route("/api/sync", axum::routing::get(sync::sync))
        .route("/api/sync/overview", axum::routing::get(sync::overview))
        .route("/api/sync/link", axum::routing::post(sync::create_link))
        .route("/api/sync/links", axum::routing::get(sync::list_links))
        .route(
            "/api/sync/links/:id",
            axum::routing::delete(sync::delete_link),
        )
        .with_state(state);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(8))
            .build();
        let get_json = |path: &str| -> Value {
            let body = agent
                .get(&format!("http://{addr}{path}"))
                .call()
                .unwrap_or_else(|e| panic!("GET {path} failed: {e}"))
                .into_string()
                .unwrap();
            serde_json::from_str(&body).unwrap()
        };
        let call = |method: &str, path: &str, body: Option<&Value>| -> Value {
            let req = agent.request(method, &format!("http://{addr}{path}"));
            let resp = match body {
                Some(b) => req
                    .set("Content-Type", "application/json")
                    .send_json(b.clone()),
                None => req.call(),
            }
            .unwrap_or_else(|e| panic!("{method} {path} failed: {e}"));
            let text = resp.into_string().unwrap();
            serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{method} {path} bad json ({e}): {text}"))
        };

        // Readiness poll.
        let mut up = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if agent
                .get(&format!("http://{addr}/api/sync/overview"))
                .call()
                .is_ok()
            {
                up = true;
                break;
            }
        }
        assert!(up, "sync API did not come up");

        // --- Phase 1: Syncthing unreachable -> 200 + degraded null block ---
        let v = get_json("/api/sync/overview");
        assert_eq!(v["ok"], true, "overview must stay ok with engine down");
        assert_eq!(v["syncthing"], Value::Null, "degraded: syncthing block is null");
        assert_eq!(v["syncthing_reachable"], false);
        assert!(
            v["syncthing_error"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "degraded reason present"
        );
        assert_eq!(v["tray"]["timer_enabled"], true, "tray snapshot wired");
        assert_eq!(v["tray"]["usb_letters"][0], "E");
        assert_eq!(v["mirror"]["log_exists"], false, "mirror log not written yet");
        assert!(v["quick_dirs"].as_array().is_some(), "quick_dirs array");

        // --- Phase 2: mirror log appears -> last_run_unix populated ---
        std::fs::write(dir.join("mirror").join("sync.log"), "robocopy done\n").unwrap();
        let v = get_json("/api/sync/overview");
        assert_eq!(v["mirror"]["log_exists"], true);
        assert!(
            v["mirror"]["last_run_unix"].as_u64().unwrap_or(0) > 0,
            "mirror last_run_unix parsed from log mtime"
        );

        // --- Phase 3: link roundtrip ---
        // Relative path rejected.
        let v = call(
            "POST",
            "/api/sync/link",
            Some(&json!({ "folder_path": "relative/photos", "target_kind": "mirror" })),
        );
        assert_eq!(v["ok"], false, "relative path rejected");

        // Missing directory rejected.
        let v = call(
            "POST",
            "/api/sync/link",
            Some(&json!({ "folder_path": "E:/definitely/not/here", "target_kind": "manual" })),
        );
        assert_eq!(v["ok"], false, "missing dir rejected");

        // Mirror link: created, listed, deleted.
        let photos = dir.join("photos").to_string_lossy().replace('\\', "/");
        let dest = dir.join("mirror").join("photos").to_string_lossy().replace('\\', "/");
        let v = call(
            "POST",
            "/api/sync/link",
            Some(&json!({ "folder_path": photos, "target_kind": "mirror", "target_id": dest })),
        );
        assert_eq!(v["ok"], true, "mirror link created: {v}");
        let id = v["link"]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("lnk-"), "link id slug: {id}");
        assert_eq!(v["link"]["kind"], "mirror");
        assert!(dir.join("data").join("sync-links.json").exists(), "registry written");
        assert!(
            !dir.join("data").join("sync-links.json.tmp").exists(),
            "atomic write: tmp renamed away"
        );

        let v = get_json("/api/sync/links");
        assert_eq!(v["ok"], true);
        assert_eq!(v["links"].as_array().map(|a| a.len()), Some(1), "one link listed");

        let v = call("DELETE", &format!("/api/sync/links/{id}"), None);
        assert_eq!(v["ok"], true, "link deleted: {v}");
        let v = get_json("/api/sync/links");
        assert_eq!(v["links"].as_array().map(|a| a.len()), Some(0), "list empty after delete");

        // Manual link records without touching any engine.
        let v = call(
            "POST",
            "/api/sync/link",
            Some(&json!({ "folder_path": photos, "target_kind": "manual" })),
        );
        assert_eq!(v["ok"], true, "manual link recorded");
        let manual_id = v["link"]["id"].as_str().unwrap().to_string();
        call("DELETE", &format!("/api/sync/links/{manual_id}"), None);

        // --- Phase 4: mock Syncthing -> live overview + engine-backed links ---
        let folders: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let mock = mock_syncthing(folders.clone());
        let ml = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let maddr = ml.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(ml, mock).await.unwrap() });
        std::env::set_var("FENGPAGE_SYNCTHING_ADDR", format!("http://{maddr}"));

        let v = get_json("/api/sync/overview");
        assert_eq!(v["syncthing_reachable"], true, "mock reachable");
        let devs = v["syncthing"]["devices"].as_array().cloned().unwrap_or_default();
        assert_eq!(devs.len(), 2, "self + peer devices");
        assert_eq!(devs.iter().filter(|d| d["connected"] == true).count(), 2, "self counts as online");
        assert!(devs.iter().any(|d| d["name"] == "phone"), "device names resolved");

        // Syncthing link creates a folder through the engine REST API.
        let v = call(
            "POST",
            "/api/sync/link",
            Some(&json!({
                "folder_path": photos,
                "target_kind": "syncthing",
                "target_id": "PEERDEV1CEID0000000000000000000000000000000000000000"
            })),
        );
        assert_eq!(v["ok"], true, "syncthing link created: {v}");
        let st_id = v["link"]["st_folder_id"].as_str().unwrap().to_string();
        assert!(st_id.starts_with("flygo-"), "engine folder id: {st_id}");
        let link_id = v["link"]["id"].as_str().unwrap().to_string();

        // The engine call carried the fixed defaults (params not UI-exposed).
        let pushed = folders.lock().unwrap().clone();
        assert_eq!(pushed.len(), 1, "one folder PUT to the engine");
        assert_eq!(pushed[0]["type"], "sendreceive");
        assert_eq!(pushed[0]["versioning"]["type"], "staggered");
        assert_eq!(pushed[0]["versioning"]["params"]["maxAge"], "604800", "7-day window");
        assert_eq!(pushed[0]["devices"].as_array().map(|a| a.len()), Some(2), "self + peer shared");

        // Overview now reports the folder; duplicate path rejected.
        let v = get_json("/api/sync/overview");
        let flds = v["syncthing"]["folders"].as_array().cloned().unwrap_or_default();
        assert_eq!(flds.len(), 1, "folder visible in overview");
        assert_eq!(flds[0]["state"], "idle", "db/status state merged");
        let v = call(
            "POST",
            "/api/sync/link",
            Some(&json!({ "folder_path": photos, "target_kind": "syncthing" })),
        );
        assert_eq!(v["ok"], false, "duplicate folder path rejected");

        // Delete removes the record and the engine folder best-effort.
        let v = call("DELETE", &format!("/api/sync/links/{link_id}"), None);
        assert_eq!(v["ok"], true, "syncthing link deleted");
        assert_eq!(folders.lock().unwrap().len(), 0, "engine folder removed");
        let v = get_json("/api/sync/links");
        assert_eq!(v["links"].as_array().map(|a| a.len()), Some(0));

        // Unknown id delete -> clean error, list untouched.
        let v = call("DELETE", "/api/sync/links/lnk-00000000", None);
        assert_eq!(v["ok"], false);

        // Legacy endpoint still serves its payload shape.
        let v = get_json("/api/sync");
        assert_eq!(v["syncthing_reachable"], true);
        assert!(v.get("mirror_src").is_some(), "legacy fields intact");

        server.abort();
    });

    // Cleanup (best effort; a failed assert leaves the temp dir for review).
    std::env::remove_var("FENGPAGE_ROOT");
    std::env::remove_var("FENGPAGE_SYNCTHING_HOME");
    std::env::remove_var("FENGPAGE_MIRROR_LOG");
    std::env::remove_var("FENGPAGE_SYNCTHING_ADDR");
    let _ = std::fs::remove_dir_all(&dir);
}
