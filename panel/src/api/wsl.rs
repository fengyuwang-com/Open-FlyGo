// /api/wsl - WSL VM keepalive + boot selfcheck (guard-design.md stages 1+2).
//
// Stage 1 (keepalive takeover): a background thread polls every 30s and
// keeps exactly one hidden `wsl -d <managed-distro> -u root -- bash` session
// alive while the engine target is ON. The target is persisted in
// %LOCALAPPDATA%\FlyGo\state.json so a panel restart restores the previous
// intent. engine on/off in dockerctl.rs drives the target state.
//
// Stage 2 (selfcheck skeleton): six ordered steps (Tailscale service,
// portproxy rules, WSL VM state, dockerd, containers, report) run on a
// background thread at boot and on demand (/api/wsl/selfcheck). Every step
// stores ok/detail for the UI; failures never block the 8388 panel port.
//
// No-wake rule (kept from dockerctl.rs): queries only check the vmmemWSL
// process via tasklist; `wsl ...` is only invoked when the engine target is
// ON or the user explicitly asks for it.
//
// All code comments are pure ASCII; runtime strings may be UTF-8.

use crate::ApiResult;
use axum::Json;
use serde::Serialize;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::System::SystemInformation::GetLocalTime;

/// CREATE_NO_WINDOW: console children spawned from a GUI parent flash a
/// black window unless this flag is set.
use crate::winproc::CREATE_NO_WINDOW;
/// Managed WSL distro (dockerd is systemd-managed inside it).
/// pub(crate): dockerctl.rs / dockernet.rs / daemon.rs run commands in the
/// same distro. Never hardcode the name at call sites; always go through
/// distro(): explicit FENGPAGE_WSL_DISTRO / [wsl].distro wins when that name
/// is actually installed, otherwise the first `wsl -l -q` entry is used, so
/// a customer PC with a differently-named distro works with zero config.
pub(crate) const DEFAULT_DISTRO: &str = "Ubuntu-24.04";

/// Cache for the installed-distro list (it never changes at runtime;
/// re-probe at most every 5 minutes). Keeps distro() cheap for callers
/// like the 30s keepalive tick.
static DISTRO_CACHE: OnceLock<Mutex<(Vec<String>, Instant)>> = OnceLock::new();

