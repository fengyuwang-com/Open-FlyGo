// api/smb.rs - /api/smb* SMB drive-share endpoints.
//
// The SMB lifecycle is panel-controllable:
//   GET  /api/smb           -> every drive present right now (hot-plugged
//                              disks included) + mounted/configured state +
//                              guest-access policy + a best-effort self-probe.
//   GET  /api/smb/status    -> (compat) live + configured share state.
//   POST /api/smb/drive     -> {"letter":"E","mount":true|false} mount or
//                              unmount one drive (persists into [smb].shares).
//   POST /api/smb/unmount-all -> remove every share, clear the persisted set.
//   POST /api/smb/guest     -> {"enabled":true|false} apply/revert the
//                              anonymous-read SMB baseline (the "cannot
//                              access" fix) and persist the flag.
//   POST /api/smb/apply     -> (compat) re-apply the configured share set.
//
// All writes spawn blocking work off the async runtime (Share/exe-ctl calls).
use crate::ApiResult;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Short-TTL cache for the full /api/smb view (stale-while-revalidate).
//
// The full view enumerates every present drive and probes shares, which spawns
// several PowerShell processes and takes ~5-8s. We cache the JSON for a short
// TTL and refresh it in the background (single-flight) so concurrent reads -
// the frontend polls every 15s - return in milliseconds instead of blocking.
// After the first ever compute, get() never blocks: if the cache is present it
// serves it immediately and schedules a background refresh when it has aged.
// Any write invalidates the cache so mount/unmount/guest/apply changes show on
// the very next read. The response structure is unchanged - only latency.
// ---------------------------------------------------------------------------
/// Age at which we consider the cached view worth refreshing in the background.
const SMB_REFRESH_AFTER: Duration = Duration::from_secs(3);
/// Oldest cached view we are willing to serve without blocking to recompute.
const SMB_SERVE_MAX: Duration = Duration::from_secs(20);
struct SmbCache {
    inner: Mutex<Option<(Instant, serde_json::Value)>>,
    refreshing: Mutex<bool>,
}
static SMB_CACHE: OnceLock<SmbCache> = OnceLock::new();
static SMB_WARMUP: OnceLock<()> = OnceLock::new();

fn smb_cache() -> &'static SmbCache {
    SMB_CACHE.get_or_init(|| SmbCache {
        inner: Mutex::new(None),
        refreshing: Mutex::new(false),
    })
}

/// Drop the cached view so the next read recomputes (used by the write routes).
fn smb_invalidate() {
    if let Some(c) = SMB_CACHE.get() {
        if let Ok(mut g) = c.inner.lock() {
            *g = None;
        }
    }
}

/// Age of the cached view in seconds, or None when the cache is empty.
fn smb_age() -> Option<Duration> {
    smb_cache()
        .inner
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|(at, _)| at.elapsed()))
}

/// Compute the full /api/smb JSON (drive enumeration + probe). Synchronous
/// heavy work; always run it off the async runtime.
fn compute_smb_view() -> serde_json::Value {
    let settings = crate::settings::get();
    let drives = crate::smb::list_drives();
    let mounted = drives.iter().filter(|d| d.mounted).count();
    let configured = drives.iter().filter(|d| d.configured).count();
    // Best-effort self-probe on the first configured drive so the UI can
    // show whether anonymous browsing is actually reachable from this host.
    let probe_letter = drives
        .iter()
        .find(|d| d.mounted)
        .map(|d| d.letter.clone())
        .or_else(|| drives.iter().find(|d| d.configured).map(|d| d.letter.clone()));
    let probe = match probe_letter {
        Some(ref l) => {
            let (ok, note) = crate::smb::probe_share(l);
            json!({ "letter": l, "ok": ok, "note": note })
        }
        None => json!(null),
    };
    json!({
        "ok": true,
        "enabled": settings.smb_enabled(),
        "guest_access": settings.smb_guest_access(),
        "host": host_hint(),
        "summary": { "total": drives.len(), "mounted": mounted, "configured": configured },
        "probe": probe,
        "drives": drives.iter().map(drive_json).collect::<Vec<_>>(),
    })
}

/// Replace the cached view with a freshly computed one.
fn smb_store(view: serde_json::Value) {
    if let Ok(mut g) = smb_cache().inner.lock() {
        *g = Some((Instant::now(), view));
    }
}

/// Recompute the view in the background and store it. Returns without waiting;
/// a single-flight guard prevents two refreshes from overlapping.
fn spawn_smb_refresh() {
    let mut slot = smb_cache()
        .refreshing
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if *slot {
        return;
    }
    *slot = true;
    drop(slot);
    tokio::spawn(async move {
        let view = tokio::task::spawn_blocking(compute_smb_view).await;
        if let Ok(view) = view {
            smb_store(view);
        }
        if let Ok(mut slot) = smb_cache().refreshing.lock() {
            *slot = false;
        }
    });
}

/// Start (once) a background loop that keeps the cached view fresh so page
/// loads and the periodic 15s poll hit the cache instead of blocking ~5-8s.
/// Relies on the single-flight guard in spawn_smb_refresh to avoid overlapping
/// expensive recomputes.
fn spawn_smb_warmup() {
    if SMB_WARMUP.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Some(age) = smb_age() {
                if age >= SMB_REFRESH_AFTER {
                    spawn_smb_refresh();
                }
            }
        }
    });
}

