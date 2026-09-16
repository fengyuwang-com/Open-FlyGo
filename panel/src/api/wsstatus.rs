// api/wsstatus.rs - /ws/status live status push (M10).
//
// A single WebSocket endpoint that pushes an aggregated status snapshot
// every ~1s while at least one client is connected:
//
//   GET /ws/status  -> WebSocket, JSON text frames:
//       {
//         "t": <unix seconds>,
//         "health":   { ...same shape as /api/health... },
//         "system":   { ...same shape as /api/system... },
//         "tray":     { ...same shape as /api/tray/status... },
//         "services": [ ...same shape as /api/health/services... ],
//       }
//
// Design rules:
//   - On-demand: with zero connected clients the ticker loop only sleeps;
//     no collectors run, no PowerShell/sysinfo cost is paid.
//   - Never blocks a Tokio worker: every blocking collector (health CPU
//     sample, system PowerShell) runs inside spawn_blocking at the wiring
//     site (main.rs); the ticker itself is pure async.
//   - Section caches: health (2s) and services probes (5s) are refreshed
//     at most once per TTL and reused across broadcasts, so a 1s push rate
//     never re-runs the expensive collectors every tick. The system and
//     tray providers carry their own pacing (system reuses the /api/system
//     12s cache; tray is in-memory and cheap).
//   - Graceful degradation: each section is independent; if one provider
//     fails (panic inside spawn_blocking, dead backend) the wiring returns
//     a fallback value and the other three sections still stream.
//   - The route lives on the guarded panel router, so the M09 auth layer
//     applies to the upgrade request exactly like any other API call.
//
// This module depends on axum + futures + tokio + serde_json + std only.
// All collectors are injected as providers, which keeps the unit tests and
// the integration test (tests/wsstatus_api.rs) free of real hardware work.
//
// All code comments are pure ASCII.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{IntoResponse, Response},
};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Snapshot sections, in broadcast order (also asserted by the tests).
/// Only read by tests in the binary build; the wire shape is fixed.
#[allow(dead_code)]
pub const SECTIONS: [&str; 4] = ["health", "system", "tray", "services"];

/// Health snapshot is cheap enough to recompute once every 2s; the heavy
/// part is the 1s CPU sample, so refreshing every tick would saturate the
/// blocking pool.
const HEALTH_TTL: Duration = Duration::from_secs(2);
/// TCP probes run per service and may wait up to 3s on a dead port; 5s
/// pacing keeps the probe fan-out bounded while still being 3x fresher
/// than the 15s REST polling fallback.
const SERVICES_TTL: Duration = Duration::from_secs(5);
/// System and tray providers manage their own pacing (system reuses the
/// /api/system 12s cache; tray is an in-memory snapshot), so the WS layer
/// never caches them.
const NO_CACHE: Duration = Duration::ZERO;

/// Broadcast buffer: enough for a few seconds of pushes at 1s cadence;
/// slow readers simply skip lagged frames instead of blocking the ticker.
const CHANNEL_CAPACITY: usize = 16;

/// One injected collector: called by the ticker to produce a section.
/// Providers run their own concurrency strategy (spawn_blocking, direct
/// async probing, in-memory reads) and must return a fallback value on
/// failure so one bad section never kills the stream.
pub type Provider = Arc<dyn Fn() -> BoxFuture<'static, Value> + Send + Sync>;

/// The four section providers (one per SECTIONS entry).
pub struct WsSources {
    pub health: Provider,
    pub system: Provider,
    pub tray: Provider,
    pub services: Provider,
}

/// Per-section snapshot cache keyed by section name. `put` overwrites;
/// `get_if_fresh` returns the stored value only while it is younger than
/// the TTL, otherwise None (caller re-collects).
struct SectionCache {
    map: Mutex<HashMap<&'static str, (Instant, Value)>>,
}

impl SectionCache {
    fn new() -> Self {
        SectionCache {
            map: Mutex::new(HashMap::new()),
        }
    }

    fn put(&self, key: &'static str, value: Value) {
        if let Ok(mut g) = self.map.lock() {
            g.insert(key, (Instant::now(), value));
        }
    }