/// Decode `wsl -l -q` stdout. wsl.exe writes UTF-16LE, usually WITHOUT a
/// BOM (BOM seen on some builds, hence both paths); fall back to lossy
/// UTF-8 for forward-compat. Pure: unit-tested.
fn decode_wsl_list(raw: &[u8]) -> Vec<String> {
    let text = if raw.starts_with(&[0xFF, 0xFE]) {
        decode_utf16le(&raw[2..])
    } else if looks_like_utf16le(raw) {
        decode_utf16le(raw)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    text.lines()
        .map(|l| l.trim().trim_matches('\0').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn decode_utf16le(raw: &[u8]) -> String {
    let u16s: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u16s)
}

/// Heuristic: ASCII-range UTF-16LE text has a zero high byte in most pairs.
/// (Checked against real `wsl -l -q` output, which carries no BOM.)
fn looks_like_utf16le(raw: &[u8]) -> bool {
    if raw.len() < 4 || !raw.len().is_multiple_of(2) {
        return false;
    }
    let mut total = 0usize;
    let mut zero_high = 0usize;
    for c in raw.chunks_exact(2).take(64) {
        total += 1;
        if c[1] == 0x00 && (c[0] == 0x0A || c[0] == 0x0D || c[0] == 0x09 || (0x20..0x7F).contains(&c[0])) {
            zero_high += 1;
        }
    }
    total > 0 && zero_high * 10 >= total * 7
}

/// Installed distro names from `wsl -l -q` (cached). `wsl -l` only reads
/// registration and never boots a distro, so this keeps the no-wake rule.
/// Empty = WSL absent or no distros installed.
fn installed_distros() -> Vec<String> {
    let cache =
        DISTRO_CACHE.get_or_init(|| Mutex::new((Vec::new(), Instant::now() - Duration::from_secs(3600))));
    if let Ok(g) = cache.lock() {
        if !g.0.is_empty() && g.1.elapsed() < Duration::from_secs(300) {
            return g.0.clone();
        }
    }
    let found = Command::new("wsl")
        .args(["-l", "-q"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| decode_wsl_list(&o.stdout))
        .unwrap_or_default();
    if let Ok(mut g) = cache.lock() {
        *g = (found.clone(), Instant::now());
    }
    found
}

pub(crate) fn distro() -> String {
    if let Ok(v) = std::env::var("FENGPAGE_WSL_DISTRO") {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    let file = crate::settings::try_get()
        .map(|s| s.wsl_distro())
        .unwrap_or_else(|| DEFAULT_DISTRO.into());
    let list = installed_distros();
    if list.is_empty() {
        return file; // WSL absent: legacy behavior, downstream errors stay explicit
    }
    if list.iter().any(|d| d == &file) {
        return file; // configured name is installed: honor it verbatim
    }
    list.into_iter().next().unwrap_or(file) // configured name absent: adapt to this machine
}
/// Poll interval of the keepalive thread (seconds).
const KEEPALIVE_POLL_SECS: u64 = 30;
/// Local state file holding the persisted engine target.
const STATE_FILE_REL: &str = r"\FlyGo\state.json";
/// docker-compose.yml of the compose project. Device-agnostic: resolved by
/// config (FENGPAGE_DOCKER_COMPOSE), same as api/docker.rs.
fn compose_file() -> Option<std::path::PathBuf> {
    crate::config::get().docker_compose.clone()
}

// ---------------- engine target state (persisted) ----------------

/// Engine target state: ON = keep WSL alive + containers resident,
/// OFF = never touch WSL. Persisted so a panel restart restores intent.
static TARGET_ON: AtomicBool = AtomicBool::new(false);
/// Set by the keepalive thread while it runs (status reporting).
static KEEPALIVE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Last keepalive pass result (served by /api/wsl/status without probing).
#[derive(Serialize, Clone)]
pub struct KeepaliveInfo {
    pub vm_running: bool,
    pub sessions: Option<usize>,
    pub last_check: String,
    pub last_action: String,
}

static LAST_TICK: Mutex<Option<KeepaliveInfo>> = Mutex::new(None);

// ---------------- selfcheck report ----------------

#[derive(Serialize, Clone)]
pub struct SelfCheckStep {
    pub id: String,
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Serialize, Clone)]
pub struct SelfCheckReport {
    pub running: bool,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub summary_ok: bool,
    pub steps: Vec<SelfCheckStep>,
}

static SELFCHECK: Mutex<Option<SelfCheckReport>> = Mutex::new(None);
/// Guards against concurrent selfcheck runs (compare-exchange at start).
static SELFCHECK_RUNNING: AtomicBool = AtomicBool::new(false);

// ---------------- target state persistence ----------------

fn state_path() -> String {
    let base = std::env::var("LOCALAPPDATA")
        .unwrap_or_else(|_| r"~/AppData/Local".to_string());
    format!("{base}{STATE_FILE_REL}")
}

/// Default network segment that may reach Docker-published ports. Tailscale
/// CGNAT 100.64.0.0/10 is the system-level default (matches access.rs).
const DEFAULT_ALLOWED_CIDR: &str = "100.64.0.0/10";

/// Persisted WSL/Docker intent. `engine_target` = keep WSL alive + dockerd
/// maintained (engine on/off). `docker_autostart` = system-level "Docker
/// engine + containers come back at panel boot". `docker_allowed_cidr` = the
/// single network segment allowed to reach every Docker-published port.
#[derive(Default, Clone)]
struct WslState {
    engine_target: bool,
    autostart: bool,
    allowed_cidr: Option<String>,
}

fn load_state() -> WslState {
    let mut s = WslState::default();
    let Ok(text) = std::fs::read_to_string(state_path()) else {
        return s;
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        s.engine_target = v
            .get("engine_target")
            .and_then(|x| x.as_str())
            .map(|s| s == "on")
            .unwrap_or(false);
        s.autostart = v
            .get("docker_autostart")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        s.allowed_cidr = v
            .get("docker_allowed_cidr")
            .and_then(|x| x.as_str())
            .map(str::to_string);
    }
    s
}

fn save_state(s: &WslState) {
    let path = state_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let value = serde_json::json!({
        "engine_target": if s.engine_target { "on" } else { "off" },
        "docker_autostart": s.autostart,
        "docker_allowed_cidr": s.allowed_cidr.clone()
            .unwrap_or_else(|| DEFAULT_ALLOWED_CIDR.to_string()),
    });
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap_or_default());
}

/// Read the persisted engine target; defaults to OFF when the file is
/// missing or unreadable (safety default: never wake WSL unexpectedly).
fn load_target() -> bool {
    load_state().engine_target
}

fn save_target(on: bool) {
    let mut s = load_state();
    s.engine_target = on;
    save_state(&s);
}

/// System-level "Docker autostart" intent (read-only getter).
pub fn autostart_enabled() -> bool {
    load_state().autostart
}

/// Persist the autostart intent. Enabling it arms the engine target at the
/// next panel boot (dockerd + containers come back automatically); disabling
/// keeps the current engine state but stops the boot-level re-arming.
pub fn set_autostart(on: bool) {
    let mut s = load_state();
    s.autostart = on;
    save_state(&s);
    eprintln!("[wsl-keepalive] docker autostart set to {on}");
}

/// The single system-wide network segment allowed to reach Docker-published
/// ports (default Tailscale 100.64.0.0/10).
pub fn allowed_cidr() -> String {
    load_state()
        .allowed_cidr
        .unwrap_or_else(|| DEFAULT_ALLOWED_CIDR.to_string())
}

pub fn set_allowed_cidr(cidr: &str) {
    let mut s = load_state();
    s.allowed_cidr = Some(cidr.trim().to_string());
    save_state(&s);
}

/// Load the persisted target once at panel boot (called by
/// daemon::start_wsl_keepalive). When system-level Docker autostart is on,
/// arm the engine target so WSL/dockerd/containers are maintained again.
pub fn init_state() {
    let on = load_target();
    TARGET_ON.store(on, Ordering::Relaxed);
    let autostart = autostart_enabled();
    if autostart && !on {
        eprintln!("[wsl-keepalive] docker autostart on -> arming engine target at boot");
        set_target(true);
    }
    eprintln!(
        "[wsl-keepalive] engine target = {} autostart = {autostart} (from {})",
        if TARGET_ON.load(Ordering::Relaxed) { "on" } else { "off" },
        state_path()
    );
}

/// Current engine target state.
pub fn target_on() -> bool {
    TARGET_ON.load(Ordering::Relaxed)
}

/// Set + persist the engine target state.
pub fn set_target(on: bool) {
    TARGET_ON.store(on, Ordering::Relaxed);
    save_target(on);
    eprintln!(
        "[wsl-keepalive] engine target set to {}",
        if on { "on" } else { "off" }
    );
}

// ---------------- keepalive session helpers ----------------

/// wsl.exe client processes whose command line references the distro.
/// The keepalive session (`wsl -d <distro> -u root -- bash`) matches;
/// plain interactive `wsl` (no -d) does not. PowerShell probe is read-only.
fn keepalive_session_pids() -> Vec<u32> {
    let query = format!("Get-CimInstance Win32_Process -Filter \"Name = 'wsl.exe'\" | Where-Object {{ $_.CommandLine -and $_.CommandLine -match '{distro}' }} | Select-Object -ExpandProperty ProcessId", distro = distro().replace('\'', "''"));
    ps_query(&query)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// Count of keepalive sessions. None = the probe itself failed (treated as
/// unknown; never a reason to spawn an extra session).
pub fn keepalive_session_count() -> Option<usize> {
    let query = format!("Get-CimInstance Win32_Process -Filter \"Name = 'wsl.exe'\" | Where-Object {{ $_.CommandLine -and $_.CommandLine -match '{distro}' }} | Select-Object -ExpandProperty ProcessId", distro = distro().replace('\'', "''"));
    let out = ps_query(&query);
    if out.starts_with("error:") {
        return None;
    }
    Some(
        out.lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .count(),
    )
}

/// Spawn one hidden keepalive session. The wsl.exe client stays attached to
/// the bash session, which keeps the distro session (and thus the VM) alive.
/// The process is detached on purpose: the panel does not wait for it.
fn spawn_keepalive_session() -> Result<u32, String> {
    let d = distro();
    let child = Command::new("wsl")
        .args(["-d", &d, "-u", "root", "--", "bash"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("spawn keepalive session failed: {e}"))?;
    Ok(child.id())
}

/// Spawn under the global dockerctl LOCK with a re-check, so the keepalive
/// thread and the boot selfcheck can never double-spawn a session.
fn spawn_keepalive_session_locked() -> Result<u32, String> {
    let _guard = crate::api::dockerctl::LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if keepalive_session_count().unwrap_or(0) > 0 {
        return Err("keepalive session already present".to_string());
    }
    spawn_keepalive_session()
}

/// Kill every keepalive session (taskkill tree+force). Returns the number
/// of processes actually killed. Used by engine off and panel exit.
pub fn kill_keepalive_sessions() -> usize {
    let pids = keepalive_session_pids();
    let mut killed = 0usize;
    for pid in pids {
        let out = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        if out.map(|o| o.status.success()).unwrap_or(false) {
            killed += 1;
        }
    }
    killed
}

/// `wsl --terminate <distro>`: tears the VM down and frees RAM. No-op when
/// the distro is already stopped.
fn wsl_terminate() -> bool {
    let d = distro();
    Command::new("wsl")
        .args(["--terminate", &d])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------- keepalive thread ----------------

/// One keepalive pass: re-check vmmemWSL + session count and pull a session
/// when the target is ON and none exists. Target OFF never touches WSL.
fn keepalive_tick() {
    let target = target_on();
    let vm = crate::api::dockerctl::wsl_vm_running();
    let sessions = keepalive_session_count();
    let action = if !target {
        "target off; WSL untouched".to_string()
    } else {
        match sessions {
            Some(0) => match spawn_keepalive_session_locked() {
                Ok(pid) => format!("spawned keepalive session (pid {pid})"),
                Err(e) => format!("keepalive spawn skipped: {e}"),
            },
            Some(n) => format!("{n} keepalive session(s) present"),
            None => "session probe failed; skipping this cycle".to_string(),
        }
    };
    *LAST_TICK.lock().unwrap_or_else(|e| e.into_inner()) = Some(KeepaliveInfo {
        vm_running: vm,
        sessions,
        last_check: now_string(),
        last_action: action.clone(),
    });
    if !action.contains("present") && !action.starts_with("target off") {
        eprintln!("[wsl-keepalive] tick: vm={vm} sessions={sessions:?} -> {action}");
    }
}

/// Set by `on_panel_exit` to tell `keepalive_loop` to stop polling. The
/// loop checks it every ~1s, so the keepalive thread (a detached std thread)
/// drains and lets the process exit cleanly instead of pinning it alive
/// forever after `main()` returns.
static KEEPALIVE_STOP: AtomicBool = AtomicBool::new(false);

/// Background keepalive loop (spawned once at panel boot). Runs an
/// immediate first pass so /api/wsl/status is fresh, then polls every
/// KEEPALIVE_POLL_SECS. Checks `KEEPALIVE_STOP` each second so a panel exit
/// can break the loop promptly - a bare `loop {}` here would keep the process
/// running as a zombie even after `[FlyGo] bye`.
pub fn keepalive_loop() {
    // P0-1: never reset the stop flag when a shutdown was already requested.
    // The tail of main sets KEEPALIVE_STOP via on_panel_exit(); a late boot
    // task racing the shutdown tail could otherwise spawn this thread after
    // that and clear the flag, resurrecting the poller while the process is
    // trying to exit.
    if KEEPALIVE_STOP.load(Ordering::Acquire) || crate::shutdown_requested() {
        return;
    }
    KEEPALIVE_ACTIVE.store(true, Ordering::Release);
    eprintln!("[wsl-keepalive] thread started (poll every {KEEPALIVE_POLL_SECS}s)");
    keepalive_tick();
    loop {
        for _ in 0..KEEPALIVE_POLL_SECS {
            if KEEPALIVE_STOP.load(Ordering::Acquire) || crate::shutdown_requested() {
                eprintln!("[wsl-keepalive] stop flag set; thread exiting");
                KEEPALIVE_ACTIVE.store(false, Ordering::Release);
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        keepalive_tick();
    }
}

/// Run one keepalive pass immediately (used after engine on so the session
/// comes up without waiting for the next poll).
pub fn kick_keepalive() {
    keepalive_tick();
}

/// Graceful degradation on panel exit: drop the keepalive session so the VM
/// falls back to vmIdleTimeout; terminate the distro right away when the
/// engine target is OFF (frees RAM immediately).
pub fn on_panel_exit() {
    // Tell the keepalive thread to stop polling so it can drain and let the
    // process exit (it is a detached std thread that would otherwise pin the
    // process alive forever).
    KEEPALIVE_STOP.store(true, Ordering::Release);
    KEEPALIVE_ACTIVE.store(false, Ordering::Release);
    let killed = kill_keepalive_sessions();
    eprintln!("[wsl-keepalive] panel exit: dropped {killed} keepalive session(s)");
    if !target_on() {
        let _ = wsl_terminate();
        eprintln!("[wsl-keepalive] panel exit: engine target off, wsl --terminate issued");
    }
}

// ---------------- selfcheck (six steps, background) ----------------

/// Start the selfcheck on a background thread unless one is already
/// running. Returns false when a run is in progress.
pub fn start_selfcheck() -> bool {
    if SELFCHECK_RUNNING.swap(true, Ordering::SeqCst) {
        return false;
    }
    std::thread::spawn(run_selfcheck);
    true
}

fn run_selfcheck() {
    let mut report = SelfCheckReport {
        running: true,
        started_at: now_string(),
        finished_at: None,
        summary_ok: false,
        steps: Vec::new(),
    };
    type Step = (&'static str, &'static str, fn() -> (bool, String));
    let steps: [Step; 7] = [
        ("tailscale", "Tailscale service", step_tailscale),
        ("portproxy", "portproxy rules", step_portproxy),
        ("tcpforward", "TCP forwarders", step_tcpforward),
        ("wsl", "WSL VM state", step_wsl_vm),
        ("dockerd", "dockerd", step_dockerd),
        ("containers", "containers", step_containers),
        ("report", "result report", step_report),
    ];
    for (id, name, f) in steps {
        let (ok, detail) = f();
        report.steps.push(SelfCheckStep {
            id: id.to_string(),
            name: name.to_string(),
            ok,
            detail: detail.clone(),
        });
        *SELFCHECK.lock().unwrap_or_else(|e| e.into_inner()) = Some(report.clone());
        eprintln!(
            "[wsl-selfcheck] {name}: {} {detail}",
            if ok { "OK" } else { "FAIL" }
        );
    }
    report.running = false;
    report.finished_at = Some(now_string());
    report.summary_ok = report.steps.iter().all(|s| s.ok);
    *SELFCHECK.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
    eprintln!("[wsl-selfcheck] done");
    SELFCHECK_RUNNING.store(false, Ordering::SeqCst);
}

/// Step 1: Tailscale service. `sc query` -> `net start` when not RUNNING,
/// verified via `tailscale ip -4` (100.x address). net start needs
/// elevation; a non-elevated panel reports the failure honestly.
fn step_tailscale() -> (bool, String) {
    let svc_state = |s: &str| {
        s.lines()
            .any(|l| l.trim().starts_with("STATE") && l.contains("RUNNING"))
    };
    let mut svc = run_capture("sc", &["query", "Tailscale"], 15);
    if !svc_state(&svc) {
        let _ = run_capture("net", &["start", "Tailscale"], 30);
        svc = run_capture("sc", &["query", "Tailscale"], 15);
    }
    if !svc_state(&svc) {
        return (
            false,
            "Tailscale service not RUNNING (net start failed; run the panel elevated?)".to_string(),
        );
    }
    match tailnet_ip() {
        Some(ip) => (true, format!("Tailscale service RUNNING, tailnet IP {ip}")),
        None => (
            false,
            "Tailscale service RUNNING but `tailscale ip -4` returned nothing".to_string(),
        ),
    }
}

/// Step 2: portproxy. Compare `netsh interface portproxy show v4tov4`
/// against the expected table derived from docker-compose.yml host ports
/// (same source apply-portproxy.ps1 uses). Missing TCP rules are re-added;
/// without elevation the add fails and the step reports the gap.
fn step_portproxy() -> (bool, String) {
    let ports = compose_host_ports();
    if ports.is_empty() {
        return (
            true,
            "no TCP host ports in docker-compose.yml; nothing to proxy".to_string(),
        );
    }
    match tailnet_ip() {
        None => (
            false,
            "tailscale ip -4 unavailable; cannot verify portproxy".to_string(),
        ),
        Some(ip) => ensure_portproxy(&ports, &ip),
    }
}

/// Which of `ports` lack a matching portproxy entry on `ip` (tailnet IP).
/// Pure read: `netsh show` only, never adds anything. Shared by
/// [ensure_portproxy] and dockernet.rs status.
pub(crate) fn missing_portproxy(ports: &[u16], ip: &str) -> Vec<u16> {
    let show = run_capture("netsh", &["interface", "portproxy", "show", "v4tov4"], 15);
    ports
        .iter()
        .copied()
        .filter(|p| {
            !show.lines().any(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() == 4
                    && f[0] == ip
                    && f[1].parse::<u16>().map(|x| x == *p).unwrap_or(false)
                    && f[2] == "127.0.0.1"
                    && f[3].parse::<u16>().map(|x| x == *p).unwrap_or(false)
            })
        })
        .collect()
}

/// Ensure TCP portproxy rules exist for every given host port on `ip`
/// (tailnet IP): missing rules are re-added. Non-elevated adds silently fail
/// and are reported. Returns (ok, detail text).
pub(crate) fn ensure_portproxy(ports: &[u16], ip: &str) -> (bool, String) {
    if ports.is_empty() {
        return (
            true,
            "no TCP host ports in docker-compose.yml; nothing to proxy".to_string(),
        );
    }
    let missing = missing_portproxy(ports, ip);
    if missing.is_empty() {
        return (
            true,
            format!("all {} portproxy rule(s) present on {ip}", ports.len()),
        );
    }
    let mut added: Vec<u16> = Vec::new();
    let mut failed: Vec<u16> = Vec::new();
    for p in &missing {
        let out = Command::new("netsh")
            .args([
                "interface",
                "portproxy",
                "add",
                "v4tov4",
                &format!("listenaddress={ip}"),
                &format!("listenport={p}"),
                "connectaddress=127.0.0.1",
                &format!("connectport={p}"),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        if out.map(|o| o.status.success()).unwrap_or(false) {
            added.push(*p);
        } else {
            failed.push(*p);
        }
    }
    if failed.is_empty() {
        (true, format!("added missing portproxy rule(s): {}", join_ports(&added)))
    } else {
        (
            false,
            format!(
                "missing portproxy rule(s) need elevation: {} (added: {})",
                join_ports(&failed),
                join_ports(&added)
            ),
        )
    }
}

/// Step 3: TCP forwarders. The panel itself listens on each compose host
/// port (tailnet IP + LAN IP -> 127.0.0.1), the only path that survives
/// DERP relay (portproxy hooks miss relayed packets, serve is DNS-only).
/// Read-only here: the forward_task background loop owns reconcile/heal.
fn step_tcpforward() -> (bool, String) {
    crate::api::tcpfwd::status_for_selfcheck()
}

/// Step 4: WSL VM state. vmmemWSL check only (never wakes). Target ON with
/// the VM stopped pulls one keepalive session (which boots the VM); target
/// OFF keeps it stopped.
fn step_wsl_vm() -> (bool, String) {
    if crate::api::dockerctl::wsl_vm_running() {
        return (true, "VM running (vmmemWSL present)".to_string());
    }
    if !target_on() {
        return (true, "VM stopped; target off, kept stopped".to_string());
    }
    match spawn_keepalive_session_locked() {
        Ok(pid) => (
            true,
            format!("VM stopped; keepalive session spawned (pid {pid}), VM booting"),
        ),
        Err(e) => (false, format!("VM stopped; keepalive spawn failed: {e}")),
    }
}

/// Step 5: dockerd. Only when the VM is up: `systemctl is-active docker`;
/// not active with target ON -> `docker-ctl engine on` (systemctl start,
/// 120s budget via the shared dockerctl LOCK + timeout-kill-tree).
fn step_dockerd() -> (bool, String) {
    if !wait_vm_up(15) {
        return (true, "skipped (WSL VM not up)".to_string());
    }
    let out = wsl_capture_timeout(&["systemctl", "is-active", "docker"], 30);
    if out.trim() == "active" {
        return (true, "docker service active".to_string());
    }
    if !target_on() {
        return (
            true,
            format!("docker not active ({out:?}); target off, left stopped"),
        );
    }
    let msg = crate::api::dockerctl::run_ctl_capture_to(
        &["engine", "on"],
        crate::api::dockerctl::ACTION_TIMEOUT_SECS,
    );
    if ok_msg(&msg) {
        (true, format!("docker service started: {msg}"))
    } else {
        (false, format!("docker start failed: {msg}"))
    }
}

/// Step 6: containers. `docker ps` (docker-ctl status) compared against the
/// compose project; missing/exited with target ON -> `docker-ctl compose on`
/// (compose up -d, 120s budget).
fn step_containers() -> (bool, String) {
    if !crate::api::dockerctl::wsl_vm_running() {
        return (true, "skipped (WSL VM not running)".to_string());
    }
    let expected = crate::api::docker::compose_list();
    let running = crate::api::docker::running_containers();
    let missing: Vec<String> = expected
        .iter()
        .filter(|s| !running.contains(&s.key) && !running.contains(&s.ctn))
        .map(|s| s.key.clone())
        .collect();
    if missing.is_empty() {
        return (true, format!("all {} compose service(s) up", expected.len()));
    }
    if !target_on() {
        return (
            false,
            format!(
                "{} container(s) down, target off (left as-is): {}",
                missing.len(),
                missing.join(",")
            ),
        );
    }
    let msg = crate::api::dockerctl::run_ctl_capture_to(
        &["compose", "on"],
        crate::api::dockerctl::ACTION_TIMEOUT_SECS,
    );
    if ok_msg(&msg) {
        (
            true,
            format!(
                "compose up -d triggered for {} missing: {}",
                missing.len(),
                missing.join(",")
            ),
        )
    } else {
        (false, format!("compose up failed: {msg}"))
    }
}

/// Step 7: result report. Summarizes the six preceding steps.
fn step_report() -> (bool, String) {
    let report = SELFCHECK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let steps = report.map(|r| r.steps).unwrap_or_default();
    let ok_count = steps.iter().filter(|s| s.ok).count();
    (
        ok_count == steps.len() && !steps.is_empty(),
        format!("{ok_count}/{} steps passed", steps.len()),
    )
}

// ---------------- API handlers ----------------

/// GET /api/wsl/status - keepalive + selfcheck status for the WSL card.
pub async fn status() -> ApiResult<Json<serde_json::Value>> {
    let tick = LAST_TICK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let selfcheck = SELFCHECK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Json(serde_json::json!({
        "ok": true,
        "target_on": target_on(),
        "keepalive_active": KEEPALIVE_ACTIVE.load(Ordering::Relaxed),
        "vm_running": crate::api::dockerctl::wsl_vm_running(),
        "keepalive_sessions": tick.as_ref().and_then(|t| t.sessions),
        "last_check": tick.as_ref().map(|t| t.last_check.clone()).unwrap_or_default(),
        "last_action": tick.as_ref().map(|t| t.last_action.clone()).unwrap_or_default(),
        "state_file": state_path(),
        "selfcheck": selfcheck,
    })))
}

/// GET /api/wsl/selfcheck - current report (null until first run).
pub async fn selfcheck_status() -> ApiResult<Json<serde_json::Value>> {
    let report = SELFCHECK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Json(serde_json::json!({
        "ok": true,
        "running": SELFCHECK_RUNNING.load(Ordering::SeqCst),
        "report": report,
    })))
}

/// POST /api/wsl/selfcheck - start a run in the background.
pub async fn selfcheck_start() -> ApiResult<Json<serde_json::Value>> {
    if start_selfcheck() {
        Ok(Json(serde_json::json!({
            "ok": true,
            "started": true,
            "message": "selfcheck started (background thread)",
        })))
    } else {
        Ok(Json(serde_json::json!({
            "ok": true,
            "started": false,
            "message": "selfcheck already running",
        })))
    }
}

/// POST /api/wsl/keepalive  body: {"enabled": true|false}
/// Toggles the engine target state: enabled=true arms the keepalive (and
/// pulls a session immediately); enabled=false stops maintenance and drops
/// existing keepalive sessions. Engine on/off via /api/docker/control is the
/// canonical path (it also starts/terminates dockerd/WSL); this endpoint is
/// the fine-grained keepalive lever for the UI.
pub async fn keepalive(body: axum::Json<serde_json::Value>) -> ApiResult<Json<serde_json::Value>> {
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    set_target(enabled);
    if enabled {
        kick_keepalive();
    } else {
        let killed = kill_keepalive_sessions();
        eprintln!("[wsl-keepalive] keepalive disabled via API: killed {killed} session(s)");
        kick_keepalive();
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "enabled": enabled,
        "sessions": keepalive_session_count(),
    })))
}

// ---------------- helpers ----------------

/// Spawn a hidden process, capture combined output, kill the whole tree on
/// timeout (same pattern as dockerctl.rs::run_ctl_capture_to; pure std).
fn capture_child(mut child: std::process::Child, timeout_secs: u64) -> String {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel::<(String, String)>();
    std::thread::spawn(move || {
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out);
        }
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut err);
        }
        let _ = tx.send((out, err));
    });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let pid = child.id();
                    let _ = child.kill();
                    let _ = child.wait();
                    // Kill the tree: wsl.exe descendants must go too.
                    let _ = Command::new("taskkill")
                        .args(["/pid", &pid.to_string(), "/t", "/f"])
                        .creation_flags(CREATE_NO_WINDOW)
                        .output();
                    return format!("error: command timed out after {timeout_secs}s (tree killed)");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return "error: waiting for child failed".to_string(),
        }
    }
    let (out, err) = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    let stdout = out.trim().to_string();
    let stderr = err.trim().to_string();
    if stdout.is_empty() && stderr.is_empty() {
        return "error: no output (timed out or failed to start)".to_string();
    }
    if stdout.is_empty() {
        stderr
    } else {
        stdout
    }
}

