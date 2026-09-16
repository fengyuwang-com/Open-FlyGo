// smb.rs - SMB share lifecycle manager + panel-controllable drive shares.
//
// Product contract: "open the app, the drives appear on the network; stop
// the app, they disappear." The panel owns the Windows SMB shares whose name
// IS the drive letter (C:\ -> \\host\C, E:\ -> \\host\E). On boot it creates
// each share configured under [smb]; on shutdown it removes exactly the
// shares it manages. A stopped/exited panel therefore never exposes any
// drive.
//
// Two hard constraints are handled here:
//   1. Drive may be offline (e.g. a removable E: not mounted). We skip and
//      record it instead of failing; the share is re-created on the next
//      boot once the drive is back.
//   2. Creating/removing shares needs elevation (`New-SmbShare` /
//      `Remove-SmbShare` are admin-only). The autostart scheduled task runs
//      with highest privileges, so the panel normally holds admin rights. If
//      it does not, creation silently degrades and the panel logs why.
//
// Panel control: GET /api/smb lists every drive currently present on the
// machine plus its mounted/configured state; toggling a drive mounts or
// unmounts it and persists the letter set into [smb].shares so the choice
// survives restarts and hot-plugged drives are mountable from the UI. When
// [smb].guest_access is on (default), mounting also applies the Windows SMB
// anonymous-read baseline - the shares were previously unreachable with
// "network name not found (error 67)" because the stock Guest account is
// disabled, Guest is denied network logon and anonymous access is
// restricted - so any device on the private network can browse without
// credentials. All machine-level policy edits are best-effort, idempotent
// and reversible via apply_guest(false).
//
// All code comments are pure ASCII; runtime strings may be UTF-8.

use std::collections::HashSet;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::log::{log_error, log_info, log_warn};

use windows::core::PWSTR;
use windows::Win32::Foundation::{
    BOOL, CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_EVENT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, WaitForSingleObject, CREATE_NO_WINDOW,
    PROCESS_INFORMATION, STARTUPINFOW, STARTF_USESHOWWINDOW, STARTF_USESTDHANDLES,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// Hard ceiling for any one PowerShell invocation. A healthy call completes in
/// milliseconds; a pathological backing drive (e.g. a failing USB volume) can
/// block `New-SmbShare` / `Get-SmbShare` indefinitely. Capping here turns an
/// infinite boot hang into a bounded "failed" outcome so one bad drive can
/// never stall the whole panel.
const PS_TIMEOUT: Duration = Duration::from_secs(25);

/// Hard ceiling for probing whether a drive root is mounted. `Path::exists`
/// issues an IRP to the volume, which can block on a failing/electing USB
/// device. We never wait longer than this for the probe answer.
const DRIVE_ONLINE_TIMEOUT: Duration = Duration::from_secs(3);

/// Hard wall-clock bound on the shutdown share-cleanup. Normally the cleanup is
/// one batched PowerShell call (~2 s for any number of drives), so this cap is
/// never exercised in the healthy case. It only protects a pathological backing
/// drive from dragging the process exit indefinitely: on overflow we log a WARN
/// and continue with `close_app` - a stuck cleanup must never block closing the
/// app. (The detached worker still finishes on its own; `ps` has its own 25 s
/// per-call timeout.)
const CLOSE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(8);

/// A live, effective share the lifecycle manager is responsible for.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ManagedShare {
    pub name: String,
    pub path: String,
    /// "created" | "already" | "skipped-offline" | "failed"
    pub state: &'static str,
    pub detail: String,
}

/// One drive as seen by the panel: present on the machine + its share state.
#[derive(Clone, Debug, serde::Serialize)]
pub struct DriveInfo {
    /// Single letter, e.g. "C".
    pub letter: String,
    /// Drive root, e.g. "C:\".
    pub path: String,
    /// Filesystem name, e.g. "NTFS" (empty when not determinable).
    pub fs: String,
    /// "fixed" | "removable" | "network" | "other".
    pub kind: &'static str,
    pub size_gb: f64,
    pub free_gb: f64,
    pub label: String,
    /// True when the letter is in the persisted [smb].shares set.
    pub configured: bool,
    /// True when a live share named `letter` points at this drive root.
    pub mounted: bool,
    /// "mounted" | "none" (live look, authoritative for the UI).
    pub state: &'static str,
    pub detail: String,
}

/// Mutable snapshot of what the manager created this boot (for /api/smb).
static RUNTIME: OnceLock<Mutex<Vec<ManagedShare>>> = OnceLock::new();

/// Letters the user explicitly unmounted this session. The hotplug watcher
/// must not re-share them (otherwise "全部不挂" / single unmount would be
/// undone on the next poll). Cleared on manual mount; reboot starts empty.
static UNMOUNT_DENY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn deny_set() -> &'static Mutex<HashSet<String>> {
    UNMOUNT_DENY.get_or_init(|| Mutex::new(HashSet::new()))
}

fn note_manual_unmount(letter: &str) {
    if let Ok(mut g) = deny_set().lock() {
        g.insert(letter.trim().to_uppercase());
    }
}

fn clear_denied(letter: &str) {
    if let Ok(mut g) = deny_set().lock() {
        g.remove(&letter.trim().to_uppercase());
    }
}

fn runtime() -> &'static Mutex<Vec<ManagedShare>> {
    RUNTIME.get_or_init(|| Mutex::new(Vec::new()))
}

/// Run a hidden PowerShell command and return trimmed stdout, guarded by a
/// hard `timeout`. The child is launched via Win32 `CreateProcessW` with
/// `SW_HIDE` + `CREATE_NO_WINDOW` and pipe-redirected stdout/stderr, so NO
/// console window ever flashes. `powershell -WindowStyle Hidden` still briefly
/// shows a black conhost window when spawned from the tray / background
/// context (the SMB black-window bug, handoff S3-C); `CreateProcessW` with a
/// hidden STARTUPINFO is the reliable fix. A child that does not exit in time
/// is killed and treated as a failure (returned `false`) so a blocked
/// `New-SmbShare` / `Get-SmbShare` on a failing drive can never stall the
/// panel; stdout/stderr are drained on reader threads so the child never
/// blocks on a full pipe.
fn ps_with_timeout(script: &str, timeout: Duration) -> (bool, String) {
    // Serialize ALL PowerShell invocations process-wide. CreateProcessW with
    // bInheritHandles=TRUE hands the child every inheritable handle in OUR
    // table - including other in-flight calls' pipes. Overlapping children
    // then hold each other's write ends open, EOF never arrives, the drain
    // hits its timeout and the output is discarded: every slow call (notably
    // list_drives) spuriously returned empty. One global lock makes overlap
    // impossible; calls are already seconds-slow, so queuing is harmless.
    static PS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = PS_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let exp = format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; {script}");
    // Pass the script via -EncodedCommand (base64 UTF-16LE): embedding it in
    // a double-quoted -Command string breaks as soon as the script itself
    // contains double quotes (the native argv parser treats backtick-escaped
    // quotes as real delimiters, so multi-quote scripts like the drive
    // enumeration arrived fragmented and died with a ParserError). Encoding
    // removes the quoting layer entirely - quotes, spaces, CJK all survive.
    let cmd = format!(
        "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand {}",
        base64_utf16le(&exp)
    );
    let h = match spawn_ps_hidden(&cmd) {
        Ok(h) => h,
        Err(e) => return (false, format!("spawn failed: {e}")),
    };
    let start = Instant::now();
    let mut timed_out = false;
    loop {
        let r = unsafe { WaitForSingleObject(h.process, 50) };
        if r == WAIT_EVENT(0) {
            break; // WAIT_OBJECT_0: process exited
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            let _ = unsafe {
                windows::Win32::System::Threading::TerminateProcess(h.process, 1)
            };
            break;
        }
    }
    let out = read_handle_to_string(h.stdout, timeout);
    let err = read_handle_to_string(h.stderr, timeout);
    let mut exit_ok = false;
    if !timed_out {
        let mut code: u32 = 0;
        if unsafe { GetExitCodeProcess(h.process, &mut code) }.is_ok() {
            exit_ok = code == 0;
        }
    }
    unsafe {
        let _ = CloseHandle(h.process);
        let _ = CloseHandle(h.thread);
    }
    if timed_out {
        return (false, format!("timed out after {}s", timeout.as_secs()));
    }
    let mut s = if out.trim().is_empty() { err } else { out };
    s = s.trim().to_string();
    (exit_ok, s)
}