    fn get_if_fresh(&self, key: &'static str, ttl: Duration) -> Option<Value> {
        let g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let hit = g.get(key).map(|(at, v)| (at, v));
        hit.and_then(|(at, v)| fresh_at(Some((at, v)), ttl, now).then(|| v.clone()))
    }

    /// Evict one section (unit-test helper for cache semantics).
    #[allow(dead_code)]
    fn drop(&self, key: &'static str) {
        if let Ok(mut g) = self.map.lock() {
            g.remove(key);
        }
    }
}

/// Pure freshness predicate (clock injected so the unit tests are exact):
/// an entry is fresh while `now` is closer than `ttl` to its fill time.
/// `checked_duration_since` keeps it total: a now-before-fill clock skew
/// counts as stale instead of panicking.
fn fresh_at(entry: Option<(&Instant, &Value)>, ttl: Duration, now: Instant) -> bool {
    match entry {
        Some((at, _)) => ttl > Duration::ZERO && now.checked_duration_since(*at).is_some_and(|d| d < ttl),
        None => false,
    }
}

/// Shared WS-push state: broadcast channel + live-client counter + the
/// injected collectors. Owned via Arc because the ticker task and every
/// upgraded connection need concurrent access.
pub struct WsStatusState {
    tx: tokio::sync::broadcast::Sender<Value>,
    active: AtomicUsize,
    sources: WsSources,
    cache: SectionCache,
}

impl WsStatusState {
    pub fn new(sources: WsSources) -> Arc<Self> {
        let (tx, _) = tokio::sync::broadcast::channel(CHANNEL_CAPACITY);
        Arc::new(WsStatusState {
            tx,
            active: AtomicUsize::new(0),
            sources,
            cache: SectionCache::new(),
        })
    }

    /// Number of live WebSocket clients (test hook for on-demand checks;
    /// the binary build reads the atomic directly in ticker_task).
    #[allow(dead_code)]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Collect the four sections (with per-section caching) and assemble
    /// the broadcast payload. All awaits are provider-owned; the ticker
    /// itself never runs blocking work.
    async fn snapshot(&self) -> Value {
        let sections = vec![
            ("health", self.section_value("health", HEALTH_TTL, &self.sources.health).await),
            ("system", self.section_value("system", NO_CACHE, &self.sources.system).await),
            ("tray", self.section_value("tray", NO_CACHE, &self.sources.tray).await),
            ("services", self.section_value("services", SERVICES_TTL, &self.sources.services).await),
        ];
        assemble(sections)
    }

    /// One section: reuse the cache when fresh, otherwise call the provider
    /// and store the result. Never panics outward; a provider that returns
    /// a fallback value on its own keeps the stream alive.
    async fn section_value(&self, key: &'static str, ttl: Duration, src: &Provider) -> Value {
        match self.cache.get_if_fresh(key, ttl) {
            Some(v) => v,
            None => {
                let v = (src)().await;
                self.cache.put(key, v.clone());
                v
            }
        }
    }
}

/// Assemble the wire payload: `t` (unix seconds) plus one entry per
/// section, in SECTIONS order. Pure and total: every section key is always
/// present even when its value is null/empty (graceful degradation).
pub fn assemble(sections: Vec<(&str, Value)>) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("t".to_string(), json!(unix_now()));
    for (key, value) in sections {
        m.insert(key.to_string(), value);
    }
    Value::Object(m)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ticker + endpoint
// ---------------------------------------------------------------------------

/// Background push loop. Runs forever but is a pure sleeper while no
/// clients are connected: the on-demand gate is checked before ANY section
/// provider is called, so the panel pays zero collection cost when the
/// frontend is closed.
pub async fn ticker_task(state: Arc<WsStatusState>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if state.active.load(Ordering::Relaxed) == 0 {
            continue;
        }
        if state.tx.receiver_count() == 0 {
            continue;
        }
        let snap = state.snapshot().await;
        // send() fails only when every receiver was dropped mid-await;
        // that is the normal shutdown race and needs no handling.
        let _ = state.tx.send(snap);
    }
}