fn host_hint() -> String {
    // `computername` never yields a colon; UserPrincipalName/NetBIOS only.
    std::env::var("COMPUTERNAME")
        .map(|n| format!("\\\\{n}"))
        .unwrap_or_else(|_| "\\\\本机".to_string())
}

fn drive_json(d: &crate::smb::DriveInfo) -> serde_json::Value {
    json!({
        "letter": d.letter,
        "path": d.path,
        "fs": d.fs,
        "kind": d.kind,
        "label": d.label,
        "size_gb": d.size_gb,
        "free_gb": d.free_gb,
        "configured": d.configured,
        "mounted": d.mounted,
        "state": d.state,
        "detail": d.detail,
    })
}

/// GET /api/smb - full panel view: present drives + mount state + policy.
/// Fast path serves the short-TTL cache (stale-while-revalidate) and schedules
/// a background refresh when it ages; after the first ever compute it never
/// blocks. A truly empty cache (first read after boot) computes once (~5s).
pub async fn get() -> ApiResult<Json<serde_json::Value>> {
    let cached = smb_cache()
        .inner
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .filter(|(at, _)| at.elapsed() < SMB_SERVE_MAX)
        .map(|(_, v)| v);
    if let Some(v) = cached {
        // Serve immediately; kick a background refresh if it is getting old so
        // the next read sees fresher data without anyone blocking.
        if let Some(age) = smb_age() {
            if age >= SMB_REFRESH_AFTER {
                spawn_smb_refresh();
            }
        }
        spawn_smb_warmup();
        return Ok(Json(v));
    }
    // Empty cache: this is the first ever read (or a write just invalidated
    // it). Compute once, then let the warmup keep it fresh from here on.
    let view = tokio::task::spawn_blocking(compute_smb_view)
        .await
        .unwrap_or_else(|_| json!({ "ok": false, "error": "worker panicked" }));
    smb_store(view.clone());
    spawn_smb_warmup();
    Ok(Json(view))
}

/// GET /api/smb/status - (compat) live + configured SMB share state.
pub async fn status() -> ApiResult<Json<serde_json::Value>> {
    let settings = crate::settings::get();
    let managed = crate::smb::snapshot();
    let configured: Vec<serde_json::Value> = settings
        .smb_shares()
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "path": s.path,
                "grant": if s.grant.trim().is_empty() { settings.smb_default_grant() } else { &s.grant },
            })
        })
        .collect();
    Ok(Json(json!({
        "ok": true,
        "enabled": settings.smb_enabled(),
        "guest_access": settings.smb_guest_access(),
        "default_grant": settings.smb_default_grant(),
        "managed": managed,
        "configured": configured,
    })))
}

/// POST /api/smb/drive - mount or unmount one drive letter.
#[derive(Deserialize)]
struct DriveReq {
    letter: String,
    mount: bool,
}
pub async fn drive(body: String) -> ApiResult<Json<serde_json::Value>> {
    let req: DriveReq = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return Ok(Json(json!({
                "ok": false, "error": format!("bad body: {e}"),
            })));
        }
    };
    match tokio::task::spawn_blocking({
        let letter = req.letter.clone();
        move || crate::smb::set_drive(&letter, req.mount)
    })
    .await
    {
        Ok(Ok(d)) => {
            smb_invalidate();
            Ok(Json(json!({ "ok": true, "drive": drive_json(&d) })))
        }
        Ok(Err(e)) => Ok(Json(json!({ "ok": false, "error": e }))),
        Err(_) => Ok(Json(json!({ "ok": false, "error": "worker panicked" }))),
    }
}

/// POST /api/smb/unmount-all - remove every share and clear the set.
pub async fn unmount_all() -> ApiResult<Json<serde_json::Value>> {
    let removed = tokio::task::spawn_blocking(crate::smb::unmount_all)
        .await
        .unwrap_or(0);
    smb_invalidate();
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

/// POST /api/smb/guest - apply or revert the anonymous-read SMB baseline.
#[derive(Deserialize)]
struct GuestReq {
    enabled: bool,
}
pub async fn guest(body: String) -> ApiResult<Json<serde_json::Value>> {
    let req: GuestReq = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return Ok(Json(json!({ "ok": false, "error": "bad body" })));
        }
    };
    let (enabled, shares) = (req.enabled, crate::settings::get().smb_shares());
    let (ok, msg) = tokio::task::spawn_blocking(move || crate::smb::apply_guest(enabled))
        .await
        .unwrap_or((false, "worker panicked".into()));
    let _ = crate::settings::persist_smb(&shares, enabled);
    smb_invalidate();
    Ok(Json(json!({ "ok": ok, "guest_access": enabled, "note": msg })))
}

/// POST /api/smb/apply - (compat) one-shot re-apply of the share set.
pub async fn apply() -> ApiResult<Json<serde_json::Value>> {
    let managed = tokio::task::spawn_blocking(crate::smb::create_all)
        .await
        .unwrap_or_default();
    smb_invalidate();
    Ok(Json(json!({ "ok": true, "managed": managed })))
}
