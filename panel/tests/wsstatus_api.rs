// Integration test for the /ws/status live push endpoint (M10).
//
// Runs as its own test binary (like control_api.rs) and includes the real
// wsstatus module via #[path]. Collectors are injected as counting fake
// providers, so the tests exercise the real route, upgrade handshake,
// broadcast protocol, snapshot assembly and the on-demand gate without
// touching PowerShell/sysinfo.
//
// Covered here:
//   1. route: GET /ws/status upgrades (HTTP 101)
//   2. protocol: first frames are JSON text with t + all four sections
//   3. snapshot: section values flow through unchanged
//   4. graceful degradation / on-demand: zero clients -> zero collection;
//      after the last client leaves the ticker idles again
//
// All code comments are pure ASCII.

#[path = "../src/api/wsstatus.rs"]
mod wsstatus;

use axum::{routing::get, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use wsstatus::{Provider, WsSources, WsStatusState};

/// A provider that counts every invocation and returns a section object
/// carrying the tag and the call number. Observing the counter tells the
/// test exactly when (and whether) collection ran.
fn counting_provider(counter: Arc<AtomicUsize>, tag: &'static str) -> Provider {
    Arc::new(move || {
        let counter = counter.clone();
        Box::pin(async move {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            json!({ "tag": tag, "calls": n })
        })
    })
}

fn test_state() -> (Arc<WsStatusState>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let health_calls = Arc::new(AtomicUsize::new(0));
    let tray_calls = Arc::new(AtomicUsize::new(0));
    let sources = WsSources {
        health: counting_provider(health_calls.clone(), "health"),
        system: counting_provider(Arc::new(AtomicUsize::new(0)), "system"),
        // tray is declared NO_CACHE, so its counter mirrors the push count
        // exactly (health carries a 2s cache; services a 5s one).
        tray: counting_provider(tray_calls.clone(), "tray"),
        services: counting_provider(Arc::new(AtomicUsize::new(0)), "services"),
    };
    (WsStatusState::new(sources), health_calls, tray_calls)
}

/// Boot a bare router with the real /ws/status handler plus the ticker on
/// an ephemeral port; returns the ws:// URL.
async fn boot(state: Arc<WsStatusState>) -> String {
    let app = Router::new()
        .route("/ws/status", get(wsstatus::ws_status))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::spawn(wsstatus::ticker_task(state));
    format!("ws://{addr}/ws/status")
}

/// Connect with retries (the listener may still be accepting).
async fn connect(url: &str) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    for _ in 0..20 {
        match tokio_tungstenite::connect_async(url).await {
            Ok((sock, resp)) => {
                assert_eq!(resp.status(), 101, "/ws/status must upgrade");
                return sock;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    panic!("could not connect to {url}");
}

/// Read the next frame, requiring it to be a JSON text message.
async fn next_snapshot(sock: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) -> Value {
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), sock.next())
        .await
        .expect("frame timeout")
        .expect("stream ended")
        .expect("frame error");
    match frame {
        tokio_tungstenite::tungstenite::Message::Text(t) => {
            serde_json::from_str(&t).expect("frame must be JSON")
        }
        other => panic!("expected a text frame, got {other:?}"),
    }
}

#[test]
fn ws_status_upgrades_and_streams_aggregated_snapshots() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (state, _hc, _tc) = test_state();
        let url = boot(state.clone()).await;
        let mut sock = connect(&url).await;

        // First snapshot: t + all four sections, values pass through.
        let snap = next_snapshot(&mut sock).await;
        let obj = snap.as_object().expect("payload is an object");
        for key in ["health", "system", "tray", "services"] {
            assert!(obj.contains_key(key), "section {key} must be present");
        }
        assert!(obj["t"].as_u64().is_some(), "'t' must be a unix timestamp");
        assert_eq!(obj["health"]["tag"], "health");
        assert_eq!(obj["health"]["calls"], 1, "first frame carries the first collection");
        assert_eq!(obj["system"]["calls"], 1);
        assert_eq!(obj["tray"]["calls"], 1);
        assert_eq!(obj["services"]["calls"], 1);

        // Second snapshot arrives on its own (push cadence), sections
        // monotonically advance (health re-collects; services is 5s-cached
        // so it may stay at 1 - only non-decreasing is asserted).
        let snap2 = next_snapshot(&mut sock).await;
        let o2 = snap2.as_object().expect("payload is an object");
        for key in ["health", "system", "tray", "services"] {
            assert!(o2.contains_key(key), "section {key} must be present");
            let calls = o2[key]["calls"].as_u64().expect("calls counter");
            let prev = obj[key]["calls"].as_u64().expect("calls counter");
            assert!(calls >= prev, "{key} calls must not regress");
        }

        // Close cleanly; the server must not panic and the counter drops.
        let _ = sock.close(None).await;
        for _ in 0..50 {
            if state.active() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(state.active(), 0, "server-side client count returns to zero");
    });
}

#[test]
fn ticker_collects_nothing_without_clients() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (state, health_calls, tray_calls) = test_state();
        let url = boot(state.clone()).await;

        // No client: the ticker must stay a pure sleeper.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(health_calls.load(Ordering::Relaxed), 0, "no collection with zero clients");

        // Connect and observe at least two pushes (2+ ticker rounds).
        // tray is uncached, so its counter mirrors the push count.
        let mut sock = connect(&url).await;
        let _ = next_snapshot(&mut sock).await;
        let _ = next_snapshot(&mut sock).await;
        assert!(tray_calls.load(Ordering::Relaxed) >= 2, "collection resumes while connected");

        // Drop the client and wait for the server to notice.
        drop(sock);
        for _ in 0..50 {
            if state.active() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(state.active(), 0, "client count returns to zero after close");

        // Let any in-flight tick drain, then verify the collector idles.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let after_drain = tray_calls.load(Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(
            tray_calls.load(Ordering::Relaxed),
            after_drain,
            "no collection after the last client left"
        );
    });
}

#[test]
fn one_failed_section_does_not_kill_the_feed() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // A provider that always fails (returns an error marker). The
        // ticker must still push frames containing every section.
        let bad: Provider = Arc::new(|| Box::pin(async { Value::Null }));
        let sources = WsSources {
            health: bad.clone(),
            system: bad,
            tray: counting_provider(Arc::new(AtomicUsize::new(0)), "tray"),
            services: counting_provider(Arc::new(AtomicUsize::new(0)), "services"),
        };
        let state = WsStatusState::new(sources);
        let url = boot(state).await;
        let mut sock = connect(&url).await;

        let snap = next_snapshot(&mut sock).await;
        let obj = snap.as_object().expect("payload is an object");
        for key in ["health", "system", "tray", "services"] {
            assert!(obj.contains_key(key), "section {key} must still be present");
        }
        assert!(obj["health"].is_null(), "failed sections degrade to their fallback value");
        assert!(obj["system"].is_null());
        assert_eq!(obj["tray"]["calls"], 1, "healthy sections keep streaming");
        assert_eq!(obj["services"]["calls"], 1);
    });
}
