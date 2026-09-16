// daemon.rs - tray-managed background child processes + sync timer.
//
// Owns three NAS children (webcli, syncthing, homepage) that the tray panel
// starts at boot and can start/stop individually, plus a periodic mirror-sync
// timer that runs a PowerShell script every 10 minutes.
//
// Also provides the single-instance mutex and the startup cleanup that kills
// stale listeners on the managed ports (guarded by command-line matching so
// unrelated processes are never touched).
//
// All code comments are pure ASCII; runtime strings may be UTF-8.

use std::collections::HashMap;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::Storage::FileSystem::GetLogicalDrives;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateMutexW, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use std::path::{Path, PathBuf};

use crate::log::{log_error, log_info, log_warn};

/// Built-in components started automatically at panel boot. The full set of
/// manageable names is dynamic (built-ins webcli/syncthing/homepage plus
/// api::native "native:<id>" shortcuts); kill_all iterates the recorded
/// children map instead of a fixed list.
pub const DEFAULT_COMPONENTS: [&str; 1] = ["webcli"];

/// Managed TCP ports per component (single source: [services].ports in the
/// config file, defaults 8788/8384/9199). Used by the stale-listener cleanup
/// and the no-pid kill path.
fn port_of(name: &str) -> String {
    let (webcli, syncthing, homepage) = crate::settings::get().services_ports();
    match name {
        "webcli" => webcli.to_string(),
        "syncthing" => syncthing.to_string(),
        "homepage" => homepage.to_string(),
        _ if name.starts_with(crate::api::native::DAEMON_PREFIX) => {
            // N-01 no-pid kill path: the shortcut's own probe port.
            crate::api::native::shortcut_port(&name[crate::api::native::DAEMON_PREFIX.len()..])
                .map(|p| p.to_string())
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

use crate::winproc::CREATE_NO_WINDOW;
/// Exit code of a live process (GetExitCodeProcess).
const STILL_ACTIVE: u32 = 259;
/// Interval between external-drive presence polls (seconds); the mirror-sync
/// interval lives in [sync].interval_secs (settings.rs).
const USB_POLL_SECS: u64 = 15;
/// Named mutex for the single-instance guard.
const INSTANCE_MUTEX_NAME: &str = "FlyGoTray";

/// One managed child: whether the daemon believes it runs and its pid.
#[derive(Clone, Copy, Serialize)]
pub struct ChildStatus {
    pub running: bool,
    pub pid: u32,
}

/// Shared daemon state (sync std Mutexes; also readable from the tray thread).
pub struct DaemonState {
    children: Mutex<HashMap<String, ChildStatus>>,
    sync_enabled: Mutex<bool>,
    sync_last_run: Mutex<Option<String>>,
    /// Last observed external-drive presence (None = guard not scanned yet).
    usb_state: Mutex<Option<bool>>,
    /// Drive letters of the detected USB disks (refreshed on mask changes).
    usb_letters: Mutex<Vec<String>>,
    /// Last GetLogicalDrives bitmask; gates the slower PowerShell USB probe.
    last_drive_mask: Mutex<Option<u32>>,
    /// Virtual display mode: None = off/unknown, Some(true) = mode on.
    vdisp_mode: Mutex<Option<bool>>,
    /// Cached Parsec virtual-display adapter state + probe timestamp (the
    /// probe is slow; reuse the result for 60s).
    vdisp_cache: Mutex<Option<(ParsecState, Instant)>>,
    /// Virtual display resolution/refresh pin: persisted target + guard
    /// bookkeeping, shared with /api/vdisp/mode and the guard task.
    vdisp_pin: Arc<VdispPinState>,
}

impl DaemonState {
    pub fn new() -> Arc<DaemonState> {
        // Probe once at boot so the sync timer starts aligned with the drive
        // state (the guard's first scan then reuses this cached result).
        let letters = usb_drive_letters();
        Arc::new(DaemonState {
            children: Mutex::new(HashMap::new()),
            // Align the sync timer with the external-drive state right away so
            // the timer never touches a missing mount before the guard aligns.
            sync_enabled: Mutex::new(!letters.is_empty()),
            sync_last_run: Mutex::new(None),
            usb_state: Mutex::new(None),
            usb_letters: Mutex::new(letters),
            last_drive_mask: Mutex::new(Some(logical_drives_mask())),
            vdisp_mode: Mutex::new(None),
            vdisp_cache: Mutex::new(None),
            vdisp_pin: VdispPinState::new(crate::config::get().vdisp_pin_path.clone()),
        })
    }

    /// Spawn a component and record its pid. Errors if it is already running
    /// or the spawn failed.
    pub fn start(&self, name: &str) -> Result<u32, String> {
        {
            let g = self.children.lock().unwrap_or_else(|e| e.into_inner());
            if g.get(name).map(|c| c.running).unwrap_or(false) {
                log_warn!("daemon.start", "{name} already running; ignoring");
                return Err(format!("{name} already running"));
            }
        }
        let t0 = Instant::now();
        let pid = match spawn_child(name) {
            Ok(p) => p,
            Err(e) => {
                log_error!("daemon.start", "E {name} spawn failed: {e}");
                return Err(e);
            }
        };
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), ChildStatus { running: true, pid });
        log_info!("daemon.start", "{name} started (pid {pid}) in {}ms", t0.elapsed().as_millis());
        Ok(pid)
    }

    /// Stop a component: taskkill the recorded pid (tree + force). Always
    /// marks it stopped, even if the process was already dead.
    pub fn stop(&self, name: &str) -> Result<(), String> {
        let pid = self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .map(|c| c.pid)
            .unwrap_or(0);
        let t0 = Instant::now();
        if pid > 0 {
            let _ = taskkill_pid(pid);
        } else if !port_of(name).is_empty() {
            // No recorded pid (e.g. adopted process): kill by port match.
            let _ = kill_stale_on_port(&port_of(name));
        }
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), ChildStatus { running: false, pid: 0 });
        log_info!("daemon.stop", "{name} stopped (pid {pid}) in {}ms", t0.elapsed().as_millis());
        Ok(())
    }

    /// Stop every child the daemon itself started (built-ins plus adopted
    /// or spawned native shortcuts): iterate the recorded map instead of
    /// a fixed list, so user shortcuts never leak as orphans.
    /// stop() is idempotent (dead pids just clear the record).
    pub fn kill_all(&self) {
        log_info!("daemon.kill_all", "stopping all managed children");
        let names: Vec<String> = self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for name in names {
            let _ = self.stop(&name);
        }
        log_info!("daemon.kill_all", "all children stopped");
    }

    /// Re-check liveness of every recorded pid and update the running flag.
    pub fn refresh(&self) {
        let mut g = self.children.lock().unwrap_or_else(|e| e.into_inner());
        for cs in g.values_mut() {
            if cs.running && cs.pid > 0 {
                cs.running = pid_alive(cs.pid);
            }
        }
    }

    pub fn status(&self, name: &str) -> ChildStatus {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .copied()
            .unwrap_or(ChildStatus { running: false, pid: 0 })
    }

    /// Adopt an already-running process (N-01): record pid without spawning,
    /// so start-then-start never double-spawns. False when pid is not alive.
    pub fn adopt(&self, name: &str, pid: u32) -> bool {
        if pid == 0 || !pid_alive(pid) {
            return false;
        }
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), ChildStatus { running: true, pid });
        log_info!("daemon.adopt", "{name} adopted running pid {pid}");
        true
    }

    /// Full status object for /api/tray/status and the tray menu.
    pub fn status_json(&self) -> serde_json::Value {
        let g = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let comp = |name: &str| {
            let cs = g.get(name).copied().unwrap_or(ChildStatus { running: false, pid: 0 });
            serde_json::json!({ "running": cs.running, "pid": cs.pid })
        };
        serde_json::json!({
            "ok": true,
            "webcli": comp("webcli"),
            "syncthing": comp("syncthing"),
            "homepage": comp("homepage"),
            "sync": {
                "enabled": self.sync_enabled(),
                "last_run": self.sync_last_run(),
            },
            "usb": {
                "present": self.external_present(),
                "letters": self.usb_letters(),
                "guard": true,
            },
        })
    }

    pub fn set_sync(&self, on: bool) {
        *self.sync_enabled.lock().unwrap_or_else(|e| e.into_inner()) = on;
    }

    pub fn sync_enabled(&self) -> bool {
        *self.sync_enabled.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn sync_last_run(&self) -> Option<String> {
        self.sync_last_run.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Kick the mirror sync script once (async: powershell runs detached).
    pub fn run_sync(&self) -> Result<(), String> {
        // Script path is user-configurable; missing = feature not configured.
        let Some(script) = &crate::config::get().sync_script else {
            return Err(
                "sync script not configured (install one or set FENGPAGE_SYNC_SCRIPT)".into(),
            );
        };
        let child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-WindowStyle",
                "Hidden",
                "-File",
            ])
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("spawn sync script failed: {e}"))?;
        *self.sync_last_run.lock().unwrap_or_else(|e| e.into_inner()) = Some(now_local_string());
        drop(child);
        Ok(())
    }

    /// Periodic timer task: every [sync].interval_secs (default 600s), run
    /// the script when enabled.
    pub async fn sync_ticker(self: Arc<Self>) {
        let interval_secs = crate::settings::get().sync_interval_secs();
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if !self.sync_enabled() {
                continue;
            }
            match self.run_sync() {
                Ok(()) => log_info!("daemon.sync_ticker", "mirror sync script spawned"),
                Err(e) => log_error!("daemon.sync_ticker", "mirror sync error: {e}"),
            }
        }
    }

    // ---------- external-drive guard ----------

    /// Live external-drive presence: any USB-bus disk currently has a drive
    /// letter (driven by the cached probe result, refreshed on mask changes).
    pub fn external_present(&self) -> bool {
        !self
            .usb_letters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Drive letters of the currently detected USB disks (for mountvol eject
    /// and WSL remount).
    pub fn usb_letters(&self) -> Vec<String> {
        self.usb_letters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stop everything that depends on the external drive for SAFE EJECT:
    /// the alist docker container (holds open handles on /mnt/*, which would
    /// block mountvol detach) and the mirror-sync timer (reads
    /// <drive>:/Sync_Important_Files). Idempotent; returns a summary string.
    /// Called ONLY from the explicit eject paths (tray menu / api/tray eject).
    /// NOTE: automatic removal transitions do NOT come here (see
    /// on_usb_removed): alist also serves internal drives now, so a yanked
    /// USB must not take the whole file service down.
    pub fn stop_ext_deps(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if alist_running() {
            match wsl_docker(&["stop", "alist"]) {
                Ok(_) => parts.push("alist 已停止".to_string()),
                Err(e) => eprintln!("[usb-guard] docker stop alist failed: {e}"),
            }
        } else {
            parts.push("alist 未在运行".to_string());
        }
        self.set_sync(false);
        parts.push("同步定时器已暂停".to_string());
        parts.join("，")
    }

    /// Restore everything that depends on the external drive after
    /// reinsertion. Idempotent; returns a summary string. No-ops when no USB
    /// drive letter is available (callers normally only invoke this when the
    /// drive is present).
    /// alist serves the whole /mnt tree (internal + USB), and its mount
    /// table freezes at creation: a recreate (not a start) is required so
    /// the newly inserted drive's /mnt/<letter> becomes visible inside.
    pub fn start_ext_deps(&self, letters: &[String]) -> String {
        if letters.is_empty() {
            return "外置盘不在，未启动依赖".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        // New USB sticks do not automount into WSL by themselves; a stale
        // /mnt/<letter> (plain dir left by a yanked drive) also blocks the
        // bind. Fix every present letter before touching alist.
        for drive in letters {
            if !wsl_ensure_mnt_letter(drive) {
                parts.push(format!("注意 /mnt/{drive} 挂载异常"));
            }
        }
        // Recreate picks up the fresh binds AND starts the container when it
        // is missing/stopped (covers boot-with-USB and eject-then-reinsert).
        let (ok, msg) = crate::api::dockerctl::recreate_container("alist");
        if ok {
            parts.push("alist 已重建（含新盘）".to_string());
        } else {
            eprintln!("[usb-guard] recreate alist failed: {msg}");
            parts.push(format!("alist 重建失败（{msg}）"));
        }
        self.set_sync(true);
        parts.push("同步定时器已恢复".to_string());
        parts.join("，")
    }

    /// Automatic-removal transition (drive yanked, no eject): keep alist
    /// RUNNING for the internal drives, but recreate it so the stale bind of
    /// the gone drive is dropped (else OpenList keeps a broken handle and
    /// the next insert may collide). Sync timer pauses (its source is gone).
    /// Best-effort; failures only log.
    pub fn on_usb_removed(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let (ok, msg) = crate::api::dockerctl::recreate_container("alist");
        if ok {
            parts.push("alist 已重建（掉盘清理）".to_string());
        } else {
            eprintln!("[usb-guard] recreate alist after removal failed: {msg}");
            parts.push("alist 重建跳过（引擎未起？）".to_string());
        }
        self.set_sync(false);
        parts.push("同步定时器已暂停".to_string());
        parts.join("，")
    }

    /// USB guard task: poll the external-drive state every USB_POLL_SECS and
    /// stop or restore the drive-dependent components on removal/insertion
    /// transitions.
    pub async fn usb_guard_task(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(USB_POLL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Boot-time self-heal: a USB storage node disabled at the PnP layer
        // (Problem code 22 / CONFIGFLAG_DISABLED) never shows up in Get-Disk,
        // so the guard below would treat an actually-inserted disk as absent
        // for the whole session (no drive letter, alist stays stopped). Re-enable
        // such nodes first, then let the first scan + poll transitions bring the
        // drive dependencies (mount /mnt/e, start alist) back within one interval.
        let healed = repair_disabled_usb_storage();
        if !healed.is_empty() {
            eprintln!("[usb-guard] re-enabled disabled USB storage node(s): {}", healed.join("; "));
        }
        // First scan aligns dependencies to the current drive state (there is
        // no transition yet), so a missing external drive at boot stays
        // stopped.
        self.usb_scan().await;
        loop {
            interval.tick().await;
            self.usb_scan().await;
        }
    }

    /// One guard pass: compare the live drive state with the last observed
    /// one and act on transitions (insert -> restore, remove -> stop). The
    /// cheap GetLogicalDrives bitmask gates the slower PowerShell USB probe:
    /// the probe only runs when the set of mounted drive letters changed.
    async fn usb_scan(self: &Arc<Self>) {
        let mask = logical_drives_mask();
        let changed = {
            let mut last = self
                .last_drive_mask
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if last.is_none_or(|m| m != mask) {
                *last = Some(mask);
                true
            } else {
                false
            }
        };
        let letters = if changed {
            usb_drive_letters()
        } else {
            self.usb_letters
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        *self
            .usb_letters
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = letters.clone();
        let present = !letters.is_empty();
        let prev = *self.usb_state.lock().unwrap_or_else(|e| e.into_inner());
        if prev == Some(present) && !changed {
            return; // no state change
        }
        // Record the observation first (idempotent dedup: a failed action is
        // logged but not retried on every poll).
        *self.usb_state.lock().unwrap_or_else(|e| e.into_inner()) = Some(present);
        let state = self.clone();
        let letters_c = letters.clone();
        let summary = tokio::task::spawn_blocking(move || {
            if present {
                state.start_ext_deps(&letters_c)
            } else if prev.is_none() {
                // Boot with no USB: just align the sync timer, do NOT churn
                // alist (it serves internal drives; nothing stale to clean).
                state.set_sync(false);
                "首检无外置盘，同步定时器保持暂停".to_string()
            } else {
                state.on_usb_removed()
            }
        })
        .await
        .unwrap_or_else(|e| format!("guard task error: {e}"));
        if prev.is_none() {
            eprintln!(
                "[usb-guard] first scan: external drive {}, deps aligned ({summary})",
                if present { "present" } else { "absent" }
            );
        } else if present {
            eprintln!("[usb-guard] external drive inserted, restored dependencies ({summary})");
        } else {
            eprintln!("[usb-guard] external drive removed, stopped dependencies ({summary})");
        }
    }
}

// ---------- virtual display mode ----------

/// Parsec virtual-display adapter state, from the live PnP probe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParsecState {
    /// Adapter present and running (Status OK) - usable as a remote fallback.
    Ready,
    /// Adapter present but disabled or errored in Device Manager.
    Disabled,
    /// No Parsec display adapter installed.
    Missing,
}

impl ParsecState {
    /// Wire value used by /api/vdisp/status.
    pub fn as_str(&self) -> &'static str {
        match self {
            ParsecState::Ready => "ok",
            ParsecState::Disabled => "disabled",
            ParsecState::Missing => "missing",
        }
    }
}

impl DaemonState {
    /// Current virtual display mode (false = off/unknown).
    pub fn vdisp_mode(&self) -> bool {
        self.vdisp_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(false)
    }

    /// Shared vdisp pin state (mode API + guard task).
    pub fn vdisp_pin(&self) -> Arc<VdispPinState> {
        self.vdisp_pin.clone()
    }

    fn set_vdisp_mode(&self, m: Option<bool>) {
        *self.vdisp_mode.lock().unwrap_or_else(|e| e.into_inner()) = m;
    }

    /// Parsec virtual-display adapter state, cached for 60s (the PowerShell
    /// probe is slow; most requests hit the cache).
    pub fn parsec_state(&self) -> ParsecState {
        let now = Instant::now();
        {
            let c = self.vdisp_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((s, t)) = *c {
                if now.duration_since(t) < Duration::from_secs(60) {
                    return s;
                }
            }
        }
        let s = parsec_detect_live();
        *self.vdisp_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((s, now));
        s
    }

    /// Turn virtual display mode on. The Parsec virtual display is a *remote
    /// fallback*: remote sessions (Parsec/RDP) can render to it even when the
    /// physical screen is off or asleep. The physical screen is deliberately
    /// left untouched - virtual display mode must never blank it, otherwise a
    /// single-screen machine would lose its only working display.
    pub fn vdisp_on(&self) -> Result<String, String> {
        match self.parsec_state() {
            ParsecState::Missing => Err(
                "未检测到 Parsec 虚拟显示器驱动，无法开启（请先安装 Parsec 虚拟显示器）".to_string(),
            ),
            ParsecState::Disabled => Err(
                "已检测到 Parsec 虚拟显示器但未启用（设备管理器中被禁用），请先启用该适配器".to_string(),
            ),
            ParsecState::Ready => {
                self.set_vdisp_mode(Some(true));
                Ok("虚拟显示器模式已开启：Parsec 虚拟显示器可用作远程兜底，物理屏保持正常显示".to_string())
            }
        }
    }

    /// Turn virtual display mode off: clear the mode and restore the physical
    /// monitor (safety net - wakes a screen blanked by any previous run of
    /// the old blank-on-enable behavior). Idempotent.
    pub fn vdisp_off(&self) -> Result<String, String> {
        self.set_vdisp_mode(None);
        let _ = monitor_power(true);
        Ok("虚拟显示器模式已关闭：物理屏已恢复点亮".to_string())
    }
}

// ---------- virtual display helpers ----------

/// Power the physical monitor via the SC_MONITORPOWER system-command
/// broadcast: on=true restores the display (wParam -1), on=false blanks it
/// (wParam 2). The message is delivered either way, so we treat a zero reply
/// as success too.
fn monitor_power(on: bool) -> Result<(), String> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageW, HWND_BROADCAST, SC_MONITORPOWER, WM_SYSCOMMAND,
    };
    let res = unsafe {
        SendMessageW(
            HWND_BROADCAST,
            WM_SYSCOMMAND,
            WPARAM(SC_MONITORPOWER as usize),
            LPARAM(if on { -1 } else { 2 }),
        )
    };
    eprintln!("[vdisp] monitor_power({on}) -> SendMessageW ret {}", res.0);
    Ok(())
}

/// Live PnP probe for the Parsec virtual display adapter (hidden PowerShell,
/// 10s timeout, Missing on any failure). Distinguishes three states:
///   Ready    - adapter present and running (Status OK)
///   Disabled - adapter present but disabled or errored in Device Manager
///   Missing  - no Parsec display adapter installed
fn parsec_detect_live() -> ParsecState {
    const QUERY: &str = "Get-PnpDevice -Class Display -ErrorAction SilentlyContinue | Where-Object { $_.FriendlyName -match 'Parsec' } | ForEach-Object { $_.Status + '|' + $_.FriendlyName }";
    let mut child = match Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", QUERY])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vdisp] Parsec probe spawn failed: {e}");
            return ParsecState::Missing;
        }
    };
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out);
        }
        let _ = tx.send(out);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("[vdisp] Parsec probe timed out");
                    return ParsecState::Missing;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return ParsecState::Missing,
        }
    }
    let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let state = parse_parsec_probe(&text);
    eprintln!(
        "[vdisp] Parsec probe: state={state:?} ({})",
        text.trim().replace('\n', " | ")
    );
    state
}

