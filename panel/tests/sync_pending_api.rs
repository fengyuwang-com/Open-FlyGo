// Integration test for the pending devices/folders endpoints
// (/api/sync/pending*) - the phone pairing closure.
//
// Runs as its own test binary (own process, so FENGPAGE_* env vars do not
// race other test binaries). The real handlers from src/api/sync.rs run over
// real HTTP against an in-process axum server backed by a mock Syncthing
// REST API. All state lives in a temp install root; the production install
// and C:/FlyGoLegacy are never touched.
//
// Phases (sequential in one test because the process env is shared):
//   1. engine unreachable -> 200 + empty lists (degraded, not an error)
//   2. pending devices/folders listed from the mock
//   3. device accept -> PUT /rest/config/devices/:id; dismiss -> DELETE
//   4. folder accept (new) -> PUT /rest/config/folders/:id unpaused with
//      self+device; folder accept (existing) -> device appended; dismiss ->
//      DELETE pending folder
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

const PHONE_ID: &str = "PHONEDEV1CEID000000000000000000000000000000000000000";

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("flygo-pend-it-{}", std::process::id()))
}

struct MockState {
    devices: Arc<Mutex<Vec<Value>>>,
    folders: Arc<Mutex<Vec<Value>>>,
    dismissed_devices: Arc<Mutex<Vec<String>>>,
    dismissed_folders: Arc<Mutex<Vec<String>>>,
}