/// Standard Base64 (RFC 4648) of the UTF-16LE bytes of `s`, as expected by
/// `powershell -EncodedCommand`. Hand-rolled to avoid a new dependency for
/// one call site.
fn base64_utf16le(s: &str) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut bytes = Vec::with_capacity(wide.len() * 2);
    for w in wide {
        bytes.push((w & 0xFF) as u8);
        bytes.push((w >> 8) as u8);
    }
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHA[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHA[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Handles returned by `spawn_ps_hidden`: the live process/thread plus the
/// read ends of the redirected stdout/stderr pipes.
struct PsHidden {
    process: HANDLE,
    thread: HANDLE,
    stdout: HANDLE,
    stderr: HANDLE,
}

/// Spawn `powershell.exe <command_line>` with no visible window: a hidden
/// STARTUPINFO (`SW_HIDE` + `STARTF_USESHOWWINDOW`) and `CREATE_NO_WINDOW`,
/// with stdout/stderr redirected to inheritable pipes. Returns the live
/// handles; the caller waits, then drains the pipes and closes them.
fn spawn_ps_hidden(command_line: &str) -> Result<PsHidden, String> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: BOOL::from(true),
        };
        let mut stdout_r = HANDLE::default();
        let mut stdout_w = HANDLE::default();
        let mut stderr_r = HANDLE::default();
        let mut stderr_w = HANDLE::default();
        if CreatePipe(&mut stdout_r, &mut stdout_w, Some(&sa), 0).is_err()
            || CreatePipe(&mut stderr_r, &mut stderr_w, Some(&sa), 0).is_err()
        {
            return Err("CreatePipe failed".into());
        }
        // Write ends MUST stay inheritable: they become the child's stdout /
        // stderr via STARTUPINFO. Only the read ends are parent-private, so
        // mark those non-inheritable (otherwise the child keeps the pipe open
        // and our drain never sees EOF). Inheriting the wrong ends silently
        // breaks ALL output capture (S3-C regression: every ps() returned
        // ok=true with empty stdout, so share creation only *looked* done).
        let _ = SetHandleInformation(stdout_r, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
        let _ = SetHandleInformation(stderr_r, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));

        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESHOWWINDOW | STARTF_USESTDHANDLES;
        si.wShowWindow = SW_HIDE.0 as u16;
        si.hStdOutput = stdout_w;
        si.hStdError = stderr_w;
        si.hStdInput = HANDLE::default();

        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let mut cmd_w: Vec<u16> = std::ffi::OsStr::new(command_line)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // NOTE: lpApplicationName is intentionally None so Windows resolves
        // `powershell.exe` from PATH (the first token of lpCommandLine). Passing
        // it as lpApplicationName skips PATH search and always fails to launch.
        let ok = CreateProcessW(
            None,
            PWSTR(cmd_w.as_mut_ptr()),
            None,
            None,
            BOOL::from(true),
            CREATE_NO_WINDOW,
            None,
            None,
            &si,
            &mut pi,
        );
        // Parent no longer needs the write ends (child has its own copies).
        let _ = CloseHandle(stdout_w);
        let _ = CloseHandle(stderr_w);
        if ok.is_err() {
            let _ = CloseHandle(stdout_r);
            let _ = CloseHandle(stderr_r);
            return Err("CreateProcessW failed".into());
        }
        Ok(PsHidden {
            process: pi.hProcess,
            thread: pi.hThread,
            stdout: stdout_r,
            stderr: stderr_r,
        })
    }
}

/// Drain a pipe read handle to a String on a worker thread (so a full pipe
/// never blocks the child), closing the handle when the pipe hits EOF.
///
/// The drain wait is bounded by the caller's `timeout` plus a grace margin:
/// the child has already exited (or been killed) before we drain, so EOF is
/// imminent - but a fixed 2 s cap was silently truncating slow commands
/// (Get-CimInstance enumeration, secedit guest baseline), making list_drives
/// report zero drives and apply_guest lose its DONE marker. The wait must
/// scale with the call's own timeout instead of using a constant.
fn read_handle_to_string(h: HANDLE, timeout: Duration) -> String {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut out: Vec<u8> = Vec::new();
        unsafe {
            loop {
                let mut read: u32 = 0;
                let res = ReadFile(h, Some(&mut buf), Some(&mut read), None);
                if res.is_err() || read == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..read as usize]);
            }
            let _ = CloseHandle(h);
        }
        let _ = tx.send(String::from_utf8_lossy(&out).to_string());
    });
    rx.recv_timeout(timeout + Duration::from_secs(5))
        .unwrap_or_default()
}

fn ps_output_yes(script: &str) -> bool {
    let (ok, out) = ps(script);
    ok && out.trim().to_uppercase().contains("YES")
}

/// Standard PowerShell call, capped at `PS_TIMEOUT` (defends against a failing
/// backing drive blocking the panel / boot).
fn ps(script: &str) -> (bool, String) {
    ps_with_timeout(script, PS_TIMEOUT)
}

/// Best-effort PowerShell call with a SHORT cap. Used for the NTFS ACE grants
/// (`icacls Everyone`) where a hung call must never stall boot - the ACL is a
/// nice-to-have on top of the share-level grant, so we wait only briefly and
/// move on (the share itself is unaffected by a missing ACE).
const ACL_PS_TIMEOUT: Duration = Duration::from_secs(3);

fn ps_acl(script: &str) -> (bool, String) {
    ps_with_timeout(script, ACL_PS_TIMEOUT)
}

/// True when a share with `name` already exists.
fn share_exists(name: &str) -> bool {
    let name_q = name.replace('\'', "''");
    ps_output_yes(&format!(
        r#"if (Get-SmbShare -Name '{name_q}' -ErrorAction SilentlyContinue) {{ 'YES' }} else {{ 'NO' }}"#
    ))
}

/// Path currently attached to `name` (None when the share is missing).
fn share_path(name: &str) -> Option<String> {
    let name_q = name.replace('\'', "''");
    let (ok, out) = ps(&format!(
        r#"$s = Get-SmbShare -Name '{name_q}' -ErrorAction SilentlyContinue; if ($s) {{ $s.Path }} else {{ '' }}"#
    ));
    if ok {
        let p = out.trim().to_string();
        if p.is_empty() { None } else { Some(p) }
    } else {
        None
    }
}

/// Which of `names` currently exist as SMB shares. ONE PowerShell cold start
/// answers for the whole set, replacing one probe per name (the boot baseline
/// used to pay one ~1.5 s cold start per name just asking "does it exist?").
/// Names are machine-generated (single letters, "C$", "ADMIN$"); anything
/// empty or containing a quote is skipped, never passed to the script.
/// Returns `None` when the batched probe failed or came back truncated: the
/// set would silently claim "nothing exists", which makes `create_all` pass
/// `Some(false)` hints and mislabel healthy existing shares as failed. Callers
/// must fall back to per-name probes on `None`.
fn shares_existing(names: &[&str]) -> Option<HashSet<String>> {
    let clean: Vec<&str> = names
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty() && !n.contains('\''))
        .collect();
    if clean.is_empty() {
        return Some(HashSet::new());
    }
    // Each name prints "1" when the share exists, "0" otherwise, in input
    // order (Write-Output per fragment).
    let frags: Vec<String> = clean
        .iter()
        .map(|n| {
            format!(
                "if (Get-SmbShare -Name '{n}' -ErrorAction SilentlyContinue) {{ '1' }} else {{ '0' }}"
            )
        })
        .collect();
    let (ok, out) = ps(&frags.join("; "));
    if !ok {
        log_warn!("smb.shares_existing", "batch probe failed; callers fall back");
        return None;
    }
    let found = parse_existing_answers(&clean, &out);
    if found.len() < clean.len() {
        // Output shorter than input: answers are only partially trustworthy,
        // so refuse the set entirely rather than report partial truth.
        log_warn!(
            "smb.shares_existing",
            "batch probe output short: {}/{} answers; callers fall back",
            found.len(),
            clean.len()
        );
        return None;
    }
    Some(found)
}

