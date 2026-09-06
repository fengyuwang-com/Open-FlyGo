// api/tray.rs - tray daemon endpoints.
//
//   GET  /api/tray/status -> daemon state for the frontend card
//   POST /api/tray/ctl    -> {"name":"webcli|syncthing|homepage|sync|eject|portproxy","action":"start|stop"}
//   POST /api/tray/exit   -> stop all children and shut the panel down
//
// All code comments are pure ASCII; JSON messages may be UTF-8.

use axum::{extract::State, Json};
use serde::Deserialize;

use crate::{AppState, ApiResult};

/// GET /api/tray/status
pub async fn status(State(st): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    st.daemon.refresh();
    Ok(Json(st.daemon.status_json()))
}

#[derive(Deserialize)]
pub struct CtlReq {
    pub name: String,
    pub action: String,
}

fn ok(msg: &str) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "message": msg }))
}

fn err(msg: &str) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": false, "message": msg }))
}

/// POST /api/tray/ctl
pub async fn ctl(State(st): State<AppState>, Json(body): Json<CtlReq>) -> Json<serde_json::Value> {
    let name = body.name.as_str();
    let action = body.action.as_str();
    match (name, action) {
        ("webcli" | "syncthing" | "homepage", "start") => match st.daemon.start(name) {
            Ok(pid) => ok(&format!("{name} 已启动 (PID {pid})")),
            Err(e) => {
                if e.contains("already running") {
                    ok(&format!("{name} 已在运行"))
                } else {
                    err(&e)
                }
            }
        },
        ("webcli" | "syncthing" | "homepage", "stop") => match st.daemon.stop(name) {
            Ok(()) => ok(&format!("{name} 已停止")),
            Err(e) => err(&e),
        },
        ("sync", "start") => {
            st.daemon.set_sync(true);
            ok("同步定时器已开启")
        }
        ("sync", "stop") => {
            st.daemon.set_sync(false);
            ok("同步定时器已关闭")
        }
        // Safe-eject every external USB drive: stop the drive-dependent
        // components first, give docker time to release the mount, then run
        // elevated mountvol /p per drive (single UAC prompt). Blocks; run on
        // spawn_blocking so the request thread stays responsive.
        ("eject", _) => {
            let d = st.daemon.clone();
            let msg = tokio::task::spawn_blocking(move || {
                let summary = d.stop_ext_deps();
                std::thread::sleep(std::time::Duration::from_secs(2));
                let letters = d.usb_letters();
                crate::tray::eject_usb_drives(&letters);
                format!("{summary}；已发起卸载 {} 个外置盘", letters.len())
            })
            .await
            .unwrap_or_else(|e| format!("eject 任务失败: {e}"));
            ok(&msg)
        }
        // Port forwarding (elevated apply-portproxy.ps1, UAC prompt expected).
        ("portproxy", _) => {
            crate::tray::run_port_proxy();
            ok("已发起端口转发（UAC 弹窗确认）")
        }
        _ => err("name 须为 webcli/syncthing/homepage/sync/eject/portproxy，action 须为 start/stop"),
    }
}

/// POST /api/tray/exit - stop every child, then shut the panel down.
pub async fn exit(State(st): State<AppState>) -> Json<serde_json::Value> {
    st.daemon.kill_all();
    crate::request_shutdown();
    ok("已停止全部后台并退出")
}
