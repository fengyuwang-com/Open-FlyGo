// Integration test for the P1 /api/config HTTP endpoints.
//
// Runs as its own test binary so FENGPAGE_CONFIG / FENGPAGE_ADDR can be set
// without racing the unit-test binary. Exercises the real handlers from
// src/api/cfg.rs over real HTTP: file layering (env > file > default),
// per-item source annotations, per-field PUT validation and the atomic
// tmp+rename write. All state lives in a temp directory; nothing in the
// repo or the running panel is touched.
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
#[path = "../src/log.rs"]
mod log;
// Shared source module: items flagged here may be used only by the main
// binary, not by this test harness.
#[allow(dead_code)]
#[path = "../src/api/cfg.rs"]
mod cfg;

use std::path::PathBuf;

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("flygo-cfg-it-{}", std::process::id()))
}

#[test]
fn config_api_over_http() {
    let dir = temp_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("flygo.toml");

    let initial = "[server]\naddr = \"127.0.0.1:18388\"\n[sync]\ninterval_secs = 300\n";
    std::fs::write(&file, initial).unwrap();
    std::env::set_var("FENGPAGE_CONFIG", &file);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let app = axum::Router::new().route(
            "/api/config",
            axum::routing::get(cfg::get_config).put(cfg::put_config),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(8))
            .build();
        let base = format!("http://{addr}/api/config");

        // Readiness poll.
        let mut up = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if agent.get(&base).call().is_ok() {
                up = true;
                break;
            }
        }
        assert!(up, "config API did not come up");

        // GET: effective merged config with per-item sources + defaults.
        let body = agent.get(&base).call().unwrap().into_string().unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["exists"], true);
        assert_eq!(v["file"], file.to_string_lossy().to_string());
        assert_eq!(v["effective"]["server"]["addr"]["value"], "127.0.0.1:18388");
        assert_eq!(v["effective"]["server"]["addr"]["source"], "file");
        assert_eq!(v["effective"]["server"]["bind_retries"]["value"], 10);
        assert_eq!(v["effective"]["server"]["bind_retry_ms"]["source"], "default");
        assert_eq!(v["effective"]["sync"]["interval_secs"]["value"], 300);
        assert_eq!(v["effective"]["sync"]["interval_secs"]["source"], "file");
        // term.webcli_addr derives from [webcli] host:port when not set.
        assert_eq!(v["effective"]["term"]["webcli_addr"]["value"], "127.0.0.1:8788");
        assert_eq!(v["effective"]["flags"]["feature"]["auto_retry"]["value"], true);
        assert_eq!(v["default"]["server"]["addr"], "0.0.0.0:8388");
        assert_eq!(v["default"]["system"]["cache_ttl_secs"], 12);

        // env override: FENGPAGE_ADDR beats the file value.
        std::env::set_var("FENGPAGE_ADDR", "127.0.0.1:19999");
        let body = agent.get(&base).call().unwrap().into_string().unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["effective"]["server"]["addr"]["value"], "127.0.0.1:19999");
        assert_eq!(v["effective"]["server"]["addr"]["source"], "env");
        std::env::remove_var("FENGPAGE_ADDR");
        let body = agent.get(&base).call().unwrap().into_string().unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["effective"]["server"]["addr"]["value"], "127.0.0.1:18388");

        // PUT with a type error: rejected per field, file untouched.
        let resp = agent
            .put(&base)
            .send_string("[server]\naddr = 123\n")
            .unwrap()
            .into_string()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["errors"][0]["field"], "server.addr");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), initial);

        // PUT violating the ttl < poll linkage rule: rejected.
        let resp = agent
            .put(&base)
            .send_string("[system]\ncache_ttl_secs = 15\n[ui]\npoll_ms = 15000\n")
            .unwrap()
            .into_string()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["errors"][0]["field"], "system.cache_ttl_secs");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), initial);

        // PUT valid document: written atomically, GET reflects it.
        let new_cfg = "[server]\naddr = \"0.0.0.0:8388\"\n[sync]\ninterval_secs = 120\n";
        let resp = agent
            .put(&base)
            .send_string(new_cfg)
            .unwrap()
            .into_string()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true);
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk.contains("interval_secs = 120"), "file rewritten: {on_disk}");
        assert!(!file.with_extension("toml.tmp").exists(), "tmp file must be renamed away");
        let body = agent.get(&base).call().unwrap().into_string().unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["effective"]["sync"]["interval_secs"]["value"], 120);
        assert_eq!(v["effective"]["sync"]["interval_secs"]["source"], "file");
        assert_eq!(v["exists"], true);

        // FENGPAGE_CONFIG pointing at a missing file: exists=false, and PUT
        // creates the file at that location.
        let missing = dir.join("new").join("flygo.toml");
        std::env::set_var("FENGPAGE_CONFIG", &missing);
        let body = agent.get(&base).call().unwrap().into_string().unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["exists"], false);
        assert_eq!(v["effective"]["server"]["addr"]["source"], "default");
        let resp = agent
            .put(&base)
            .send_string("[server]\naddr = \"127.0.0.1:18444\"\n")
            .unwrap()
            .into_string()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true, "PUT must create the missing file: {resp}");
        assert!(missing.exists(), "file created by PUT");

        server.abort();
    });

    // Cleanup (best effort; a failed assert leaves the temp dir for review).
    std::env::remove_var("FENGPAGE_CONFIG");
    std::env::remove_var("FENGPAGE_ADDR");
    let _ = std::fs::remove_dir_all(&dir);
}
