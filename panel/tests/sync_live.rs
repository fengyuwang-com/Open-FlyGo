// Live probe for GET /api/sync/overview against the machine's real
// Syncthing (opt-in only; no assertions on engine state).
//
// Runs when SYNC2_LIVE=1 is set, otherwise prints SKIP. Mirrors the
// SYNCVIEW_HOLD pattern in src/api/syncview.rs: the real handler is served
// on an ephemeral port, then held open (SYNC2_HOLD seconds, default 20) so
// a shell curl can capture the JSON. Read-only against the engine; the
// GUI apikey is only ever placed in a request header, never printed.
//
// The install root is a temp dir so data/ writes never touch the real one;
// the legacy syncthing home still resolves for the real apikey, and the
// REST base comes from the real [sync].syncthing_addr (default
// http://127.0.0.1:8384). The tray snapshot is a stub (the daemon lives in
// the panel binary, not in this test).
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
#[path = "../src/api/sync.rs"]
mod sync;

use std::sync::Arc;

use axum::Router;

fn temp_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("flygo-sync-live-{}", std::process::id()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_overview_http() {
    let live = std::env::var("SYNC2_LIVE")
        .map(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !live {
        eprintln!("SKIP live_overview_http: set SYNC2_LIVE=1 to probe the real engine");
        return;
    }

    let dir = temp_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("FENGPAGE_ROOT", &dir);
    config::init(); // real settings + legacy syncthing home resolution

    let state = Arc::new(sync::SyncState {
        tray: Box::new(sync::TraySnapshot::default), // stub: no daemon here
        links_path: dir.join("data").join("sync-links.json"),
    });
    let app = Router::new()
        .route("/api/sync/overview", axum::routing::get(sync::overview))
        .route("/api/sync/links", axum::routing::get(sync::list_links))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(8))
        .build();
    let base = format!("http://{addr}/api/sync/overview");
    let mut up = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if agent.get(&base).call().is_ok() {
            up = true;
            break;
        }
    }
    assert!(up, "live probe server did not come up on {addr}");

    let body = agent.get(&base).call().unwrap().into_string().unwrap();
    eprintln!("LIVE OVERVIEW:\n{body}");
    let links = agent
        .get(&format!("http://{addr}/api/sync/links"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    eprintln!("LIVE LINKS:\n{links}");

    let hold: u64 = std::env::var("SYNC2_HOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    eprintln!("[hold] serving on {addr} for {hold}s (curl {base})");
    tokio::time::sleep(std::time::Duration::from_secs(hold)).await;
    server.abort();

    std::env::remove_var("FENGPAGE_ROOT");
    let _ = std::fs::remove_dir_all(&dir);
}
