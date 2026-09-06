// Integration test for the /api/plainapp/* endpoints.
//
// Runs as its own test binary (like gather_api.rs) so FENGPAGE_* env vars do
// not race the unit-test binary. The real handlers from src/api/plainapp.rs
// run over real HTTP against a temp install root. Covers:
//   - register create / list / update / delete roundtrip
//   - status probing an unreachable port still returns HTTP 200 with
//     reachable:false (offline is a normal phone state, never an error)
//   - status probing a live TCP listener reports reachable:true
//   - persistence: data/phones.json survives a fresh disk re-load
//   - validation failures (empty name, bad host, bad port)
//
// The live-sample section registers 127.0.0.1:8388 and prints the real
// status JSON (run with -- --nocapture) as a manual smoke check.
//
// All code comments are pure ASCII.

#[path = "../src/settings.rs"]
mod settings;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/api/plainapp.rs"]
mod plainapp;

use axum::routing::get;
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
    std::env::temp_dir().join(format!("flygo-plainapp-it-{}", std::process::id()))
}

#[test]
fn plainapp_register_status_roundtrip() {
    let dir = temp_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Version"), "test-0.1.0\n").unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // A real TCP listener we can probe green (kept alive until the end).
        let probe_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_port = probe_listener.local_addr().unwrap().port();
        let accept_task = tokio::spawn(async move {
            // Accept and drop connections; the port stays reachable.
            loop {
                let _ = probe_listener.accept().await;
            }
        });

        // A port that is definitely closed: bind then drop the listener.
        let closed_port;
        {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            closed_port = l.local_addr().unwrap().port();
        }

        // Panel install root in a temp dir; explicit phones.json path.
        let phones_json = dir.join("data").join("phones.json");
        std::env::set_var("FENGPAGE_ROOT", &dir);
        std::env::set_var("FENGPAGE_CONFIG", dir.join("flygo.toml"));
        std::env::set_var("FENGPAGE_PHONES", &phones_json);
        std::fs::write(dir.join("flygo.toml"), "# plainapp it\n[server]\naddr = \"0.0.0.0:8388\"\n").unwrap();
        config::init();

        let app = axum::Router::new()
            .route(
                "/api/plainapp/register",
                get(plainapp::register_list)
                    .post(plainapp::register_add)
                    .delete(plainapp::register_del),
            )
            .route("/api/plainapp/status", get(plainapp::status));
        let panel_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let panel_addr = panel_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(panel_listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(10))
            .build();
        let base = format!("http://{panel_addr}");
        let post_json = |body: Value| {
            agent
                .post(&format!("{base}/api/plainapp/register"))
                .send_json(body)
                .unwrap()
        };

        // readiness poll
        let mut up = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if agent.get(&format!("{base}/api/plainapp/register")).call().is_ok() {
                up = true;
                break;
            }
        }
        assert!(up, "panel did not come up");

        // 1) empty list at first
        let v: Value = agent
            .get(&format!("{base}/api/plainapp/register"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 0);
        assert_eq!(v["phones"].as_array().unwrap().len(), 0);

        // 2) register two phones (one closed port, one live listener)
        let v: Value = post_json(json!({"name":"Feng-Phone","host":"127.0.0.1","port":closed_port,"note":"test phone"}))
            .into_json()
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 1);
        let v: Value = post_json(json!({"name":"Live-Self","host":"127.0.0.1","port":live_port}))
            .into_json()
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 2);

        // 3) list roundtrip
        let v: Value = agent
            .get(&format!("{base}/api/plainapp/register"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        let arr = v["phones"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "Feng-Phone");
        assert_eq!(arr[0]["port"], closed_port);
        assert_eq!(arr[0]["note"], "test phone");
        assert_eq!(arr[1]["name"], "Live-Self");
        assert_eq!(arr[1]["port"], live_port);
        // default port applied when omitted is verified by the update below

        // 4) status: closed port -> 200 + reachable:false (normal state);
        //    live listener -> reachable:true. Both carry latency + last_check.
        let resp = agent.get(&format!("{base}/api/plainapp/status")).call().unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.into_json().unwrap();
        assert_eq!(v["ok"], true);
        let arr = v["phones"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        for row in arr {
            assert!(row["latency_ms"].as_u64().is_some(), "latency_ms always present");
            assert!(row["last_check"].as_i64().unwrap_or(0) > 0, "last_check always present");
            assert!(["ok", "refused", "timeout"].contains(&row["reason"].as_str().unwrap_or("")));
            if row["name"] == "Feng-Phone" {
                assert_eq!(row["reachable"], false, "closed port must be unreachable");
                assert_eq!(row["note"], "test phone");
            }
            if row["name"] == "Live-Self" {
                assert_eq!(row["reachable"], true, "live listener must be reachable");
                assert_eq!(row["reason"], "ok");
            }
        }

        // 5) update by same name keeps the list length and changes the port
        let v: Value = post_json(json!({"name":"Live-Self","host":"127.0.0.1"}))
            .into_json()
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 2, "update must not duplicate");
        let live = v["phones"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "Live-Self")
            .unwrap();
        assert_eq!(live["port"], 8080, "omitted port falls back to 8080");

        // 6) validation failures (contract: HTTP 200 + ok:false)
        let bad = |body: Value| {
            agent
                .post(&format!("{base}/api/plainapp/register"))
                .send_json(body)
                .unwrap()
                .into_json::<Value>()
                .unwrap()
        };
        assert_eq!(bad(json!({"name":"","host":"127.0.0.1"}))["ok"], false);
        assert_eq!(bad(json!({"name":"X","host":"http://127.0.0.1"}))["ok"], false);
        assert_eq!(bad(json!({"name":"X","host":"a b"}))["ok"], false);
        assert_eq!(bad(json!({"name":"X","host":"127.0.0.1","port":0}))["ok"], false);

        // 7) delete roundtrip (+ unknown name error)
        let v: Value = agent
            .delete(&format!("{base}/api/plainapp/register?name=Feng-Phone"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 1);
        let v: Value = agent
            .get(&format!("{base}/api/plainapp/register"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["phones"][0]["name"], "Live-Self");
        let v: Value = agent
            .delete(&format!("{base}/api/plainapp/register?name=NoSuchPhone"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(v["ok"], false);

        // 8) persistence: the file exists, matches the registry, and a fresh
        //    disk load (what a restarted process does) returns the same list
        assert!(phones_json.exists(), "phones.json must be persisted");
        assert!(!phones_json.with_extension("json.tmp").exists(), "tmp file renamed away");
        let text = std::fs::read_to_string(&phones_json).unwrap();
        let file_v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(file_v["phones"][0]["name"], "Live-Self");
        let reloaded = plainapp::load_phones();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].name, "Live-Self");
        assert_eq!(reloaded[0].port, 8080);

        // 9) live sample for the report: register 127.0.0.1:8388 and print
        //    the real status JSON (reachable depends on the host machine).
        let _: Value = post_json(json!({"name":"Sample-8388","host":"127.0.0.1","port":8388,"note":"live smoke sample"}))
            .into_json()
            .unwrap();
        let resp = agent.get(&format!("{base}/api/plainapp/status")).call().unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.into_json().unwrap();
        let sample = v["phones"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "Sample-8388")
            .cloned()
            .unwrap();
        assert!(sample["reachable"].is_boolean());
        println!("--- /api/plainapp/status sample (127.0.0.1:8388) ---");
        println!("{}", serde_json::to_string_pretty(&sample).unwrap());

        // 10) far-address path: RFC 5737 test address. On machines behind a
        //     TUN-mode proxy the local proxy may complete the TCP handshake
        //     for ANY destination, so reachability of 192.0.2.1 is host
        //     dependent; what must hold: HTTP 200, sane fields, no hang
        //     (the unreachable contract itself is covered by the closed
        //     local port in step 4).
        let _: Value = post_json(json!({"name":"Far-Net","host":"192.0.2.1","port":8080}))
            .into_json()
            .unwrap();
        let started = std::time::Instant::now();
        let resp = agent.get(&format!("{base}/api/plainapp/status")).call().unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.into_json().unwrap();
        let row = v["phones"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "Far-Net")
            .cloned()
            .unwrap();
        assert!(row["reachable"].is_boolean());
        assert!(row["latency_ms"].as_u64().unwrap() <= 2000, "probe must respect the 1.5s timeout");
        assert!(started.elapsed().as_secs() < 10, "status must not hang");

        accept_task.abort();
    });

    // Cleanup (best effort).
    std::env::remove_var("FENGPAGE_ROOT");
    std::env::remove_var("FENGPAGE_CONFIG");
    std::env::remove_var("FENGPAGE_PHONES");
    let _ = std::fs::remove_dir_all(&dir);
}