/// Pure pairing of batched "0"/"1" answer lines with the probed names, in
/// order. Only exact "0"/"1" lines count (stray output cannot desync the
/// pairing); names without an answer are simply absent from the result.
/// Pure and unit-testable (no PS calls).
fn parse_existing_answers(names: &[&str], out: &str) -> HashSet<String> {
    let mut it = out.lines().map(str::trim).filter(|l| *l == "0" || *l == "1");
    let mut found = HashSet::new();
    for n in names {
        match it.next() {
            Some("1") => {
                found.insert((*n).to_string());
            }
            Some(_) => {}
            None => break,
        }
    }
    found
}

fn drive_root_online(path: &str) -> bool {
    // "C:\" or "C:/" -> strip to "C"
    let letter = match path.trim().chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c,
        _ => return false,
    };
    let root = format!("{letter}:\\");
    // `Path::exists` issues an IRP to the volume and can block indefinitely on
    // a failing/electing removable drive. Probe it on a worker thread and only
    // wait `DRIVE_ONLINE_TIMEOUT`; an unresponsive volume is treated as
    // offline (skip) so a slow drive never stalls boot. The orphaned probe
    // thread is harmless: the OS eventually reaps the syscall.
    let done = Arc::new(AtomicBool::new(false));
    let done_probe = Arc::clone(&done);
    let probe_root = root.clone();
    thread::spawn(move || {
        let online = std::path::Path::new(&probe_root).exists();
        done_probe.store(online, Ordering::Relaxed);
    });
    let start = Instant::now();
    loop {
        if done.load(Ordering::Relaxed) {
            return done.load(Ordering::Relaxed);
        }
        if start.elapsed() >= DRIVE_ONLINE_TIMEOUT {
            return false; // volume unresponsive -> treat as offline
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Parse a grant string like `Everyone,READ` into New-SmbShare access flags.
/// Accepts a comma list of {READ, CHANGE, FULL} (case-insensitive).
fn access_flags(grant: &str) -> (bool, bool) {
    let mut read = false;
    let mut change = false;
    for part in grant.split(',') {
        let t = part.trim().to_uppercase();
        if t.contains("FULL") || t.contains("CHANGE") {
            change = true;
        }
        if t.contains("READ") || t.contains("FULL") {
            read = true;
        }
    }
    if !read {
        read = true; // never create a share with no access at all
    }
    (read, change)
}

/// "c" (or "C") -> "C:\".
fn normalized_root(letter: &str) -> String {
    let l = letter.trim().chars().next().unwrap_or(' ').to_ascii_uppercase();
    format!("{l}:\\")
}

// ---------------------------------------------------------------------------
// guest / anonymous access baseline (the "cannot access" fix)
// ---------------------------------------------------------------------------

/// Apply or revert the stock Windows SMB restriction set so that the panel's
/// Everyone shares are browsable without per-device credentials:
///   enable: Guest account ON, everyone-includes-anonymous, allow insecure
///           guest logons on server + client, drop Guest from the
///           SeDenyNetworkLogonRight deny list.
///   revert: undo all of the above (Guest OFF, deny Guest again, restore the
///           registry flags).
/// Best-effort and idempotent; returns (ok, human-readable summary).
pub fn apply_guest(enabled: bool) -> (bool, String) {
    let (enable_regs, deny_val, guest_state) = if enabled {
        (
            "Set-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Lsa' EveryoneIncludesAnonymous 1;
             Set-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\LanmanServer\\Parameters' AllowInsecureGuestAuth 1;
             Set-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\LanmanServer\\Parameters' RestrictNullSessAccess 0;
             Set-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\LanmanWorkstation\\Parameters' AllowInsecureGuestAuth 1",
            "SeDenyNetworkLogonRight =",
            "net user Guest /active:yes",
        )
    } else {
        (
            "Set-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Lsa' EveryoneIncludesAnonymous 0;
             Set-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\LanmanServer\\Parameters' AllowInsecureGuestAuth 0;
             Set-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\LanmanServer\\Parameters' RestrictNullSessAccess 1;
             Set-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Services\\LanmanWorkstation\\Parameters' AllowInsecureGuestAuth 0",
            "SeDenyNetworkLogonRight = Guest",
            "net user Guest /active:no",
        )
    };
    let script = format!(
        r#"
{enable_regs}
{guest_state} | Out-Null
$cfg = Join-Path $env:TEMP 'flygo-smb-secpol.cfg'
$sdb = Join-Path $env:TEMP 'flygo-smb-secpol.sdb'
secedit.exe /export /cfg $cfg | Out-Null
if (Test-Path $cfg) {{
  $c = Get-Content $cfg -Raw
  $c = $c -replace '(?m)^SeDenyNetworkLogonRight = .*', '{deny_val}'
  Set-Content $cfg $c -Encoding Ascii
  secedit.exe /configure /db $sdb /cfg $cfg /areas USER_RIGHTS | Out-Null
}}
'DONE'
"#
    );
    let (ok, out) = ps(&script);
    let ok = ok && out.trim().to_uppercase().contains("DONE");
    if ok {
        log_info!("smb.apply_guest", "guest/anonymous baseline {}", if enabled { "applied" } else { "reverted" });
    } else {
        log_error!(
            "smb.apply_guest",
            "E {} guest baseline fail: {}",
            if enabled { "apply" } else { "revert" },
            if out.trim().is_empty() { "no output".into() } else { out.trim().to_string() }
        );
    }
    (ok, out.trim().to_string())
}

/// Grant read+list on a drive root to Everyone so anonymous SMB clients can
/// enumerate it (the share-level grant alone does not bypass NTFS).
fn grant_root_read(path: &str) -> (bool, String) {
    let path_q = path.replace('\'', "''");
    ps_acl(&format!(
        "icacls.exe '{path_q}' /grant 'Everyone:(OI)(CI)RX' | Out-Null; if ($?) {{ 'OK' }} else {{ 'ERR' }}"
    ))
}

/// Remove the Everyone ACE the panel added on `path` (best-effort, only when
/// the drive is being unmounted and guest access is no longer wanted).
fn revoke_root_read(path: &str) -> (bool, String) {
    let path_q = path.replace('\'', "''");
    ps_acl(&format!(
        "icacls.exe '{path_q}' /remove 'Everyone' | Out-Null; if ($?) {{ 'OK' }} else {{ 'ERR' }}"
    ))
}

/// Best-effort removal of the panel-added Everyone ACE on several roots in a
/// single PowerShell invocation (unmount-all path). Neither a missing drive nor
/// a failed icacls is fatal here.
fn revoke_many(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let frags: Vec<String> = paths
        .iter()
        .map(|p| {
            let pq = p.replace('\'', "''");
            format!("icacls.exe '{pq}' /remove 'Everyone' | Out-Null; if (-not $?) {{ 'ERR' }}")
        })
        .collect();
    ps(&frags.join("; "));
}

/// One-shot SMB read probe for a share, from this machine. Uses the true
/// computer name so it exercises the same path a remote device takes. The
/// result is informational: a local client misbehaving does not mean the
/// server is down (the panel UI shows the share state separately).
/// Empty/non-letter input is rejected instead of silently probing C:.
pub fn probe_share(letter: &str) -> (bool, String) {
    let l = match letter.trim().chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => return (false, "empty drive letter".into()),
    };
    let (ok, out) = ps(&format!(
        r#"$u='\\' + $env:COMPUTERNAME + '\' + '{l}'; if (Test-Path $u) {{ 'OK' }} else {{ 'NOK' }}"#
    ));
    // Exact match: "NOK" contains "OK" as a substring, so a loose contains()
    // reported an unreachable share as browsable.
    (ok && out.trim().eq_ignore_ascii_case("OK"), out.trim().to_string())
}

// ---------------------------------------------------------------------------
// drive enumeration
// ---------------------------------------------------------------------------

/// Raw drive row parsed from the PowerShell enumeration line. Pure parse;
/// live share lookups happen in `list_drives` so unit tests never touch the
/// machine's SMB state.
struct DriveRaw {
    letter: String,
    path: String,
    fs: String,
    kind: &'static str,
    size_gb: f64,
    free_gb: f64,
    label: String,
}

/// "C:|NTFS|3|123.4|475.2|OS" -> DriveRaw. Pure and unit-testable.
fn parse_drive_line_pure(line: &str) -> Option<DriveRaw> {
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 6 {
        return None;
    }
    let letter = parts[0].trim().trim_end_matches(':').to_uppercase();
    if letter.len() != 1 || !letter.chars().next().unwrap().is_ascii_alphabetic() {
        return None;
    }
    let fs = parts[1].trim().to_string();
    let dt: i64 = parts[2].trim().parse().unwrap_or(-1);
    let free_gb: f64 = parts[3].trim().parse().unwrap_or(0.0);
    let size_gb: f64 = parts[4].trim().parse().unwrap_or(0.0);
    let label = parts[5].trim().to_string();
    let kind = match dt {
        2 => "removable",
        3 => "fixed",
        4 => "network",
        _ => "other",
    };
    Some(DriveRaw {
        path: format!("{letter}:\\"),
        letter,
        fs,
        kind,
        size_gb,
        free_gb,
        label,
    })
}

/// Enumerate the drives present on the machine right now (so hot-plugged
/// disks appear), tagged with their share state. DriveType 2 = removable,
/// 3 = fixed, 4 = network; CD-ROM (5) and RAM (6) are skipped.
pub fn list_drives() -> Vec<DriveInfo> {
    let configured: Vec<String> = crate::settings::get()
        .smb_shares()
        .iter()
        .map(|s| s.name.trim().to_uppercase())
        .collect();
    let (enum_ok, out) = ps(
        r#"@(Get-CimInstance Win32_LogicalDisk -Filter "DriveType=2 OR DriveType=3 OR DriveType=4" | ForEach-Object {
            $t=[math]::Round($_.Size/1GB,1); $f=[math]::Round($_.FreeSpace/1GB,1)
            Write-Output ("{0}|{1}|{2}|{3}|{4}|{5}" -f $_.DeviceID,$_.FileSystem,$_.DriveType,$f,$t,$_.VolumeName)
        }) | ConvertTo-Json -Compress"#,
    );
    let mut lines: Vec<String> = Vec::new();
    let parsed = serde_json::from_str::<serde_json::Value>(&out);
    if lines.is_empty() && parsed.is_err() {
        // Diagnostic: the panel depends on this probe for the drive list,
        // hotplug auto-share and the /api/smb view. Log the raw outcome so
        // a future probe failure is actionable instead of silent.
        let preview: String = out.chars().take(200).collect();
        log_warn!(
            "smb.list_drives",
            "enum probe unparsable (ok={enum_ok}, len={}, out={preview:?})",
            out.len()
        );
    }
    if let Ok(v) = parsed {
        match v.as_array() {
            Some(a) => {
                for item in a {
                    if let Some(s) = item.as_str() {
                        lines.push(s.to_string());
                    }
                }
            }
            None => {
                if let Some(s) = v.as_str() {
                    lines.push(s.to_string());
                }
            }
        }
    }
    let mut drives: Vec<DriveInfo> = Vec::new();
    for line in &lines {
        let raw = match parse_drive_line_pure(line) {
            Some(r) => r,
            None => continue,
        };
        let configured_letter = configured.contains(&raw.letter);
        let (mounted, state, detail) = if share_exists(&raw.letter)
            && share_path(&raw.letter).map(|p| p.eq_ignore_ascii_case(&raw.path)).unwrap_or(false)
        {
            (true, "mounted", "已挂载".into())
        } else {
            (false, "none", "未挂载".into())
        };
        drives.push(DriveInfo {
            letter: raw.letter,
            path: raw.path,
            fs: raw.fs,
            kind: raw.kind,
            size_gb: raw.size_gb,
            free_gb: raw.free_gb,
            label: raw.label,
            configured: configured_letter,
            mounted,
            state,
            detail,
        });
    }
    drives.sort_by(|a, b| a.letter.cmp(&b.letter));
    drives
}

