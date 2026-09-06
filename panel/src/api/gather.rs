// /gather - mobile gather onboarding page (tech plan link 2: phones upload
// into the node, never the other way around).
//
// Endpoints:
//   GET /gather               -> embedded static/gather.html (standalone
//                                six-step guide; loads no panel JS bundle)
//   GET /api/gather/pair-data -> node pairing info (step 3): tailscale IP,
//                                Syncthing device ID, node name
//   GET /api/gather/verify    -> sync verification (step 6): last activity
//                                time, file counts, folder states
//
// Three-state payloads (DESIGN.md soul 1 - honest states, no fake errors):
//   device id source: "rest" (live daemon) / "config" (config.xml fallback,
//   may be stale) / null (nothing available). `degraded` names every part
//   that did not come from a live source; the HTTP status stays 200 so the
//   mobile page can render the real state with a what-to-do hint.
//
// The Syncthing API key is read from config.xml and used only in the
// X-API-Key request header; it is never logged or serialized.
//
// All code comments are pure ASCII.

use crate::ApiResult;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::time::Duration;

/// Embedded six-step page (same single-file include pattern as index.html).
const GATHER_HTML: &str = include_str!("../../static/gather.html");

/// REST timeout for both gather endpoints. Spec: 2s - a phone waiting on a
/// mobile link must get a fast honest answer, not a hanging request.
const REST_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// payloads
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PairPayload {
    pub ok: bool,
    /// Tailscale IPv4 of this node; null when Tailscale is absent or down.
    pub ts_ip: Option<String>,
    /// Syncthing device ID; null when the daemon is down AND config.xml
    /// carries no device entry.
    pub syncthing_id: Option<String>,
    /// Machine hostname, so the user can confirm the phone reaches the
    /// intended node.
    pub node_name: String,
    /// "rest" | "config" | null - where syncthing_id came from.
    pub source: Option<String>,
    /// null when everything came from a live source, else a comma list of
    /// degraded parts ("syncthing", "tailscale").
    pub degraded: Option<String>,
}

#[derive(Serialize)]
pub struct FolderRow {
    pub id: String,
    pub label: String,
    /// db/status state ("idle"/"syncing"/"scanning"/"error"), "unknown" when
    /// the per-folder call failed (row degrades, the view does not).
    pub state: String,
    pub local_files: u64,
}

#[derive(Serialize)]
pub struct VerifyPayload {
    pub ok: bool,
    pub syncthing_up: bool,
    pub degraded: Option<String>,
    /// Newest last-seen timestamp among remote devices (unix seconds);
    /// null when no remote device has ever been seen.
    pub last_activity_unix: Option<i64>,
    /// Configured devices excluding this node itself.
    pub paired_count: usize,
    /// Remote devices currently connected.
    pub connected_count: usize,
    /// Sum of localFiles over all folders (files actually on this node).
    pub total_files: u64,
    /// Sum of inBytesTotal over remote devices (bytes phones pushed in).
    pub total_in_bytes: u64,
    pub folders: Vec<FolderRow>,
}

// ---------------------------------------------------------------------------
// Syncthing plumbing (self-contained, same convention as onboarding.rs:
// each REST module keeps its own helpers so they stay independent)
// ---------------------------------------------------------------------------

fn sync_rest() -> String {
    crate::settings::get().syncthing_addr()
}

/// Read the Syncthing API key from config.xml (request header only).
fn read_apikey() -> Option<String> {
    let home = crate::config::get().syncthing_home.clone()?;
    let cfg = std::fs::read_to_string(home.join("config.xml")).ok()?;
    let start = cfg.find("<apikey>")? + "<apikey>".len();
    let end = cfg[start..].find("</apikey>")? + start;
    Some(cfg[start..end].to_string())
}

fn rest_get_json(key: &str, path: &str) -> Option<serde_json::Value> {
    let agent = ureq::AgentBuilder::new().timeout(REST_TIMEOUT).build();
    agent
        .get(&format!("{}{path}", sync_rest()))
        .set("X-API-Key", key)
        .call()
        .ok()?
        .into_json::<serde_json::Value>()
        .ok()
}

/// Live device ID from /rest/system/status -> myID.
fn rest_myid(key: &str) -> Option<String> {
    rest_get_json(key, "/rest/system/status")
        .and_then(|v| v.get("myID").and_then(|m| m.as_str()).map(|s| s.to_string()))
}

/// Best-effort device ID from config.xml (daemon down). The local entry is
/// not guaranteed to be first, so with several named devices the answer may
/// be wrong - callers must surface the degraded state to the user.
fn config_device_id() -> Option<String> {
    let home = crate::config::get().syncthing_home.clone()?;
    let cfg = std::fs::read_to_string(home.join("config.xml")).ok()?;
    let start = cfg.find("<device id=\"")? + "<device id=\"".len();
    let end = cfg[start..].find('"')? + start;
    let id = &cfg[start..end];
    if id.is_empty() { None } else { Some(id.to_string()) }
}

/// Best-effort tailnet IPv4 (`tailscale ip -4`, first line). None when
/// Tailscale is not installed / not running / not on PATH.
fn tailnet_ip() -> Option<String> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
}

/// Machine hostname (same source as /api/health).
fn node_name() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "?unknown".into())
}

/// null for an empty part list, else the joined degraded-part string.
fn degrade(parts: &[&str]) -> Option<String> {
    if parts.is_empty() { None } else { Some(parts.join(",")) }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// GET /gather -> the embedded standalone six-step page.
pub async fn page() -> Response {
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        GATHER_HTML,
    )
        .into_response()
}

