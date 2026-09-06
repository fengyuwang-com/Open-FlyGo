// api/vdisp.rs - virtual display mode endpoints.
//
//   GET  /api/vdisp/status -> {"ok":true,"parsec_detected":bool,
//                              "parsec_status":"ok"|"disabled"|"missing","mode":bool}
//   POST /api/vdisp/ctl    -> {"action":"on"|"off"}
//   GET  /api/vdisp/mode   -> pinned mode + live actual mode + guard status
//                             {"ok":true,"enabled":bool,"width","height",
//                              "refresh","actual":{...}|null,"guard":{...}}
//   PUT  /api/vdisp/mode   -> {"width","height","refresh"} (bounds-checked,
//                             persisted to data/vdisp-pin.json, guard kicked)
//   DELETE /api/vdisp/mode -> remove the pin (display keeps its current mode)
//
// Virtual display mode: the Parsec virtual display is a *remote fallback*.
// Remote sessions (Parsec/RDP) can render to it even when the physical screen
// is off or asleep. Turning the mode on only verifies the virtual display
// adapter is present and running; it never blanks the physical screen - on a
// single-screen machine the physical display must keep working normally.
//
// Resolution/refresh pinning (vdisp-pinning-design.md): the mode API only
// validates and persists; the daemon guard task (60s + post-PUT kick) does
// the Win32 reapply, so these handlers stay testable without display side
// effects.
//
// All code comments are pure ASCII; JSON messages may be UTF-8.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Deserialize;

use crate::daemon::{query_vdisp_live_mode, VdispPinState, PIN_MAX_HEIGHT, PIN_MAX_REFRESH, PIN_MAX_WIDTH, PIN_MIN_HEIGHT, PIN_MIN_REFRESH, PIN_MIN_WIDTH};
use crate::{daemon::ParsecState, AppState, ApiResult};

/// GET /api/vdisp/status
pub async fn status(State(st): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    // The Parsec probe can block up to 10s on a cold cache; run it off the
    // async worker.
    let d = st.daemon.clone();
    let state = tokio::task::spawn_blocking(move || d.parsec_state())
        .await
        .unwrap_or(ParsecState::Missing);
    Ok(Json(serde_json::json!({
        "ok": true,
        "parsec_detected": state == ParsecState::Ready,
        "parsec_status": state.as_str(),
        "mode": st.daemon.vdisp_mode(),
    })))
}

#[derive(Deserialize)]
pub struct CtlReq {
    pub action: String,
}