// ---------------------------------------------------------------------------
// share create / remove primitives
// ---------------------------------------------------------------------------

/// Create the default admin shares if they are missing so the rest of the
/// tooling keeps a consistent baseline. Best-effort and idempotent; a
/// non-admin panel (or an offline backing drive) simply skips.
/// NOTE: only ADMIN$ (Windows dir) and C$ (system drive) are baselined here.
/// Windows auto-creates a hidden X$ share for every present volume, so
/// external letters must NOT be listed: on machines without that letter the
/// entry is dead weight, and with it present the OS share already exists.
fn ensure_admin_shares() {
    let t0 = Instant::now();
    let candidates: [(&str, &str); 2] = [("ADMIN$", "C:\\WINDOWS"), ("C$", "C:\\")];
    let online: Vec<&str> = candidates
        .iter()
        .filter(|(_, path)| drive_root_online(path))
        .map(|(name, _)| *name)
        .collect();
    // One cold start answers existence for ALL candidates (was one per name).
    let known = shares_existing(&online);
    let mut created = 0usize;
    for (name, path) in candidates {
        if !online.contains(&name)
            || known.as_ref().is_some_and(|s| s.contains(name))
            || (known.is_none() && share_exists(name))
        {
            continue;
        }
        let name_q = name.replace('\'', "''");
        let path_q = path.replace('\'', "''");
        let (ok, out) = ps(&format!(
            "New-SmbShare -Name '{name_q}' -Path '{path_q}' -FullAccess Everyone | Out-Null; if ($?) {{ 'OK' }} else {{ 'ERR' }}"
        ));
        if ok {
            created += 1;
            log_info!("smb.ensure_admin_shares", "{name} created ({path})");
        } else {
            log_warn!(
                "smb.ensure_admin_shares",
                "E {name} create fail: {}",
                if out.trim().is_empty() { "no output".into() } else { out.trim().to_string() }
            );
        }
    }
    log_info!(
        "smb.boot",
        "step ensure_admin_shares took {}ms ({} checked, {created} created)",
        t0.elapsed().as_millis(),
        online.len()
    );
}

/// Policy for share names: quotes are rejected outright (empty names too).
/// Single quotes need PowerShell escaping and the stale-share reconcile pass
/// refuses to touch such names; double quotes break single-quoted -Name
/// usage. One rule for every path (create / reconcile / remove) keeps the
/// handling consistent - validation here, not per-site escaping.
pub fn valid_share_name(name: &str) -> bool {
    !name.trim().is_empty() && !name.contains('\'') && !name.contains('"')
}

/// Create one share (idempotent). Also grants Everyone:Read on the root when
/// guest access is on so anonymous listings pass the NTFS layer. Returns the
/// per-share outcome.
fn create_one(name: &str, path: &str, grant: &str) -> ManagedShare {
    create_one_h(name, path, grant, None)
}

