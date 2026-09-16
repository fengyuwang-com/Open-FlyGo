// Integration test for the mutual-backup endpoints (/api/pair/*).
//
// Runs as its own test binary (like gather_api.rs) so FENGPAGE_* env vars
// do not race the unit-test binary. Two in-memory axum mocks:
//   - mock Syncthing REST daemon (/rest/system/status|connections,
//     /rest/config/devices|folders PUT+GET, /rest/db/status) which records
//     every PUT body and serves the captured folders back on GET;
//   - mock FlyGo control plane (/api/pair/invite|accept|poll) whose poll
//     answer flips pending -> matched.
// The real handlers from src/api/pair.rs run over real HTTP against both.
// Phases: Syncthing down (create -> real 400 with a reason; status/recover
// degrade to 200), happy inviter + accepter flows, folder provisioning with
// forced staggered versioning, recovery progress, control plane down
// (create -> 400), Syncthing down again (recover -> 200 degraded).
//
// All code comments are pure ASCII.

// Shared source module: items flagged here may be used only by the main
// binary, not by this test harness.
#[allow(dead_code)]
#[path = "../src/settings.rs"]
mod settings;
// Shared source module: items flagged here may be used only by the main
// binary, not by this test harness.
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
// Shared source module: items flagged here may be used only by the main
// binary, not by this test harness.
#[allow(dead_code)]
#[path = "../src/api/pair.rs"]
mod pair;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Shared state of the two mocks (captured PUTs + poll flip). Mutation goes
/// through Mutex/Atomic (the mocks are shared across handler tasks).
#[derive(Default)]
struct Mocks {
    devices_put: Mutex<Vec<Value>>,
    folders_put: Mutex<Vec<Value>>,
    /// false = poll pending, true = poll matched (accept happened).
    matched: AtomicBool,
    invite_calls: Mutex<Vec<Value>>,
    accept_calls: Mutex<Vec<Value>>,
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("flygo-pair-it-{}", std::process::id()))
}