/// POST /api/vdisp/ctl
pub async fn ctl(State(st): State<AppState>, Json(body): Json<CtlReq>) -> Json<serde_json::Value> {
    let d = st.daemon.clone();
    let res = tokio::task::spawn_blocking(move || match body.action.as_str() {
        "on" => d.vdisp_on(),
        "off" => d.vdisp_off(),
        other => Err(format!("action 须为 on/off，收到: {other}")),
    })
    .await;
    match res {
        Ok(Ok(msg)) => Json(serde_json::json!({ "ok": true, "message": msg })),
        Ok(Err(msg)) => Json(serde_json::json!({ "ok": false, "message": msg })),
        Err(e) => Json(serde_json::json!({ "ok": false, "message": format!("任务执行失败: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// resolution/refresh pinning (/api/vdisp/mode, state = Arc<VdispPinState>)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ModeReq {
    pub width: i64,
    pub height: i64,
    pub refresh: i64,
}

/// Body numbers are i64 so negative or huge values surface as a range error
/// message instead of a serde rejection (the bounds live in daemon.rs).
fn pin_from_body(b: &ModeReq) -> Result<crate::daemon::PinMode, String> {
    let w = u32::try_from(b.width)
        .map_err(|_| format!("width must be {PIN_MIN_WIDTH}-{PIN_MAX_WIDTH}, got {}", b.width))?;
    let h = u32::try_from(b.height)
        .map_err(|_| format!("height must be {PIN_MIN_HEIGHT}-{PIN_MAX_HEIGHT}, got {}", b.height))?;
    let r = u32::try_from(b.refresh)
        .map_err(|_| format!("refresh must be {PIN_MIN_REFRESH}-{PIN_MAX_REFRESH} Hz, got {}", b.refresh))?;
    crate::daemon::validate_pin_mode(w, h, r)
}

/// GET /api/vdisp/mode: the pinned target, the live actual mode of the
/// virtual display and the guard status.
pub async fn mode_get(
    State(pin): State<Arc<VdispPinState>>,
) -> Json<serde_json::Value> {
    let target = pin.pin_mode();
    let actual = tokio::task::spawn_blocking(query_vdisp_live_mode)
        .await
        .ok()
        .flatten();
    Json(serde_json::json!({
        "ok": true,
        "enabled": target.is_some(),
        "width": target.map(|m| m.width).unwrap_or(0),
        "height": target.map(|m| m.height).unwrap_or(0),
        "refresh": target.map(|m| m.refresh).unwrap_or(0),
        "actual": actual.as_ref().map(|a| serde_json::json!({
            "device": a.device,
            "monitor": a.monitor,
            "width": a.width,
            "height": a.height,
            "refresh": a.refresh,
        })),
        "guard": serde_json::to_value(pin.guard_info()).unwrap_or(serde_json::Value::Null),
    }))
}

/// PUT /api/vdisp/mode: validate, persist, then wake the guard so the mode
/// is applied within ~1s (and re-checked every 60s afterwards).
pub async fn mode_put(
    State(pin): State<Arc<VdispPinState>>,
    Json(body): Json<ModeReq>,
) -> Json<serde_json::Value> {
    let mode = match pin_from_body(&body) {
        Ok(m) => m,
        Err(msg) => return Json(serde_json::json!({ "ok": false, "message": msg })),
    };
    match pin.set_pin(mode.width, mode.height, mode.refresh) {
        Ok(saved) => {
            pin.kick_guard();
            Json(serde_json::json!({
                "ok": true, "enabled": true,
                "width": saved.width, "height": saved.height, "refresh": saved.refresh,
                "message": "固定模式已保存；守护将在数秒内应用到虚拟显示器，此后每 60 秒校验一次",
            }))
        }
        Err(msg) => Json(serde_json::json!({ "ok": false, "message": msg })),
    }
}

/// DELETE /api/vdisp/mode: remove the pin. The display keeps its current
/// mode; nothing is restored (Windows owns the default topology behavior).
pub async fn mode_del(State(pin): State<Arc<VdispPinState>>) -> Json<serde_json::Value> {
    match pin.clear_pin() {
        Ok(()) => Json(serde_json::json!({
            "ok": true, "enabled": false,
            "message": "已取消固定；虚拟显示器保持当前模式",
        })),
        Err(msg) => Json(serde_json::json!({ "ok": false, "message": msg })),
    }
}

// ---------------------------------------------------------------------------
// tests: GET/PUT/DELETE roundtrip over real HTTP (same style as syncview.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mode_tests {
    use super::*;

    /// Serve the three mode routes on an ephemeral port backed by a temp
    /// pin file, then exercise the full GET/PUT/DELETE roundtrip plus the
    /// validation boundaries. No guard task is spawned here, so the test
    /// never touches the real display configuration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mode_api_roundtrip_over_http() {
        let path = std::env::temp_dir()
            .join(format!("flygo-vdisp-mode-api-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let state = VdispPinState::new(path.clone());
        let app = axum::Router::new()
            .route(
                "/api/vdisp/mode",
                axum::routing::get(mode_get)
                    .put(mode_put)
                    .delete(mode_del),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(8))
            .build();
        let url = format!("http://{addr}/api/vdisp/mode");

        // GET before any PUT: pin disabled (task spec: enabled:false).
        let g: serde_json::Value = agent.get(&url).call().unwrap().into_json().unwrap();
        assert_eq!(g["ok"], true);
        assert_eq!(g["enabled"], false);

        // PUT with an illegal width (task spec example: 400) is rejected
        // with a range message, and nothing is persisted.
        let r: serde_json::Value = agent
            .request("PUT", &url)
            .send_json(serde_json::json!({"width": 400, "height": 1080, "refresh": 60}))
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(r["ok"], false, "width 400 must be rejected");
        assert!(r["message"].as_str().unwrap().contains("width"));
        assert!(!path.exists(), "rejected pin must not be persisted");

        // PUT refresh out of bounds is rejected too.
        let r: serde_json::Value = agent
            .request("PUT", &url)
            .send_json(serde_json::json!({"width": 1920, "height": 1080, "refresh": 241}))
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(r["ok"], false);
        assert!(r["message"].as_str().unwrap().contains("refresh"));

        // Negative height surfaces as a range message (i64 body).
        let r: serde_json::Value = agent
            .request("PUT", &url)
            .send_json(serde_json::json!({"width": 1920, "height": -5, "refresh": 60}))
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(r["ok"], false);
        assert!(r["message"].as_str().unwrap().contains("height"));

        // Valid PUT: stored and echoed.
        let r: serde_json::Value = agent
            .request("PUT", &url)
            .send_json(serde_json::json!({"width": 1920, "height": 1080, "refresh": 60}))
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(r["ok"], true, "valid pin accepted: {r}");
        assert_eq!(r["enabled"], true);
        assert_eq!(r["width"], 1920u32);
        assert_eq!(r["height"], 1080u32);
        assert_eq!(r["refresh"], 60u32);

        // GET now reports the pin (actual/guard are environment-dependent:
        // a live Parsec display may or may not exist on the test host).
        let g: serde_json::Value = agent.get(&url).call().unwrap().into_json().unwrap();
        eprintln!("LIVE /api/vdisp/mode: {g}");
        assert_eq!(g["enabled"], true);
        assert_eq!(g["width"], 1920u32);
        assert_eq!(g["height"], 1080u32);
        assert_eq!(g["refresh"], 60u32);
        assert!(g["guard"].is_object(), "guard status object present");

        // Persistence across a "restart" (fresh store over the same file).
        assert_eq!(
            VdispPinState::new(path.clone()).pin_mode(),
            Some(crate::daemon::PinMode { width: 1920, height: 1080, refresh: 60 })
        );

        // DELETE removes the pin and reports enabled:false.
        let r: serde_json::Value = agent
            .request("DELETE", &url)
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(r["ok"], true);
        assert_eq!(r["enabled"], false);
        let g: serde_json::Value = agent.get(&url).call().unwrap().into_json().unwrap();
        assert_eq!(g["enabled"], false);
        assert_eq!(VdispPinState::new(path.clone()).pin_mode(), None);

        let _ = std::fs::remove_file(&path);
    }
}
