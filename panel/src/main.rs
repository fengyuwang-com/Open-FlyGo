// FlyGo - unified panel + tray caretaker.
// Rust backend (axum), memory-first, Tailscale-only network exposure.
//
// Since the tray upgrade this process also acts as a "tray manager":
//   - starts the default NAS children at boot (webcli only; syncthing and
//     homepage stay stopped until started manually from the panel page)
//   - runs a periodic mirror-sync timer (PowerShell script every 10 min)
//   - lives in the system tray with a compact control menu
//   - exiting kills all children first
//
// Routes:
//   GET /              -> embedded static/index.html single page
//   GET /api/health    -> host health (CPU/mem/disk/hostname/IP) + error rate
//   GET /api/services  -> SSH/Tailscale/Syncthing status
//   GET /api/docker    -> sleeping docker service list
//   GET /api/sync      -> Syncthing sync + cross-disk mirror task status (legacy)
//   GET /api/sync/overview -> sync view aggregate: tray timer + mirror result +
//                             Syncthing devices/folders (degraded:null when down)
//   POST /api/sync/link    -> create one sync link (mirror/syncthing/manual)
//   GET /api/sync/links    -> list recorded sync links
//   DELETE /api/sync/links/:id -> remove one link (engine cleanup best-effort)
//   GET /api/syncview/overview -> aggregated sync network view (devices + folders)
//   GET /api/syncview/devices  -> device list with live connection state
//   GET /api/system    -> scheduled tasks / disks / network drives overview
//   GET/POST /api/autostart -> logon autostart list + toggles (tasks + Run keys)
//   POST /api/system/usb / /api/docker/control -> control endpoints
//   GET /api/stats     -> request/error statistics
//   GET/POST /api/flags -> runtime feature flags
//   POST /api/restart  -> self restart (spawn new instance, then exit)
//   GET  /api/tray/status -> tray daemon status (children + sync)
//   POST /api/tray/ctl -> start/stop one child or the sync timer
//   POST /api/tray/exit -> stop all children and exit
//   GET  /api/vdisp/status -> Parsec virtual display state (ok/disabled/missing) + mode
//   POST /api/vdisp/ctl -> virtual display mode on/off (remote fallback; physical screen untouched)
//   GET/PUT/DELETE /api/vdisp/mode -> virtual display resolution/refresh pin
//        (daemon.rs VdispPinState + 60s guard task; see vdisp-pinning-design.md)
//   GET  /api/access/rules -> FlyGo-* firewall allow rules (access control)
//   POST/DELETE /api/access/allow -> add / remove one FlyGo-* firewall rule
//   GET  /api/term/status -> terminal lock state; POST /api/term/unlock
//   POST /api/term/password -> change terminal password; GET /term-ws -> webcli proxy
//   GET  /vendor/{file} -> embedded xterm/addon-fit static assets
//   GET  /api/onboarding/syncthing-qr -> phone onboarding pairing QR (SVG)
//   GET  /api/onboarding/status -> Syncthing paired/connected device state
//   GET  /api/onboarding/plainapp -> PlainApp guide (download QR text + steps)
//   GET/POST/DELETE /api/plainapp/register -> phone node registry (data/phones.json)
//   GET  /api/plainapp/status -> concurrent TCP probe of registered phones
//   GET  /gather -> standalone mobile six-step gather guide (gather.html)
//   GET  /api/gather/pair-data -> node pairing info (ts IP + device ID)
//   GET  /api/gather/verify -> last activity / file counts for the guide
//   POST /api/pair/create|join|folders, GET /api/pair/status|recover
//        -> mutual-backup orchestration (control-plane signaling + Syncthing
//           provisioning + recovery progress; see api/pair.rs)
//   GET/PUT /api/config -> global flygo.toml config (read/write; see settings.rs)
//   GET/PUT /api/config/components -> editable component config (nav/tiles/cards/frames)
//   GET  /api/config/components/default -> built-in default template (editor preview)
//   POST /api/config/components/reset -> delete config file, back to built-in defaults
//   POST /api/control/register|activate|settings, GET /api/control/status
//        -> FlyGo control-plane client (activation wizard + subscription cache)
//   POST /api/upload -> multipart upload target (phone "share to panel")
//   GET  /ws/status   -> live status push (M10): aggregated health/system/
//                        tray/service snapshots, ~1s while clients connected
//
// All code comments are pure ASCII; HTML/JSON payloads are UTF-8.

