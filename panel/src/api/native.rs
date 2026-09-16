// /api/native — user-defined native shortcuts (N-01).
//
// The user adds ANY local program from the panel UI (exe / script / url):
// name + path + args + workdir + probes + autostart flag. Status lights,
// start/stop and boot autostart (FlyGo-<id> scheduled task) included.
//
// API (all behind the global auth guard like every other /api route):
//   GET    /api/native                 -> {ok, items: [status...]}
//   POST   /api/native                 -> body Shortcut -> {ok, id}
//   GET    /api/native/:id             -> {ok, item}
//   PUT    /api/native/:id             -> body Shortcut -> {ok, autostart_msg?}
//   DELETE /api/native/:id             -> {ok} (stops child + removes task, best-effort)
//   POST   /api/native/:id/start       -> {ok, pid, adopted}
//   POST   /api/native/:id/stop        -> {ok}
//   GET    /api/native/policy          -> {ok, allowed_roots}
//   PUT    /api/native/policy          -> {allowed_roots: [...]} -> {ok}
//
// Storage: data/native-shortcuts.json {version, items} + .bak backup,
// atomic tmp+rename write (same discipline as components.rs / sync.rs).
// Policy: data/native-policy.json {allowed_roots}. Missing file = install
// root only (secure by default; the user widens it in the UI).
//
// Spawning never goes through a shell for kind=exe (args array passed
// directly, so no command-injection surface by construction). kind=script
// runs via cmd /c (.bat/.cmd) or powershell -File (.ps1), and every spawn
// and stop is audit-logged with the full command line.
//
// Daemon naming: a managed native child is registered in DaemonState under
// "native:<id>" (see daemon.rs spawn_child / adopt / kill_all).
//
// All code comments are pure ASCII; user-facing messages may be UTF-8.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, TcpStream};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use crate::log::{log_error, log_info, log_warn};
use crate::{ApiError, ApiResult, AppState};