/// Parse the PnP probe output ("Status|FriendlyName" per line) into a
/// ParsecState. Pure function so the logic is unit-testable.
fn parse_parsec_probe(text: &str) -> ParsecState {
    let mut present = false;
    let mut ready = false;
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let low = l.to_ascii_lowercase();
        if !low.contains("parsec") {
            continue;
        }
        present = true;
        if low.starts_with("ok|") {
            ready = true;
        }
    }
    if present && ready {
        ParsecState::Ready
    } else if present {
        ParsecState::Disabled
    } else {
        ParsecState::Missing
    }
}

// ---------- virtual display resolution/refresh pinning ----------
//
// vdisp-pinning-design.md, scheme 4: IDD virtual displays (Parsec/GameViewer)
// have NO static config to lock - Windows topology recompute, device-node
// rebuilds and streaming clients all reset the mode at runtime. The only
// reliable fix is "detect drift, reapply" (eventual consistency):
//   - detect: QueryDisplayConfig over the active topology (Win32_Devices_Display)
//   - reapply: ChangeDisplaySettingsExW (already in the enabled Gdi feature)
//   - CDS_UPDATEREGISTRY refreshes the persisted topology mode, which is the
//     value Windows falls back to on recompute - the closest thing to
//     "source locking" the platform allows.
// The guard polls every 60s (and right after a PUT), never panics/exits, and
// logs an explicit line after 3 consecutive apply failures while continuing.