#![windows_subsystem = "windows"]

mod api;
mod config;
mod daemon;
mod log;
mod settings;
mod smb;
mod tray;

use axum::{
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Router,
};
use std::net::SocketAddr;
use std::os::windows::process::CommandExt;
use std::sync::Arc;

/// Application shared state (paths + tray daemon).
#[derive(Clone)]
struct AppState {
    mirror_log: String,
    mirror_src: String,
    mirror_dst: String,
    daemon: Arc<daemon::DaemonState>,
}

// Graceful shutdown trigger: set by the tray exit / API exit / restart paths.
static SHUTDOWN_TX: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> =
    std::sync::OnceLock::new();
static SHUTDOWN_RX: std::sync::Mutex<Option<tokio::sync::watch::Receiver<bool>>> =
    std::sync::Mutex::new(None);

/// Ask the server to shut down gracefully (used by tray exit, /api/tray/exit
/// and the restart handler).
pub fn request_shutdown() {
    if let Some(tx) = SHUTDOWN_TX.get() {
        let _ = tx.send(true);
    }
}

/// Simple request log middleware (zero deps: method path status latency).
/// Uses the structured logger so every HTTP request is traceable on disk.
async fn log_requests(req: axum::extract::Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let start = std::time::Instant::now();
    let res = next.run(req).await;
    let status = res.status().as_u16();
    api::stats::record(method.as_str(), &path, status);
    let level = if status >= 500 { "ERROR" } else if status >= 400 { "WARN" } else { "INFO" };
    crate::log::log(
        level,
        "http.req",
        &format!("{method} {path} -> {status} ({}ms)", start.elapsed().as_millis()),
    );
    res
}