use crate::winproc::CREATE_NO_WINDOW;
const STORE_VERSION: u32 = 1;
/// Prefix for DaemonState child names owned by this module.
pub const DAEMON_PREFIX: &str = "native:";
/// Scheduled-task name for the autostart flag: FlyGo-<id>.
fn task_name(id: &str) -> String {
    format!("FlyGo-{id}")
}
pub fn daemon_name(id: &str) -> String {
    format!("{DAEMON_PREFIX}{id}")
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Shortcut {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// exe | script | url
    #[serde(default)]
    pub kind: String,
    /// exe/script: program to launch. exe kind: any .exe; script kind:
    /// .bat/.cmd/.ps1 file. Checked against the user whitelist + existence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// Passed as a real argv array for kind=exe (no shell involved).
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Optional TCP liveness probe (127.0.0.1:port, 300ms timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Optional process-name probe (case-insensitive substring over the
    /// process name, exe path, full command line AND working directory —
    /// the cwd match is what pins interpreter services like
    /// `node dist/index.js` to their project dir; same spirit as
    /// services.rs plus cmd/cwd for interpreter coverage).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needle: Option<String>,
    /// url kind only: http(s) link target (status light via port when set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub autostart: bool,
}

impl Default for Shortcut {
    fn default() -> Self {
        Shortcut {
            id: String::new(),
            label: String::new(),
            icon: None,
            kind: "exe".into(),
            exe: None,
            args: Vec::new(),
            workdir: None,
            port: None,
            needle: None,
            url: None,
            autostart: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Store {
    version: Option<u32>,
    #[serde(default)]
    items: Vec<Shortcut>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Policy {
    #[serde(default)]
    pub allowed_roots: Vec<String>,
}

// ---------------------------------------------------------------------------
// File I/O (path-parameterized so tests never touch the real install)
// ---------------------------------------------------------------------------

fn load_store(path: &std::path::Path) -> Store {
    match std::fs::read_to_string(path) {
        Err(_) => Store::default(),
        Ok(s) => serde_json::from_str::<Store>(&s).unwrap_or_default(),
    }
}

fn save_store(path: &std::path::Path, store: &Store) -> Result<(), String> {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let text =
        serde_json::to_string_pretty(store).map_err(|e| format!("serialize shortcuts failed: {e}"))?;
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("json.bak"));
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace {} failed: {e}", path.display())
    })?;
    Ok(())
}

fn load_policy(path: &std::path::Path) -> Policy {
    match std::fs::read_to_string(path) {
        Err(_) => Policy::default(),
        Ok(s) => serde_json::from_str::<Policy>(&s).unwrap_or_default(),
    }
}

fn save_policy(path: &std::path::Path, policy: &Policy) -> Result<(), String> {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let text =
        serde_json::to_string_pretty(policy).map_err(|e| format!("serialize policy failed: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace {} failed: {e}", path.display())
    })?;
    Ok(())
}

fn store_path() -> std::path::PathBuf {
    crate::config::get().native_path.clone()
}

fn policy_path() -> std::path::PathBuf {
    crate::config::get().native_policy_path.clone()
}

/// Effective whitelist roots. Empty policy file = install root only
/// (secure default; the user widens it from the UI).
fn effective_roots(policy: &Policy) -> Vec<std::path::PathBuf> {
    if policy.allowed_roots.is_empty() {
        return vec![crate::config::get().root.clone()];
    }
    policy
        .allowed_roots
        .iter()
        .map(std::path::PathBuf::from)
        .collect()
}

// ---------------------------------------------------------------------------
// Validation (pure: fully unit-tested, no disk writes)
// ---------------------------------------------------------------------------

fn check_id(id: &str) -> Option<String> {
    if id.is_empty() || id.len() > 64 {
        return Some("id 须为 1-64 字符".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Some("id 只允许字母数字 . _ -".into());
    }
    None
}

fn has_control(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// Canonicalize for prefix comparison. canonicalize() returns verbatim
/// \\?\ paths on Windows; both sides go through it so comparison is fair.
fn canonical(p: &str) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(p).ok()
}

fn under_whitelist(exe: &str, roots: &[std::path::PathBuf]) -> bool {
    let Some(c) = canonical(exe) else {
        return false;
    };
    roots.iter().any(|r| canonical(r.to_string_lossy().as_ref()).is_some_and(|rc| c.starts_with(&rc)))
}

pub fn validate(sc: &Shortcut, roots: &[std::path::PathBuf]) -> Vec<String> {
    let mut errs = Vec::new();
    if let Some(e) = check_id(&sc.id) {
        errs.push(format!("id: {e}"));
    }
    if sc.label.is_empty() || sc.label.len() > 64 || has_control(&sc.label) {
        errs.push("label 须为 1-64 字符且无控制字符".into());
    }
    match sc.kind.as_str() {
        "url" => {
            match &sc.url {
                Some(u) if (u.starts_with("http://") || u.starts_with("https://")) && !has_control(u) => {}
                _ => errs.push("url 须为 http(s):// 开头".into()),
            }
            if sc.exe.is_some() {
                errs.push("url 类型不应填 exe".into());
            }
        }
        "exe" | "script" => {
            let exe = sc.exe.clone().unwrap_or_default();
            if exe.is_empty() || has_control(&exe) {
                errs.push("exe 路径必填".into());
            } else if !std::path::Path::new(&exe).is_file() {
                errs.push(format!("exe 不存在: {exe}"));
            } else {
                let lower = exe.to_lowercase();
                if sc.kind == "exe" && !lower.ends_with(".exe") {
                    errs.push("exe 类型只允许 .exe 文件（node/python 请填解释器 exe，脚本走参数）".into());
                }
                if sc.kind == "script"
                    && !(lower.ends_with(".bat") || lower.ends_with(".cmd") || lower.ends_with(".ps1"))
                {
                    errs.push("script 类型只允许 .bat/.cmd/.ps1".into());
                }
                if !under_whitelist(&exe, roots) {
                    errs.push("exe 不在白名单目录内，请先在白名单页添加其所在目录".into());
                }
            }
            for (i, a) in sc.args.iter().enumerate() {
                if has_control(a) || a.contains('\0') {
                    errs.push(format!("args[{i}] 含控制字符"));
                }
            }
            if let Some(w) = &sc.workdir {
                if !std::path::Path::new(w).is_dir() {
                    errs.push(format!("workdir 不是有效目录: {w}"));
                }
            }
        }
        other => errs.push(format!("kind 非法: {other}（exe/script/url）")),
    }
    if let Some(p) = sc.port {
        if p == 0 {
            errs.push("port 须为 1-65535".into());
        }
    }
    if let Some(n) = &sc.needle {
        if n.is_empty() || n.len() > 64 || has_control(n) {
            errs.push("needle 须为 1-64 字符且无控制字符".into());
        }
    }
    errs
}

pub fn validate_policy(roots: &[String]) -> Vec<String> {
    let mut errs = Vec::new();
    if roots.len() > 32 {
        errs.push("白名单最多 32 条".into());
    }
    for (i, r) in roots.iter().enumerate() {
        let p = std::path::Path::new(r);
        if !p.is_absolute() {
            errs.push(format!("roots[{i}] 须为绝对路径"));
        } else if !p.is_dir() {
            errs.push(format!("roots[{i}] 目录不存在: {r}"));
        }
    }
    errs
}

// ---------------------------------------------------------------------------
// Lookups for daemon.rs (spawn branch + port kill path)
// ---------------------------------------------------------------------------

/// Fully-resolved spawn plan for a shortcut, re-validated at spawn time
/// (the whitelist may have narrowed since the shortcut was saved).
/// Daemon executes it verbatim; all policy lives here.
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub workdir: Option<std::path::PathBuf>,
    pub audit: String,
}

pub fn spawn_spec(id: &str) -> Result<SpawnSpec, String> {
    let sc = find_shortcut(id).ok_or_else(|| format!("unknown native shortcut: {id}"))?;
    if sc.kind == "url" {
        return Err(format!("{id} is a url shortcut; nothing to spawn"));
    }
    let errs = validate(&sc, &roots_now());
    if !errs.is_empty() {
        return Err(errs.join("; "));
    }
    let exe = sc.exe.clone().unwrap_or_default();
    let wd = sc.workdir.clone().map(std::path::PathBuf::from).or_else(|| {
        std::path::Path::new(&exe).parent().map(|p| p.to_path_buf())
    });
    let lower = exe.to_lowercase();
    let (program, args) = if sc.kind == "script" {
        if lower.ends_with(".ps1") {
            (
                "powershell.exe".to_string(),
                [
                    vec![
                        "-NoProfile".to_string(),
                        "-ExecutionPolicy".to_string(),
                        "Bypass".to_string(),
                        "-File".to_string(),
                        exe.clone(),
                    ],
                    sc.args.clone(),
                ]
                .concat(),
            )
        } else {
            // .bat/.cmd via cmd /c; the file itself is whitelisted, and the
            // full command line is audit-logged by the caller.
            (
                "cmd.exe".to_string(),
                [
                    vec!["/c".to_string(), format!("\"{exe}\"")],
                    sc.args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>(),
                ]
                .concat(),
            )
        }
    } else {
        (exe.clone(), sc.args.clone())
    };
    let audit = format!("{program} {}", args.join(" "));
    Ok(SpawnSpec { program, args, workdir: wd, audit })
}

/// Find a shortcut by id (loads the store; spawn-time frequency is fine).
pub fn find_shortcut(id: &str) -> Option<Shortcut> {
    load_store(&store_path()).items.into_iter().find(|s| s.id == id)
}

pub fn shortcut_port(id: &str) -> Option<u16> {
    find_shortcut(id).and_then(|s| s.port)
}

// ---------------------------------------------------------------------------
// Liveness probes (sysinfo needle + TCP port, same rules as services.rs)
// ---------------------------------------------------------------------------

fn needle_pids(needle: &str) -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, UpdateKind};
    let needle_l = needle.to_lowercase();
    let own = std::process::id();
    let mut sys = sysinfo::System::new();
    // NOTE: refresh_processes() skips cmd/cwd/environ; the interpreter
    // pinning below needs them explicitly (exe alone cannot tell two
    // node/python services apart).
    sys.refresh_processes_specifics(
        ProcessRefreshKind::new()
            .with_memory()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet),
    );
    let mut pids = Vec::new();
    for (pid, p) in sys.processes() {
        // Never match the panel itself: its own exe path / command line can
        // contain a needle (e.g. needle "FlyGo" vs C:\...\FlyGo\bin\flygo.exe).
        if pid.as_u32() == own {
            continue;
        }
        let name = p.name().to_lowercase();
        let exe = p.exe().map(|e| e.to_string_lossy().into_owned().to_lowercase());
        // Command line too: interpreter services (node/python) carry the
        // script path in argv, not in the exe path (e.g. node dist/index.js).
        let cmd = p.cmd().iter().map(|a| a.to_lowercase()).collect::<Vec<_>>().join(" ");
        // Working directory: pins an interpreter to its project dir.
        let cwd = p.cwd().map(|c| c.to_string_lossy().into_owned().to_lowercase());
        if name.contains(&needle_l)
            || exe.is_some_and(|e| e.contains(&needle_l))
            || cmd.contains(&needle_l)
            || cwd.is_some_and(|c| c.contains(&needle_l))
        {
            pids.push(pid.as_u32());
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn port_open(port: u16) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Owning pid of the process listening on a local TCP port (precise
/// pid<->service binding via the OS TCP table). None = nobody listening
/// or the lookup failed. Never returns the panel's own pid.
fn port_owner_pid(port: u16) -> Option<u32> {
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &format!(
                "(Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty OwningProcess)"
            ),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    // Confirm it is still alive before the caller trusts it.
    let alive = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false);
    alive.then_some(pid)
}

/// Adopt decision for an already-running service. Pure: unit-tested.
/// Port ownership is exact; the name probe only adopts on a UNIQUE match
/// (ambiguous matches are never recorded, so stop can never kill a
/// bystander like an editor with the same path in its command line).
enum Adopt {
    Pid(u32),
    RunningUnrecorded,
    Ambiguous,
}

fn choose_adopt(port_owner: Option<u32>, pids: &[u32]) -> Adopt {
    if let Some(pid) = port_owner {
        return Adopt::Pid(pid);
    }
    match pids {
        [pid] => Adopt::Pid(*pid),
        [] => Adopt::RunningUnrecorded, // port-only liveness, nothing to record
        _ => Adopt::Ambiguous,
    }
}

struct Live {
    running: bool,
    pids: Vec<u32>,
    managed: bool,
    managed_pid: u32,
}

/// Pure predicate (unit-tested): when a port probe is configured it is the
/// service identity — if the port is closed, multiple needle matches are
/// bystanders (e.g. an editor whose cwd contains the needle), not the
/// service. A single needle match still counts (the service may simply not
/// listen).
fn needle_alive(pids: usize, port_configured: bool, port_open: bool) -> bool {
    match pids {
        0 => false,
        1 => true,
        _ => !port_configured || port_open,
    }
}

fn probe(sc: &Shortcut, daemon: &crate::daemon::DaemonState) -> Live {
    let cs = daemon.status(&daemon_name(&sc.id));
    let mut pids = Vec::new();
    if let Some(n) = &sc.needle {
        pids = needle_pids(n);
    }
    let open = sc.port.map(port_open).unwrap_or(false);
    Live {
        running: (cs.running && cs.pid > 0)
            || needle_alive(pids.len(), sc.port.is_some(), open)
            || open,
        pids,
        managed: cs.running,
        managed_pid: cs.pid,
    }
}

// ---------------------------------------------------------------------------
// Scheduled-task autostart (FlyGo-<id>, current user, no admin needed)
// ---------------------------------------------------------------------------

fn ps_run(script: &str) -> Result<String, String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("powershell 启动失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "计划任务操作失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn quote_arg(a: &str) -> String {
    if a.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", a.replace('"', "\"\""))
    } else {
        a.to_string()
    }
}

/// Create/enable the logon task for an autostart shortcut. Working directory
/// is set natively via the scheduled-task Action (no cmd wrapper, no shell).
fn sync_autostart_task(sc: &Shortcut) -> Result<String, String> {
    let exe = sc.exe.clone().unwrap_or_default();
    let tn = task_name(&sc.id);
    let action_args = sc.args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    let wd = sc.workdir.clone().unwrap_or_else(|| {
        std::path::Path::new(&exe)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let esc = |s: &str| s.replace('\'', "''");
    let script = format!(
        "$a=New-ScheduledTaskAction -Execute '{}' -Argument '{}' -WorkingDirectory '{}'; \
         $t=New-ScheduledTaskTrigger -AtLogOn; \
         Register-ScheduledTask -TaskName '{}' -Action $a -Trigger $t -Force | Out-Null; \
         schtasks /change /tn '{}' /enable | Out-Null; 'ok'",
        esc(&exe),
        esc(&action_args),
        esc(&wd),
        esc(&tn),
        esc(&tn)
    );
    ps_run(&script)?;
    log_info!("native.autostart", "task {tn} registered for {}", sc.id);
    Ok(format!("开机自启任务 {tn} 已创建"))
}

fn remove_autostart_task(id: &str) {
    let tn = task_name(id).replace('\'', "''");
    match ps_run(&format!("schtasks /delete /tn '{tn}' /f | Out-Null; 'ok'")) {
        Ok(_) => log_info!("native.autostart", "task FlyGo-{id} removed"),
        Err(e) => log_warn!("native.autostart", "remove task FlyGo-{id} failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatusOut {
    id: String,
    label: String,
    icon: Option<String>,
    kind: String,
    url: Option<String>,
    port: Option<u16>,
    autostart: bool,
    running: bool,
    managed: bool,
    pids: Vec<u32>,
}

fn status_of(sc: &Shortcut, daemon: &crate::daemon::DaemonState) -> StatusOut {
    let live = probe(sc, daemon);
    StatusOut {
        id: sc.id.clone(),
        label: sc.label.clone(),
        icon: sc.icon.clone(),
        kind: sc.kind.clone(),
        url: sc.url.clone(),
        port: sc.port,
        autostart: sc.autostart,
        running: live.running,
        managed: live.managed,
        pids: live.pids,
    }
}

/// GET /api/native
pub async fn list(State(st): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    st.daemon.refresh();
    let items: Vec<StatusOut> = load_store(&store_path())
        .items
        .iter()
        .map(|s| status_of(s, &st.daemon))
        .collect();
    Ok(Json(serde_json::json!({ "ok": true, "items": items })))
}

fn ok_msg(msg: &str) -> serde_json::Value {
    serde_json::json!({ "ok": true, "message": msg })
}
fn ok_id(id: &str) -> serde_json::Value {
    serde_json::json!({ "ok": true, "id": id })
}
fn bad(errs: &[String]) -> serde_json::Value {
    serde_json::json!({ "ok": false, "errors": errs })
}

fn roots_now() -> Vec<std::path::PathBuf> {
    let p = load_policy(&policy_path());
    effective_roots(&p)
}

/// POST /api/native (body = Shortcut JSON)
pub async fn add(State(st): State<AppState>, body: String) -> ApiResult<Json<serde_json::Value>> {
    let sc: Shortcut = serde_json::from_str(&body)
        .map_err(|e| ApiError(format!("JSON 解析失败: {e}")))?;
    let errs = validate(&sc, &roots_now());
    if !errs.is_empty() {
        return Ok(Json(bad(&errs)));
    }
    let path = store_path();
    let mut store = load_store(&path);
    if store.items.iter().any(|s| s.id == sc.id) {
        return Ok(Json(bad(&[format!("id 已存在: {}", sc.id)])));
    }
    let _ = st; // daemon untouched on add; autostart sync below needs no daemon
    let msg = if sc.autostart && sc.kind != "url" {
        sync_autostart_task(&sc).unwrap_or_else(|e| format!("自启任务创建失败（条目已保存）: {e}"))
    } else {
        String::new()
    };
    store.version = Some(STORE_VERSION);
    store.items.push(sc.clone());
    save_store(&path, &store).map_err(ApiError)?;
    log_info!("native.add", "shortcut {} added ({})", sc.id, sc.kind);
    let mut r = ok_id(&sc.id);
    if !msg.is_empty() {
        r["autostart_msg"] = serde_json::Value::String(msg);
    }
    Ok(Json(r))
}

/// GET /api/native/:id
pub async fn one(Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    match load_store(&store_path()).items.into_iter().find(|s| s.id == id) {
        Some(sc) => Ok(Json(serde_json::json!({ "ok": true, "item": sc }))),
        None => Ok(Json(serde_json::json!({ "ok": false, "error": format!("不存在: {id}") }))),
    }
}

/// PUT /api/native/:id (body = Shortcut JSON; id in body must match path)
pub async fn update(Path(id): Path<String>, body: String) -> ApiResult<Json<serde_json::Value>> {
    let sc: Shortcut = serde_json::from_str(&body)
        .map_err(|e| ApiError(format!("JSON 解析失败: {e}")))?;
    if sc.id != id {
        return Ok(Json(bad(&["body.id 与路径 id 不一致".into()])));
    }
    let errs = validate(&sc, &roots_now());
    if !errs.is_empty() {
        return Ok(Json(bad(&errs)));
    }
    let path = store_path();
    let mut store = load_store(&path);
    let pos = match store.items.iter().position(|s| s.id == id) {
        Some(p) => p,
        None => return Ok(Json(bad(&[format!("不存在: {id}")] ))),
    };
    let was_auto = store.items[pos].autostart;
    store.items[pos] = sc.clone();
    save_store(&path, &store).map_err(ApiError)?;
    log_info!("native.update", "shortcut {id} updated");
    let mut r = ok_msg("已保存");
    if sc.autostart && sc.kind != "url" {
        r["autostart_msg"] = serde_json::Value::String(
            sync_autostart_task(&sc).unwrap_or_else(|e| format!("自启任务同步失败: {e}")),
        );
    } else if was_auto && !sc.autostart {
        remove_autostart_task(&id);
        r["autostart_msg"] = serde_json::Value::String("开机自启任务已删除".into());
    }
    Ok(Json(r))
}

/// DELETE /api/native/:id
pub async fn remove(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let path = store_path();
    let mut store = load_store(&path);
    let pos = match store.items.iter().position(|s| s.id == id) {
        Some(p) => p,
        None => return Ok(Json(bad(&[format!("不存在: {id}")] ))),
    };
    // Best-effort: stop the child and drop its logon task before forgetting it.
    let name = daemon_name(&id);
    let _ = st.daemon.stop(&name);
    remove_autostart_task(&id);
    store.items.remove(pos);
    save_store(&path, &store).map_err(ApiError)?;
    log_info!("native.remove", "shortcut {id} removed");
    Ok(Json(ok_msg("已删除")))
}

/// POST /api/native/:id/start
pub async fn start(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let sc = match find_shortcut(&id) {
        Some(s) => s,
        None => return Ok(Json(bad(&[format!("不存在: {id}")] ))),
    };
    if sc.kind == "url" {
        return Ok(Json(bad(&["url 类型无需启动，直接打开链接".into()])));
    }
    // The whitelist may have narrowed since the shortcut was saved.
    let errs = validate(&sc, &roots_now());
    if !errs.is_empty() {
        return Ok(Json(bad(&errs)));
    }
    st.daemon.refresh();
    let live = probe(&sc, &st.daemon);
    let name = daemon_name(&id);
    if live.running {
        // Adopt an already-running process instead of double-spawning —
        // but only on exact evidence (see choose_adopt). Anything else is
        // reported as running WITHOUT recording a pid, so a later stop
        // can never kill an unrelated process.
        if !live.managed {
            let owner = sc.port.and_then(port_owner_pid);
            match choose_adopt(owner, &live.pids) {
                Adopt::Pid(pid) => {
                    if st.daemon.adopt(&name, pid) {
                        log_info!("native.start", "{id} adopted running pid {pid}");
                        return Ok(Json(serde_json::json!({ "ok": true, "pid": pid, "adopted": true })));
                    }
                }
                Adopt::Ambiguous => {
                    return Ok(Json(serde_json::json!({
                        "ok": true, "adopted": false,
                        "message": "已在运行，但多个进程匹配（未认领，不会误杀）；如需面板启停请配置端口探针"
                    })));
                }
                Adopt::RunningUnrecorded => {}
            }
            return Ok(Json(serde_json::json!({ "ok": true, "adopted": true, "message": "已在运行" })));
        }
        return Ok(Json(serde_json::json!({ "ok": true, "message": "已在运行" })));
    }
    match st.daemon.start(&name) {
        Ok(pid) => Ok(Json(serde_json::json!({ "ok": true, "pid": pid, "adopted": false }))),
        Err(e) => {
            log_error!("native.start", "{id} start failed: {e}");
            Ok(Json(bad(&[e])))
        }
    }
}

/// POST /api/native/:id/stop
pub async fn stop(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let sc = match find_shortcut(&id) {
        Some(s) => s,
        None => return Ok(Json(bad(&[format!("不存在: {id}")] ))),
    };
    st.daemon.refresh();
    let name = daemon_name(&id);
    let live = probe(&sc, &st.daemon);
    // Exact recorded pid first. Otherwise the port is the service identity
    // (precise even when several services share one interpreter, e.g. two
    // node apps); the name probe is the last resort when no port is set.
    if live.managed_pid > 0 {
        let _ = crate::daemon::taskkill_pid(live.managed_pid);
    } else if let Some(port) = sc.port {
        let _ = crate::daemon::kill_stale_on_port(&port.to_string());
    } else if live.pids.len() == 1 {
        let _ = crate::daemon::taskkill_pid(live.pids[0]);
    } else if !live.pids.is_empty() {
        // Refuse: killing every name match could take down bystanders.
        return Ok(Json(bad(&["多个进程匹配且无端口探针，拒绝停止（请先配置端口探针精确定位）".into()])));
    }
    let _ = st.daemon.stop(&name); // clears the record (idempotent)
    log_info!("native.stop", "{id} stopped (managed_pid {}, probe_pids {:?})", live.managed_pid, live.pids);
    Ok(Json(ok_msg("已停止")))
}

/// GET /api/native/policy
pub async fn get_policy() -> ApiResult<Json<serde_json::Value>> {
    let p = load_policy(&policy_path());
    let roots: Vec<String> = if p.allowed_roots.is_empty() {
        vec![crate::config::get().root.to_string_lossy().into_owned()]
    } else {
        p.allowed_roots.clone()
    };
    Ok(Json(serde_json::json!({ "ok": true, "allowed_roots": roots, "is_default": p.allowed_roots.is_empty() })))
}

/// PUT /api/native/policy {allowed_roots: [...]}
pub async fn put_policy(body: String) -> ApiResult<Json<serde_json::Value>> {
    let p: Policy = serde_json::from_str(&body)
        .map_err(|e| ApiError(format!("JSON 解析失败: {e}")))?;
    let errs = validate_policy(&p.allowed_roots);
    if !errs.is_empty() {
        return Ok(Json(bad(&errs)));
    }
    save_policy(&policy_path(), &p).map_err(ApiError)?;
    log_info!("native.policy", "whitelist roots updated ({} entries)", p.allowed_roots.len());
    Ok(Json(ok_msg("白名单已保存")))
}

// ---------------------------------------------------------------------------
// Unit tests (pure logic + temp files; never touch the real install)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("flygo-native-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn roots_of(d: &std::path::Path) -> Vec<std::path::PathBuf> {
        vec![d.to_path_buf()]
    }

    fn good_exe(d: &std::path::Path) -> Shortcut {
        // A fake .exe under the whitelisted root (existence is what matters).
        let exe = d.join("tools").join("srv.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"MZ").unwrap();
        Shortcut {
            id: "fengweb".into(),
            label: "FengWeb".into(),
            kind: "exe".into(),
            exe: Some(exe.to_string_lossy().into_owned()),
            args: vec!["dist/index.js".into()],
            workdir: Some(d.to_string_lossy().into_owned()),
            port: Some(23456),
            needle: Some("srv".into()),
            ..Default::default()
        }
    }

    #[test]
    fn validate_accepts_good_exe_shortcut() {
        let d = tmpdir("good");
        let sc = good_exe(&d);
        assert!(validate(&sc, &roots_of(&d)).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn validate_rejects_bad_fields() {
        let d = tmpdir("bad");
        let mut sc = good_exe(&d);
        sc.id = "has space".into();
        sc.kind = "daemon".into();
        sc.port = Some(0);
        let errs = validate(&sc, &roots_of(&d));
        assert!(errs.iter().any(|e| e.starts_with("id:")));
        assert!(errs.iter().any(|e| e.contains("kind 非法")));
        assert!(errs.iter().any(|e| e.contains("port")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn validate_rejects_exe_outside_whitelist_and_missing() {
        let d = tmpdir("wl");
        let other = tmpdir("other");
        let mut sc = good_exe(&other);
        // exists but outside the whitelisted root
        let errs = validate(&sc, &roots_of(&d));
        assert!(errs.iter().any(|e| e.contains("白名单")), "{errs:?}");
        // missing file
        sc.exe = Some(other.join("nope.exe").to_string_lossy().into_owned());
        let errs = validate(&sc, &roots_of(&other));
        assert!(errs.iter().any(|e| e.contains("不存在")), "{errs:?}");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn validate_rejects_wrong_extension_and_bad_url() {
        let d = tmpdir("ext");
        let mut sc = good_exe(&d);
        let dll = d.join("x.dll");
        std::fs::write(&dll, b"MZ").unwrap();
        sc.exe = Some(dll.to_string_lossy().into_owned());
        assert!(validate(&sc, &roots_of(&d)).iter().any(|e| e.contains(".exe")));
        sc.kind = "url".into();
        sc.exe = None;
        sc.url = Some("ftp://x".into());
        assert!(validate(&sc, &roots_of(&d)).iter().any(|e| e.contains("http")));
        sc.url = Some("https://example.com/".into());
        assert!(validate(&sc, &roots_of(&d)).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn validate_policy_rejects_relative_and_missing_dirs() {
        let d = tmpdir("pol");
        assert!(validate_policy(&[d.to_string_lossy().into_owned()]).is_empty());
        let errs = validate_policy(&["relative/dir".into()]);
        assert!(errs.iter().any(|e| e.contains("绝对路径")));
        let errs = validate_policy(&[d.join("nope").to_string_lossy().into_owned()]);
        assert!(errs.iter().any(|e| e.contains("不存在")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn store_roundtrip_over_temp_path() {
        let d = tmpdir("store");
        let p = d.join("native-shortcuts.json");
        let sc = good_exe(&d);
        let store = Store { version: Some(STORE_VERSION), items: vec![sc] };
        save_store(&p, &store).unwrap();
        assert!(p.exists() && !p.with_extension("json.tmp").exists());
        // second save writes a .bak of the first
        save_store(&p, &store).unwrap();
        assert!(p.with_extension("json.bak").exists());
        let back = load_store(&p);
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].id, "fengweb");
        // corrupt file -> default (never crash the panel)
        std::fs::write(&p, b"{oops").unwrap();
        assert!(load_store(&p).items.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn quote_arg_only_quotes_when_needed() {
        assert_eq!(quote_arg("dist/index.js"), "dist/index.js");
        assert_eq!(quote_arg("C:\\My Dir\\x.js"), "\"C:\\My Dir\\x.js\"");
    }

    #[test]
    fn task_and_daemon_names_are_stable() {
        assert_eq!(task_name("fengweb"), "FlyGo-fengweb");
        assert_eq!(daemon_name("fengweb"), "native:fengweb");
    }

    #[test]
    fn choose_adopt_prefers_port_owner_and_rejects_ambiguity() {
        assert!(matches!(choose_adopt(Some(11), &[22, 33]), Adopt::Pid(11)));
        assert!(matches!(choose_adopt(None, &[22]), Adopt::Pid(22)));
        assert!(matches!(choose_adopt(None, &[]), Adopt::RunningUnrecorded));
        assert!(matches!(choose_adopt(None, &[22, 33]), Adopt::Ambiguous));
    }

    #[test]
    fn needle_liveness_respects_port_identity() {
        // No port probe: any needle match keeps the entry alive.
        assert!(needle_alive(3, false, false));
        // Port configured and closed: a single match may be a non-listener,
        // multiple matches are bystanders (AIExport vs ZCode cwd case).
        assert!(needle_alive(1, true, false));
        assert!(!needle_alive(3, true, false));
        // Port open: alive regardless of match count.
        assert!(needle_alive(3, true, true));
        // No matches: no needle signal (an open port is OR'd separately
        // in probe()).
        assert!(!needle_alive(0, false, true));
    }
}