/// Hidden host command with output capture + timeout (no LOCK: host tools
/// like sc/netsh/tailscale do not touch the WSL serialization domain).
fn run_capture(program: &str, args: &[&str], timeout_secs: u64) -> String {
    match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => capture_child(c, timeout_secs),
        Err(e) => format!("error: spawn {program} failed: {e}"),
    }
}

/// Hidden wsl command inside the distro, serialized under the global
/// dockerctl LOCK with the same timeout-kill-tree behavior.
fn wsl_capture_timeout(inner: &[&str], timeout_secs: u64) -> String {
    let _guard = crate::api::dockerctl::LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let d = distro();
    match Command::new("wsl")
        .args(["-d", &d, "-u", "root", "--"])
        .args(inner)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => capture_child(c, timeout_secs),
        Err(e) => format!("error: wsl spawn failed: {e}"),
    }
}

/// Hidden PowerShell query with output capture (10s timeout).
fn ps_query(script: &str) -> String {
    match Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => capture_child(c, 10),
        Err(e) => format!("error: powershell spawn failed: {e}"),
    }
}

/// Best-effort tailnet IPv4 (`tailscale ip -4`, first non-empty line).
pub(crate) fn tailnet_ip() -> Option<String> {
    let out = Command::new("tailscale")
        .args(["ip", "-4"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
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

/// Poll vmmemWSL until the VM is up (used after the selfcheck wakes it).
fn wait_vm_up(max_secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    while Instant::now() < deadline {
        if crate::api::dockerctl::wsl_vm_running() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    crate::api::dockerctl::wsl_vm_running()
}

/// Success check matching dockerctl.rs: message must not carry an error
/// marker and must not be empty.
fn ok_msg(msg: &str) -> bool {
    !msg.to_lowercase().contains("error") && !msg.is_empty()
}

fn join_ports(ports: &[u16]) -> String {
    ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// 通用端口全集：compose 声明的宿主端口 ∪ Docker 实际发布的宿主端口。
/// 新容器只要发布了端口（compose 或 docker run -p），下一轮调和自动接入，
/// 无需改 FlyGo 任何配置。UDP 跳过（转发层 TCP-only，与 portproxy 同限）。
/// pub(crate)：tcpfwd 转发层与 dockernet 规则组同源消费。
pub(crate) fn all_forward_ports() -> Vec<u16> {
    let mut ports = compose_host_ports();
    for p in docker_published_ports() {
        if !ports.contains(&p) {
            ports.push(p);
        }
    }
    ports.sort_unstable();
    ports
}

/// Docker 实际发布的 TCP 宿主端口（`docker ps --format {{.Ports}}`），
///
/// 覆盖 compose 之外的容器（如 docker run -p）。只认已发布项
/// （`HOST->CONTAINER/tcp`），纯 EXPOSE（无 `->`）与 /udp 跳过。
/// 非阻塞：自检/引擎启停占着 dockerctl::LOCK 时直接用缓存顶一轮
/// （读操作与启停并发无害，失败只返回空，compose 源照常工作）；
/// 为避死锁此处不走 wsl_capture_timeout（它内部会再加同一把锁）。
pub(crate) fn docker_published_ports() -> Vec<u16> {
    static PS_CACHE: Mutex<(Option<Instant>, Vec<u16>)> = Mutex::new((None, Vec::new()));
    let _probe = match crate::api::dockerctl::LOCK.try_lock() {
        Ok(g) => g,
        Err(_) => {
            return PS_CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .1
                .clone()
        }
    };
    let d = distro();
    let spawned = Command::new("wsl")
        .args(["-d", &d, "-u", "root", "--", "docker", "ps", "--format", "{{.Ports}}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let out = match spawned {
        Ok(c) => capture_child(c, 30),
        Err(_) => String::new(),
    };
    drop(_probe);
    let ports = parse_docker_published_ports(&out);
    *PS_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = (Some(Instant::now()), ports.clone());
    ports
}

/// 解析 `docker ps --format {{.Ports}}` 输出（纯字符串逻辑，可单测）：
/// `0.0.0.0:5244->5244/tcp, :::5244->5244/tcp` -> [5244]。
fn parse_docker_published_ports(out: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::new();
    for line in out.lines() {
        for tok in line.split(',') {
            let tok = tok.trim().trim_matches(|c| c == '[' || c == ']');
            let Some(arrow) = tok.find("->") else {
                continue;
            };
            let (left, right) = tok.split_at(arrow);
            if right.contains("/udp") || right.contains("/UDP") {
                continue;
            }
            let host = left.rsplit(':').next().unwrap_or("").trim();
            if host.is_empty() || !host.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Ok(p) = host.parse::<u16>() {
                if p > 0 && !ports.contains(&p) {
                    ports.push(p);
                }
            }
        }
    }
    ports.sort_unstable();
    ports
}
/// Expected portproxy listen ports: host-side ports parsed from
/// docker-compose.yml (`- "HOST:CONTAINER"` / `- "HOST:CONTAINER/udp"`),
/// same source apply-portproxy.ps1 uses. UDP entries are skipped (Windows
/// portproxy is TCP-only). pub(crate): dockernet.rs builds the one system-level
/// rule set from the same source.
pub(crate) fn compose_host_ports() -> Vec<u16> {
    let contents = match compose_file() {
        Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
        None => String::new(),
    };
    parse_host_ports(&contents)
}

/// Parse host-side listen ports from compose port lines (see
/// compose_host_ports for the line format). Pure string logic, unit-tested.
fn parse_host_ports(contents: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::new();
    for line in contents.lines() {
        let t = line.trim();
        if !t.starts_with('-') {
            continue;
        }
        let inner = t
            .trim_start_matches('-')
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        let Some(colon) = inner.find(':') else {
            continue;
        };
        let (host, rest) = inner.split_at(colon);
        let rest = &rest[1..];
        if host.is_empty() || !host.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cont = rest.split('/').next().unwrap_or("");
        if cont.is_empty() || !cont.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if rest.contains("/udp") || rest.contains("/UDP") {
            continue;
        }
        if let Ok(p) = host.parse::<u16>() {
            if p > 0 && !ports.contains(&p) {
                ports.push(p);
            }
        }
    }
    ports
}

/// Local time as "YYYY-MM-DD HH:MM:SS" (via GetLocalTime).
fn now_string() -> String {
    unsafe {
        let st = GetLocalTime();
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests below temporarily repoint the process-global LOCALAPPDATA env var
    /// at throwaway dirs; cargo runs tests on parallel threads, so they must
    /// be serialized against each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn sample_compose() -> &'static str {
        "services:\n\
         \x20 alist:\n\
         \x20\x20 ports:\n\
         \x20\x20\x20 - \"5244:5244\"\n\
         \x20\x20\x20 - \"5244:5244/udp\"\n\
         \x20\x20\x20 - \"5244:5244\"\n\
         \x20 metube:\n\
         \x20\x20 ports:\n\
         \x20\x20\x20 - 8080-8085:80\n\
         \x20\x20\x20 - \"9443:9443\"\n\
         \x20 web:\n\
         \x20\x20 ports:\n\
         \x20\x20\x20 - \"5005:80\"\n\
         \x20\x20\x20 - \"0:80\"\n\
         \x20 filebrowser:\n\
         \x20\x20 volumes:\n\
         \x20\x20\x20 - ./data:/data\n"
    }

    /// Deterministic parser check: quoted/unquoted ports, UDP skip, range
    /// skip, volume skip, host-port-0 skip, duplicate dedup.
    #[test]
    fn parse_host_ports_filters_and_dedups() {
        assert_eq!(parse_host_ports(sample_compose()), vec![5244, 9443, 5005]);
        assert!(parse_host_ports("").is_empty());
        assert!(parse_host_ports("volumes:\n  - ./a:/b\n").is_empty());
    }

    /// docker ps Ports 解析：已发布 TCP 收录（v4/v6 去重），纯 EXPOSE、
    /// /udp、空行跳过；error 文本零收录（WSL 未起时不炸）。
    #[test]
    fn parse_docker_published_ports_filters_and_dedups() {
        let out = "0.0.0.0:5244->5244/tcp, :::5244->5244/tcp\n\
                   0.0.0.0:8096->8096/tcp\n\
                   8080/tcp\n\
                   0.0.0.0:53->53/udp\n\
                   \n\
                   192.168.1.1:9000->9000/tcp, [::]:9000->9000/tcp\n";
        assert_eq!(
            parse_docker_published_ports(out),
            vec![5244, 8096, 9000]
        );
        assert!(parse_docker_published_ports("").is_empty());
        assert!(parse_docker_published_ports("error: wsl spawn failed: x\n").is_empty());
    }

    /// Engine-target / autostart / allowed-cidr persistence roundtrip against
    /// a throwaway LOCALAPPDATA (no real state file is touched).
    #[test]
    fn state_roundtrip_uses_localappdata() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hermes-verify-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = std::env::var("LOCALAPPDATA").ok();
        std::env::set_var("LOCALAPPDATA", &dir);
        set_target(true);
        assert!(load_target());
        set_target(false);
        assert!(!load_target());
        assert!(dir.join(r"FlyGo\state.json").exists(), "state file must be written");
        match old {
            Some(v) => std::env::set_var("LOCALAPPDATA", v),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Autostart + allowed-CIDR must survive a save/load roundtrip and not
    /// clobber the engine target (the panel writes all three keys together).
    #[test]
    fn autostart_and_cidr_roundtrip() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("flygo-act-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = std::env::var("LOCALAPPDATA").ok();
        std::env::set_var("LOCALAPPDATA", &dir);

        set_target(true);
        set_autostart(true);
        set_allowed_cidr("10.0.0.0/8");

        assert!(autostart_enabled(), "autostart flag must persist");
        assert_eq!(allowed_cidr(), "10.0.0.0/8");
        assert!(load_target(), "engine target must survive autostart write");

        set_target(false);
        assert!(
            autostart_enabled(),
            "toggling engine target must not clear autostart"
        );
        set_autostart(false);
        assert!(!autostart_enabled());

        match old {
            Some(v) => std::env::set_var("LOCALAPPDATA", v),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Default allowed CIDR = Tailscale 100.64.0.0/10 on a fresh state.
    #[test]
    fn allowed_cidr_defaults_to_tailscale_cgnat() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("flygo-ac-dflt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = std::env::var("LOCALAPPDATA").ok();
        std::env::set_var("LOCALAPPDATA", &dir);
        let _ = std::fs::remove_file(dir.join(r"FlyGo\state.json"));
        assert_eq!(allowed_cidr(), "100.64.0.0/10");
        match old {
            Some(v) => std::env::set_var("LOCALAPPDATA", v),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- live host probes (ad-hoc verification, not unit determinism) ----
    // These execute the exact commands the keepalive/selfcheck paths run.

    /// The keepalive session probe must not error on a healthy host.
    #[test]
    fn keepalive_session_probe_live() {
        assert!(
            keepalive_session_count().is_some(),
            "PowerShell WMI probe must return a count, not an error"
        );
    }

    /// `sc query Tailscale` output must match step_tailscale's parse shape.
    #[test]
    fn tailscale_sc_query_live() {
        let out = run_capture("sc", &["query", "Tailscale"], 15);
        assert!(out.contains("SERVICE_NAME"), "sc query must return a service block");
        let state_line = out.lines().find(|l| l.trim().starts_with("STATE"));
        assert!(state_line.is_some(), "sc query must include a STATE line");
        let running = out
            .lines()
            .any(|l| l.trim().starts_with("STATE") && l.contains("RUNNING"));
        eprintln!("[verify] Tailscale STATE line: {state_line:?} (running={running})");
    }

    /// Real `wsl -l -q` bytes: UTF-16LE with NO BOM (captured 2026-09-06).
    #[test]
    fn decode_wsl_list_utf16le_no_bom() {
        let raw: Vec<u8> = "docker-desktop\r\n\r\nUbuntu-24.04\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert!(!raw.starts_with(&[0xFF, 0xFE]));
        assert_eq!(decode_wsl_list(&raw), vec!["docker-desktop", "Ubuntu-24.04"]);
    }

    /// BOM variant (seen on some wsl.exe builds).
    #[test]
    fn decode_wsl_list_utf16le_bom() {
        let s = "\u{FEFF}Ubuntu-24.04\r\nDebian\r\n";
        let raw: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode_wsl_list(&raw), vec!["Ubuntu-24.04", "Debian"]);
    }

    #[test]
    fn decode_wsl_list_utf8_fallback_and_empty() {
        assert_eq!(decode_wsl_list(b"Ubuntu\r\n\r\n"), vec!["Ubuntu"]);
        assert!(decode_wsl_list(b"").is_empty());
        assert!(decode_wsl_list(b"\r\n").is_empty());
    }

    /// `netsh portproxy show` must yield the 4-token data rows that
    /// step_portproxy compares. Headers may be localized (Chinese here);
    /// the production parse only reads locale-independent data rows.
    #[test]
    fn portproxy_show_live() {
        let show = run_capture("netsh", &["interface", "portproxy", "show", "v4tov4"], 15);
        assert!(!show.starts_with("error:"), "netsh show must run: {show}");
        let rows = show
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .filter(|f| {
                f.len() == 4
                    && f[1].parse::<u16>().is_ok()
                    && f[3].parse::<u16>().is_ok()
            })
            .count();
        assert!(rows > 0, "expected portproxy data rows, got:\n{show}");
        eprintln!("[verify] portproxy data rows: {rows}");
    }
}