/// Pin bounds accepted by PUT /api/vdisp/mode (task spec).
pub const PIN_MIN_WIDTH: u32 = 640;
pub const PIN_MAX_WIDTH: u32 = 7680;
pub const PIN_MIN_HEIGHT: u32 = 480;
pub const PIN_MAX_HEIGHT: u32 = 4320;
pub const PIN_MIN_REFRESH: u32 = 24;
pub const PIN_MAX_REFRESH: u32 = 240;
/// Guard poll interval in seconds.
const VDISP_GUARD_POLL_SECS: u64 = 60;
/// Consecutive reapply failures after which the guard logs one explicit
/// "still continuing" line (it keeps retrying either way).
const VDISP_GUARD_FAIL_LOG_STREAK: u32 = 3;

/// Pinned resolution/refresh target for the virtual display.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct PinMode {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

/// Validate a pin target against the accepted bounds. Pure function (unit
/// tested; the API and the store both go through it).
pub fn validate_pin_mode(width: u32, height: u32, refresh: u32) -> Result<PinMode, String> {
    if !(PIN_MIN_WIDTH..=PIN_MAX_WIDTH).contains(&width) {
        return Err(format!("width must be {PIN_MIN_WIDTH}-{PIN_MAX_WIDTH}, got {width}"));
    }
    if !(PIN_MIN_HEIGHT..=PIN_MAX_HEIGHT).contains(&height) {
        return Err(format!("height must be {PIN_MIN_HEIGHT}-{PIN_MAX_HEIGHT}, got {height}"));
    }
    if !(PIN_MIN_REFRESH..=PIN_MAX_REFRESH).contains(&refresh) {
        return Err(format!("refresh must be {PIN_MIN_REFRESH}-{PIN_MAX_REFRESH} Hz, got {refresh}"));
    }
    Ok(PinMode { width, height, refresh })
}

/// Live mode of the virtual display as seen by QueryDisplayConfig.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VdispLiveMode {
    /// GDI device name (\\.\DISPLAYn) - the ChangeDisplaySettingsExW target.
    pub device: String,
    /// Monitor friendly name (status/logs only).
    pub monitor: String,
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

/// True when the live mode no longer matches the pinned target. Refresh gets
/// a +/-1 Hz tolerance: QueryDisplayConfig reports rationals, so 59.95 Hz
/// rounds to 60 and 143.999 to 144 while the DEVMODE value stays 59/143.
/// Pure function.
pub fn mode_drifted(target: &PinMode, actual: &VdispLiveMode) -> bool {
    actual.width != target.width
        || actual.height != target.height
        || actual.refresh.abs_diff(target.refresh) > 1
}

/// One guard-pass decision before any apply (pure, unit tested).
#[derive(PartialEq, Eq, Debug)]
pub enum GuardDecision {
    /// Live mode matches the pin: nothing to do.
    Match,
    /// Live mode differs: reapply the pin.
    Drift,
    /// No active virtual display found: log only (auto device-node rebuild
    /// is design phase P1, intentionally out of scope here).
    MissingDisplay,
}

