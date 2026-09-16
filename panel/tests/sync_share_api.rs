// Integration test for POST /api/sync/folders/:id/share - the
// phone-to-phone relay endpoint (D6): an existing Syncthing folder is shared
// with one or more already-paired devices so two phones sync with each other
// through this PC as the relay node.
//
// Runs as its own test binary (own process, so FENGPAGE_* env vars do not
// race other test binaries). The real handler from src/api/sync.rs runs over
// real HTTP against an in-process axum server backed by a mock Syncthing REST
// API. All state lives in a temp install root; the production install and
// C:/FlyGoLegacy are never touched.
//
// Phases (sequential in one test because the process env is shared):
//   1. share appends both devices to the folder share list and PUTs unknown
//      devices into the engine config first
//   2. re-sharing an already-shared device is a no-op in the share list
//   3. unknown folder id -> ok:false with a clear message
//   4. empty device_ids -> rejected before any engine call
//
// All code comments are pure ASCII.

#[allow(dead_code)]
#[path = "../src/settings.rs"]
mod settings;
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/api/sync.rs"]
mod sync;

use axum::extract::Path as AxPath;
use axum::Json;
use axum::Router;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const DEV_A: &str = "PHONEA1CEID000000000000000000000000000000000000000000";
const DEV_B: &str = "PHONEB1CEID000000000000000000000000000000000000000000";

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("flygo-share-it-{}", std::process::id()))
}

struct MockState {
    devices: Arc<Mutex<Vec<Value>>>,
    folders: Arc<Mutex<Vec<Value>>>,
}

/// Mock Syncthing REST API covering every endpoint the share handler calls.
fn mock_syncthing(st: Arc<MockState>) -> Router {
    let f_list = st.folders.clone();
    let f_put = st.folders.clone();
    let d_put = st.devices.clone();
    Router::new()
        .route(
            "/rest/config/folders",
            axum::routing::get(move || {
                let f = f_list.clone();
                async move { Json(Value::Array(f.lock().unwrap().clone())) }
            }),
        )
        .route(
            "/rest/config/folders/:id",
            axum::routing::put(move |AxPath(id): AxPath<String>, Json(body): Json<Value>| {
                let f = f_put.clone();
                async move {
                    let mut g = f.lock().unwrap();
                    let mut body = body;
                    body["id"] = json!(id);
                    g.retain(|x| x.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
                    g.push(body.clone());
                    Json(json!({ "ok": true }))
                }
            }),
        )
        .route(
            "/rest/config/devices/:id",
            axum::routing::put(move |AxPath(id): AxPath<String>, Json(body): Json<Value>| {
                let d = d_put.clone();
                async move {
                    let mut body = body;
                    body["deviceID"] = json!(id);
                    d.lock().unwrap().push(body);
                    Json(json!({ "ok": true }))
                }
            }),
        )
}

#[test]
fn share_folder_api_over_http() {
    let dir = temp_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("syncthing-home")).unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(
        dir.join("syncthing-home").join("config.xml"),
        "<configuration><gui><apikey>test-api-key-not-secret</apikey></gui></configuration>",
    )
    .unwrap();

    std::env::set_var("FENGPAGE_ROOT", &dir);
    std::env::set_var("FENGPAGE_SYNCTHING_HOME", dir.join("syncthing-home"));
    std::env::set_var("FENGPAGE_SYNC_LINKS", dir.join("data").join("sync-links.json"));
    std::env::set_var("FENGPAGE_SYNCTHING_ADDR", "http://127.0.0.1:59997");
    config::init();

    // Engine config already holds one existing folder shared with self only.
    let st = Arc::new(MockState {
        devices: Arc::new(Mutex::new(Vec::new())),
        folders: Arc::new(Mutex::new(vec![json!({
            "id": "flygo-photos",
            "label": "Photos",
            "path": "D:/Photos",
            "type": "sendreceive",
            "devices": [ { "deviceID": "SELF" } ],
        })])),
    });

    let app = Router::new().route(
        "/api/sync/folders/:id/share",
        axum::routing::post(sync::share_folder),
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(8))
            .build();
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

        // Readiness poll; the mock is not up yet at first, so tolerate errors.
        let mock = mock_syncthing(st.clone());
        let ml = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let maddr = ml.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(ml, mock).await.unwrap() });
        std::env::set_var("FENGPAGE_SYNCTHING_ADDR", format!("http://{maddr}"));
        let mut up = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if agent.get(&format!("http://{maddr}/rest/config/folders")).call().is_ok() {
                up = true;
                break;
            }
        }
        assert!(up, "mock syncthing did not come up");

        // --- Phase 1: share with two devices; both land in the share list ---
        let v = call(
            "POST",
            "/api/sync/folders/flygo-photos/share",
            Some(&json!({ "device_ids": [DEV_A, DEV_B] })),
        );
        assert_eq!(v["ok"], true, "share accepted: {v}");
        let pushed = st.folders.lock().unwrap().clone();
        let photos = pushed
            .iter()
            .find(|f| f["id"] == "flygo-photos")
            .cloned()
            .unwrap_or(Value::Null);
        let peers = photos["devices"].as_array().cloned().unwrap_or_default();
        assert_eq!(peers.len(), 3, "self + two phones");
        assert!(peers.iter().any(|d| d["deviceID"] == DEV_A));
        assert!(peers.iter().any(|d| d["deviceID"] == DEV_B));
        // Unknown devices were PUT into the engine config before sharing.
        let devs = st.devices.lock().unwrap().clone();
        assert!(
            devs.iter().any(|d| d["deviceID"] == DEV_A) && devs.iter().any(|d| d["deviceID"] == DEV_B),
            "both devices registered in engine config",
        );
        // Folder attributes preserved by the share PUT.
        assert_eq!(photos["type"], "sendreceive");
        assert_eq!(photos["path"], "D:/Photos");

        // --- Phase 2: re-share DEV_A only -> share list unchanged ---
        let v = call(
            "POST",
            "/api/sync/folders/flygo-photos/share",
            Some(&json!({ "device_ids": [DEV_A] })),
        );
        assert_eq!(v["ok"], true, "re-share accepted: {v}");
        let pushed = st.folders.lock().unwrap().clone();
        let photos = pushed
            .iter()
            .find(|f| f["id"] == "flygo-photos")
            .cloned()
            .unwrap_or(Value::Null);
        let peers = photos["devices"].as_array().cloned().unwrap_or_default();
        assert_eq!(peers.len(), 3, "already-shared device not duplicated");

        // --- Phase 3: unknown folder -> ok:false ---
        let v = call(
            "POST",
            "/api/sync/folders/no-such-folder/share",
            Some(&json!({ "device_ids": [DEV_A] })),
        );
        assert_eq!(v["ok"], false, "unknown folder rejected");

        // --- Phase 4: empty device list -> rejected before engine calls ---
        let v = call(
            "POST",
            "/api/sync/folders/flygo-photos/share",
            Some(&json!({ "device_ids": [] })),
        );
        assert_eq!(v["ok"], false, "empty device_ids rejected");

        server.abort();
    });

    // Cleanup (best effort; a failed assert leaves the temp dir for review).
    std::env::remove_var("FENGPAGE_ROOT");
    std::env::remove_var("FENGPAGE_SYNCTHING_HOME");
    std::env::remove_var("FENGPAGE_SYNC_LINKS");
    std::env::remove_var("FENGPAGE_SYNCTHING_ADDR");
    let _ = std::fs::remove_dir_all(&dir);
}
