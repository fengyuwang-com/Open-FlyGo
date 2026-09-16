// Integration test for the control-plane client + upload endpoint.
//
// Runs as its own test binary (like config_api.rs) so FENGPAGE_* env vars
// can be set without racing the unit-test binary. A mock control plane
// (axum, in-memory) serves /api/register|activate|status|heartbeat; the
// real handlers from src/api/control.rs and src/api/upload.rs run over real
// HTTP against it. All panel state lives in a temp install root.
//
// All code comments are pure ASCII.

// Hidden-spawn helper shared with the panel crate (winproc) so the
// included handler modules can resolve crate::winproc.
#[allow(dead_code)]
#[path = "../src/winproc.rs"]
mod winproc;

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
#[path = "../src/api/control.rs"]
mod control;
// Shared source module: items flagged here may be used only by the main
// binary, not by this test harness.
#[allow(dead_code)]
#[path = "../src/api/upload.rs"]
mod upload;
// stats.rs resolves its sibling modules as crate::api::stats (same layout as
// the main crate) and references crate::request_shutdown in the restart
// handler; provide both here.
mod api {
    // Relative to this test module's directory (tests/api/).
// Shared source module: items flagged here may be used only by the main
// binary, not by this test harness.
#[allow(dead_code)]
    #[path = "../../src/api/stats.rs"]
    pub mod stats;
}

#[allow(dead_code)]
fn request_shutdown() {}

// control.rs (P1-4 heartbeat loop) also resolves these in the crate root of
// the main binary; this test binary has no main.rs, so provide the same
// contract here (no logger file, never shutting down).
#[allow(dead_code)]
fn shutdown_requested() -> bool {
    false
}

pub mod log {
    #[allow(dead_code)]
    pub fn log(level: &str, tag: &str, msg: &str) {
        eprintln!("[{level}] {tag}: {msg}");
    }
}

use api::stats;

// stats.rs uses crate::ApiResult; in this test binary that is this file's
// root, so provide the same alias main.rs exposes.
#[derive(Debug)]
pub struct ApiError(pub axum::http::StatusCode, pub String);

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, axum::Json(serde_json::json!({"ok": false, "message": self.1}))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

use axum::Json;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("flygo-ctl-it-{}", std::process::id()))
}