pub fn guard_decide(target: &PinMode, actual: Option<&VdispLiveMode>) -> GuardDecision {
    match actual {
        None => GuardDecision::MissingDisplay,
        Some(live) if mode_drifted(target, live) => GuardDecision::Drift,
        Some(_) => GuardDecision::Match,
    }
}

/// File shape of data/vdisp-pin.json (object wrapper for forward compat).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct PinFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pin: Option<PinMode>,
}

/// Load the pin from disk. A missing or corrupt file simply means "no pin"
/// (same convention as api/plainapp.rs).
pub fn load_pin(path: &Path) -> Option<PinMode> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<PinFile>(&t).ok())
        .and_then(|f| f.pin)
}

/// Persist the pin atomically (tmp + rename, same pattern as api/plainapp.rs).
/// Callers hold the store mutex, so writes never interleave.
pub fn save_pin(path: &Path, pin: Option<PinMode>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {} failed: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&PinFile { pin })
        .map_err(|e| format!("serialize vdisp pin failed: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace {} failed: {e}", path.display())
    })
}

/// Guard status surfaced by GET /api/vdisp/mode.
#[derive(Serialize, Clone, Debug, Default)]
pub struct GuardInfo {
    /// Last completed check ("YYYY-MM-DD HH:MM:SS"), None before the first.
    pub last_check: Option<String>,
    /// Last successful reapply after a drift.
    pub last_reapply: Option<String>,
    /// Short machine-readable result of the last pass
    /// ("match" / "reapplied" / "display-not-found" / "reapply-failed: ...").
    pub last_result: String,
    /// Times a drift was detected since the pin was set.
    pub drift_count: u64,
    /// Consecutive reapply failures (reset on success).
    pub fail_streak: u32,
}

/// Shared vdisp pin state: persisted target + guard bookkeeping. The mode
/// API handlers read/set it; the guard task (spawned from main) applies it.
pub struct VdispPinState {
    path: PathBuf,
    pin: Mutex<Option<PinMode>>,
    guard: Mutex<GuardInfo>,
    /// Wakes the guard right after a PUT so a fresh pin is applied within
    /// ~1s instead of waiting for the next 60s tick.
    kick: tokio::sync::Notify,
}

impl VdispPinState {
    /// Load the persisted pin (if any) and build the shared state.
    pub fn new(path: PathBuf) -> Arc<VdispPinState> {
        let pin = load_pin(&path);
        if let Some(p) = pin {
            eprintln!(
                "[vdisp-pin] loaded pin {}x{}@{}Hz from {}",
                p.width, p.height, p.refresh, path.display()
            );
        }
        Arc::new(VdispPinState {
            path,
            pin: Mutex::new(pin),
            guard: Mutex::new(GuardInfo::default()),
            kick: tokio::sync::Notify::new(),
        })
    }

    /// Current pin target (None = pinning off).
    pub fn pin_mode(&self) -> Option<PinMode> {
        *self.pin.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Guard status snapshot.
    pub fn guard_info(&self) -> GuardInfo {
        self.guard.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Validate + persist the pin (PUT /api/vdisp/mode). The guard task then
    /// applies it; the store itself never touches Win32, which keeps the
    /// unit tests free of display side effects.
    pub fn set_pin(&self, width: u32, height: u32, refresh: u32) -> Result<PinMode, String> {
        let mode = validate_pin_mode(width, height, refresh)?;
        let mut g = self.pin.lock().unwrap_or_else(|e| e.into_inner());
        save_pin(&self.path, Some(mode))?;
        *g = Some(mode);
        // Fresh target: drop any stale failure streak from the previous pin.
        self.guard.lock().unwrap_or_else(|e| e.into_inner()).fail_streak = 0;
        Ok(mode)
    }

    /// Remove the pin (DELETE /api/vdisp/mode). The display keeps whatever
    /// mode it currently runs; nothing is "restored" - Windows owns the
    /// default topology behavior.
    pub fn clear_pin(&self) -> Result<(), String> {
        let mut g = self.pin.lock().unwrap_or_else(|e| e.into_inner());
        save_pin(&self.path, None)?;
        *g = None;
        Ok(())
    }

    /// Wake the guard task for an immediate pass (after a PUT). Harmless
    /// when no guard is running (the permit is just never consumed).
    pub fn kick_guard(&self) {
        self.kick.notify_one();
    }

    /// Guard task: every VDISP_GUARD_POLL_SECS (and right after a PUT kick)
    /// check the virtual display mode while a pin is set and reapply it on
    /// drift. Never panics or exits; failures are counted and logged, and
    /// after VDISP_GUARD_FAIL_LOG_STREAK consecutive failures an explicit
    /// warning is written while the guard keeps retrying.
    pub async fn vdisp_guard_task(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(VDISP_GUARD_POLL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.kick.notified() => {}
            }
            if self.pin_mode().is_none() {
                continue; // pinning off: idle, no probes
            }
            let st = self.clone();
            // Win32 + eprintln inside: keep it off the async worker.
            let _ = tokio::task::spawn_blocking(move || st.vdisp_guard_pass()).await;
        }
    }

    /// One synchronous guard pass (Win32 calls inside).
    fn vdisp_guard_pass(&self) {
        let Some(target) = self.pin_mode() else { return };
        let now = now_local_string();
        let live = query_vdisp_live_mode();
        match guard_decide(&target, live.as_ref()) {
            GuardDecision::MissingDisplay => {
                self.guard_update(|g| {
                    g.last_check = Some(now);
                    g.last_result = "display-not-found".into();
                });
                eprintln!(
                    "[vdisp-pin] guard: virtual display not found (pin {}x{}@{}Hz kept)",
                    target.width, target.height, target.refresh
                );
            }
            GuardDecision::Match => {
                self.guard_update(|g| {
                    g.last_check = Some(now);
                    g.last_result = "match".into();
                    g.fail_streak = 0;
                });
            }
            GuardDecision::Drift => {
                let live = live.expect("drift decision implies a live mode");
                eprintln!(
                    "[vdisp-pin] drift: {}x{}@{}Hz -> pin {}x{}@{}Hz, reapplying",
                    live.width, live.height, live.refresh,
                    target.width, target.height, target.refresh
                );
                match apply_pin_mode(&live.device, &target) {
                    Ok(()) => self.guard_update(|g| {
                        g.last_check = Some(now.clone());
                        g.last_reapply = Some(now);
                        g.last_result = "reapplied".into();
                        g.drift_count += 1;
                        g.fail_streak = 0;
                    }),
                    Err(e) => {
                        let streak = {
                            let mut g = self.guard.lock().unwrap_or_else(|e| e.into_inner());
                            g.last_check = Some(now.clone());
                            g.last_result = format!("reapply-failed: {e}");
                            g.drift_count += 1;
                            g.fail_streak += 1;
                            g.fail_streak
                        };
                        eprintln!("[vdisp-pin] reapply failed (streak {streak}): {e}");
                        if streak >= VDISP_GUARD_FAIL_LOG_STREAK {
                            eprintln!(
                                "[vdisp-pin] {streak} consecutive reapply failures; guard keeps retrying every {VDISP_GUARD_POLL_SECS}s (no exit)"
                            );
                        }
                    }
                }
            }
        }
    }

    fn guard_update(&self, f: impl FnOnce(&mut GuardInfo)) {
        f(&mut self.guard.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

// ---------- vdisp pin Win32 helpers ----------
//
// Detection route (design doc P0.2/P0.3): QueryDisplayConfig over the active
// topology + DisplayConfigGetDeviceInfo for the per-path GDI/monitor names.
// Reapply route: ChangeDisplaySettingsExW from the already-enabled Gdi
// feature (SetDisplayConfig stays an unused option per the design doc).

/// [u16; N] wide array -> String up to the first NUL.
fn wide_array_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// DISPLAYCONFIG_RATIONAL (Hz as a fraction) -> rounded integer Hz.
fn rational_hz(r: windows::Win32::Devices::Display::DISPLAYCONFIG_RATIONAL) -> u32 {
    if r.Denominator == 0 {
        return 0;
    }
    (r.Numerator as f64 / r.Denominator as f64).round() as u32
}

/// Query the active display topology (paths + their mode table). Retries
/// with doubled buffers on ERROR_INSUFFICIENT_BUFFER (122).
fn query_active_topology()
-> Result<
    (
        Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO>,
        Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO>,
    ),
    String,
> {
    use windows::Win32::Devices::Display::{
        QueryDisplayConfig, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, QDC_ONLY_ACTIVE_PATHS,
    };
    let mut path_cap: u32 = 16;
    let mut mode_cap: u32 = 64;
    for _ in 0..4 {
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
            unsafe { vec![std::mem::zeroed(); path_cap as usize] };
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            unsafe { vec![std::mem::zeroed(); mode_cap as usize] };
        let mut pn = path_cap;
        let mut mn = mode_cap;
        let res = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut pn,
                paths.as_mut_ptr(),
                &mut mn,
                modes.as_mut_ptr(),
                None,
            )
        };
        match res {
            Ok(()) => {
                paths.truncate(pn as usize);
                modes.truncate(mn as usize);
                return Ok((paths, modes));
            }
            Err(e) if e.code().0 as u32 == 0x8007_007A => {
                // HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER): required
                // sizes are returned in pn/mn; grow and retry.
                path_cap = (path_cap * 2).max(pn);
                mode_cap = (mode_cap * 2).max(mn);
            }
            Err(e) => return Err(format!("QueryDisplayConfig failed: {e}")),
        }
    }
    Err("QueryDisplayConfig: buffers never sufficient".into())
}

/// True when the GDI adapter behind \\.\DISPLAYn reports a Parsec virtual
/// display driver (EnumDisplayDevicesW device description match). Kept in
/// sync with the PnP probe above ("FriendlyName -match 'Parsec'"); the
/// GameViewer adapter stays out of scope until the UI grows an adapter
/// selector (design doc P0.1, follow-up).
fn gdi_adapter_is_parsec(devicename: &str) -> bool {
    use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
    let mut i: u32 = 0;
    loop {
        let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) };
        if !ok.as_bool() {
            return false;
        }
        if wide_array_to_string(&dd.DeviceName).eq_ignore_ascii_case(devicename) {
            return wide_array_to_string(&dd.DeviceString).to_lowercase().contains("parsec");
        }
        i += 1;
    }
}