/// exists_hint: when Some, the caller already learned the share's existence
/// from a batched boot probe (one cold start for the whole configured set);
/// None probes on demand (single-drive paths like set_drive).
fn create_one_h(name: &str, path: &str, grant: &str, exists_hint: Option<bool>) -> ManagedShare {
    if !valid_share_name(name) {
        log_error!(
            "smb.create_one",
            "E {name}: share name rejected (quote characters are not allowed)"
        );
        return ManagedShare {
            name: name.to_string(),
            path: format!("{}\\", path.trim()),
            state: "failed",
            detail: "share name contains quote characters; not allowed".into(),
        };
    }
    let mut path = path.trim().to_string();
    if !path.ends_with('\\') {
        path.push('\\');
    }
    // Skip offline drives: creating a share for a missing mount fails and
    // pollutes the set; we re-create on the next boot once it is back.
    if !drive_root_online(&path) {
        log_warn!("smb.create_one", "skip {name} ({path}): drive offline");
        return ManagedShare {
            name: name.to_string(),
            path,
            state: "skipped-offline",
            detail: "盘未挂载，插入后可在面板重挂".into(),
        };
    }
    let exists = exists_hint.unwrap_or_else(|| share_exists(name));
    if exists {
        log_info!("smb.create_one", "{name} already shared ({path})");
        return ManagedShare {
            name: name.to_string(),
            path,
            state: "already",
            detail: "共享已存在".into(),
        };
    }
    let name_q = name.replace('\'', "''");
    let path_q = path.replace('\'', "''");
    let (read, change) = access_flags(grant);
    let flags = match (read, change) {
        (_, true) => "-ChangeAccess Everyone".to_string(),
        (true, false) => "-ReadAccess Everyone".to_string(),
        (false, false) => "-ReadAccess Everyone".to_string(),
    };
    let (ok, out) = ps(&format!(
        "New-SmbShare -Name '{name_q}' -Path '{path_q}' {flags} | Out-Null; if ($?) {{ 'OK' }} else {{ 'ERR' }}"
    ));
    let trimmed = out.trim().to_string();
    let failed = !ok || trimmed.to_uppercase().contains("ERR");
    // Anonymous read baseline on the NTFS layer (best-effort).
    let acl_note = if !failed && crate::settings::get().smb_guest_access() && read {
        let (aok, aout) = grant_root_read(&path);
        if !aok {
            format!("; ACL note (best-effort, skipped): {aout}")
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let res = ManagedShare {
        name: name.to_string(),
        path,
        state: if failed { "failed" } else { "created" },
        detail: if failed {
            trimmed.clone().chars().take(160).collect()
        } else {
            format!("已创建{acl_note}")
        },
    };
    if failed {
        log_error!(
            "smb.create_one",
            "E {name} ({}) fail: {}",
            res.path,
            if trimmed.is_empty() { "no output".into() } else { trimmed.clone() }
        );
    } else if !acl_note.is_empty() {
        log_warn!("smb.create_one", "{name} created ({}){}", res.path, acl_note);
    } else {
        log_info!("smb.create_one", "{name} created ({})", res.path);
    }
    res
}

/// Remove many managed shares in a single PowerShell invocation.
///
/// Each share is still probed (Get-SmbShare + path match) before removal so we
/// NEVER tear down a share the user created or re-pointed. Batching N removals
/// into one `-Command` collapses N cold PowerShell starts (~1.5 s each) into a
/// single start, so removing N drives costs ~1 cold start + per-share syscalls
/// instead of N x 2 cold starts (probe + remove each). Returns how many shares
/// were actually removed; per-share outcomes are logged.
fn remove_many(managed: &[(String, String)]) -> usize {
    if managed.is_empty() {
        return 0;
    }
    let frags: Vec<String> = managed
        .iter()
        .map(|(name, path)| {
            let nq = name.replace('\'', "''");
            let pq = path.replace('\'', "''");
            format!(
                "$s = Get-SmbShare -Name '{nq}' -ErrorAction SilentlyContinue; if ($s -and $s.Path -eq '{pq}') {{ Remove-SmbShare -Name '{nq}' -Force -ErrorAction SilentlyContinue; if ($?) {{ 'OK:{name}' }} else {{ 'ERR:{name}' }} }} else {{ 'SKIP:{name}' }}"
            )
        })
        .collect();
    let (ok, out) = ps(&frags.join("; "));
    let mut removed = 0;
    if ok {
        for line in out.lines() {
            let t = line.trim();
            if let Some(n) = t.strip_prefix("OK:") {
                removed += 1;
                log_info!("smb.remove_many", "{n} removed");
            } else if let Some(n) = t.strip_prefix("ERR:") {
                log_error!("smb.remove_many", "E {n} remove failed");
            } else if let Some(n) = t.strip_prefix("SKIP:") {
                log_warn!("smb.remove_many", "{n} skip: not matching managed path");
            }
        }
    } else {
        log_warn!(
            "smb.remove_many",
            "batch removal failed (ps ok=false, out={:?})",
            out.trim()
        );
    }
    removed
}

// ---------------------------------------------------------------------------
// lifecycle entry points used by /api/smb and boot/shutdown
// ---------------------------------------------------------------------------

/// Create all configured shares. Idempotent. Returns the per-share outcomes.
pub fn create_all() -> Vec<ManagedShare> {
    let settings = crate::settings::get();
    if !settings.smb_enabled() {
        log_warn!("smb.create_all", "disabled by config; no shares managed");
        let out = runtime().lock().unwrap().clone();
        return out;
    }
    let t0 = Instant::now();
    let base = settings.smb_default_grant().to_string();
    let items: Vec<(String, String, String)> = settings
        .smb_shares()
        .iter()
        .map(|s| {
            let name = s.name.trim().to_string();
            let grant = if s.grant.trim().is_empty() {
                base.clone()
            } else {
                s.grant.clone()
            };
            (name, s.path.clone(), grant)
        })
        .collect();
    // One batched existence probe for the whole configured set (was one
    // cold start per share just to learn "already" vs "create"). On probe
    // failure the hint stays None so each create_one_h re-probes by name.
    let names: Vec<&str> = items.iter().map(|(n, _, _)| n.as_str()).collect();
    let existing = shares_existing(&names);
    let mut results = Vec::new();
    for (name, path, grant) in &items {
        results.push(create_one_h(
            name,
            path,
            grant,
            existing
                .as_ref()
                .map(|s| s.contains(name.as_str())),
        ));
    }
    *runtime().lock().unwrap() = results.clone();
    let created = results.iter().filter(|r| r.state == "created").count();
    let failed = results.iter().filter(|r| r.state == "failed").count();
    log_info!(
        "smb.create_all",
        "done in {}ms: {} created / {} already / {} skipped / {} failed of {} configured",
        t0.elapsed().as_millis(),
        created,
        results.iter().filter(|r| r.state == "already").count(),
        results.iter().filter(|r| r.state == "skipped-offline").count(),
        failed,
        results.len()
    );
    results
}

/// Remove every share this session created (state == "created" or "already").
/// Used at shutdown. Best-effort; failures are logged, never fatal.
pub fn remove_managed() {
    let managed: Vec<(String, String)> = runtime()
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.state == "created" || s.state == "already")
        .map(|s| (s.name.clone(), s.path.clone()))
        .collect();
    log_info!("smb.remove_managed", "removing managed shares ({})", managed.len());
    remove_many(&managed);
    runtime().lock().unwrap().clear();
    log_info!("smb.remove_managed", "managed share set cleared");
}

/// Exposure-check helper: list whether our configured shares currently exist.
/// Used by /api/smb/status so the UI can show the live state.
pub fn snapshot() -> Vec<ManagedShare> {
    runtime().lock().unwrap().clone()
}

/// Mount or unmount a single drive letter. Mounting requires the drive to be
/// present right now; unmounting works even when the drive is gone (removes
/// the share + the persisted setting). The persisted letter set ([smb].shares)
/// is updated so the choice survives restarts and drives return on re-plug.
pub fn set_drive(letter: &str, mount: bool) -> Result<DriveInfo, String> {
    let letter = letter.trim().to_uppercase();
    if letter.len() != 1 || !letter.chars().next().unwrap().is_ascii_alphabetic() {
        log_error!("smb.set_drive", "E invalid letter spec {:?}", letter);
        return Err("盘符必须是单个字母（如 C、E）".into());
    }
    let root = normalized_root(&letter);
    let settings = crate::settings::get();
    let mut shares = settings.smb_shares();
    log_info!("smb.set_drive", "{} {letter} ({root})", if mount { "mount" } else { "unmount" });
    if mount {
        clear_denied(&letter);
        let present = list_drives().iter().any(|d| d.letter == letter);
        if !present {
            log_warn!("smb.set_drive", "E mount {letter}: drive not present");
            return Err(format!("盘 {letter}: 不在当前机器上（已插入？）"));
        }
        if !shares.iter().any(|s| s.name.eq_ignore_ascii_case(&letter)) {
            shares.push(crate::settings::SmbShareItem {
                name: letter.clone(),
                path: root.clone(),
                grant: String::new(),
            });
        }
        let _ = crate::settings::persist_smb(&shares, settings.smb_guest_access());
        let managed = create_one(&letter, &root, settings.smb_default_grant());
        push_runtime(managed);
    } else {
        note_manual_unmount(&letter);
        shares.retain(|s| !s.name.eq_ignore_ascii_case(&letter));
        let _ = crate::settings::persist_smb(&shares, settings.smb_guest_access());
        remove_many(&[(letter.clone(), root.clone())]);
        if settings.smb_guest_access() {
            let _ = revoke_root_read(&root);
        }
        drop_runtime(&letter);
    }
    let result = refresh_drive(letter);
    match &result {
        Ok(d) => {
            let dl = &d.letter;
            log_info!("smb.set_drive", "done {dl}: mounted={}", d.mounted)
        }
        Err(e) => log_warn!("smb.set_drive", "refresh E: {e}"),
    }
    result
}

/// Unmount every drive and clear the persisted set (the user said "全部不挂").
/// Returns how many shares were actually removed.
pub fn unmount_all() -> usize {
    let settings = crate::settings::get();
    let shares = settings.smb_shares();
    log_info!("smb.unmount_all", "unmounting {} configured share(s)", shares.len());
    let pairs: Vec<(String, String)> = shares
        .iter()
        .map(|s| (s.name.trim().to_string(), s.path.trim().to_string()))
        .filter(|(n, _)| !n.is_empty())
        .collect();
    let removed = remove_many(&pairs);
    if settings.smb_guest_access() {
        let paths: Vec<String> = pairs.iter().map(|(_, p)| p.clone()).collect();
        revoke_many(&paths);
    }
    // Respect "全部不挂": never auto-reshare these letters this session.
    // A manual mount (set_drive true) or a reboot clears the deny entry.
    for (n, _) in &pairs {
        note_manual_unmount(n);
    }
    // Also deny currently-present drives: unmount-all means "share nothing",
    // so a drive inserted afterwards must still be auto-shared, but drives
    // that were present at click time must stay unmounted.
    for d in list_drives() {
        note_manual_unmount(&d.letter);
    }
    let _ = crate::settings::persist_smb(&[], settings.smb_guest_access());
    runtime().lock().unwrap().clear();
    log_info!("smb.unmount_all", "done: {removed} of {} removed, set cleared", shares.len());
    removed
}

/// Pure decision: which present drives want (re-)sharing.
/// A drive is a candidate when it is fixed/removable (never network/CD),
/// not denied by an explicit unmount this session, and either never
/// configured (brand-new disk) or configured but its share is currently
/// missing (drive was offline at boot / share was reaped, now back).
/// Pure and unit-testable (no PS calls).
fn pending_auto_share(
    present: &[(String, &'static str, bool)],
    configured: &HashSet<String>,
    denied: &HashSet<String>,
) -> Vec<String> {
    present
        .iter()
        .filter(|(letter, kind, mounted)| {
            (*kind == "fixed" || *kind == "removable")
                && !denied.contains(&letter.trim().to_uppercase())
                && (!configured.contains(&letter.trim().to_uppercase()) || !mounted)
        })
        .map(|(letter, _, _)| letter.trim().to_uppercase())
        .collect()
}

/// One hotplug pass: share newly-appeared drives and re-share configured
/// drives whose share went missing (offline at boot, reaped residue).
/// Returns the per-share outcomes (empty when nothing to do). Skipped
/// entirely when [smb] management is disabled. Heavy (spawns PowerShell);
/// always call it off the async runtime via spawn_blocking.
pub fn auto_share_new_drives() -> Vec<ManagedShare> {
    if !crate::settings::get().smb_enabled() {
        return Vec::new();
    }
    let configured: HashSet<String> = crate::settings::get()
        .smb_shares()
        .iter()
        .map(|s| s.name.trim().to_uppercase())
        .collect();
    let denied: HashSet<String> = deny_set()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    // Single enumeration for this pass; set_drive re-enumerates per letter
    // but pending is normally empty (no new drive) so the cost is one pass.
    let drives = list_drives();
    // Visibility watchdog: C: always exists, so an empty enumeration means
    // the PowerShell probe failed, not "no disks". Log on change only.
    static LAST_COUNT: OnceLock<Mutex<Option<usize>>> = OnceLock::new();
    let last = LAST_COUNT.get_or_init(|| Mutex::new(None));
    let count = drives.len();
    let changed = last.lock().map(|mut g| {
        let c = *g != Some(count);
        *g = Some(count);
        c
    });
    if changed.unwrap_or(false) {
        if count == 0 {
            log_warn!("smb.hotplug", "drive enumeration returned empty (probe failed?)");
        } else {
            log_info!("smb.hotplug", "drive enumeration sees {count} drive(s)");
        }
    }
    let present: Vec<(String, &'static str, bool)> = drives
        .iter()
        .map(|d| (d.letter.clone(), d.kind, d.mounted))
        .collect();
    let mut out = Vec::new();
    for letter in pending_auto_share(&present, &configured, &denied) {
        log_info!("smb.hotplug", "new drive {letter} appeared, auto-sharing");        match set_drive(&letter, true) {
            Ok(d) => out.push(ManagedShare {
                name: d.letter,
                path: d.path,
                state: "created",
                detail: "新盘自动分享".into(),
            }),
            Err(e) => {
                log_warn!("smb.hotplug", "auto-share {letter} failed: {e}");
                out.push(ManagedShare {
                    name: letter,
                    path: String::new(),
                    state: "failed",
                    detail: e.chars().take(160).collect(),
                })
            }
        }
    }
    // OpenList 存储同步：与 SMB 分享相互独立（用户可能关 SMB 但要列表）。
    // 对本轮在线的每块 fixed/removable 盘 ensure 一次；模块内部有 SYNCED
    // 缓存（全命中零 I/O）+ 登录失败冷却，失败只记日志不影响上层。
    for line in crate::api::openlist::sync_all_present_drives() {
        log_info!("smb.hotplug", "openlist: {line}");
    }
    out
}

/// Background watcher: every 15 s, auto-share newly-inserted drives so
/// "插盘即分享" holds without a restart or manual mount. Cheap when idle
/// (one enumeration pass, no writes); writes only happen on actual arrival.
/// Never exits on failure.
pub async fn smb_hotplug_task() {
    const POLL_SECS: u64 = 15;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(POLL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let res = tokio::task::spawn_blocking(auto_share_new_drives).await;
        match res {
            Ok(shared) if !shared.is_empty() => {
                log_info!("smb.hotplug", "pass shared {} drive(s)", shared.len());
            }
            Err(e) => {
                log_warn!("smb.hotplug", "worker join failed: {e:?}");
            }
            _ => {}
        }
    }
}

fn push_runtime(ms: ManagedShare) {
    let mut g = runtime().lock().unwrap();
    g.retain(|x| !x.name.eq_ignore_ascii_case(&ms.name));
    g.push(ms);
}

fn drop_runtime(letter: &str) {
    let mut g = runtime().lock().unwrap();
    g.retain(|x| !x.name.eq_ignore_ascii_case(letter));
}

/// Re-read the live state of a single letter (after mount/unmount).
fn refresh_drive(letter: String) -> Result<DriveInfo, String> {
    let configured: Vec<String> = crate::settings::get()
        .smb_shares()
        .iter()
        .map(|s| s.name.trim().to_uppercase())
        .collect();
    let configured_letter = configured.contains(&letter);
    match list_drives().into_iter().find(|d| d.letter == letter) {
        Some(mut d) => {
            d.configured = configured_letter;
            Ok(d)
        }
        None => Err(format!("盘 {letter}: 未找到")),
    }
}

/// Remove leftover managed shares from an unclean previous run.
///
/// Windows SMB shares are system-scoped, not process-scoped: if the panel is
/// killed hard (task manager, console close, crash) the graceful-shutdown hook
/// that removes its shares never runs, so `C:\ -> \\host\C` etc. can survive
/// the process. This runs at boot BEFORE creating, and tears down any managed
/// letter-share it finds, so "open the app" always ends with a fresh share set
/// owned by a live panel instead of inheriting residue from a killed one.
/// Only configured letters are touched - Windows admin shares and anything the
/// user created outside the panel are never removed.
fn reconcile_stale_shares() {
    let t0 = Instant::now();
    let settings = crate::settings::get();
    let shares_cfg = settings.smb_shares();
    let names: Vec<&str> = shares_cfg
        .iter()
        .map(|s| s.name.trim())
        // Same quote policy as create_one_h (valid_share_name): a name we
        // would refuse to create is also a name we refuse to probe/remove.
        .filter(|n| valid_share_name(n))
        .collect();
    if names.is_empty() {
        return;
    }
    // One cold start probes AND removes every stale share (was two calls per
    // name: one Get-SmbShare probe + one Remove when found). Semantics are
    // unchanged: only configured letter-shares are touched, admin shares and
    // user-created shares outside [smb].shares are never removed.
    let frags: Vec<String> = names
        .iter()
        .map(|n| {
            format!(
                "$s = Get-SmbShare -Name '{n}' -ErrorAction SilentlyContinue; if ($s) {{ Remove-SmbShare -Name '{n}' -Force -ErrorAction SilentlyContinue; if ($?) {{ 'OK:{n}' }} else {{ 'ERR:{n}' }} }}"
            )
        })
        .collect();
    let (ok, out) = ps(&frags.join("; "));
    if !ok && out.trim().is_empty() {
        log_warn!("smb.reconcile_stale_shares", "batch reconcile failed (ps ok=false)");
    }
    let mut removed = 0usize;
    for line in out.lines() {
        let t = line.trim();
        if let Some(n) = t.strip_prefix("OK:") {
            removed += 1;
            log_info!("smb.reconcile_stale_shares", "stale {n} found (left by a previous run); removed");
        } else if let Some(n) = t.strip_prefix("ERR:") {
            log_error!("smb.reconcile_stale_shares", "E {n} stale remove fail");
        }
    }
    log_info!(
        "smb.boot",
        "step reconcile_stale_shares took {}ms ({} checked, {removed} removed)",
        t0.elapsed().as_millis(),
        names.len()
    );
}

/// Run the whole "open the app" step: leftover-share reconciliation + guest
/// baseline (if drives are being managed with guest access on) + admin
/// baseline + create configured.
pub fn open_app() {
    let t0 = Instant::now();
    log_info!("smb.open_app", "opening app: admin baseline + reconcile + guest + create");
    ensure_admin_shares();
    let settings = crate::settings::get();
    if settings.smb_enabled() {
        // Heal residue from a hard-killed previous run so the panel's share
        // set is always freshly created (not inherited stale shares).
        reconcile_stale_shares();
    }
    // create_all runs BEFORE the guest baseline is spawned. ALL PowerShell
    // invocations serialize on the process-wide PS lock inside
    // ps_with_timeout (the cross-inherited-pipe fix), so a baseline spawned
    // first would hold that lock for its whole secedit run (seconds up to
    // the 25 s cap) and delay share creation by exactly that long. The
    // baseline is machine-level policy and fully independent of share
    // creation, so it goes last and stays async / non-blocking.
    let r = create_all();
    let created = r.iter().filter(|s| s.state == "created").count();
    let failed = r.iter().filter(|s| s.state == "failed").count();
    log_info!(
        "smb.open_app",
        "boot share setup done: {created} created / {failed} failed / {} managed in {}ms",
        r.len(),
        t0.elapsed().as_millis()
    );
    if settings.smb_enabled() && settings.smb_guest_access() && !settings.smb_shares().is_empty() {
        // Apply the guest/anonymous baseline on a detached background thread.
        // It is slow (secedit export/configure on the SMB policy DB can take
        // 30-90s depending on machine state) and is NOT required for the shares
        // themselves to come up, so we no longer block boot on it: the shares
        // appear right away and the baseline settles in shortly after. A failure
        // here only means anonymous browse may need credentials; the panel still
        // works and logs the outcome.
        let spawned_at_ms = t0.elapsed().as_millis();
        thread::spawn(move || {
            let g0 = Instant::now();
            let (ok, msg) = apply_guest(true);
            log_info!(
                "smb.open_app",
                "guest/anon baseline (async, spawned {}ms into boot): {} ({}) in {}ms",
                spawned_at_ms,
                if ok { "applied" } else { "failed" },
                if msg.is_empty() { "ok".to_string() } else { msg.trim().to_string() },
                g0.elapsed().as_millis()
            );
        });
    }
}

/// Run the whole "stop the app" step: remove what we manage. The cleanup is
/// bounded by `CLOSE_CLEANUP_TIMEOUT` so that a slow share removal can never
/// drag (or dead-lock) the process exit.
pub fn close_app() {
    let t0 = Instant::now();
    log_info!("smb.close_app", "closing app: removing managed shares");
    // Run the batched removal on a worker thread and wait no longer than the
    // cap. Normal completion is a single ~2 s PowerShell call and rejoins
    // immediately; only a pathological drive hits the deadline, in which case
    // we log and move on so exit is never blocked.
    let handle = thread::spawn(remove_managed);
    let deadline_start = Instant::now();
    loop {
        if handle.is_finished() {
            break;
        }
        if deadline_start.elapsed() >= CLOSE_CLEANUP_TIMEOUT {
            log_warn!(
                "smb.close_app",
                "cleanup exceeded {}s; abandoning and continuing exit",
                CLOSE_CLEANUP_TIMEOUT.as_secs()
            );
            return; // leave the detached worker to finish on its own
        }
        thread::sleep(Duration::from_millis(50));
    }
    if let Err(e) = handle.join() {
        log_error!("smb.close_app", "E cleanup worker panicked: {e:?}");
    }
    log_info!("smb.close_app", "share removal complete in {}ms", t0.elapsed().as_millis());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_share_name_rejects_quotes_and_empty() {
        // Normal names pass.
        assert!(valid_share_name("FlyGo-C"));
        assert!(valid_share_name("Data Drive"));
        assert!(valid_share_name("c"));
        // Quote policy: single AND double quotes are refused, same rule on
        // the create and the reconcile path.
        assert!(!valid_share_name("Fly'Go"));
        assert!(!valid_share_name("Fly\"Go"));
        assert!(!valid_share_name("'"));
        assert!(!valid_share_name("\""));
        // Empty / whitespace-only names are refused too.
        assert!(!valid_share_name(""));
        assert!(!valid_share_name("   "));
    }

    #[test]
    fn access_flags_parses_grant_strings() {
        assert_eq!(access_flags("Everyone,READ"), (true, false));
        assert_eq!(access_flags("Everyone,CHANGE"), (true, true));
        assert_eq!(access_flags("Everyone,FULL"), (true, true));
        assert_eq!(access_flags("everyone,read,change"), (true, true));
        // A grant with no read still yields read-only, never an empty share.
        assert_eq!(access_flags(""), (true, false));
        assert_eq!(access_flags("X"), (true, false));
    }

    #[test]
    fn normalized_root_keeps_one_letter() {
        assert_eq!(normalized_root("c"), "C:\\");
        assert_eq!(normalized_root(" E "), "E:\\");
        assert_eq!(normalized_root("AB"), "A:\\");
        assert_eq!(normalized_root(""), " :\\");
    }

    #[test]
    fn drive_root_online_checks_the_machine() {
        // C: exists on any Windows box the panel targets.
        assert!(drive_root_online("C:\\") || super::drive_root_online("C:\\"));
        // A nonsense root is never "online".
        assert!(!drive_root_online("Z:\\"));
        assert!(!drive_root_online("this-is-not-a-drive"));
    }

    #[test]
    fn parse_drive_line_pure_accepts_fixed() {
        let d = parse_drive_line_pure("C:|NTFS|3|123.4|475.2|OS").expect("valid line");
        assert_eq!(d.letter, "C");
        assert_eq!(d.path, "C:\\");
        assert_eq!(d.fs, "NTFS");
        assert_eq!(d.kind, "fixed");
        assert_eq!(d.size_gb, 475.2);
        assert_eq!(d.free_gb, 123.4);
        assert_eq!(d.label, "OS");
    }

    #[test]
    fn parse_drive_line_pure_handles_removable_and_network() {
        let r = parse_drive_line_pure("E:|FAT32|2|8.0|32.0|USB").expect("removable");
        assert_eq!(r.kind, "removable");
        let n = parse_drive_line_pure("T:||4|10.0|100.0||").expect("network");
        assert_eq!(n.kind, "network");
        assert_eq!(n.fs, "");
        assert_eq!(n.label, "");
    }

    #[test]
    fn parse_drive_line_pure_tolerates_bad_numbers_and_lowercase() {
        // Unknown DriveType falls back to "other" instead of panicking.
        let o = parse_drive_line_pure("Q:|NTFS|5|10.0|100.0|RAM-ish").expect("dt=5");
        assert_eq!(o.kind, "other");
        // Non-numeric sizes degrade to 0.0 (never None: the drive still exists).
        let n = parse_drive_line_pure("D:|NTFS|3|abc|def|Data").expect("bad numbers");
        assert_eq!(n.kind, "fixed");
        assert_eq!(n.size_gb, 0.0);
        assert_eq!(n.free_gb, 0.0);
        // Lowercase letters with a trailing colon normalize to the upper letter.
        let l = parse_drive_line_pure("e:|FAT32|2|1.0|8.0|").expect("lowercase");
        assert_eq!(l.letter, "E");
        assert_eq!(l.path, "E:\\");
    }

    #[test]
    fn parse_drive_line_pure_rejects_junk() {
        assert!(parse_drive_line_pure("").is_none());
        assert!(parse_drive_line_pure("|||").is_none());
        assert!(parse_drive_line_pure("1:|NTFS|3|0|0|").is_none(), "digit letters rejected");
        assert!(parse_drive_line_pure("AB|NTFS|3|0|0|").is_none(), "two letters rejected");
    }

    #[test]
    fn drive_root_online_normalization_of_forward_slash() {
        // The check strips the letter and probes "<letter>:\\".
        assert!(super::drive_root_online("C:/"));
    }

    /// Guards the hidden-spawn pipe path (S3-C regression: the child ran but
    /// every ps() returned empty stdout, so share creation only *looked*
    /// done). Spawns real powershells, so #[ignore]d by default like the
    /// other live tests (parallel-suite load starves tight timeouts).
    /// Run explicitly: cargo test --bin flygo -- --ignored
    #[test]
    #[ignore]
    fn ps_hidden_echo_roundtrip() {
        let (ok, out) = ps("Write-Output 'HELLO-SINGLE'");
        assert!(ok, "single-quote echo failed: {out:?}");
        assert!(out.contains("HELLO-SINGLE"), "lost stdout: {out:?}");
        let (ok, out) = ps("Write-Output \"HELLO-DOUBLE\"");
        assert!(ok, "double-quote echo failed: {out:?}");
        assert!(out.contains("HELLO-DOUBLE"), "lost stdout: {out:?}");
    }

    #[test]
    fn base64_utf16le_matches_powershell_expectation() {
        // "A" -> UTF-16LE bytes 41 00 -> base64 "QQA=". If this is wrong,
        // -EncodedCommand decodes to garbage and every ps() call breaks.
        assert_eq!(base64_utf16le("A"), "QQA=");
        assert_eq!(base64_utf16le(""), "");
        // Round-trip spot check through pairs (3 bytes -> 4 chars, no pad).
        assert_eq!(base64_utf16le("AB").len(), 8);
    }

    // Live-PS test (spawns Get-CimInstance, ~seconds): #[ignore]d so the
    // default suite stays fast and doesn't contend with the other live
    // environment tests. Run explicitly: cargo test --bin flygo -- --ignored
    #[test]
    #[ignore]
    fn enum_script_delivers_parseable_json() {
        // The exact drive-enumeration script must survive the spawn path with
        // its double quotes intact (the -Command quoting bug killed it with a
        // ParserError: no drives seen, no hotplug, empty /api/smb).
        let script = r#"@(Get-CimInstance Win32_LogicalDisk -Filter "DriveType=2 OR DriveType=3 OR DriveType=4" | ForEach-Object {
            $t=[math]::Round($_.Size/1GB,1); $f=[math]::Round($_.FreeSpace/1GB,1)
            Write-Output ("{0}|{1}|{2}|{3}|{4}|{5}" -f $_.DeviceID,$_.FileSystem,$_.DriveType,$f,$t,$_.VolumeName)
        }) | ConvertTo-Json -Compress"#;
        let (ok, out) = ps(script);
        assert!(ok, "enum script failed: {out:?}");
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("enum output must be JSON");
        let items: Vec<&str> = match &v {
            serde_json::Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(),
            serde_json::Value::String(s) => vec![s.as_str()],
            _ => panic!("unexpected enum JSON shape: {v:?}"),
        };
        assert!(!items.is_empty(), "no drives enumerated");
        assert!(
            items.iter().any(|l| parse_drive_line_pure(l).is_some()),
            "no parseable drive line in {items:?}"
        );
    }
    // Heavy live-PS test: boots real powershells, so it is #[ignore]d by
    // default - running it inside the parallel suite starves the other live
    // environment tests' timeouts (keepalive/portproxy probes). Run it
    // explicitly: cargo test --bin flygo -- --ignored
    #[test]
    #[ignore]
    fn ps_concurrent_calls_all_roundtrip() {
        // 4 concurrent echo calls must ALL round-trip: guards the
        // cross-inherited-pipe failure (overlapping children holding each
        // other's write ends so EOF never arrives and output is dropped).
        // Kept small on purpose: each call boots a full powershell, and the
        // suite runs tests in parallel (heavy live-PS tests starve others'
        // timeouts - keepalive_session_probe_live flaked at 8).
        let mut hs = Vec::new();
        for i in 0..4 {
            hs.push(thread::spawn(move || {
                let marker = format!("MARK-{i}");
                let (ok, out) = ps(&format!("Write-Output '{marker}'"));
                (ok, out.contains(&marker))
            }));
        }
        for h in hs {
            let (ok, has) = h.join().expect("ps thread panicked");
            assert!(ok && has, "concurrent ps call lost its output");
        }
    }

    #[test]
    fn pending_auto_share_picks_only_new_fixed_removable() {
        let present = vec![
            ("C".to_string(), "fixed", true),
            ("D".to_string(), "fixed", false),
            ("E".to_string(), "removable", false),
            ("T".to_string(), "network", false),
            ("X".to_string(), "other", false),
        ];
        let configured: HashSet<String> = ["C".to_string()].into_iter().collect();
        let denied: HashSet<String> = HashSet::new();
        // D (fixed) + E (removable) are new; C already shared; network/other
        // never auto-share.
        assert_eq!(
            pending_auto_share(&present, &configured, &denied),
            vec!["D".to_string(), "E".to_string()]
        );
    }

    #[test]
    fn pending_auto_share_reshares_configured_but_missing() {
        // D configured but share gone (offline at boot, back now) -> re-share.
        // C configured and live -> untouched.
        let present = vec![("C".to_string(), "fixed", true), ("D".to_string(), "fixed", false)];
        let configured: HashSet<String> = ["C".to_string(), "D".to_string()].into_iter().collect();
        let denied: HashSet<String> = HashSet::new();
        assert_eq!(
            pending_auto_share(&present, &configured, &denied),
            vec!["D".to_string()]
        );
    }

    #[test]
    fn pending_auto_share_respects_deny_list() {
        let present = vec![("D".to_string(), "fixed", false), ("E".to_string(), "fixed", false)];
        let configured: HashSet<String> = ["D".to_string()].into_iter().collect();
        let denied: HashSet<String> = ["D".to_string()].into_iter().collect();
        // D explicitly unmounted this session stays off despite missing share.
        assert_eq!(
            pending_auto_share(&present, &configured, &denied),
            vec!["E".to_string()]
        );
    }

    #[test]
    fn pending_auto_share_empty_when_all_configured() {
        let present = vec![("C".to_string(), "fixed", true)];
        let configured: HashSet<String> = ["C".to_string()].into_iter().collect();
        let denied: HashSet<String> = HashSet::new();
        assert!(pending_auto_share(&present, &configured, &denied).is_empty());
    }

    #[test]
    fn parse_existing_answers_pairs_in_order() {
        let names = vec!["C", "E", "ADMIN$"];
        // Exactly the 0/1 lines pair with the names; order is the contract.
        let found = parse_existing_answers(&names, "0\n1\n0\n");
        assert!(!found.contains("C"));
        assert!(found.contains("E"));
        assert!(!found.contains("ADMIN$"));
    }

    #[test]
    fn parse_existing_answers_ignores_stray_output() {
        let names = vec!["C", "E"];
        // A rogue warning line between answers must not shift the pairing.
        let found = parse_existing_answers(&names, "WARNING: blah\n0\nnoise\n1\n");
        assert!(!found.contains("C"));
        assert!(found.contains("E"));
    }

    #[test]
    fn parse_existing_answers_tolerates_missing_answers() {
        let names = vec!["C", "E", "D"];
        // Only two answers arrived (truncated run): C seen absent, E seen
        // present, D simply unknown (absent from the set).
        let found = parse_existing_answers(&names, "0\n1\n");
        assert_eq!(found, ["E".to_string()].into_iter().collect());
        // No answers at all -> nothing claimed to exist.
        assert!(parse_existing_answers(&names, "").is_empty());
    }

    #[test]
    fn shares_existing_filters_bad_names_without_touching_ps() {
        // Quote-bearing/empty names are dropped; an all-empty list must not
        // spawn PowerShell at all (returns before any ps() call).
        assert_eq!(shares_existing(&["", "a'b"]), Some(HashSet::new()));
    }
}