fn multipart_body(filename: &str, content: &[u8]) -> Vec<u8> {
    let boundary = "----flygotestboundary";
    let mut b = Vec::new();
    b.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    b.extend_from_slice(content);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

#[test]
fn control_plane_client_over_http() {
    let dir = temp_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Version"), "test-0.1.0\n").unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // --- mock control plane (in-memory) ---
        let plane_code: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let plane_hw: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let last_hb: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));

        let pc = plane_code.clone();
        let ph = plane_hw.clone();
        let plane = Router::new()
            .route(
                "/api/register",
                post(move |Json(body): Json<Value>| {
                    let pc = pc.clone();
                    async move {
                        let hw = body.get("hardware_id").and_then(|h| h.as_str()).unwrap_or("").to_string();
                        let code = pc.lock().unwrap().clone().unwrap_or_else(|| "ABCD2345".to_string());
                        *pc.lock().unwrap() = Some(code.clone());
                        *ph.lock().unwrap() = Some(hw.clone());
                        Json(json!({
                            "ok": true,
                            "device": {
                                "device_id": 1,
                                "hardware_id": hw,
                                "name": body.get("device_name").and_then(|n| n.as_str()).unwrap_or(""),
                                "node_kind": body.get("node_kind").and_then(|n| n.as_str()).unwrap_or(""),
                                "plan": "trial",
                                "subscription_state": "pending",
                                "activated_at": null,
                                "expires_at": null,
                                "quota_bytes": 0,
                                "last_heartbeat_at": null,
                                "disk_free_bytes": null,
                                "online": false,
                                "version": null,
                                "created_at": 1780000000,
                                "activation_code": code,
                            }
                        }))
                    }
                }),
            )
            .route(
                "/api/activate",
                post(|Json(body): Json<Value>| async move {
                    let code = body.get("activation_code").and_then(|c| c.as_str()).unwrap_or("");
                    let hw = body.get("hardware_id").and_then(|h| h.as_str()).unwrap_or("");
                    if code != "ABCD2345" {
                        return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"ok": false, "message": "invalid activation code"})));
                    }
                    let now = chrono_now();
                    (axum::http::StatusCode::OK, Json(json!({
                        "ok": true,
                        "token": "mock-jwt-token",
                        "device": {"device_id": 1, "hardware_id": hw, "name": "flygo-panel", "plan": "trial"},
                        "subscription_state": "trial",
                        "expires_at": now + 30 * 86400,
                    })))
                }),
            )
            .route(
                "/api/status",
                get(|headers: axum::http::HeaderMap| async move {
                    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
                    if auth != "Bearer mock-jwt-token" {
                        return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({"ok": false, "message": "bad token"})));
                    }
                    (axum::http::StatusCode::OK, Json(json!({
                        "ok": true,
                        "server_time": chrono_now(),
                        "device": {"device_id": 1, "hardware_id": "x", "name": "flygo-panel", "plan": "active", "subscription_state": "active", "online": true},
                        "subscription_state": "active",
                        "expires_at": chrono_now() + 365 * 86400,
                    })))
                }),
            )
            .route(
                "/api/heartbeat",
                post({
                    let lh = last_hb.clone();
                    move |Json(body): Json<Value>| {
                        let lh = lh.clone();
                        async move {
                            *lh.lock().unwrap() = Some(body);
                            Json(json!({
                                "ok": true,
                                "server_time": chrono_now(),
                                "subscription_state": "active",
                                "expires_at": chrono_now() + 365 * 86400,
                            }))
                        }
                    }
                }),
            );
        let plane_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let plane_addr = plane_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(plane_listener, plane).await.unwrap() });

        // --- panel config pointing at the mock plane ---
        std::env::set_var("FENGPAGE_ROOT", &dir);
        std::env::set_var("FENGPAGE_CONFIG", dir.join("flygo.toml"));
        std::fs::write(
            dir.join("flygo.toml"),
            format!(
                "# top comment\n[server]\naddr = \"0.0.0.0:8388\"\n\n[sync]\ninterval_secs = 600\n\n[control]\nbase_url = \"http://{plane_addr}\"\n"
            ),
        )
        .unwrap();
        config::init();

        let app = Router::new()
            .route("/api/control/register", post(control::register))
            .route("/api/control/activate", post(control::activate))
            .route("/api/control/status", get(control::status))
            .route("/api/control/settings", post(control::settings))
            .route("/api/control/heartbeat", post(control::heartbeat_now))
            // Same route-level limit as main.rs (axum's 2 MB DefaultBodyLimit
            // would break real uploads).
            .route(
                "/api/upload",
                post(upload::upload).layer(axum::extract::DefaultBodyLimit::max(
                    2 * 1024 * 1024 * 1024,
                )),
            )
            .route("/api/upload/recent", get(upload::recent))
            .route("/api/stats", get(stats::stats));
        let panel_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let panel_addr = panel_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(panel_listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
        let base = format!("http://{panel_addr}");
        let post_json = |path: &str, body: &str| -> Value {
            let r = agent
                .post(&format!("{base}{path}"))
                .set("Content-Type", "application/json")
                .send_string(body)
                .unwrap();
            r.into_json().unwrap()
        };
        let get_json = |path: &str| -> Value { agent.get(&format!("{base}{path}")).call().unwrap().into_json().unwrap() };

        // readiness poll
        let mut up = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if agent.get(&format!("{base}/api/control/status")).call().is_ok() {
                up = true;
                break;
            }
        }
        assert!(up, "panel did not come up");

        // 1) initial status: disabled, unregistered
        let st = get_json("/api/control/status");
        assert_eq!(st["ok"], true);
        assert_eq!(st["enabled"], false);
        assert_eq!(st["registered"], false);
        assert_eq!(st["activated"], false);
        assert_eq!(st["base_url"], format!("http://{plane_addr}"));

        // 2) enable via /api/control/settings; flygo.toml keeps other sections
        let r = post_json("/api/control/settings", r#"{"enabled": true}"#);
        assert_eq!(r["ok"], true);
        let file_text = std::fs::read_to_string(dir.join("flygo.toml")).unwrap();
        assert!(file_text.contains("# top comment"), "comments preserved: {file_text}");
        assert!(file_text.contains("[server]"), "server section preserved");
        assert_eq!(file_text.matches("[control]").count(), 1, "single [control] section");
        assert!(file_text.contains("enabled = true"));
        let st = get_json("/api/control/status");
        assert_eq!(st["enabled"], true);

        // 3) register -> activation code from the mock plane, persisted
        let r = post_json("/api/control/register", "{}");
        assert_eq!(r["ok"], true);
        assert_eq!(r["activation_code"], "ABCD2345");
        assert_eq!(r["cached"], false);
        assert!(r["hardware_id"].as_str().unwrap().starts_with("PANEL-"));
        let st = get_json("/api/control/status");
        assert_eq!(st["registered"], true);
        assert_eq!(st["activation_code"], "ABCD2345");
        assert_eq!(st["device_name"], "flygo-panel");

        // 4) register again -> idempotent, served from the local cache
        let r = post_json("/api/control/register", "{}");
        assert_eq!(r["cached"], true);
        assert_eq!(r["activation_code"], "ABCD2345");

        // 5) activate with a wrong code -> plane 400 surfaces its message
        let r = post_json("/api/control/activate", r#"{"code": "ZZZZ-ZZZZ"}"#);
        assert_eq!(r["ok"], false);
        assert!(r["message"].as_str().unwrap().contains("invalid activation code"));

        // 6) activate with the right code (lowercase is uppercased client-side)
        let r = post_json("/api/control/activate", r#"{"code": "abcd-2345"}"#);
        assert_eq!(r["ok"], true);
        assert_eq!(r["token"], "mock-jwt-token");
        assert_eq!(r["subscription"]["state"], "trial");
        assert_eq!(r["subscription"]["tier"], "trial");
        assert!(r["subscription"]["expires_at"].as_i64().unwrap() > chrono_now());
        let st = get_json("/api/control/status");
        assert_eq!(st["activated"], true);
        assert_eq!(st["subscription"]["state"], "trial");

        // 7) live status refresh (Bearer JWT against the mock plane)
        let st = get_json("/api/control/status?live=1");
        assert_eq!(st["live"]["subscription_state"], "active");
        assert_eq!(st["subscription"]["state"], "active", "live refresh updates the cache");

        // 8) heartbeat once: disk_free >= 0, version from the Version file,
        //    subscription refreshed to active
        let now = chrono_now();
        control::heartbeat_once(&format!("http://{plane_addr}")).await.unwrap();
        let hb = last_hb.lock().unwrap().clone().unwrap();
        assert!(hb["disk_free_bytes"].as_u64().unwrap() > 0);
        assert_eq!(hb["online"], true);
        assert_eq!(hb["version"], "test-0.1.0");
        let st = get_json("/api/control/status");
        assert_eq!(st["subscription"]["state"], "active");
        assert_eq!(st["last_heartbeat_ok"], true);
        assert!(st["last_heartbeat_at"].as_i64().unwrap() >= now);

        // 9) upload: file lands in <root>/data/uploads/<YYYYMMDD>/<name>
        let resp = agent
            .post(&format!("{base}/api/upload"))
            .set("Content-Type", "multipart/form-data; boundary=----flygotestboundary")
            .send_bytes(&multipart_body("hello.txt", b"hello world"))
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.into_json().unwrap();
        assert_eq!(v["ok"], true);
        let saved = PathBuf::from(v["path"].as_str().unwrap());
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), "hello world");
        assert_eq!(v["size"], 11);
        assert!(saved.starts_with(dir.join("data").join("uploads")), "saved under uploads dir");
        assert!(saved.parent().unwrap().file_name().unwrap().to_string_lossy().len() == 8, "date subdir");

        // 10) traversal name is neutralized to its basename, stays inside
        let resp = agent
            .post(&format!("{base}/api/upload"))
            .set("Content-Type", "multipart/form-data; boundary=----flygotestboundary")
            .send_bytes(&multipart_body("..\\..\\..\\escape.txt", b"x"))
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.into_json().unwrap();
        let saved = PathBuf::from(v["path"].as_str().unwrap());
        assert_eq!(saved.file_name().unwrap().to_string_lossy(), "escape.txt");
        assert!(saved.starts_with(dir.join("data").join("uploads")), "cannot escape uploads dir");

        // 11) duplicate name gets a -1 suffix instead of overwriting
        let resp = agent
            .post(&format!("{base}/api/upload"))
            .set("Content-Type", "multipart/form-data; boundary=----flygotestboundary")
            .send_bytes(&multipart_body("hello.txt", b"again"))
            .unwrap();
        let v: Value = resp.into_json().unwrap();
        assert_eq!(
            PathBuf::from(v["path"].as_str().unwrap()).file_name().unwrap().to_string_lossy(),
            "hello-1.txt"
        );

        // 12) size cap: drop upload_max_mb to 1, then a 2 MB body -> 413
        let r = post_json("/api/control/settings", r#"{"upload_max_mb": 1}"#);
        assert_eq!(r["ok"], true);
        let big = vec![b'0'; 2 * 1024 * 1024];
        let resp = agent
            .post(&format!("{base}/api/upload"))
            .set("Content-Type", "multipart/form-data; boundary=----flygotestboundary")
            .send_bytes(&multipart_body("big.bin", &big));
        match resp {
            Err(ureq::Error::Status(413, r)) => {
                let v: Value = r.into_json().unwrap();
                assert_eq!(v["ok"], false);
                assert!(v["message"].as_str().unwrap().contains("too large"));
                // Too-large is a permanent rejection: a naive retry repeats
                // the failure, so the contract flags it non-retryable.
                assert_eq!(v["retryable"], false);
            }
            other => panic!("expected 413, got {:?}", other.map(|r| r.status())),
        }
        // 13) idempotent activate replay: same code -> cached identity, no
        // second bind round-trip (works offline; never double-binds).
        let r = post_json("/api/control/activate", r#"{"code": "ABCD2345"}"#);
        assert_eq!(r["ok"], true);
        assert_eq!(r["cached"], true, "re-activating the same code is served from cache");
        assert_eq!(r["token"], "mock-jwt-token");
        let st = get_json("/api/control/status");
        assert_eq!(st["activated"], true);

        // 14) manual heartbeat ("retry now"): one POST /api/control/heartbeat
        // against the mock plane succeeds and refreshes the cache.
        let r = post_json("/api/control/heartbeat", "{}");
        assert_eq!(r["ok"], true);
        let st = get_json("/api/control/status");
        assert_eq!(st["last_heartbeat_ok"], true);
        assert_eq!(st["heartbeat_fail_count"], 0);

        // 15) plane goes away: heartbeat against a dead port fails, and the
        // status exposes a truthful retry state (fail count + countdown).
        let err = control::heartbeat_once("http://127.0.0.1:1").await.unwrap_err();
        assert!(err.contains("unreachable") || err.contains("connection"), "got: {err}");
        let st = get_json("/api/control/status");
        assert_eq!(st["plane_unreachable"], true);
        assert_eq!(st["last_heartbeat_ok"], false);
        assert_eq!(st["heartbeat_fail_count"], 1);
        assert!(
            st["next_retry_in"].is_i64() || st["next_retry_in"].is_u64(),
            "next_retry_in must be present while unreachable"
        );

        // 16) recovery: the next successful heartbeat resets the streak.
        control::heartbeat_once(&format!("http://{plane_addr}")).await.unwrap();
        let st = get_json("/api/control/status");
        assert_eq!(st["plane_unreachable"], false);
        assert_eq!(st["heartbeat_fail_count"], 0);

        // 17) upload idempotency: same upload_id twice -> the second request
        // replays the first result instead of writing a "-1" duplicate.
        let send_up = |id: &str| -> Value {
            let resp = agent
                .post(&format!("{base}/api/upload"))
                .set("Content-Type", "multipart/form-data; boundary=----flygotestboundary")
                .set("X-FlyGo-Upload-Id", id)
                .send_bytes(&multipart_body("hello-id.txt", b"idem payload"))
                .unwrap();
            assert_eq!(resp.status(), 200);
            resp.into_json().unwrap()
        };
        let first = send_up("up-test-001");
        assert_eq!(first["repeated"], false);
        let path1 = first["path"].as_str().unwrap().to_string();
        assert!(path1.ends_with("hello-id.txt"), "first upload uses the real name: {path1}");
        let second = send_up("up-test-001");
        assert_eq!(second["repeated"], true, "same id replays the stored result");
        assert_eq!(second["path"].as_str().unwrap(), path1, "same file, no -1 duplicate");
        assert_eq!(std::fs::read_to_string(&path1).unwrap(), "idem payload");

        // 18) recent uploads endpoint lists the completion (with retry count).
        let r = get_json("/api/upload/recent");
        assert_eq!(r["ok"], true);
        let names: Vec<String> = r["recent"]
            .as_array().unwrap()
            .iter()
            .filter(|e| e["upload_id"].as_str() == Some("up-test-001"))
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "hello-id.txt"), "recent lists the upload: {names:?}");
        let entry = r["recent"].as_array().unwrap().iter()
            .find(|e| e["upload_id"].as_str() == Some("up-test-001"))
            .unwrap();
        assert!(entry["times"].as_u64().unwrap() >= 2, "replay counted");

        // 19) /api/stats reflects control-plane connectivity (ok + fail).
        let r = get_json("/api/stats");
        assert!(r["control_ok_total"].as_u64().unwrap() >= 1);
        assert!(r["control_fail_total"].as_u64().unwrap() >= 1);

        let state_file = dir.join("data").join("control-state.json");
        assert!(state_file.exists(), "control state persisted");
        assert!(!state_file.with_extension("json.tmp").exists(), "tmp renamed away");
        // The upload journal is persisted next to the uploaded files.
        assert!(dir.join("data").join("uploads").join(".upload-journal.json").exists());
    });

    // Cleanup (best effort).
    std::env::remove_var("FENGPAGE_ROOT");
    std::env::remove_var("FENGPAGE_CONFIG");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Local helper: current unix seconds (kept out of the modules under test).
fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