#[test]
fn pair_api_over_http() {
    let dir = temp_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Version"), "test-0.1.0\n").unwrap();

    // Syncthing home with only the apikey (config.xml).
    let st_home = dir.join("syncthing-config");
    std::fs::create_dir_all(&st_home).unwrap();
    std::fs::write(
        st_home.join("config.xml"),
        "<configuration><gui><apikey>TESTKEY123</apikey></gui></configuration>",
    )
    .unwrap();

    // Control-plane state: an activated node with a stored JWT.
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(
        dir.join("data").join("control-state.json"),
        r#"{"hardware_id":"PANEL-TEST","token":"test-jwt-token","subscription":{"tier":"trial","state":"trial"}}"#,
    )
    .unwrap();

    // A guaranteed-dead Syncthing port: bind then drop the listener.
    let dead_port = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        })
    };

    let mocks = Arc::new(Mocks::default());

    let mocks_cleanup_dir = dir.clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        // --- mock control plane ---
        const MY_ID: &str = "MYNODEAA-AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGG";
        const ACCEPTER_ID: &str = "PEERBBBB-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHH";
        const INVITER_ID: &str = "PEERCCCC-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH-IIIIII";
        let cp_mocks = mocks.clone();
        let cp = Router::new()
            .route(
                "/api/pair/invite",
                post(move |State(m): State<Arc<Mocks>>, Json(b): Json<Value>| {
                    m.invite_calls.lock().unwrap().push(b);
                    async move { Json(json!({"ok": true, "code": "246810", "expires_at": 4102444800u64, "expires_in_secs": 600})) }
                }),
            )
            .route(
                "/api/pair/accept",
                post(move |State(m): State<Arc<Mocks>>, Json(b): Json<Value>| {
                    m.accept_calls.lock().unwrap().push(b);
                    async move { Json(json!({"ok": true, "inviter_syncthing_id": INVITER_ID})) }
                }),
            )
            .route(
                "/api/pair/poll",
                get(move |State(m): State<Arc<Mocks>>| async move {
                    if m.matched.load(Ordering::SeqCst) {
                        Json(json!({"ok": true, "status": "matched", "accepter_syncthing_id": ACCEPTER_ID, "matched_at": 1755400000}))
                    } else {
                        Json(json!({"ok": true, "status": "pending", "expires_at": 4102444800u64, "expires_in_secs": 600}))
                    }
                }),
            )
            .with_state(cp_mocks);
        let cp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cp_addr = cp_listener.local_addr().unwrap();
        let cp_server = tokio::spawn(async move { axum::serve(cp_listener, cp).await.unwrap() });

        // --- mock Syncthing REST daemon (records PUTs) ---
        let st_mocks = mocks.clone();
        let st = Router::new()
            .route(
                "/rest/system/status",
                get(|| async { Json(json!({"myID": MY_ID, "uptime": 42})) }),
            )
            .route(
                "/rest/system/connections",
                get(|| async {
                    Json(json!({
                        "connections": {
                            ACCEPTER_ID: {"connected": true, "at": now_secs() - 3, "address": "100.1.2.3:22000", "clientVersion": "v2.1.3", "inBytesTotal": 2048, "outBytesTotal": 4096},
                            INVITER_ID: {"connected": true, "at": now_secs() - 5, "address": "100.1.2.4:22000", "clientVersion": "v2.1.3", "inBytesTotal": 1024, "outBytesTotal": 512},
                        }
                    }))
                }),
            )
            .route(
                "/rest/config/devices",
                axum::routing::put(move |State(m): State<Arc<Mocks>>, Json(b): Json<Value>| {
                    if let Some(arr) = b.as_array() {
                        m.devices_put.lock().unwrap().extend(arr.iter().cloned());
                    }
                    async { Json(json!({"ok": true})) }
                }),
            )
            .route(
                "/rest/config/folders",
                axum::routing::put(move |State(m): State<Arc<Mocks>>, Json(b): Json<Value>| {
                    if let Some(arr) = b.as_array() {
                        m.folders_put.lock().unwrap().extend(arr.iter().cloned());
                    }
                    async { Json(json!({"ok": true})) }
                })
                .get(move |State(m): State<Arc<Mocks>>| async move {
                    Json(json!({ "folders": m.folders_put.lock().unwrap().clone() }))
                }),
            )
            .route(
                "/rest/db/status",
                get(|| async { Json(json!({"state": "syncing", "localFiles": 900, "globalFiles": 1000, "localBytes": 9000, "globalBytes": 10000, "needBytes": 1000, "needFiles": 100})) }),
            )
            .with_state(st_mocks);
        let st_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let st_addr = st_listener.local_addr().unwrap();
        let st_server = tokio::spawn(async move { axum::serve(st_listener, st).await.unwrap() });

        // --- panel routes (real pair.rs handlers) ---
        let app = Router::new()
            .route("/api/pair/create", post(pair::create))
            .route("/api/pair/join", post(pair::join))
            .route("/api/pair/status", get(pair::status))
            .route("/api/pair/folders", post(pair::folders))
            .route("/api/pair/recover", get(pair::recover));
        let panel_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let panel_addr = panel_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(panel_listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).build();
        let base = format!("http://{panel_addr}");

        // Request helper: keeps status + JSON body for BOTH 2xx and 4xx
        // (ureq surfaces non-2xx as Err(Status); the 400 contracts matter).
        let req = |method: &str, path: &str, body: Option<String>| -> (u16, Value) {
            let url = format!("{base}{path}");
            let r = agent.request(method, &url);
            let res = match body {
                Some(b) => r.set("Content-Type", "application/json").send_string(&b),
                None => r.call(),
            };
            match res {
                Ok(resp) => (resp.status(), resp.into_json().unwrap_or(Value::Null)),
                Err(ureq::Error::Status(code, resp)) => {
                    (code, resp.into_json().unwrap_or(Value::Null))
                }
                Err(e) => panic!("request {method} {url} failed: {e}"),
            }
        };

        let write_toml = |syncthing_addr: String| {
            std::fs::write(
                dir.join("flygo.toml"),
                format!(
                    "# pair it\n[sync]\nsyncthing_addr = \"{syncthing_addr}\"\nsync_timeout_secs = 2\n\n[control]\nbase_url = \"http://{cp_addr}\"\n"
                ),
            )
            .unwrap();
        };

        // Point at a dead Syncthing port first; config::init freezes paths.
        std::env::set_var("FENGPAGE_ROOT", &dir);
        std::env::set_var("FENGPAGE_CONFIG", dir.join("flygo.toml"));
        std::env::set_var("FENGPAGE_SYNCTHING_HOME", &st_home);
        write_toml(format!("http://127.0.0.1:{dead_port}"));
        config::init();

        // 1) Syncthing down: create -> REAL HTTP 400 with the reason; no panic.
        let (code, v) = req("POST", "/api/pair/create", None);
        println!("[measured] pair/create, Syncthing down -> HTTP {code}: {v}");
        assert_eq!(code, 400, "Syncthing down must be a hard 400 for create");
        assert_eq!(v["ok"], false);
        assert!(
            v["error"].as_str().unwrap_or("").to_lowercase().contains("syncthing"),
            "error must name Syncthing, got: {}",
            v["error"]
        );

        // status/recover stay HTTP 200 + degraded (honest states, no fake error).
        let v: Value = agent.get(&format!("{base}/api/pair/status")).call().unwrap().into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["syncthing_up"], false);
        assert!(v["degraded"].as_str().unwrap_or("").contains("syncthing"));
        let v: Value = agent.get(&format!("{base}/api/pair/recover")).call().unwrap().into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["paired"], false, "no peer yet -> unpaired snapshot");

        // 2) Point at the live mock daemon (handlers re-read flygo.toml).
        write_toml(format!("http://{st_addr}"));

        // create -> invite code relayed from the control plane.
        let (code, v) = req("POST", "/api/pair/create", None);
        assert_eq!(code, 200, "create failed: {v}");
        assert_eq!(v["ok"], true, "create failed: {v}");
        assert_eq!(v["code"], "246810");
        assert_eq!(v["my_syncthing_id"], MY_ID);
        // Lock guards are scoped so none is held across the awaits below.
        {
            let invites = mocks.invite_calls.lock().unwrap();
            assert_eq!(invites.len(), 1);
            assert_eq!(invites[0], json!({"syncthing_id": MY_ID}), "invite relays the device id only");
        }

        // status (invite outstanding, poll still pending).
        let v: Value = agent.get(&format!("{base}/api/pair/status")).call().unwrap().into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["paired"], false);
        assert_eq!(v["invite"]["code"], "246810");
        assert_eq!(v["plane"]["status"], "pending");

        // 3) Inviter auto-complete: accepter redeemed the code elsewhere ->
        //    poll flips to matched, the panel adds the peer device itself.
        mocks.matched.store(true, Ordering::SeqCst);
        let v: Value = agent.get(&format!("{base}/api/pair/status")).call().unwrap().into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["paired"], true);
        assert_eq!(v["peer_syncthing_id"], ACCEPTER_ID);
        assert_eq!(v["syncthing_up"], true);
        assert_eq!(v["degraded"], Value::Null, "everything live -> no degraded parts");
        // peer health folded from /rest/system/connections
        assert_eq!(v["peer_health"]["online"], true);
        assert_eq!(v["peer_health"]["address"], "100.1.2.3:22000");
        assert_eq!(v["peer_health"]["in_bytes"], 2048);
        assert_eq!(v["probe_failures"], 0);
        // the panel PUT the peer into the local Syncthing config
        {
            let devs = mocks.devices_put.lock().unwrap();
            assert_eq!(devs.len(), 1);
            assert_eq!(devs[0]["deviceID"], ACCEPTER_ID);
        }

        // 4) Join (accepter side): redeem a code, learn the inviter, add it.
        let (code, v) = req("POST", "/api/pair/join", Some(r#"{"code":"246810"}"#.to_string()));
        assert_eq!(code, 200, "join against live mocks must succeed");
        assert_eq!(v["ok"], true);
        assert_eq!(v["peer_syncthing_id"], INVITER_ID);
        {
            let accepts = mocks.accept_calls.lock().unwrap();
            assert_eq!(accepts.len(), 1);
            assert_eq!(accepts[0]["code"], "246810");
            assert_eq!(accepts[0]["accepter_syncthing_id"], MY_ID);
        }
        {
            let devs = mocks.devices_put.lock().unwrap();
            assert_eq!(devs.len(), 2);
            assert_eq!(devs[1]["deviceID"], INVITER_ID);
        }

        // bad code shape -> 400 without touching the network
        let (code, _) = req("POST", "/api/pair/join", Some(r#"{"code":"abc"}"#.to_string()));
        assert_eq!(code, 400);

        // 5) Folder provisioning: one real temp dir + one missing path.
        let real_dir = dir.join("backup-me");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_path = real_dir.to_string_lossy().replace('\\', "/");
        let missing = "Z:/flygo/definitely/not/here";
        let (code, v) = req(
            "POST",
            "/api/pair/folders",
            Some(json!({"paths": [real_path, missing]}).to_string()),
        );
        assert_eq!(code, 200);
        assert_eq!(v["ok"], false, "one failed path -> overall ok=false");
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["ok"], true);
        assert_eq!(results[1]["ok"], false);

        // the provisioned folder: send-receive + FORCED staggered versioning
        let folder = mocks.folders_put.lock().unwrap()[0].clone();
        assert_eq!(mocks.folders_put.lock().unwrap().len(), 1);
        assert_eq!(folder["type"], "sendreceive");
        assert_eq!(folder["versioning"]["type"], "staggered");
        assert_eq!(folder["versioning"]["params"]["maxAge"], "604800");
        assert_eq!(folder["versioning"]["params"]["cleanoutDays"], "7");
        let devs = folder["devices"].as_array().unwrap();
        assert!(devs.iter().any(|d| d["deviceID"] == MY_ID));
        assert!(devs.iter().any(|d| d["deviceID"] == INVITER_ID));
        // the local state kept the successful path only
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("data").join("pair-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["folders"].as_array().unwrap().len(), 1);
        assert_eq!(saved["folders"][0].as_str().unwrap().to_lowercase(), real_path.to_lowercase());

        // 6) Recovery progress: the mock serves captured folders + db/status.
        let v: Value = agent.get(&format!("{base}/api/pair/recover")).call().unwrap().into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["syncthing_up"], true);
        assert_eq!(v["paired"], true);
        let rows = v["folders"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "syncing");
        assert_eq!(rows[0]["need_bytes"], 1000);
        assert_eq!(rows[0]["global_bytes"], 10000);
        assert_eq!(rows[0]["completion_pct"], 90.0);
        assert_eq!(v["total_need_bytes"], 1000);
        assert_eq!(v["total_completion_pct"], 90.0);

        // 7) Control plane down: create -> 400 naming the control plane.
        cp_server.abort();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let (code, v) = req("POST", "/api/pair/create", None);
        println!("[measured] pair/create, control plane down -> HTTP {code}: {v}");
        assert_eq!(code, 400, "control plane down must be a hard 400 for create");
        assert!(
            v["error"].as_str().unwrap_or("").contains("control plane"),
            "error must name the control plane, got: {}",
            v["error"]
        );
        // status degrades to 200 (local data still shown).
        let v: Value = agent.get(&format!("{base}/api/pair/status")).call().unwrap().into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["paired"], true, "pairing survives a control-plane outage locally");

        // 8) Syncthing down with a paired state: recover degrades to 200.
        st_server.abort();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let v: Value = agent.get(&format!("{base}/api/pair/recover")).call().unwrap().into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["syncthing_up"], false);
        assert_eq!(v["degraded"], "syncthing");
        assert!(v["error"].as_str().unwrap_or("").contains("Syncthing"));
        // create fails hard again (Syncthing is the first probe).
        let (code, _) = req("POST", "/api/pair/create", None);
        assert_eq!(code, 400);
    });

    // Cleanup (best effort).
    std::env::remove_var("FENGPAGE_ROOT");
    std::env::remove_var("FENGPAGE_CONFIG");
    std::env::remove_var("FENGPAGE_SYNCTHING_HOME");
    let _ = std::fs::remove_dir_all(&mocks_cleanup_dir);
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
