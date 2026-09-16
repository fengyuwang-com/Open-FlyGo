// log.rs - zero-dependency, file-backed operation logger for the FlyGo panel.
//
// Every non-trivial operation logs one structured line to a local file AND
// mirrors it to stderr (preserving the pre-existing stderr behavior) so that
// any suspicious behaviour ("why did the share stay after exit?", "why is it
// slow?") can be traced back step by step in seconds: timestamp, level, module,
// action, key data, result.
//
// Design goals:
//   * No new dependencies (no log/env_logger/fern crates): a global Mutex<File>
//     plus a couple of macros is plenty for operation-level logging.
//   * SINGLE sink: the whole crate routes through this one module so there is
//     exactly one Mutex<File> writing one `flygo.log` (no interleaving, one
//     rotation policy).
//   * Log file lives at <install-root>/data/logs/flygo.log (CEO-pinned path).
//     init() creates the directory and opens the file once. Until init() runs
//     the file side stays disabled (stderr still receives).
//   * Rollover: once flygo.log grows past ROLLOVER_BYTES it is rotated to
//     flygo.log.1, the previous .1 -> .2 etc., and the newest few are kept
//     (KEEP_ROTATIONS). The log never grows unbounded.
//   * Format (one line per record):
//       2026-08-20T19:40:00.123 INFO smb.close_app removing managed shares (2)
//
// All code comments are pure ASCII; runtime log text is ASCII too (董事长 rule:
// code/logs are English ASCII; UI copy stays UTF-8).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Max size of the active log file before it is rotated (5 MB).
const ROLLOVER_BYTES: u64 = 5 * 1024 * 1024;
/// How many rotated backups to keep (flygo.log.1 .. flygo.log.N).
const KEEP_ROTATIONS: usize = 5;
/// Active log file name (before any rotation suffix).
const LOG_FILE: &str = "flygo.log";

/// Global handle to the currently active log file (None until init()).
static FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
/// Directory holding the log file (for rollover naming); empty before init().
static DIR: OnceLock<Mutex<PathBuf>> = OnceLock::new();
/// Whether file logging is enabled (set true by init()).
static ENABLED: OnceLock<Mutex<bool>> = OnceLock::new();

fn file_lock() -> &'static Mutex<Option<File>> {
    FILE.get_or_init(|| Mutex::new(None))
}
fn dir_lock() -> &'static Mutex<PathBuf> {
    DIR.get_or_init(|| Mutex::new(PathBuf::new()))
}
fn enabled_lock() -> &'static Mutex<bool> {
    ENABLED.get_or_init(|| Mutex::new(false))
}

/// Initialize the file logger against an install root. Creates
/// `<root>/data/logs` if missing and opens `flygo.log` in append mode. Safe to
/// call once at startup; until this runs the file side stays disabled.
pub fn init(root: &Path) {
    let dir = root.join("data").join("logs");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        // Fall back to stderr-only: never crash the panel over a log file.
        eprintln!("[log] create_dir_all {} failed: {e}; file logging disabled", dir.display());
        *enabled_lock().lock().unwrap_or_else(|e| e.into_inner()) = false;
        return;
    }
    let path = dir.join(LOG_FILE);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => {
            *dir_lock().lock().unwrap_or_else(|e| e.into_inner()) = dir;
            *file_lock().lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
            *enabled_lock().lock().unwrap_or_else(|e| e.into_inner()) = true;
            eprintln!("[log] file logging -> {}", path.display());
        }
        Err(e) => {
            eprintln!("[log] open {} failed: {e}; file logging disabled", path.display());
            *enabled_lock().lock().unwrap_or_else(|e| e.into_inner()) = false;
        }
    }
}