/// Live mode of the Parsec virtual display: QueryDisplayConfig over the
/// active paths. A path matches when the monitor friendly name, the target
/// device path or the GDI adapter description mentions Parsec. None when no
/// virtual display is active (or the topology query itself fails).
pub fn query_vdisp_live_mode() -> Option<VdispLiveMode> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
        DISPLAYCONFIG_MODE_INFO_TYPE_TARGET, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
        DISPLAYCONFIG_TARGET_DEVICE_NAME,
    };
    let (paths, modes) = match query_active_topology() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[vdisp-pin] {e}");
            return None;
        }
    };
    for path in &paths {
        // GDI device name of the path source (the apply target).
        let mut src: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { std::mem::zeroed() };
        src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        src.header.adapterId = path.sourceInfo.adapterId;
        src.header.id = path.sourceInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut src.header) } != 0 {
            continue;
        }
        let device = wide_array_to_string(&src.viewGdiDeviceName);
        // Monitor friendly name + target device path.
        let mut tgt: DISPLAYCONFIG_TARGET_DEVICE_NAME = unsafe { std::mem::zeroed() };
        tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        tgt.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        tgt.header.adapterId = path.targetInfo.adapterId;
        tgt.header.id = path.targetInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut tgt.header) } != 0 {
            continue;
        }
        let monitor_raw = wide_array_to_string(&tgt.monitorFriendlyDeviceName);
        let monitor = monitor_raw.to_lowercase();
        let dev_path = wide_array_to_string(&tgt.monitorDevicePath).to_lowercase();
        if !(monitor.contains("parsec") || dev_path.contains("parsec") || gdi_adapter_is_parsec(&device))
        {
            continue;
        }
        // Current desktop mode: source-mode entry of the path.
        let (mut w, mut h) = (0u32, 0u32);
        let src_idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
        if src_idx < modes.len() && modes[src_idx].infoType.0 == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE.0
        {
            let m = unsafe { modes[src_idx].Anonymous.sourceMode };
            w = m.width;
            h = m.height;
        }
        // Current refresh: target-mode vSyncFreq, falling back to the
        // refreshRate carried on the path itself.
        let mut hz = 0u32;
        let tgt_idx = unsafe { path.targetInfo.Anonymous.modeInfoIdx } as usize;
        if tgt_idx < modes.len() && modes[tgt_idx].infoType.0 == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET.0
        {
            let m = unsafe { modes[tgt_idx].Anonymous.targetMode };
            hz = rational_hz(m.targetVideoSignalInfo.vSyncFreq);
        }
        if hz == 0 {
            hz = rational_hz(path.targetInfo.refreshRate);
        }
        return Some(VdispLiveMode {
            device,
            monitor: monitor_raw,
            width: w,
            height: h,
            refresh: hz,
        });
    }
    None
}

/// ChangeDisplaySettingsExW result code -> log-friendly name.
fn disp_change_str(code: i32) -> &'static str {
    match code {
        0 => "DISP_CHANGE_SUCCESSFUL",
        1 => "DISP_CHANGE_RESTART",
        -1 => "DISP_CHANGE_FAILED",
        -2 => "DISP_CHANGE_BADMODE",
        -3 => "DISP_CHANGE_NOTUPDATED",
        -4 => "DISP_CHANGE_BADFLAGS",
        -5 => "DISP_CHANGE_BADPARAM",
        -6 => "DISP_CHANGE_BADDUALVIEW",
        _ => "DISP_CHANGE_UNKNOWN",
    }
}

/// Reapply the pinned mode to one display via ChangeDisplaySettingsExW
/// (design doc P0.3: Gdi route, zero new dependencies). The current DEVMODE
/// is read first so everything not pinned (position, orientation, depth)
/// survives the change. CDS_UPDATEREGISTRY also refreshes the persisted
/// topology mode - the value Windows falls back to on topology recompute,
/// which is as close to "source locking" as the platform allows.
fn apply_pin_mode(device: &str, target: &PinMode) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, DEVMODEW, DEVMODE_FIELD_FLAGS, DISP_CHANGE_SUCCESSFUL,
        DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION,
        EnumDisplaySettingsExW, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS, CDS_UPDATEREGISTRY,
    };
    let dev = wstr(device);
    unsafe {
        let mut dm: DEVMODEW = std::mem::zeroed();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        // Seed from the live mode so position/orientation/depth survive.
        let _ = EnumDisplaySettingsExW(
            PCWSTR(dev.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
        .as_bool();
        dm.dmPelsWidth = target.width;
        dm.dmPelsHeight = target.height;
        dm.dmDisplayFrequency = target.refresh;
        dm.dmFields = DEVMODE_FIELD_FLAGS(
            DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0 | DM_POSITION.0,
        );
        let ret = ChangeDisplaySettingsExW(
            PCWSTR(dev.as_ptr()),
            Some(&dm),
            HWND::default(),
            CDS_UPDATEREGISTRY,
            None,
        );
        if ret.0 == DISP_CHANGE_SUCCESSFUL.0 {
            Ok(())
        } else {
            Err(format!(
                "ChangeDisplaySettingsExW on {device} returned {} ({})",
                disp_change_str(ret.0),
                ret.0
            ))
        }
    }
}

// ---------- child spawning ----------

fn spawn_child(name: &str) -> Result<u32, String> {
    let cfg = crate::config::get();
    let mut cmd: Command;
    match name {
        "webcli" => {
            // Bundled sibling component; missing = wrong install layout.
            let Some(exe) = &cfg.webcli_exe else {
                return Err("webcli not found; set FENGPAGE_WEB_CLI_EXE or FENGPAGE_ROOT".into());
            };
            let dir = cfg
                .webcli_dir
                .clone()
                .unwrap_or_else(|| exe.parent().unwrap_or(Path::new(".")).to_path_buf());
            let st = crate::settings::get();
            cmd = Command::new(exe);
            cmd.env("WEBHOST", st.webcli_host())
                .env("WEBPORT", st.webcli_port().to_string())
                .env("TERM_SHELL", st.webcli_shell())
                .env("WEBTOKEN", read_token()?)
                .current_dir(dir);
        }
        "syncthing" => {
            // Optional external component: missing = not installed, hint only.
            // T-09: resolve the exe through the shared finder (config env >
            // fetch-system target > winget shim) so a component installed via
            // the self-check page starts without any env wiring.
            let Some(exe) = crate::config::syncthing_exe_found().or_else(|| cfg.syncthing_exe.clone()) else {
                return Err(
                    "syncthing not found; install it (system view) or set FENGPAGE_SYNCTHING_EXE".into(),
                );
            };
            let home = cfg
                .syncthing_home
                .clone()
                .unwrap_or_else(|| crate::config::get().root.join("external/syncthing/config"));
            let dir = cfg
                .syncthing_dir
                .clone()
                .unwrap_or_else(|| exe.parent().unwrap_or(Path::new(".")).to_path_buf());
            let st_port = crate::settings::get().services_ports().1;
            cmd = Command::new(exe);
            cmd.args([
                "serve",
                &format!("--home={}", home.display()),
                // T-09: GUI/REST binds localhost only; external access goes through
                // the panel proxy / Tailscale Serve, never raw 8384.
                &format!("--gui-address=127.0.0.1:{st_port}"),
                "--no-browser",
                "--no-restart",
                &format!("--logfile={}", home.join("log.txt").display()),
            ])
            .current_dir(dir);
        }
        "homepage" => {
            // Optional external component; node resolves via PATH unless
            // FENGPAGE_NODE points at a full node.exe path.
            let Some(dir) = &cfg.homepage_dir else {
                return Err("homepage not found; set FENGPAGE_HOMEPAGE_DIR".into());
            };
            let config_dir = cfg
                .homepage_config
                .clone()
                .unwrap_or_else(|| dir.join("config"));
            let st_port = crate::settings::get().services_ports().2;
            cmd = Command::new(&cfg.homepage_node);
            cmd.args(["node_modules/next/dist/bin/next", "start"])
                .env("HOMEPAGE_CONFIG_DIR", &config_dir)
                .env("HOMEPAGE_ALLOWED_HOSTS", &cfg.homepage_allowed)
                .env("PORT", st_port.to_string())
                .current_dir(dir);
        }
        _ if name.starts_with(crate::api::native::DAEMON_PREFIX) => {
            // User-defined native shortcut (N-01): the full spawn plan,
            // re-validated against the current whitelist, comes from
            // api::native; the daemon only executes it verbatim.
            let id = &name[crate::api::native::DAEMON_PREFIX.len()..];
            let spec = crate::api::native::spawn_spec(id)?;
            log_info!("daemon.spawn", "native {id}: {}", spec.audit);
            cmd = Command::new(&spec.program);
            if !spec.args.is_empty() {
                cmd.args(&spec.args);
            }
            if let Some(wd) = spec.workdir {
                cmd.current_dir(wd);
            }
        }
        _ => return Err(format!("unknown component: {name}")),
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let child = cmd.spawn().map_err(|e| format!("spawn {name} failed: {e}"))?;
    Ok(child.id())
}

/// Read the WEBTOKEN value for webcli from term_token.env (TOKEN= line).
/// The path is resolved by config (FENGPAGE_TERM_ENV overrides it),
/// matching api/term.rs.
fn read_token() -> Result<String, String> {
    let path = crate::config::get().term_env.clone();
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("read {} failed: {e}", path.display()))?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("TOKEN=") {
            let v = v.trim();
            if !v.is_empty() {
                return Ok(v.to_string());
            }
        }
    }
    Err(format!("TOKEN not found in {}", path.display()))
}