/// Mock Syncthing REST API covering every endpoint the pending handlers call.
fn mock_syncthing(st: Arc<MockState>) -> Router {
    async fn status() -> Json<Value> {
        Json(json!({ "myID": "SELFDEV1CEID0000000000000000000000000000000000000000" }))
    }
    let f_list = st.folders.clone();
    let f_put = st.folders.clone();
    let d_put = st.devices.clone();
    let dd = st.dismissed_devices.clone();
    let df = st.dismissed_folders.clone();
    Router::new()
        .route("/rest/system/status", axum::routing::get(status))
        .route(
            "/rest/config/pending/devices",
            axum::routing::get(|| async move {
                // Object-keyed shape; body carries the announced name.
                Json(json!({
                    PHONE_ID: { "time": "2026-09-11T00:00:00Z", "deviceID": PHONE_ID, "name": "pixel" }
                }))
            }),
        )
        .route(
            "/rest/config/pending/devices/:id",
            axum::routing::delete(move |AxPath(id): AxPath<String>| {
                let dd = dd.clone();
                async move {
                    dd.lock().unwrap().push(id);
                    Json(json!({ "ok": true }))
                }
            }),
        )
        .route(
            "/rest/config/pending/folders",
            axum::routing::get(|| async move {
                // Object-keyed shape keyed by folder id; no "id" field in the
                // body (the key fallback path in pending_entries).
                Json(json!({
                    "photos": {
                        "time": "2026-09-11T00:00:00Z",
                        "offeredBy": {
                            PHONE_ID: { "label": "Photos", "receiveEncrypted": false }
                        }
                    }
                }))
            }),
        )
        .route(
            "/rest/config/pending/folders/:id",
            axum::routing::delete(move |AxPath(id): AxPath<String>| {
                let df = df.clone();
                async move {
                    df.lock().unwrap().push(id);
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
}

#[test]
fn pending_api_over_http() {
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
    // Phase 1: nothing listening -> degraded empty pending lists.
    std::env::set_var("FENGPAGE_SYNCTHING_ADDR", "http://127.0.0.1:59998");
    config::init();

    let app = Router::new()
        .route("/api/sync/pending", axum::routing::get(sync::pending))
        .route(
            "/api/sync/pending/devices/:id/accept",
            axum::routing::post(sync::accept_pending_device),
        )
        .route(
            "/api/sync/pending/devices/:id/dismiss",
            axum::routing::delete(sync::dismiss_pending_device),
        )
        .route(
            "/api/sync/pending/folders/:folder/:device/accept",
            axum::routing::post(sync::accept_pending_folder),
        )
        .route(
            "/api/sync/pending/folders/:folder/:device/dismiss",
            axum::routing::delete(sync::dismiss_pending_folder),
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

        // Readiness poll against a request that always answers.
        let mut up = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if agent.get(&format!("http://{addr}/api/sync/pending")).call().is_ok() {
                up = true;
                break;
            }
        }
        assert!(up, "pending API did not come up");

        // --- Phase 1: engine down -> 200, empty lists, no error throw ---
        let v = call("GET", "/api/sync/pending", None);
        assert_eq!(v["ok"], true, "pending degrades to ok with engine down");
        assert_eq!(v["devices"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(v["folders"].as_array().map(|a| a.len()), Some(0));

        // --- Phase 2: mock up -> pending lists populated ---
        let st = Arc::new(MockState {
            devices: Arc::new(Mutex::new(Vec::new())),
            folders: Arc::new(Mutex::new(Vec::new())),
            dismissed_devices: Arc::new(Mutex::new(Vec::new())),
            dismissed_folders: Arc::new(Mutex::new(Vec::new())),
        });
        let mock = mock_syncthing(st.clone());
        let ml = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let maddr = ml.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(ml, mock).await.unwrap() });
        std::env::set_var("FENGPAGE_SYNCTHING_ADDR", format!("http://{maddr}"));

        let v = call("GET", "/api/sync/pending", None);
        assert_eq!(v["ok"], true);
        let devs = v["devices"].as_array().cloned().unwrap_or_default();
        assert_eq!(devs.len(), 1, "one pending phone listed");
        assert_eq!(devs[0]["id"], PHONE_ID);
        assert_eq!(devs[0]["name"], "pixel", "announced name surfaced");
        let folds = v["folders"].as_array().cloned().unwrap_or_default();
        assert_eq!(folds.len(), 1, "one pending folder listed");
        assert_eq!(folds[0]["id"], "photos");
        assert_eq!(folds[0]["label"], "Photos", "label from offeredBy");
        assert_eq!(folds[0]["offered_by"][0]["device"], PHONE_ID);

        // --- Phase 3: device accept / dismiss ---
        let v = call("POST", &format!("/api/sync/pending/devices/{PHONE_ID}/accept"), None);
        assert_eq!(v["ok"], true, "device accepted: {v}");
        let pushed = st.devices.lock().unwrap().clone();
        assert_eq!(pushed.len(), 1, "one device PUT to the engine");
        assert_eq!(pushed[0]["deviceID"], PHONE_ID);
        assert_eq!(pushed[0]["addresses"][0], "dynamic", "dynamic addresses");
        assert_eq!(pushed[0]["paused"], false);

        let v = call("DELETE", &format!("/api/sync/pending/devices/{PHONE_ID}/dismiss"), None);
        assert_eq!(v["ok"], true, "device dismissed: {v}");
        assert_eq!(
            st.dismissed_devices.lock().unwrap().clone(),
            vec![PHONE_ID.to_string()],
            "dismiss DELETE hit the engine pending store"
        );

        // --- Phase 4: folder accept (new) / dismiss / accept (existing) ---
        // New folder: default path under <root>/sync/<folder id>.
        let v = call(
            "POST",
            &format!("/api/sync/pending/folders/photos/{PHONE_ID}/accept"),
            None,
        );
        assert_eq!(v["ok"], true, "new folder accepted: {v}");
        let pushed = st.folders.lock().unwrap().clone();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0]["id"], "photos");
        assert_eq!(pushed[0]["paused"], false, "accepted folder is unpaused");
        assert_eq!(pushed[0]["type"], "sendreceive");
        assert_eq!(pushed[0]["versioning"]["type"], "staggered", "engine defaults kept");
        assert_eq!(pushed[0]["label"], "Photos", "label carried from the offer");
        let peers = pushed[0]["devices"].as_array().cloned().unwrap_or_default();
        assert_eq!(peers.len(), 2, "self + offering device");
        assert!(
            peers.iter().any(|d| d["deviceID"] == PHONE_ID),
            "offering device added to the share list"
        );
        assert!(
            PathBuf::from(pushed[0]["path"].as_str().unwrap_or("")).is_dir(),
            "default local folder created on disk"
        );

        // Custom absolute path honored.
        let custom = dir.join("custom-photos").to_string_lossy().replace('\\', "/");
        let v = call(
            "POST",
            &format!("/api/sync/pending/folders/other/{PHONE_ID}/accept"),
            Some(&json!({ "path": format!("{custom}") })),
        );
        assert_eq!(v["ok"], true, "custom path accepted: {v}");
        let pushed = st.folders.lock().unwrap().clone();
        assert_eq!(pushed.len(), 2);
        assert_eq!(pushed[1]["path"], custom);

        // Relative path rejected.
        let v = call(
            "POST",
            &format!("/api/sync/pending/folders/bad/{PHONE_ID}/accept"),
            Some(&json!({ "path": "relative/x" })),
        );
        assert_eq!(v["ok"], false, "relative path rejected");

        // Dismiss hits the pending store.
        let v = call(
            "DELETE",
            &format!("/api/sync/pending/folders/photos/{PHONE_ID}/dismiss"),
            None,
        );
        assert_eq!(v["ok"], true, "folder dismissed: {v}");
        assert_eq!(
            st.dismissed_folders.lock().unwrap().clone(),
            vec!["photos".to_string()]
        );

        // Existing local folder gains the device in its share list.
        let v = call(
            "POST",
            &format!("/api/sync/pending/folders/other/{PHONE_ID}/accept"),
            None,
        );
        assert_eq!(v["ok"], true, "existing folder re-accepted: {v}");
        let pushed = st.folders.lock().unwrap().clone();
        let other = pushed
            .iter()
            .find(|f| f["id"] == "other")
            .cloned()
            .unwrap_or(Value::Null);
        assert_eq!(other["paused"], false);
        let peers = other["devices"].as_array().cloned().unwrap_or_default();
        assert_eq!(peers.len(), 2, "device appended to existing share list");
        assert!(peers.iter().any(|d| d["deviceID"] == PHONE_ID));

        server.abort();
    });

    // Cleanup (best effort; a failed assert leaves the temp dir for review).
    std::env::remove_var("FENGPAGE_ROOT");
    std::env::remove_var("FENGPAGE_SYNCTHING_HOME");
    std::env::remove_var("FENGPAGE_SYNC_LINKS");
    std::env::remove_var("FENGPAGE_SYNCTHING_ADDR");
    let _ = std::fs::remove_dir_all(&dir);
}
