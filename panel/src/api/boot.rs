// api/boot.rs - early-bind boot progress probe.
//
// The panel binds its port FIRST and runs the slow init (stale-child
// cleanup, SMB share lifecycle, default children, WSL keepalive) in a
// background task, so a cold boot shows 1-2 min of nothing without this.
// GET /api/boot/status reports {"ready","stage","elapsed_ms"} where stage
// is one of: binding|stale_cleanup|smb|children|selfcheck|ready. The splash
// page polls it until ready=true.
//
// State is process-global (OnceLock + Mutex) because the background startup
// task and the axum handler share it without an AppState dependency.
//
// All code comments are pure ASCII.

use axum::{http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Boot stages in wire order; the handler only ever reports one of these.
pub const STAGES: [&str; 6] =
    ["binding", "stale_cleanup", "smb", "children", "selfcheck", "ready"];

struct BootInner {
    stage: String,
    ready: bool,
}

static BOOT_T0: OnceLock<Instant> = OnceLock::new();
static BOOT_STATE: OnceLock<Mutex<BootInner>> = OnceLock::new();

fn state() -> &'static Mutex<BootInner> {
    BOOT_STATE.get_or_init(|| Mutex::new(BootInner { stage: "binding".into(), ready: false }))
}

/// Mark boot start (idempotent): freezes t0, resets stage to binding.
pub fn init() {
    let _ = BOOT_T0.set(Instant::now());
    if let Ok(mut g) = state().lock() {
        g.stage = "binding".into();
        g.ready = false;
    }
}

/// Advance the boot stage. Unknown stages are ignored (never leak typos).
pub fn set_stage(stage: &'static str) {
    if !STAGES.contains(&stage) {
        return;
    }
    if let Ok(mut g) = state().lock() {
        g.stage = stage.into();
    }
}

/// Mark boot complete (stage = ready, ready = true).
pub fn set_ready() {
    if let Ok(mut g) = state().lock() {
        g.stage = "ready".into();
        g.ready = true;
    }
}

/// Milliseconds since init() (0 when init() never ran, e.g. unit tests).
pub fn elapsed_ms() -> u64 {
    BOOT_T0.get().map(|t| t.elapsed().as_millis() as u64).unwrap_or(0)
}

/// Synchronous snapshot for tests and non-async callers.
pub fn snapshot() -> (bool, String, u64) {
    let (ready, stage) = match state().lock() {
        Ok(g) => (g.ready, g.stage.clone()),
        Err(e) => {
            let g = e.into_inner();
            (g.ready, g.stage.clone())
        }
    };
    (ready, stage, elapsed_ms())
}

/// GET /api/boot/status -> {"ready":bool,"stage":"string","elapsed_ms":n}.
pub async fn status() -> impl IntoResponse {
    let (ready, stage, ms) = snapshot();
    (StatusCode::OK, Json(json!({"ready": ready, "stage": stage, "elapsed_ms": ms})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_lifecycle_reports_valid_stages_then_ready() {
        init();
        // Walk every non-ready stage in wire order; each must be reported
        // verbatim with ready=false.
        for s in &STAGES[..STAGES.len() - 1] {
            set_stage(s);
            let (ready, stage, _) = snapshot();
            assert!(!ready, "stage {s} must report ready=false");
            assert_eq!(stage, *s);
        }
        // Unknown stages never leak onto the wire.
        set_stage("bogus_stage");
        let (_, stage, _) = snapshot();
        assert_eq!(stage, "selfcheck");
        // Completion flips both fields together.
        set_ready();
        let (ready, stage, _) = snapshot();
        assert!(ready);
        assert_eq!(stage, "ready");
    }
}