#[tokio::main]
async fn main() {
    // Velopack (T-10) hooks MUST run first: the crate parses its own
    // command-line switches (--velopack-*) and needs to observe them before
    // any other startup code. on_before_uninstall removes the SMB shares this
    // session created; data volumes are never touched.
    velopack::VelopackApp::build()
        .on_before_uninstall_fast_callback(|_| {
            log::log(
                "INFO",
                "FlyGo.uninstall",
                "uninstall hook: removing SMB shares; data volumes are kept",
            );
            crate::smb::close_app();
        })
        .run();

    // Resolve all machine-specific paths once (env > install-root > legacy).
    config::init();
    // Start the file logger as early as possible so every step below is on disk.
    log::init(&config::get().root);
    log::log("INFO", "FlyGo.boot", &format!("install root: {}", config::get().root.display()));
    match &config::get().config_path {
        Some(p) => log::log("INFO", "FlyGo.boot", &format!("config file: {}", p.display())),
        None => log::log("INFO", "FlyGo.boot", "config file: none (built-in defaults)"),
    }

    // --- tray caretaker startup sequence ---
    // 1) single instance (named mutex, with retry for restart overlap)
    if !daemon::acquire_single_instance() {
        log::log("WARN", "FlyGo.boot", "another tray instance already running; exiting");
        eprintln!("[FlyGo] another tray instance is already running; exiting");
        // The panel is already up in another process (it owns the mutex and
        // is bound to the port). Give the double-click / repeated launch real
        // feedback instead of silently dying: open the panel page in the
        // default browser so the user always "sees something happen". The
        // launcher binary already does this; this covers direct flygo.exe use.
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", settings::get().panel_browser_url().as_str()])
            .creation_flags(0x0800_0000)
            .spawn();
        std::process::exit(1);
    }
    // Early-bind boot: freeze t0 now; /api/boot/status reports "binding"
    // until the background startup task below advances it. The port binds
    // BEFORE the slow init, so the panel answers within seconds on a cold
    // boot instead of after 1-2 min of silence.
    api::boot::init();

    // Fast daemon handle only. The SLOW init (stale cleanup, SMB share
    // lifecycle, default children, WSL keepalive) runs in the background
    // startup task AFTER the port is bound (see below). cleanup_stale only
    // touches the child ports (webcli/syncthing/homepage), never the panel
    // port, so running it late is safe.
    let daemon_state = daemon::DaemonState::new();
    let mut bg_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let ticker = daemon_state.clone();
    bg_tasks.push(tokio::spawn(async move { ticker.sync_ticker().await }));
    // External-drive guard: polls the USB drive state and stops/restores the
    // drive-dependent components (alist container + sync timer) on removal or
    // reinsertion, so a missing mount never breaks or corrupts data.
    let usb_guard = daemon_state.clone();
    bg_tasks.push(tokio::spawn(async move { usb_guard.usb_guard_task().await }));
    // SMB hotplug guard: polls the present drive set and auto-shares newly
    // inserted fixed/removable drives (persisted into [smb].shares), so
    // "插盘即分享" holds without a restart. Manual unmounts are respected
    // via a session deny-list and are never re-shared. Never exits.
    bg_tasks.push(tokio::spawn(async move { smb::smb_hotplug_task().await }));
    // Compose TCP forwarders: the panel listens on each compose host port
    // (tailnet IP + LAN IP -> 127.0.0.1), the only forwarding path that
    // survives DERP relay. Owns reconcile/heal; selfcheck only reports.
    // Never exits on failure.
    bg_tasks.push(tokio::spawn(crate::api::tcpfwd::forward_task()));
    // vdisp pin guard: while a resolution/refresh pin is set, re-apply it to
    // the Parsec virtual display every 60s (and right after a PUT) whenever
    // the mode drifted - topology recompute and streaming clients keep
    // resetting IDD modes, and "detect + reapply" is the only reliable fix
    // (vdisp-pinning-design.md scheme 4). Never exits on failure.
    let vdisp_guard = daemon_state.vdisp_pin();
    bg_tasks.push(tokio::spawn(async move { vdisp_guard.vdisp_guard_task().await }));
    // Control-plane heartbeat: registers/activates on demand via the panel
    // UI and then reports disk/version periodically while [control].enabled.
    // Failures are silent and unpenalized (retry next tick), so a dead
    // control plane never disturbs local operation.
    bg_tasks.push(tokio::spawn(crate::api::control::heartbeat_task()));
    // 4) tray thread (menu commands arrive on the channel below)
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<tray::TrayCmd>();
    tray::spawn(daemon_state.clone(), cmd_tx);

    // 5) graceful-shutdown watch channel
    let (sh_tx, sh_rx) = tokio::sync::watch::channel(false);
    let _ = SHUTDOWN_TX.set(sh_tx);
    *SHUTDOWN_RX.lock().unwrap_or_else(|e| e.into_inner()) = Some(sh_rx);

    // 6) tray menu command runner (blocking thread; executes daemon actions)
    {
        let daemon_state = daemon_state.clone();
        tokio::task::spawn_blocking(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    tray::TrayCmd::ToggleDocker => {
                        crate::log::log("INFO", "tray.cmd", "docker engine toggle requested");
                        // Engine on/off can take up to 120s (cold WSL boot);
                        // run it off the menu thread so the tray stays usable.
                        tokio::task::spawn_blocking(move || {
                            let on = crate::api::dockerctl::wsl_vm_running();
                            let action = if on { "off" } else { "on" };
                            let r = crate::api::dockerctl::engine_toggle(action);
                            crate::log::log(
                                "INFO",
                                "tray.cmd",
                                &format!("docker engine {action}: {}", r.message),
                            );
                            tray::notify(
                                if on { "Docker 引擎已关闭" } else { "Docker 引擎已开启" },
                                &r.message,
                            );
                        });
                    }
                    tray::TrayCmd::RestartWebcli => {
                        crate::log::log("INFO", "tray.cmd", "restart webcli requested");
                        let _ = daemon_state.stop("webcli");
                        std::thread::sleep(std::time::Duration::from_millis(600));
                        match daemon_state.start("webcli") {
                            Ok(pid) => {
                                crate::log::log("INFO", "tray.cmd", &format!("webcli restarted (pid {pid})"));
                                tray::notify("webcli", &format!("已重启 (PID {pid})"));
                            }
                            Err(e) => {
                                crate::log::log("ERROR", "tray.cmd", &format!("restart webcli: {e}"));
                                eprintln!("[tray] restart webcli: {e}")
                            }
                        }
                    }
                    tray::TrayCmd::EjectUsb => {
                        crate::log::log("INFO", "tray.cmd", "safe-eject external drive(s) requested");
                        let summary = daemon_state.stop_ext_deps();
                        crate::log::log("INFO", "tray.cmd", &format!("safe-eject deps: {summary}"));
                        // Give docker time to release the mount before detaching.
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        let letters = daemon_state.usb_letters();
                        crate::log::log(
                            "INFO",
                            "tray.cmd",
                            &format!("launching elevated mountvol for {} drive(s) (UAC prompt)", letters.len()),
                        );
                        tray::eject_usb_drives(&letters);
                        tray::notify("外置盘安全弹出", "依赖已停止，外置盘已安全卸载，可以拔出");
                    }
                    tray::TrayCmd::OpenPanel => {
                        crate::log::log("INFO", "tray.cmd", "open panel requested");
                        open_panel_browser();
                    }
                    tray::TrayCmd::Exit => {
                        crate::log::log("INFO", "tray.cmd", "menu exit requested; stopping children");
                        eprintln!("[tray] exit requested: stopping all children");
                        daemon_state.kill_all();
                        // Retire the tray icon now for immediate UX feedback. The
                        // tray thread only removes its icon and returns; it must
                        // NOT call process::exit() (see tray.rs), so it cannot
                        // preempt the graceful shutdown below. request_shutdown()
                        // drives axum's with_graceful_shutdown, which returns to
                        // the tail of main() where smb::close_app() and the rest
                        // of the cleanup always run to completion before exit.
                        crate::log::log("INFO", "tray.cmd", "requesting graceful shutdown");
                        tray::request_tray_cleanup();
                        request_shutdown();
                    }
                }
            }
        });
    }

    // --- axum router (existing routes unchanged) ---
    let state = AppState {
        mirror_log: config::get().mirror_log.clone(),
        mirror_src: config::get().mirror_src.clone(),
        mirror_dst: config::get().mirror_dst.clone(),
        daemon: daemon_state.clone(),
    };

    // Sync view state: a tray snapshot closure (mirror timer + syncthing
    // child + usb) plus the links registry path. It rides its own sub-router
    // so the /api/sync/* handlers stay testable without the binary AppState.
    let sync_state = {
        let ds = daemon_state.clone();
        std::sync::Arc::new(api::sync::SyncState {
            tray: Box::new(move || api::sync::TraySnapshot {
                timer_enabled: ds.sync_enabled(),
                timer_last_run: ds.sync_last_run(),
                syncthing_running: ds.status("syncthing").running,
                syncthing_pid: ds.status("syncthing").pid,
                usb_present: ds.external_present(),
                usb_letters: ds.usb_letters(),
            }),
            links_path: config::get().sync_links.clone(),
        })
    };
    let sync_routes = Router::new()
        .route("/api/sync/overview", get(api::sync::overview))
        .route("/api/sync/link", post(api::sync::create_link))
        .route("/api/sync/links", get(api::sync::list_links))
        .route("/api/sync/links/:id", delete(api::sync::delete_link))
        .with_state(sync_state);

    // vdisp mode routes ride their own sub-router (state = the shared pin
    // store) so the handlers stay testable without the binary AppState,
    // mirroring the sync_routes pattern above.
    let vdisp_routes = Router::new()
        .route(
            "/api/vdisp/mode",
            get(api::vdisp::mode_get)
                .put(api::vdisp::mode_put)
                .delete(api::vdisp::mode_del),
        )
        .with_state(daemon_state.vdisp_pin());

    // Auth routes use their own AuthState (no AppState dependency).
    let auth_state = api::auth::AuthState::new(
        settings::get().auth_enabled(),
        &settings::get().auth_password(),
    );

    // /ws/status push feed (M10). Providers aggregate the existing status
    // data: health (/api/health collector), system (/api/system cache),
    // tray (daemon children) and service probes (/api/health/services).
    // All blocking collectors run inside spawn_blocking at this wiring
    // site, so the async ticker never occupies a Tokio worker; each
    // provider degrades to a fallback value on failure, never the feed.
    let tray_src = daemon_state.clone();
    let ws_sources = api::wsstatus::WsSources {
        health: {
            let p: api::wsstatus::Provider = Arc::new(|| {
                Box::pin(async {
                    let h = tokio::task::spawn_blocking(api::health::collect_health)
                        .await
                        .unwrap_or_else(|_| api::health::fallback_health());
                    serde_json::to_value(h).unwrap_or_else(|_| serde_json::json!(null))
                })
            });
            p
        },
        system: {
            let p: api::wsstatus::Provider = Arc::new(|| {
                Box::pin(async {
                    let s = api::system::cached_system().await;
                    serde_json::to_value(s).unwrap_or_else(|_| serde_json::json!(null))
                })
            });
            p
        },
        tray: {
            let p: api::wsstatus::Provider = Arc::new(move || {
                let d = tray_src.clone();
                Box::pin(async move {
                    d.refresh();
                    d.status_json()
                })
            });
            p
        },
        services: {
            let p: api::wsstatus::Provider = Arc::new(|| {
                Box::pin(async {
                    match api::health::services_health().await {
                        Ok(axum::Json(v)) => serde_json::to_value(v).unwrap_or_else(|_| serde_json::json!([])),
                        Err(_) => serde_json::json!([]),
                    }
                })
            });
            p
        },
    };
    let ws_state = api::wsstatus::WsStatusState::new(ws_sources);
    bg_tasks.push(tokio::spawn(api::wsstatus::ticker_task(ws_state.clone())));
    let ws_routes = Router::new()
        .route("/ws/status", get(api::wsstatus::ws_status))
        .with_state(ws_state);

    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(api::health::health))
        .route("/api/boot/status", get(api::boot::status))
        .route("/api/health/services", get(api::health::services_health))
        .route("/api/health/diagnostics", get(api::diagnostics::diagnostics))
        .route("/api/diagnostics/bundle", get(api::diagbundle::bundle))
        .route("/api/services", get(api::services::services))
        .route("/api/docker", get(api::docker::docker))
        .route("/api/docker/control", post(api::dockerctl::control))
        .route("/api/docker/network", get(api::dockernet::network_status).post(api::dockernet::network_apply))
        .route("/api/docker/autostart", post(api::dockernet::autostart))
        .route("/api/sync", get(api::sync::sync))
        .merge(sync_routes)
        .route("/api/syncview/overview", get(api::syncview::overview))
        .route("/api/syncview/devices", get(api::syncview::devices))
        .route("/api/system", get(api::system::system))
        .route("/api/system/usb", post(api::system::usb))
        .route("/api/system/usb/list", get(api::system::usb_list))
        .route("/api/autostart", get(api::autostart::autostart).post(api::autostart::toggle))
        .route("/api/stats", get(api::stats::stats))
        .route("/api/flags", get(api::stats::flags).post(api::stats::flags))
        .route("/api/restart", post(api::stats::restart))
        .route("/api/term/status", get(api::term::status))
        .route("/api/term/unlock", post(api::term::unlock))
        .route("/api/term/password", post(api::term::password))
        .route("/term-ws", get(api::term::term_ws))
        .route("/vendor/:file", get(api::term::vendor))
        .route("/api/tray/status", get(api::tray::status))
        .route("/api/tray/ctl", post(api::tray::ctl))
        .route("/api/tray/exit", post(api::tray::exit))
        .route("/api/vdisp/status", get(api::vdisp::status))
        .route("/api/vdisp/ctl", post(api::vdisp::ctl))
        .merge(vdisp_routes)
        .route("/api/wsl/status", get(api::wsl::status))
        .route(
            "/api/wsl/selfcheck",
            get(api::wsl::selfcheck_status).post(api::wsl::selfcheck_start),
        )
        .route("/api/wsl/keepalive", post(api::wsl::keepalive))
        .route("/api/openlist/sync", post(api::openlist::sync))
        .route("/api/fetch/components", get(api::fetch::components))
        .route("/api/fetch/install", post(api::fetch::install))
        .route("/api/fetch/install-all", post(api::fetch::install_all))
        .route("/api/native", get(api::native::list).post(api::native::add))
        .route("/api/native/policy", get(api::native::get_policy).put(api::native::put_policy))
        .route("/api/native/:id", get(api::native::one).put(api::native::update).delete(api::native::remove))
        .route("/api/native/:id/start", post(api::native::start))
        .route("/api/native/:id/stop", post(api::native::stop))
        .route("/api/access/rules", get(api::access::rules))
        .route("/api/access/allow", post(api::access::allow).delete(api::access::remove))
        .route("/api/onboarding/syncthing-qr", get(api::onboarding::syncthing_qr))
        .route("/api/onboarding/status", get(api::onboarding::status))
        .route("/api/onboarding/plainapp", get(api::onboarding::plainapp))
        .route(
            "/api/plainapp/register",
            get(api::plainapp::register_list)
                .post(api::plainapp::register_add)
                .delete(api::plainapp::register_del),
        )
        .route("/api/plainapp/status", get(api::plainapp::status))
        .route("/gather", get(api::gather::page))
        .route("/api/gather/pair-data", get(api::gather::pair_data))
        .route("/api/gather/verify", get(api::gather::verify))
        .route("/api/pair/create", post(api::pair::create))
        .route("/api/pair/join", post(api::pair::join))
        .route("/api/pair/status", get(api::pair::status))
        .route("/api/pair/folders", post(api::pair::folders))
        .route("/api/pair/recover", get(api::pair::recover))
        .route("/api/config", get(api::cfg::get_config).put(api::cfg::put_config))
        .route("/api/config/components", get(api::components::get_config).put(api::components::put_config))
        .route("/api/config/components/default", get(api::components::default_config_handler))
        .route("/api/config/components/reset", post(api::components::reset_config))
        .route("/api/control/register", post(api::control::register))
        .route("/api/control/activate", post(api::control::activate))
        .route("/api/control/status", get(api::control::status))
        .route("/api/control/settings", post(api::control::settings))
        .route("/api/control/heartbeat", post(api::control::heartbeat_now))
        .route("/api/smb", get(api::smb::get))
        .route("/api/smb/status", get(api::smb::status))
        .route("/api/smb/apply", post(api::smb::apply))
        .route("/api/smb/drive", post(api::smb::drive))
        .route("/api/smb/unmount-all", post(api::smb::unmount_all))
        .route("/api/smb/guest", post(api::smb::guest))
        // The upload handler enforces the real streaming cap
        // ([control].upload_max_mb, default 500 MB); this route-level limit
        // only lifts axum's 2 MB DefaultBodyLimit so real uploads pass.
        .route(
            "/api/upload",
            post(api::upload::upload).layer(axum::extract::DefaultBodyLimit::max(
                2 * 1024 * 1024 * 1024,
            )),
        )
        .route("/api/upload/recent", get(api::upload::recent))
        .merge(ws_routes)
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            auth_state.clone(),
            api::auth::guard,
        ))
        .layer(middleware::from_fn(log_requests));

    // Merge: auth routes (unprotected, own state) merge into guarded app.
    let auth_routes = Router::new()
        .route("/login", get(api::auth::page))
        .route("/api/auth/login", post(api::auth::login))
        .route("/api/auth/logout", post(api::auth::logout))
        .route("/api/auth/status", get(api::auth::status))
        .with_state(auth_state);
    let app = auth_routes.merge(app);

    api::stats::init_flags();

    // Listen on the effective panel address: FENGPAGE_ADDR env >
    // [server].addr in flygo.toml > default 0.0.0.0:8388 (local + Tailscale
    // range only, firewall-tight). Restart-effective: the socket is bound
    // here once; changing it needs a restart (design doc 4.5).
    let settings = settings::get();
    let addr: SocketAddr = settings.server_addr().0.parse().unwrap();
    let port = addr.port();
    let bind_retries = settings.server_bind_retries();
    let retry_ms = settings.server_bind_retry_ms();

    // Port-conflict self-heal: bind failure may be a restart overlap.
    let mut bind_retry = 0u8;
    let listener = loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) => {
                let ead = e.raw_os_error() == Some(10048); // EADDRINUSE
                if ead && bind_retry < bind_retries {
                    bind_retry += 1;
                    eprintln!(
                        "[FlyGo] port {port} busy, retry in {retry_ms}ms ({}..)",
                        bind_retries as u32 + 1 - bind_retry as u32
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(retry_ms)).await;
                    continue;
                }
                eprintln!("[FlyGo] FATAL: cannot bind {addr}: {e}");
                eprintln!("  -> port in use? netstat -ano | findstr :{port}");
                eprintln!("  -> change port: set FENGPAGE_ADDR=0.0.0.0:<port> or edit [server].addr");
                std::process::exit(1);
            }
        }
    };

    eprintln!("[FlyGo] listening on {addr}");
    log::log("INFO", "FlyGo.boot", &format!("listening on {addr}"));
    eprintln!("[FlyGo] local   http://127.0.0.1:{port}");
    match tailnet_ip() {
        Some(ip) => eprintln!("[FlyGo] tailnet http://{ip}:{port}  (if Tailscale up)"),
        None => eprintln!("[FlyGo] tailnet: run `tailscale ip -4` to find your node IP"),
    }

    // Serve FIRST: the socket is already bound, so the panel (including
    // /api/boot/status) answers while the slow init below still runs.
    let serve_handle = tokio::spawn(async move {
        axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await
    });

    // --open: the listener is bound and the app is serving; open the panel
    // page now instead of waiting for the full slow init to finish.
    if std::env::args().any(|a| a == "--open") {
        open_panel_browser();
    }

    // Background startup task: the slow init that used to block the bind
    // (stale-child cleanup, SMB share lifecycle, default children, WSL
    // keepalive). Progress is visible at GET /api/boot/status
    // (binding -> stale_cleanup -> smb -> children -> selfcheck -> ready).
    // Every step degrades gracefully; none of them can kill the panel.
    {
        let daemon_state = daemon_state.clone();
        tokio::spawn(async move {
            api::boot::set_stage("stale_cleanup");
            let bg_t0 = std::time::Instant::now();
            let _ = tokio::task::spawn_blocking(daemon::cleanup_stale).await;
            log::log(
                "INFO",
                "FlyGo.boot",
                &format!("stale cleanup done in {}ms", bg_t0.elapsed().as_millis()),
            );
            api::boot::set_stage("smb");
            let smb_start = std::time::Instant::now();
            let _ = tokio::task::spawn_blocking(smb::open_app).await;
            log::log(
                "INFO",
                "FlyGo.boot",
                &format!("SMB lifecycle ready in {}ms", smb_start.elapsed().as_millis()),
            );
            api::boot::set_stage("children");
            for name in daemon::DEFAULT_COMPONENTS {
                match daemon_state.start(name) {
                    Ok(pid) => {
                        log::log("INFO", "FlyGo.boot", &format!("{name} started (pid {pid})"));
                        eprintln!("[daemon] {name} started (pid {pid})")
                    }
                    Err(e) => {
                        log::log("WARN", "FlyGo.boot", &format!("{name} start failed: {e}"));
                        eprintln!("[daemon] {name} start failed: {e}")
                    }
                }
            }
            api::boot::set_stage("selfcheck");
            // WSL keepalive + boot selfcheck (guard-design.md stage 1+2):
            // background threads; failures don't block startup.
            daemon::start_wsl_keepalive();
            api::boot::set_ready();
            log::log(
                "INFO",
                "FlyGo.boot",
                &format!("background startup ready in {}ms", bg_t0.elapsed().as_millis()),
            );
        });
    }

    match serve_handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[FlyGo] serve exited: {e}"),
        Err(e) => eprintln!("[FlyGo] serve task exited: {e}"),
    }

    // Shutdown: stop children, release the instance mutex, clean up the tray.
    log::log("INFO", "FlyGo.shutdown", "shutting down; stopping background children");
    eprintln!("[FlyGo] shutting down; stopping background children");
    let t0 = std::time::Instant::now();
    daemon_state.kill_all();
    log::log(
        "INFO",
        "FlyGo.shutdown",
        &format!("children stopped in {}ms", t0.elapsed().as_millis()),
    );
    // SMB share lifecycle: "stop the app, the drives disappear". Remove the
    // shares this session created/managed so a stopped panel never exposes
    // the disks. Best-effort (a removed drive share is a no-op).
    let smb_t0 = std::time::Instant::now();
    smb::close_app();
    log::log(
        "INFO",
        "FlyGo.shutdown",
        &format!("SMB close done in {}ms", smb_t0.elapsed().as_millis()),
    );
    // WSL graceful degradation: drop the keepalive session; terminate the
    // distro right away when the engine target is off (frees RAM).
    daemon::shutdown_wsl();
    // Abort the long-lived background tokio tasks (sync ticker / usb guard /
    // vdisp guard / heartbeat / ws ticker) so the #[tokio::main] runtime can
    // drop and the process actually exits. They are infinite loops; without
    // this, `main()` returning leaves the runtime waiting forever (zombie).
    for h in bg_tasks.drain(..) {
        h.abort();
    }
    daemon::release_single_instance();
    tray::request_tray_cleanup();
    std::thread::sleep(std::time::Duration::from_millis(400)); // let the tray icon go
    log::log("INFO", "FlyGo.shutdown", "[FlyGo] bye");
    eprintln!("[FlyGo] bye");
    // Force the process to terminate. All graceful cleanup (children, SMB
    // shares, keepalive session, instance mutex, tray icon) above has
    // completed; we still hard-exit here because detached std threads and the
    // tokio runtime's own worker threads (WSL keepalive, sync ticker, usb
    // guard, vdisp guard, heartbeat, ws ticker) can otherwise keep the process
    // alive forever after `main()` returns. A non-zero delay here leaves a
    // zombie "closed" panel. NOTE: this is intentionally in main()'s tail, NOT
    // the tray thread - the tray thread must never call exit() (it would race
    // the cleanup above); see tray.rs.
    std::process::exit(0);
}