/// GET /ws/status -> upgrade, join the broadcast feed, echo pings.
/// The M09 auth guard runs before this handler (route is on the guarded
/// router), so an unauthenticated upgrade request never reaches the socket.
pub async fn ws_status(ws: WebSocketUpgrade, State(state): State<Arc<WsStatusState>>) -> Response {
    ws.on_upgrade(move |socket| client_loop(socket, state))
        .into_response()
}

/// Per-connection feed pump: forwards every broadcast frame to the socket
/// and watches the client side for close/error. Exits (and decrements the
/// live-client counter) when either direction dies.
async fn client_loop(mut socket: WebSocket, state: Arc<WsStatusState>) {
    state.active.fetch_add(1, Ordering::Relaxed);
    let mut rx = state.tx.subscribe();
    loop {
        tokio::select! {
            res = rx.recv() => {
                match res {
                    Ok(value) => {
                        let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
                        if socket.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    // Slow reader: drop the backlog, keep the newest frames.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    // Sender gone (shutdown) -> close this connection too.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    // axum does not auto-reply to pings; browsers drop the
                    // connection without a timely pong (handled like the
                    // /term-ws proxy).
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    // Client text/binary/pong frames are ignored.
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
    state.active.fetch_sub(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// unit tests (pure logic only; no collectors run)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_always_contains_every_section() {
        let snap = assemble(vec![
            ("health", Value::Null),
            ("system", Value::Null),
            ("tray", json!({"ok": true})),
            ("services", Value::Null),
        ]);
        let obj = snap.as_object().expect("payload is an object");
        for key in SECTIONS {
            assert!(obj.contains_key(key), "section {key} must be present");
        }
        assert!(obj["t"].as_u64().is_some(), "'t' must be a unix timestamp");
        assert!(obj["health"].is_null(), "null sections are preserved as-is");

        // Partial input is tolerated: no panic, only the given keys appear
        // (the real caller always passes all four, but the wire format must
        // not crash if a future caller forgets one).
        let partial = assemble(vec![("tray", json!({"ok": true}))]);
        let p = partial.as_object().expect("payload is an object");
        assert!(p.contains_key("tray"));
        assert!(!p.contains_key("health"), "absent sections stay absent");
    }

    #[test]
    fn freshness_predicate_is_exact() {
        let now = Instant::now();
        let entry = (now, Value::Null);
        let ttl = Duration::from_secs(2);
        assert!(fresh_at(Some((&entry.0, &entry.1)), ttl, now), "just-filled entry is fresh");
        let later = now + Duration::from_secs(1);
        assert!(fresh_at(Some((&entry.0, &entry.1)), ttl, later), "younger than TTL is fresh");
        let expired = now + Duration::from_secs(2);
        assert!(!fresh_at(Some((&entry.0, &entry.1)), ttl, expired), "at TTL it is stale");
        let past = now + Duration::from_secs(3);
        assert!(!fresh_at(Some((&entry.0, &entry.1)), ttl, past), "older than TTL is stale");
        assert!(!fresh_at(None, ttl, now), "absent entry is never fresh");
        // Zero TTL disables caching entirely (system/tray sections).
        assert!(!fresh_at(Some((&entry.0, &entry.1)), Duration::ZERO, now));
    }

    #[test]
    fn section_cache_respects_ttl() {
        let cache = SectionCache::new();
        cache.put("health", json!({"cpu": 42}));
        assert_eq!(
            cache.get_if_fresh("health", Duration::from_secs(60)).unwrap()["cpu"],
            42,
            "fresh entry is returned"
        );
        cache.put("health", json!({"cpu": 99}));
        assert_eq!(
            cache.get_if_fresh("health", Duration::from_secs(60)).unwrap()["cpu"],
            99,
            "put overwrites the previous value"
        );
        cache.drop("health");
        assert!(cache.get_if_fresh("health", Duration::from_secs(60)).is_none(), "dropped entry is gone");
        // ZERO TTL never hits the cache even when an entry exists.
        cache.put("tray", json!({"ok": true}));
        assert!(cache.get_if_fresh("tray", Duration::ZERO).is_none(), "NO_CACHE sections bypass the cache");
    }
}