/// GET /api/gather/pair-data
pub async fn pair_data() -> ApiResult<Json<PairPayload>> {
    let payload = tokio::task::spawn_blocking(|| {
        // Live daemon call and the tailscale CLI probe run in parallel; the
        // config.xml fallback only runs when the daemon did not answer.
        let key = read_apikey();
        let (rest_id, ts_ip) = std::thread::scope(|s| {
            let a = s.spawn(|| match &key {
                Some(k) => rest_myid(k),
                None => None,
            });
            let b = s.spawn(tailnet_ip);
            (a.join().unwrap_or(None), b.join().unwrap_or(None))
        });

        let (syncthing_id, source) = match rest_id {
            Some(id) => (Some(id), Some("rest".to_string())),
            None => match config_device_id() {
                Some(id) => (Some(id), Some("config".to_string())),
                None => (None, None),
            },
        };

        let mut parts: Vec<&str> = Vec::new();
        if source.as_deref() != Some("rest") {
            parts.push("syncthing");
        }
        if ts_ip.is_none() {
            parts.push("tailscale");
        }

        PairPayload {
            ok: true,
            ts_ip,
            syncthing_id,
            node_name: node_name(),
            source,
            degraded: degrade(&parts),
        }
    })
    .await
    .unwrap_or(PairPayload {
        ok: true,
        ts_ip: None,
        syncthing_id: None,
        node_name: "?unknown".into(),
        source: None,
        degraded: degrade(&["syncthing", "tailscale"]),
    });

    Ok(Json(payload))
}

/// GET /api/gather/verify
pub async fn verify() -> ApiResult<Json<VerifyPayload>> {
    let payload = tokio::task::spawn_blocking(|| {
        let Some(key) = read_apikey() else {
            return degraded_verify("syncthing");
        };
        let Some(my_id) = rest_myid(&key) else {
            return degraded_verify("syncthing");
        };

        // Connections: remote devices, last activity, inbound bytes.
        let conns = rest_get_json(&key, "/rest/system/connections")
            .and_then(|v| v.get("connections").and_then(|c| c.as_object()).cloned());
        let (mut paired, mut connected, mut last_at, mut in_bytes, conns_ok) = match &conns {
            Some(map) => {
                let (mut p, mut c, mut at, mut b) = (0usize, 0usize, None::<i64>, 0u64);
                for (id, info) in map {
                    if id == &my_id {
                        continue;
                    }
                    p += 1;
                    if info.get("connected").and_then(|v| v.as_bool()).unwrap_or(false) {
                        c += 1;
                    }
                    if let Some(t) = info.get("at").and_then(|v| v.as_f64()) {
                        let t = t as i64;
                        if t > 0 && at.map(|cur| t > cur).unwrap_or(true) {
                            at = Some(t);
                        }
                    }
                    if let Some(n) = info.get("inBytesTotal").and_then(|v| v.as_u64()) {
                        b += n;
                    }
                }
                (p, c, at, b, true)
            }
            None => (0, 0, None, 0, false),
        };

        // Folders: per-folder db/status in parallel; one failed call only
        // degrades that row to state "unknown" (syncview convention).
        let folders_cfg = rest_get_json(&key, "/rest/config/folders")
            .and_then(|v| v.get("folders").and_then(|f| f.as_array()).cloned())
            .unwrap_or_default();
        let rows: Vec<FolderRow> = std::thread::scope(|s| {
            let handles: Vec<_> = folders_cfg
                .iter()
                .map(|f| {
                    let id = f.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let label = f
                        .get("label")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| id.clone());
                    let key = key.clone();
                    s.spawn(move || {
                        // db/status needs a non-empty folder id (empty -> 404).
                        let st = if id.is_empty() {
                            None
                        } else {
                            rest_get_json(&key, &format!("/rest/db/status?folder={id}"))
                        };
                        FolderRow {
                            id,
                            label,
                            state: st
                                .as_ref()
                                .and_then(|v| v.get("state").and_then(|x| x.as_str()))
                                .unwrap_or("unknown")
                                .to_string(),
                            local_files: st
                                .and_then(|v| v.get("localFiles").and_then(|x| x.as_u64()))
                                .unwrap_or(0),
                        }
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or(FolderRow {
                id: String::new(),
                label: String::new(),
                state: "unknown".into(),
                local_files: 0,
            })).collect()
        });
        let total_files = rows.iter().map(|r| r.local_files).sum();
        let parts: Vec<&str> = if conns_ok { vec![] } else { vec!["connections"] };
        VerifyPayload {
            ok: true,
            syncthing_up: true,
            degraded: degrade(&parts),
            last_activity_unix: last_at,
            paired_count: paired,
            connected_count: connected,
            total_files,
            total_in_bytes: in_bytes,
            folders: rows,
        }
    })
    .await
    .unwrap_or_else(|_| degraded_verify("syncthing"));

    Ok(Json(payload))
}

/// Uniform degraded answer when Syncthing is unreachable (still HTTP 200 so
/// the mobile page renders "Syncthing not running" + what to do).
fn degraded_verify(part: &str) -> VerifyPayload {
    VerifyPayload {
        ok: true,
        syncthing_up: false,
        degraded: degrade(&[part]),
        last_activity_unix: None,
        paired_count: 0,
        connected_count: 0,
        total_files: 0,
        total_in_bytes: 0,
        folders: vec![],
    }
}