pub(crate) fn taskkill_pid(pid: u32) -> Result<(), String> {
    let out = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("taskkill failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "taskkill pid {pid} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Is the process with this pid still alive?
fn pid_alive(pid: u32) -> bool {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        if handle.is_invalid() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code).is_ok();
        let _ = CloseHandle(handle);
        ok && code == STILL_ACTIVE
    }
}

// ---------- external-drive guard helpers ----------

/// Bitmask of currently mounted drive letters (GetLogicalDrives).
fn logical_drives_mask() -> u32 {
    unsafe { GetLogicalDrives() }
}

/// Drive letters of all USB-bus disks that currently have a drive letter,
/// via one hidden PowerShell query (10s timeout, empty vec on failure).
fn usb_drive_letters() -> Vec<String> {
    const QUERY: &str = "Get-Disk -ErrorAction SilentlyContinue | Where-Object BusType -eq 'USB' | Get-Partition -ErrorAction SilentlyContinue | Get-Volume -ErrorAction SilentlyContinue | Where-Object DriveLetter | Select-Object -ExpandProperty DriveLetter";
    let mut child = match Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", QUERY])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[usb-guard] USB probe spawn failed: {e}");
            return Vec::new();
        }
    };
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out);
        }
        let _ = tx.send(out);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("[usb-guard] USB probe timed out");
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Vec::new(),
        }
    }
    let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// True when a PnP DiskDrive InstanceId belongs to a USB-bus storage disk
/// (the external drive this panel manages). The system NVMe shows as
/// `SCSI\DISK&VEN_NVME...`; removable USB storage shows as `USBSTOR\DISK...`.
fn is_usb_storage_disk(instance_id: &str) -> bool {
    let t = instance_id.trim();
    t.starts_with("USBSTOR\\DISK")
}

/// Pure decision for the boot repair probe: one line of
/// `InstanceId|ProblemCode` output is a re-attachable target when it names a
/// USB storage disk node that is disabled at the PnP layer (22 =
/// CM_PROB_DISABLED / CONFIGFLAG_DISABLED). A disk "ejected" via
/// `Set-Disk -IsOffline` keeps ProblemCode 0 (CM_PROB_NONE), so a normal
/// logical eject is never treated as broken here.
fn pnp_disabled_usb_disk(line: &str) -> bool {
    let mut it = line.splitn(2, '|');
    let id = it.next().unwrap_or("").trim();
    let code = it.next().unwrap_or("").trim();
    if code != "22" {
        return false;
    }
    is_usb_storage_disk(id)
}

/// Boot self-heal: find USB-bus disk nodes that are disabled at the PnP layer
/// and re-enable each (same as Device Manager "Enable device"). Returns the
/// re-enabled InstanceIds, or an empty Vec when there is nothing to fix or the
/// probe failed. Spawns one hidden PowerShell command with a 25s deadline.
fn repair_disabled_usb_storage() -> Vec<String> {
    const QUERY: &str = "$d = Get-PnpDevice -Class DiskDrive -ErrorAction SilentlyContinue; $d | Where-Object { $_.Problem -eq 22 } | ForEach-Object { Enable-PnpDevice -InstanceId $_.InstanceId -Confirm:$false -ErrorAction SilentlyContinue | Out-Null }; $d | Where-Object { $_.Problem -eq 22 } | ForEach-Object { $_.InstanceId + '|' + $_.Problem }";
    let mut child = match Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", QUERY])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[usb-guard] repair probe spawn failed: {e}");
            return Vec::new();
        }
    };
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out);
        }
        let _ = tx.send(out);
    });
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("[usb-guard] repair probe timed out");
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Vec::new(),
        }
    }
    let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && pnp_disabled_usb_disk(l))
        .collect()
}