/// Wait for Ctrl+C or a shutdown request (tray exit / API exit / restart).
async fn shutdown_signal() {
    let mut rx = SHUTDOWN_RX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match rx.as_mut() {
                Some(r) => { let _ = r.changed().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }
    log::log("INFO", "FlyGo.shutdown", "shutdown signal received");
    eprintln!("[FlyGo] shutdown signal received");
}

/// Respond with the embedded static/index.html.
const INDEX_HTML: &str = include_str!("../static/index.html");

/// Best-effort tailnet IPv4 address (`tailscale ip -4`, first line).
/// Device-agnostic: never assume a fixed node IP. None when Tailscale is
/// not installed / not running / not on PATH.
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

async fn index() -> impl IntoResponse {
    (
        axum::http::StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // The panel is an authenticated control surface; never let a browser
            // reuse a stale embedded bundle after a panel restart/update.
            (axum::http::header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
            (axum::http::header::PRAGMA, "no-cache"),
        ],
        INDEX_HTML,
    )
}

/// Open the panel page in the default browser (hidden helper console).
/// URL derives from the effective panel address ([server].addr / env).
fn open_panel_browser() {
    let url = settings::get().panel_browser_url();
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url.as_str()])
        .creation_flags(0x0800_0000)
        .spawn();
}

/// Shared error type for the API modules.
pub type ApiResult<T> = Result<T, ApiError>;

pub struct ApiError(pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::json!({"ok": false, "error": self.0}).to_string(),
        )
            .into_response()
    }
}
