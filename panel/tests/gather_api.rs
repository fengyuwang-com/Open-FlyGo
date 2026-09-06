// Integration test for /gather and the /api/gather/* endpoints.
//
// Runs as its own test binary (like control_api.rs) so FENGPAGE_* env vars
// do not race the unit-test binary. A mock Syncthing REST daemon (axum,
// in-memory) serves /rest/system/status, /rest/system/connections,
// /rest/config/folders and /rest/db/status; the real handlers from
// src/api/gather.rs run over real HTTP against it. Panel state lives in a
// temp install root. The second half of the test aborts the mock daemon to
// exercise the degraded paths (still HTTP 200 with a degraded field).
//
// All code comments are pure ASCII.

#[path = "../src/settings.rs"]
mod settings;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/api/gather.rs"]
mod gather;

use axum::routing::get;
use axum::Json;
use axum::Router;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Same error contract as the panel binary (src/main.rs).
pub struct ApiError(pub String);
impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::json!({"ok": false, "error": self.0}).to_string(),
        )
            .into_response()
    }
}
pub type ApiResult<T> = Result<T, ApiError>;

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("flygo-gather-it-{}", std::process::id()))
}

#[test]
fn gather_page_and_api_over_http() {
    let dir = temp_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Version"), "test-0.1.0\n").unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // --- mock Syncthing REST daemon (in-memory) ---
        const MY_ID: &str = "GATH-NODE-AAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        const PHONE_ID: &str = "GATH-PHONE-BBBBBBBBBBBBBBBBBBBBBBBBBBBB";
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
                            MY_ID: {"connected": false, "at": 0, "inBytesTotal": 0, "address": ""},
                            PHONE_ID: {"connected": true, "at": 1755400000.5, "inBytesTotal": 1048576, "address": "100.1.2.3:22000"},
                        }
                    }))
                }),
            )
            .route(
                "/rest/config/folders",
                get(|| async {
                    Json(json!({"folders": [
                        {"id": "phone-photos", "label": "手机相册", "path": "E:/Gather/Phone"},
                        {"id": "phone-dl", "label": "手机下载", "path": "E:/Gather/DL"},
                    ]}))
                }),
            )
            .route(
                "/rest/db/status",
                get(|| async { Json(json!({"state": "idle", "localFiles": 120, "globalFiles": 120})) }),
            );
        let st_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let st_addr = st_listener.local_addr().unwrap();
        let st_server = tokio::spawn(async move { axum::serve(st_listener, st).await.unwrap() });

        // --- panel install root: config.xml (apikey only, no <device>
        //     entries so the config.xml fallback path yields None) ---
        let st_home = dir.join("syncthing-config");
        std::fs::create_dir_all(&st_home).unwrap();
        std::fs::write(
            st_home.join("config.xml"),
            "<configuration><gui><apikey>TESTKEY123</apikey></gui></configuration>",
        )
        .unwrap();

        std::env::set_var("FENGPAGE_ROOT", &dir);
        std::env::set_var("FENGPAGE_CONFIG", dir.join("flygo.toml"));
        std::env::set_var("FENGPAGE_SYNCTHING_HOME", &st_home);
        std::fs::write(
            dir.join("flygo.toml"),
            format!(
                "# gather it\n[server]\naddr = \"0.0.0.0:8388\"\n\n[sync]\nsyncthing_addr = \"http://{st_addr}\"\n"
            ),
        )
        .unwrap();
        config::init();

        let app = Router::new()
            .route("/gather", get(gather::page))
            .route("/api/gather/pair-data", get(gather::pair_data))
            .route("/api/gather/verify", get(gather::verify));
        let panel_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let panel_addr = panel_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(panel_listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
        let base = format!("http://{panel_addr}");

        // readiness poll
        let mut up = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if agent.get(&format!("{base}/gather")).call().is_ok() {
                up = true;
                break;
            }
        }
        assert!(up, "panel did not come up");

        // 1) /gather page: 200 + the six-step keywords
        let resp = agent.get(&format!("{base}/gather")).call().unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.content_type().starts_with("text/html"));
        let html = resp.into_string().unwrap();
        for kw in [
            "Tailscale",                  // step 1
            "Syncthing-Fork",             // step 2
            "F-Droid",                    // step 2 links
            "扫码配对",                    // step 3
            "仅发送",                      // step 4 (send-only)
            "保活",                        // step 5
            "HyperOS",                    // step 5 vendors
            "EMUI",
            "ColorOS",
            "MagicOS",
            "验证",                        // step 6
            "api/gather/pair-data",       // data wiring
            "api/gather/verify",
        ] {
            assert!(html.contains(kw), "/gather missing keyword: {kw}");
        }
        // design discipline: no emoji rainbow (the single warn marker only)
        assert!(html.matches('\u{26A0}').count() <= 2, "warn markers kept minimal");

        // 2) pair-data with the mock daemon up: live source, no syncthing degrade
        let v: Value = agent
            .get(&format!("{base}/api/gather/pair-data"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["syncthing_id"], MY_ID);
        assert_eq!(v["source"], "rest");
        assert!(v["node_name"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
        // ts_ip / degraded depend on the host's Tailscale state: the field
        // must exist, and syncthing must NOT be degraded while the daemon
        // answers.
        let deg = v["degraded"].as_str().unwrap_or("");
        assert!(!deg.contains("syncthing"), "live daemon must not degrade syncthing: {deg}");
        assert!(v.get("ts_ip").is_some(), "ts_ip field always present");

        // 3) verify with the mock daemon up: counts folded from connections
        //    and both folder db/status rows
        let v: Value = agent
            .get(&format!("{base}/api/gather/verify"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["syncthing_up"], true);
        assert_eq!(v["degraded"], Value::Null);
        assert_eq!(v["paired_count"], 1, "self excluded from paired devices");
        assert_eq!(v["connected_count"], 1);
        assert_eq!(v["last_activity_unix"], 1755400000);
        assert_eq!(v["total_files"], 240, "two folders x 120 localFiles");
        assert_eq!(v["total_in_bytes"], 1048576);
        assert_eq!(v["folders"].as_array().unwrap().len(), 2);

        // 4) daemon down (mock aborted): degraded three-state, still HTTP 200
        st_server.abort();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let resp = agent.get(&format!("{base}/api/gather/pair-data")).call().unwrap();
        assert_eq!(resp.status(), 200, "degraded stays 200 for the mobile page");
        let v: Value = resp.into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["syncthing_id"], Value::Null, "no <device> in test config.xml");
        assert_eq!(v["source"], Value::Null);
        let deg = v["degraded"].as_str().unwrap_or("");
        assert!(deg.contains("syncthing"), "daemon down must degrade syncthing: {deg}");

        let resp = agent.get(&format!("{base}/api/gather/verify")).call().unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.into_json().unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["syncthing_up"], false);
        assert_eq!(v["degraded"], "syncthing");
        assert_eq!(v["folders"].as_array().unwrap().len(), 0);
    });

    // Cleanup (best effort).
    std::env::remove_var("FENGPAGE_ROOT");
    std::env::remove_var("FENGPAGE_CONFIG");
    std::env::remove_var("FENGPAGE_SYNCTHING_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}