/// Central append point. Builds one structured line and writes it to the file
/// (when enabled) and to stderr (always). Never panics on I/O failure - a log
/// problem must never take the panel down.
pub fn log(level: &str, module: &str, msg: &str) {
    let line = format!("{} {} {} {}\n", timestamp(), level, module, msg);
    eprint!("{line}");
    if !*enabled_lock().lock().unwrap_or_else(|e| e.into_inner()) {
        return;
    }
    let mut guard = file_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = guard.as_mut() else {
        return;
    };
    if should_rollover(f) {
        // Rotate away from under the held file handle, then reopen fresh.
        if let Some(active) = rollover() {
            *f = active;
        }
    }
    let _ = f.write_all(line.as_bytes());
    let _ = f.flush();
}

/// Rotate on disk and return a freshly opened active file. Runs while the file
/// mutex is held (the previous handle is closed first). Returns None if the
/// reopen fails.
fn rollover() -> Option<File> {
    let dir = dir_lock().lock().unwrap_or_else(|e| e.into_inner()).clone();
    if dir.as_os_str().is_empty() {
        return None;
    }
    // Drop the oldest backup first, then shift .N-1 -> .N .. .1 -> .2 right to
    // left, then move the active file to .1, and reopen a fresh active one.
    let _ = std::fs::remove_file(dir.join(format!("{LOG_FILE}.{KEEP_ROTATIONS}")));
    for n in (2..=KEEP_ROTATIONS).rev() {
        let from = dir.join(format!("{LOG_FILE}.{}", n - 1));
        let to = dir.join(format!("{LOG_FILE}.{n}"));
        let _ = std::fs::rename(&from, &to);
    }
    let _ = std::fs::rename(dir.join(LOG_FILE), dir.join(format!("{LOG_FILE}.1")));
    let path = dir.join(LOG_FILE);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(new) => Some(new),
        Err(e) => {
            eprintln!("[log] reopen {} failed: {e}", path.display());
            None
        }
    }
}

/// True when the current file handle has grown past the rollover threshold.
fn should_rollover(f: &File) -> bool {
    match f.metadata() {
        Ok(m) => m.len() >= ROLLOVER_BYTES,
        Err(_) => false,
    }
}

/// UTC timestamp with milliseconds, e.g. "2026-08-20T19:40:00.123".
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = secs / 86_400;
    // Civil-from-days (Howard Hinnant's algorithm), UTC.
    let z = days + 719_468;
    // `z` is u64 (days since epoch + offset), always positive, so the
    // signed-input guard of the original algorithm is omitted here.
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0,146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0,399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0,365]
    let mp = (5 * doy + 2) / 153; // [0,11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1,31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1,12]
    let year = if m <= 2 { y + 1 } else { y };
    let h = (secs % 86_400) / 3600;
    let mi = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{year:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}")
}

/// Convenience macros, each takes (module, format-args) and routes through
/// `log` with the matching severity. ASCII everywhere.
// Some compile targets (e.g. the launcher bin) never expand log_warn/log_debug,
// so rustc flags them unused. Keep the family complete and re-exported; the
// allow prevents cargo fix from pruning them and breaking other targets.
#[allow(unused_macros)]
macro_rules! log_info {
    ($mod:expr, $($arg:tt)*) => { crate::log::log("INFO", $mod, &format!($($arg)*)) };
}
#[allow(unused_macros)]
macro_rules! log_warn {
    ($mod:expr, $($arg:tt)*) => { crate::log::log("WARN", $mod, &format!($($arg)*)) };
}
#[allow(unused_macros)]
macro_rules! log_error {
    ($mod:expr, $($arg:tt)*) => { crate::log::log("ERROR", $mod, &format!($($arg)*)) };
}
#[allow(unused_macros)]
macro_rules! log_debug {
    ($mod:expr, $($arg:tt)*) => { crate::log::log("DEBUG", $mod, &format!($($arg)*)) };
}

// Re-export so `use crate::log::{log_info, ...}` brings the macros into scope.
// log_warn/log_debug are used by other modules via this re-export; do not prune.
#[allow(unused_imports)]
pub(crate) use {log_debug, log_error, log_info, log_warn};
