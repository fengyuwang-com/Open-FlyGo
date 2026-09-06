// Launcher (panel switch) for the FlyGo tray panel.
// Double-click behavior: probe the panel on the effective panel address
// (FENGPAGE_ADDR env > [server].addr in flygo.toml > 127.0.0.1:8388); if it
// is down, spawn the sibling flygo.exe --open (the tray panel starts its own
// background children and lives in the system tray); poll until the panel
// accepts connections (budget from [launcher] in the same config file), then
// open the default browser.
// Standard library only; no console window is shown.
#![windows_subsystem = "windows"]

// Reuse the panel's settings module so the launcher reads the same
// flygo.toml (single source of truth for the panel port and the wait
// budget). It is std-only, so this stays dependency-light. Only the
// panel-address and launcher-section accessors are used here; the rest of
// the schema is panel-side, so dead-code is expected.
#[allow(dead_code)]
#[path = "../settings.rs"]
mod settings;

use std::net::{SocketAddr, TcpStream};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

// CREATE_NO_WINDOW: keep child console apps from flashing a black window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// True when the panel accepts TCP connections on the panel port.
fn panel_is_up(connect_timeout: Duration) -> bool {
    let addr: SocketAddr = match panel_probe_addr().parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, connect_timeout).is_ok()
}

/// Probe target: effective panel address with an unspecified host mapped to
/// 127.0.0.1 (the panel binds 0.0.0.0; probing must use a concrete address).
fn panel_probe_addr() -> String {
    let (addr, _) = settings::get().server_addr();
    let sa: SocketAddr = addr
        .parse()
        .unwrap_or_else(|_| settings::DEFAULT_SERVER_ADDR.parse().unwrap());
    let host = if sa.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        sa.ip().to_string()
    };
    format!("{host}:{}", sa.port())
}

// Start the tray panel with --open (next to this launcher first, then the
// detected install root, then FENGPAGE_PANEL_EXE). The panel itself opens
// the browser when ready.
fn start_panel() {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("flygo.exe"));
        }
    }
    if let Some(root) = detect_root() {
        candidates.push(root.join("bin").join("flygo.exe"));
    }
    if let Ok(p) = std::env::var("FENGPAGE_PANEL_EXE") {
        if !p.trim().is_empty() {
            candidates.push(PathBuf::from(p.trim()));
        }
    }
    for cand in candidates {
        if cand.is_file() {
            let _ = Command::new(cand)
                .arg("--open")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
            break;
        }
    }
}

/// Install root: FENGPAGE_ROOT > walk up from this exe looking for a
/// `webcli/` or `scripts/` sibling. Mirrors panel/src/config.rs.
fn detect_root() -> Option<PathBuf> {
    if let Ok(r) = std::env::var("FENGPAGE_ROOT") {
        if !r.trim().is_empty() {
            return Some(PathBuf::from(r.trim()));
        }
    }
    let mut cand = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for _ in 0..=3 {
        if cand.join("webcli").is_dir() || cand.join("scripts").is_dir() {
            return Some(cand);
        }
        if !cand.pop() {
            break;
        }
    }
    None
}

// Open the default browser on the panel page. The empty quoted title argument
// after `start` is required, otherwise the URL is treated as the window title.
fn open_browser() {
    let url = settings::get().panel_browser_url();
    let _ = Command::new("cmd")
        .args(["/c", "start", "", url.as_str()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

fn main() {
    // Read flygo.toml once (FENGPAGE_CONFIG > install root config/ > legacy
    // C:/FlyGoLegacy; defaults when absent) so the probe/URL/wait budget all
    // come from the same source as the panel itself.
    settings::init();
    // Cold boots can take 1-2 min (SMB share setup stalls on LanmanServer);
    // never give up before 120s even if an old config carries a lower budget.
    let wait_secs = settings::get().launcher_wait_secs().max(120);
    let poll_interval = Duration::from_secs(settings::get().launcher_poll_interval_secs());
    let connect_timeout = Duration::from_millis(settings::get().launcher_connect_timeout_ms());

    // Cold start: spawn the tray panel with --open; the panel itself opens
    // the browser once ready. Warm start: panel already running, browser opens here.
    let cold_start = !panel_is_up(connect_timeout);
    if cold_start {
        start_panel();
    }

    // Poll until the panel answers, at most the [launcher].wait_secs budget
    // (floor 120s; the panel binds its port first, so TCP accepts quickly).
    let mut attempt: u64 = 0;
    while attempt < wait_secs && !panel_is_up(connect_timeout) {
        std::thread::sleep(poll_interval);
        attempt += 1;
    }

    // Warm start only opens here -- cold start hands the browser opening to
    // the panel (via --open) so the page is not opened twice. But a cold
    // start that outlasts the budget must NOT give up silently: open the
    // browser anyway on the waiting state (the splash polls
    // /api/boot/status until the slow init finishes).
    if !cold_start {
        open_browser();
    } else if !panel_is_up(connect_timeout) {
        open_browser();
    }
}