/// Run a command inside the managed WSL distro as root, capturing
/// combined output. Distro name comes from settings (env/file/default),
/// never hardcoded. Always runs hidden (no console window).
fn wsl_run(inner: &[&str]) -> Result<String, String> {
    let d = crate::settings::try_get()
        .map(|s| s.wsl_distro())
        .unwrap_or_else(|| crate::api::wsl::DEFAULT_DISTRO.into());
    let out = Command::new("wsl")
        .args(["-d", &d, "-u", "root", "--"])
        .args(inner)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("wsl failed: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// Run a docker command inside the WSL distro (docker CLI as root).
fn wsl_docker(args: &[&str]) -> Result<String, String> {
    let mut full: Vec<&str> = vec!["docker"];
    full.extend_from_slice(args);
    wsl_run(&full)
}

/// True when a `findmnt -rn -o SOURCE /mnt/<letter>` line shows the path is
/// really a drvfs mount of `drive` (e.g. `E:\x5c`, `E:`, `E:/...`). An empty
/// line or a source that does not start with the drive letter means the path
/// is either missing or a stale leftover that needs a real remount. Pure
/// decision used by [wsl_ensure_mnt_letter]; kept separate so the regression
/// is unit-testable without touching WSL.
fn mnt_letter_sourced_from(line: &str, drive: &str) -> bool {
    let token = line.split_whitespace().next().unwrap_or("").trim();
    if token.is_empty() {
        return false;
    }
    let drive_upper = drive.trim().to_uppercase();
    token.to_uppercase().starts_with(&format!("{drive_upper}:"))
}

/// Ensure /mnt/<letter> is a healthy drvfs mount inside WSL before docker
/// touches it. A drive that went offline leaves a stale (broken) mount
/// entry: after a WSL restart the mount point survives as a plain readable
/// EMPTY DIRECTORY, so a bare `test -r` check wrongly reports healthy and
/// the panel never re-runs the real drvfs mount on reinsertion - the
/// container then binds the empty dir and the inserted drive's files stay
/// invisible. Only an actual mount sourced from the given drive letter
/// counts as healthy; anything else is remounted here.
/// New USB sticks do not automount into WSL on their own, so the explicit
/// `mount -t drvfs` here is also what makes a fresh insert visible at all.
/// Returns true when the path is usable afterwards.
fn wsl_ensure_mnt_letter(drive: &str) -> bool {
    let letter = drive.trim().to_lowercase();
    if letter.len() != 1 {
        return false;
    }
    let healthy = match wsl_run(&["sh", "-c", &format!("findmnt -rn -o SOURCE /mnt/{letter} 2>/dev/null; true")]) {
        Ok(out) => mnt_letter_sourced_from(&out, drive),
        Err(_) => false,
    };
    if healthy {
        return true;
    }
    eprintln!("[usb-guard] /mnt/{letter} stale or unmounted in WSL, attempting remount from {drive}:");
    wsl_run(&[
        "sh",
        "-c",
        &format!("mkdir -p /mnt/{letter}; umount -f /mnt/{letter} 2>/dev/null; mount -t drvfs {drive}: /mnt/{letter}"),
    ])
    .is_ok()
}

/// True when the alist container is currently running (docker ps name match).
fn alist_running() -> bool {
    match wsl_docker(&["ps", "--format", "{{.Names}}"]) {
        Ok(out) => out.lines().any(|l| l.trim() == "alist"),
        Err(e) => {
            eprintln!("[usb-guard] docker ps failed: {e}");
            false
        }
    }
}

// ---------- single instance ----------

static MUTEX_HANDLE: Mutex<Option<HANDLE>> = Mutex::new(None);

/// Acquire the named instance mutex. Returns false when another tray instance
/// holds it (retries briefly so a restarting predecessor can release it).
pub fn acquire_single_instance() -> bool {
    for _ in 0..15 {
        unsafe {
            let name = wstr(INSTANCE_MUTEX_NAME);
            let res = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
            let already = last_error_is(ERROR_ALREADY_EXISTS.0);
            match res {
                Ok(h) => {
                    if !already {
                        *MUTEX_HANDLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
                        log_info!("daemon.single_instance", "acquired instance mutex");
                        return true;
                    }
                    let _ = CloseHandle(h);
                }
                Err(_) => {
                    // Mutex could not be created for another reason: run anyway.
                    log_warn!("daemon.single_instance", "mutex create failed; running anyway");
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    log_warn!("daemon.single_instance", "another instance holds the mutex; denied");
    false
}

/// Release the instance mutex (called before the process exits).
pub fn release_single_instance() {
    if let Some(h) = MUTEX_HANDLE.lock().unwrap_or_else(|e| e.into_inner()).take() {
        unsafe {
            let _ = CloseHandle(h);
        }
        log_info!("daemon.single_instance", "released instance mutex");
    }
}

// ---------- stale listener cleanup ----------

/// Kill leftover listeners on the managed ports ([services].ports: webcli/
/// syncthing/homepage) that were started by our child programs. Matching on
/// the command line prevents killing unrelated processes that happen to use
/// the same port.
pub fn cleanup_stale() {
    let (webcli, syncthing, homepage) = crate::settings::get().services_ports();
    let ports: [String; 3] = [webcli.to_string(), syncthing.to_string(), homepage.to_string()];
    let my_pid = std::process::id();

    let out = match Command::new("netstat")
        .args(["-ano"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            log_error!("daemon.cleanup_stale", "netstat failed: {e}");
            eprintln!("[daemon] cleanup: netstat failed: {e}");
            return;
        }
    };

    let mut pids: Vec<u32> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if !line.contains("LISTENING") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let local = fields.get(1).copied().unwrap_or("");
        if !ports.iter().any(|p| local.ends_with(&format!(":{p}"))) {
            continue;
        }
        if let Some(pid) = fields.last().and_then(|s| s.parse::<u32>().ok()) {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids.retain(|p| *p != my_pid);
    if pids.is_empty() {
        return;
    }

    let pid_list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let script = format!(
        "Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -in @({pid_list}) }} | ForEach-Object {{ \"$($_.ProcessId)`t$($_.Name)`t$($_.CommandLine)\" }}"
    );
    let out = match Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            log_error!("daemon.cleanup_stale", "powershell failed: {e}");
            eprintln!("[daemon] cleanup: powershell failed: {e}");
            return;
        }
    };

    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split('\t');
        let Some(pid) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let name = it.next().unwrap_or("").to_lowercase();
        let cmdline = it.next().unwrap_or("").to_lowercase();
        let hit = name.contains("webcli")
            || name.contains("syncthing")
            || (name.contains("node")
                && (cmdline.contains("homepage-src")
                    || cmdline.contains("next/dist/bin/next")));
        if hit {
            log_info!("daemon.cleanup_stale", "stale listener pid {pid} ({name}) -> taskkill");
            eprintln!("[daemon] cleanup: stale listener pid {pid} ({name}) -> taskkill");
            let _ = taskkill_pid(pid);
        }
    }
}

/// Kill any stale listener on a single port (used by stop() when no pid is
/// recorded, e.g. a process adopted from a previous panel instance).
pub(crate) fn kill_stale_on_port(port: &str) -> Result<(), String> {
    let out = Command::new("netstat")
        .args(["-ano"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("netstat failed: {e}"))?;
    let mut killed: Vec<u32> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if !line.contains("LISTENING") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let local = fields.get(1).copied().unwrap_or("");
        if !local.ends_with(&format!(":{port}")) {
            continue;
        }
        if let Some(pid) = fields.last().and_then(|s| s.parse::<u32>().ok()) {
            if pid != std::process::id() {
                killed.push(pid);
            }
        }
    }
    killed.sort_unstable();
    killed.dedup();
    for pid in killed {
        let _ = taskkill_pid(pid);
    }
    Ok(())
}

// ---------- helpers ----------

/// UTF-16 wide string with a trailing NUL (for PCWSTR params).
fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Local time as "YYYY-MM-DD HH:MM:SS" (via GetLocalTime).
fn now_local_string() -> String {
    unsafe {
        let st = GetLocalTime();
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

/// True when the last Win32 error (GetLastError) equals the given code.
/// The windows 0.52 binding maps a nonzero WIN32_ERROR to an Err(Error)
/// whose HRESULT is HRESULT_FROM_WIN32(code).
fn last_error_is(code: u32) -> bool {
    let hr = (0x8007_0000u32 | code) as i32;
    unsafe { matches!(GetLastError(), Err(e) if e.code().0 == hr) }
}

// ---------- WSL keepalive + selfcheck registration (guard-design.md) ----------

/// Start the WSL keepalive thread and the boot selfcheck on background
/// threads, and load the persisted engine target first. Never blocks
/// startup; a failure here only disables the keepalive until the next run.
/// P0-1: no-op when a shutdown was already requested, so the late boot task
/// cannot spawn the keepalive thread while the exit tail is tearing down.
pub fn start_wsl_keepalive() {
    if crate::shutdown_requested() {
        return;
    }
    crate::api::wsl::init_state();
    std::thread::spawn(crate::api::wsl::keepalive_loop);
    crate::api::wsl::start_selfcheck();
}

/// Graceful WSL degradation on panel exit: drop the keepalive session so the
/// VM falls back to vmIdleTimeout, and terminate the distro immediately when
/// the engine target is off (frees RAM).
pub fn shutdown_wsl() {
    crate::api::wsl::on_panel_exit();
}

#[cfg(test)]
mod vdisp_tests {
    use super::{parse_parsec_probe, ParsecState};

    #[test]
    fn probe_missing_on_empty_output() {
        assert_eq!(parse_parsec_probe(""), ParsecState::Missing);
        assert_eq!(parse_parsec_probe("\r\n  \r\n"), ParsecState::Missing);
    }

    #[test]
    fn probe_missing_when_no_parsec_device() {
        let out = "OK|AMD Radeon(TM) Graphics\nUnknown|Generic Monitor (S2719DGF)";
        assert_eq!(parse_parsec_probe(out), ParsecState::Missing);
    }

    #[test]
    fn probe_ready_on_ok_adapter() {
        let out = "OK|AMD Radeon(TM) Graphics\r\nOK|Parsec Virtual Display Adapter\r\n";
        assert_eq!(parse_parsec_probe(out), ParsecState::Ready);
    }

    #[test]
    fn probe_ready_when_monitor_ok_adapter_ok() {
        let out = "OK|Generic Monitor (ParsecVDA)\nOK|Parsec Virtual Display Adapter";
        assert_eq!(parse_parsec_probe(out), ParsecState::Ready);
    }

    #[test]
    fn probe_disabled_on_unknown_status() {
        let out = "Unknown|Parsec Virtual Display Adapter";
        assert_eq!(parse_parsec_probe(out), ParsecState::Disabled);
    }

    #[test]
    fn probe_disabled_when_only_monitor_present() {
        // Monitor present but adapter missing/disabled - not usable.
        let out = "Unknown|Generic Monitor (ParsecVDA)";
        assert_eq!(parse_parsec_probe(out), ParsecState::Disabled);
    }

    #[test]
    fn probe_ignores_case_and_crlf() {
        let out = "ok|parsec virtual display adapter\r\n";
        assert_eq!(parse_parsec_probe(out), ParsecState::Ready);
    }
}

#[cfg(test)]
mod vdisp_pin_tests {
    use super::*;

    fn live(w: u32, h: u32, r: u32) -> VdispLiveMode {
        VdispLiveMode {
            device: "\\\\.\\DISPLAY2".into(),
            monitor: "ParsecVDA".into(),
            width: w,
            height: h,
            refresh: r,
        }
    }

    #[test]
    fn pin_validation_accepts_bounds_and_rejects_out_of_range() {
        // exact bounds are legal
        assert!(validate_pin_mode(640, 480, 24).is_ok());
        assert!(validate_pin_mode(7680, 4320, 240).is_ok());
        assert!(validate_pin_mode(1920, 1080, 60).is_ok());
        // illegal values (task spec example: width 400)
        assert!(validate_pin_mode(400, 1080, 60).unwrap_err().contains("width"));
        assert!(validate_pin_mode(639, 1080, 60).is_err());
        assert!(validate_pin_mode(7681, 1080, 60).is_err());
        assert!(validate_pin_mode(1920, 479, 60).unwrap_err().contains("height"));
        assert!(validate_pin_mode(1920, 4321, 60).is_err());
        assert!(validate_pin_mode(1920, 1080, 23).unwrap_err().contains("refresh"));
        assert!(validate_pin_mode(1920, 1080, 241).is_err());
    }

    #[test]
    fn pin_drift_ignores_one_hz_rounding() {
        let t = PinMode { width: 1920, height: 1080, refresh: 60 };
        assert!(!mode_drifted(&t, &live(1920, 1080, 60)));
        // QueryDisplayConfig rationals round 59.95 -> 60 while DEVMODE says 59
        assert!(!mode_drifted(&t, &live(1920, 1080, 59)));
        assert!(!mode_drifted(&t, &live(1920, 1080, 61)));
        // real drifts: topology recompute fallback, streaming client override
        assert!(mode_drifted(&t, &live(1366, 768, 60)));
        assert!(mode_drifted(&t, &live(1920, 1080, 100)));
        assert!(mode_drifted(&t, &live(3840, 2160, 144)));
    }

    #[test]
    fn pin_guard_decide_covers_three_outcomes() {
        let t = PinMode { width: 2560, height: 1440, refresh: 60 };
        assert_eq!(guard_decide(&t, None), GuardDecision::MissingDisplay);
        assert_eq!(guard_decide(&t, Some(&live(2560, 1440, 60))), GuardDecision::Match);
        assert_eq!(guard_decide(&t, Some(&live(1920, 1080, 60))), GuardDecision::Drift);
    }

    #[test]
    fn pin_store_roundtrip_on_disk() {
        let path = std::env::temp_dir()
            .join(format!("flygo-vdisp-pin-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // fresh store: no pin
        let st = VdispPinState::new(path.clone());
        assert_eq!(st.pin_mode(), None);
        // invalid target: rejected before anything is persisted
        assert!(st.set_pin(400, 1080, 60).is_err());
        assert_eq!(st.pin_mode(), None);
        assert!(!path.exists(), "rejected pin must not be persisted");
        // valid target: stored in memory and on disk
        let m = st.set_pin(1920, 1080, 60).expect("valid pin");
        assert_eq!((m.width, m.height, m.refresh), (1920, 1080, 60));
        assert_eq!(st.pin_mode(), Some(m));
        // a second store over the same path loads it back (persistence proof)
        assert_eq!(VdispPinState::new(path.clone()).pin_mode(), Some(m));
        // clear: persisted as no-pin, visible to a fresh store
        st.clear_pin().expect("clear ok");
        assert_eq!(st.pin_mode(), None);
        assert_eq!(VdispPinState::new(path.clone()).pin_mode(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pin_guard_info_serializes_for_api() {
        let g = GuardInfo {
            last_check: Some("2026-08-17 03:00:00".into()),
            last_result: "match".into(),
            drift_count: 2,
            ..Default::default()
        };
        let v = serde_json::to_value(&g).expect("serialize");
        assert_eq!(v["last_check"], "2026-08-17 03:00:00");
        assert_eq!(v["drift_count"], 2u64);
        assert!(v.get("last_reapply").is_some());
    }
}

#[cfg(test)]
mod mnt_letter_tests {
    use super::mnt_letter_sourced_from;

    /// Regression: a stale readable EMPTY DIRECTORY (drive detached, then a
    /// WSL restart recreated /mnt/<letter> as a plain dir) must NOT be
    /// treated as a healthy mount - the old `test -r` check passed here and
    /// skipped the real drvfs remount, so a reinserted drive never appeared.
    #[test]
    fn stale_empty_dir_is_not_healthy() {
        // findmnt -rn -o SOURCE prints nothing for a non-mount.
        assert!(!mnt_letter_sourced_from("", "E"));
        assert!(!mnt_letter_sourced_from("\n", "F"));
        assert!(!mnt_letter_sourced_from("  \r\n  ", "E"));
    }

    /// A real drvfs mount of the expected drive is healthy.
    #[test]
    fn real_drvfs_mount_of_expected_drive_is_healthy() {
        assert!(mnt_letter_sourced_from("E:\\x5c /mnt/e", "E"));
        assert!(mnt_letter_sourced_from("E:", "E"));
        assert!(mnt_letter_sourced_from("F:/ /mnt/f", "F"));
        // case/whitespace tolerance
        assert!(mnt_letter_sourced_from("  e:\\x5c /mnt/e  ", "E"));
    }

    /// A mount sourced from a different letter must not satisfy the check.
    #[test]
    fn mount_of_other_drive_is_not_healthy() {
        assert!(!mnt_letter_sourced_from("C:\\x5c /mnt/c", "E"));
        assert!(!mnt_letter_sourced_from("C:", "E"));
        assert!(!mnt_letter_sourced_from("D:/ /mnt/d", "E"));
        assert!(!mnt_letter_sourced_from("E:", "F"));
    }
}

#[cfg(test)]
mod usb_enum_repair_tests {
    use super::{is_usb_storage_disk, pnp_disabled_usb_disk};

    #[test]
    fn recognizes_usb_storage_and_rejects_system_nvme() {
        // removable USB disks enumerate under USBSTOR\DISK... (synthetic
        // vendor/product IDs; fixtures must not name real hardware)
        assert!(is_usb_storage_disk(
            "USBSTOR\\DISK&VEN_TESTV&PROD_TESTDISK&REV_9203\\40B127F6801C&0"
        ));
        assert!(is_usb_storage_disk(
            "USBSTOR\\DISK&VEN_XDISK&PROD_X1&REV_A3T0\\5DFE950C5B87&0"
        ));
        // the system NVMe lives on the SCSI bus, never a re-attach target
        assert!(!is_usb_storage_disk(
            "SCSI\\DISK&VEN_NVME&PROD_TESTNVME1234\\5&1CD50DD6&0&000000"
        ));
        assert!(!is_usb_storage_disk(""));
        assert!(!is_usb_storage_disk("   "));
    }

    #[test]
    fn only_pnp_disabled_usb_disks_are_reattach_targets() {
        // Problem 22 (CM_PROB_DISABLED) on a USB node -> fix candidate
        assert!(pnp_disabled_usb_disk(
            "USBSTOR\\DISK&VEN_TESTV&PROD_TESTDISK&REV_9203\\40B127F6801C&0|22"
        ));
        // a logical Set-Disk offline eject keeps ProblemCode 0 -> never touched
        assert!(!pnp_disabled_usb_disk(
            "USBSTOR\\DISK&VEN_TESTV&PROD_TESTDISK&REV_9203\\40B127F6801C&0|0"
        ));
        // system disk disabled (should not happen) is still not a USB target
        assert!(!pnp_disabled_usb_disk(
            "SCSI\\DISK&VEN_NVME&PROD_TESTNVME1234\\5&1CD50DD6&0&000000|22"
        ));
        // malformed lines never match
        assert!(!pnp_disabled_usb_disk(""));
        assert!(!pnp_disabled_usb_disk("garbage"));
        assert!(!pnp_disabled_usb_disk("USBSTOR\\DISK&VEN_TESTV"));
    }
}
