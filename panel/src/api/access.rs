// access.rs - /api/access/* - firewall access control (philosophy #6).
//
// Which network ranges may reach this machine's services is configurable
// here. Each "allow" is materialized as a Windows Defender Firewall inbound
// rule named FlyGo-LAN-* / FlyGo-TS-* / FlyGo-ALL-* (existing MVP-era rules
// follow the same FlyGo-* prefix and show up in the list automatically).
//
//   GET    /api/access/rules  -> list FlyGo-* rules (no admin needed)
//   POST   /api/access/allow  -> add one allow rule (admin needed)
//   DELETE /api/access/allow  -> remove one allow rule (admin needed)
//
// Scope -> remoteip mapping:
//   local     -> no rule at all (loopback 127.0.0.1 is always allowed)
//   tailscale -> remoteip 100.64.0.0/10
//   lan       -> remoteip LocalSubnet, or a user-supplied CIDR (e.g. 192.168.1.0/24)
//   all       -> remoteip any (0.0.0.0/0); requires explicit=true from the UI
//
// Elevation: firewall writes need admin. When the panel runs non-elevated
// we spawn one elevated PowerShell (UAC prompt; same pattern as the tray
// portproxy/eject paths) that runs netsh and writes the outcome to a temp
// file; the handler polls it for a few seconds so the UI gets the real
// result. When the panel already runs elevated, the same script runs
// inline with no UAC. Reading rules needs no elevation.
//
// All code comments are pure ASCII; JSON payloads are UTF-8.

use crate::ApiResult;
use crate::log::{log_error, log_info};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// CREATE_NO_WINDOW: powershell.exe / netsh are console apps; spawned from a
/// GUI parent they flash a black window unless this flag is set.
use crate::winproc::CREATE_NO_WINDOW;

/// Elevated helper script + result file, both in the current user's %TEMP%
/// (same %TEMP% for the elevated same-user process, so no path mismatch).
const OP_SCRIPT: &str = "flygo_access_op.ps1";
/// Result file the elevated helper writes. pub(crate): dockernet.rs builds
/// its own elevated script but must write the same result file for run_op's
/// reader (libutil#run_op) to pick it up.
pub(crate) const OP_RESULT: &str = "flygo_access_op.txt";
/// Seconds to wait for the UAC prompt while a non-elevated panel writes.
const UAC_WAIT_SECS: u64 = 12;

// ---------- data structures ----------

#[derive(Serialize, Clone)]
pub struct AccessRule {
    /// Firewall rule DisplayName, e.g. FlyGo-LAN-8388-Panel.
    pub name: String,
    /// local | tailscale | lan | all (derived from the name prefix + remote).
    pub scope: String,
    /// TCP | UDP (netsh protocol string, uppercased).
    pub protocol: String,
    /// Local port spec: single port, list or range.
    pub port: String,
    /// Remote addresses: Any / LocalSubnet / CIDR (netmask normalized to /n).
    pub remote: String,
    /// Rule enabled state (True/False from the firewall).
    pub enabled: bool,
    /// Inbound | Outbound.
    pub direction: String,
}

#[derive(Serialize)]
pub struct RulesOut {
    pub ok: bool,
    /// Whether the panel process itself runs elevated (UI hint for UAC).
    pub elevated: bool,
    pub rules: Vec<AccessRule>,
}

#[derive(Deserialize)]
pub struct AllowReq {
    /// Port spec: "8388" | "8388,8788" | "8000-9000".
    pub port: String,
    /// Legacy single scope. Kept for older clients; `scopes` takes precedence.
    #[serde(default)]
    pub scope: String,
    /// Multiple scopes may be submitted in one request (for example tailscale + lan).
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Optional CIDR for scope=lan (empty => LocalSubnet).
    #[serde(default)]
    pub cidr: String,
    /// Optional friendly label appended to the rule name (safe charset only).
    #[serde(default)]
    pub name: String,
    /// scope=all requires explicit=true (the UI shows a red warning).
    #[serde(default)]
    pub explicit: bool,
}

#[derive(Deserialize)]
pub struct DelReq {
    /// Rule DisplayName to remove (must start with "FlyGo-").
    pub name: String,
}

#[derive(Serialize)]
pub struct AccessCtl {
    pub ok: bool,
    pub message: String,
}

// ---------- small helpers ----------

/// Run a PowerShell snippet and return its stdout (UTF-8, BOM stripped).
/// pub(crate): dockernet.rs probes the firewall rule set with the same helper.
pub(crate) fn ps(script: &str) -> String {
    let exp = format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; {script}");
    let out = Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &exp])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim_start_matches('\u{feff}').trim().to_string()
        }
        Err(_) => String::new(),
    }
}

/// Is the current process running elevated?
pub(crate) fn is_elevated() -> bool {
    let out = ps(
        "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent())\
         .IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    );
    out.eq_ignore_ascii_case("True")
}

/// Port spec charset + value check: comma-separated ports / dash ranges,
/// every token 1-65535. Rejects anything that could break a netsh command.
fn valid_port_spec(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return false;
        }
        for num in part.split('-') {
            if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            match num.parse::<u16>() {
                Ok(n) if n > 0 => {}
                _ => return false,
            }
        }
    }
    true
}

/// CIDR check: dotted quad + /prefix (0-32). Nothing else is accepted.
/// pub(crate): dockernet.rs validates the system-level Docker allowed CIDR
/// with the same rule.
pub(crate) fn valid_cidr(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 2 {
        return false;
    }
    let octets: Vec<&str> = parts[0].split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    for o in &octets {
        if o.is_empty() || !o.chars().all(|c| c.is_ascii_digit()) || o.parse::<u8>().is_err() {
            return false;
        }
    }
    matches!(parts[1].parse::<u8>(), Ok(p) if p <= 32)
}

/// Optional friendly label: ASCII alphanumeric + . _ - , max 24 chars.
fn valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Scope tag -> rule name segment (also used as the name prefix).
fn scope_tag(scope: &str) -> Option<&'static str> {
    match scope {
        "tailscale" => Some("TS"),
        "lan" => Some("LAN"),
        "all" => Some("ALL"),
        _ => None,
    }
}

/// Build the DisplayName: FlyGo-<TAG>-<port>[-<label>].
fn rule_name(tag: &str, port: &str, label: &str) -> String {
    if label.is_empty() {
        format!("FlyGo-{tag}-{port}")
    } else {
        format!("FlyGo-{tag}-{port}-{label}")
    }
}

/// remoteip value for a scope (validated upstream).
fn remote_for(scope: &str, cidr: &str) -> String {
    match scope {
        "tailscale" => "100.64.0.0/10".to_string(),
        "all" => "any".to_string(),
        _ => {
            if cidr.is_empty() {
                "LocalSubnet".to_string()
            } else {
                cidr.to_string()
            }
        }
    }
}

/// Normalize a netmask-form address for display: "1.2.3.0/255.255.255.0" ->
/// "1.2.3.0/24". Plain "Any"/"LocalSubnet"/CIDR pass through untouched.
fn norm_remote(r: &str) -> String {
    let r = r.trim();
    if let Some((ip, mask)) = r.split_once('/') {
        if mask.contains('.') {
            let mut bits: u32 = 0;
            let mut ok = true;
            for o in mask.split('.') {
                match o.parse::<u8>() {
                    Ok(v) => bits += v.count_ones(),
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                return format!("{ip}/{bits}");
            }
        }
    }
    r.to_string()
}

/// Classify a rule: name prefix wins (that is what we set), then fall back
/// to the remote addresses for rules created by older tooling.
fn detect_scope(name: &str, remote: &str) -> String {
    let n = name.to_ascii_uppercase();
    if n.starts_with("FlyGo-TS-") {
        return "tailscale".to_string();
    }
    if n.starts_with("FlyGo-ALL-") {
        return "all".to_string();
    }
    if n.starts_with("FlyGo-LAN-") {
        return "lan".to_string();
    }
    let r = norm_remote(remote);
    if r.eq_ignore_ascii_case("Any") {
        return "all".to_string();
    }
    if r == "100.64.0.0/10" {
        return "tailscale".to_string();
    }
    "lan".to_string()
}

// ---------- listing ----------

/// Max age of a cached rule listing. Short enough that an external firewall
/// change is reflected almost immediately; long enough that repeatedly opening
/// the access-control page is instant. Successful writes invalidate the cache
/// right away, so the cache is purely a brief accelerator and the firewall
/// remains the durable source of truth.
const RULES_CACHE_TTL: Duration = Duration::from_secs(4);

/// One cached rule listing snapshot.
struct CacheEntry {
    at: Instant,
    elevated: bool,
    rules: Vec<AccessRule>,
}

/// Short-lived process-local cache of the Firewall-* listing. The firewall is
/// still the durable source of truth; the cache only avoids re-enumerating the
/// firewall on rapid consecutive reads of the access-control page, and is
/// invalidated by every successful write (see run_op).
static RULES_CACHE: std::sync::OnceLock<std::sync::Mutex<Option<CacheEntry>>> =
    std::sync::OnceLock::new();

/// Test-only helper: lazily initialise the cache holder so unit tests can seed
/// and inspect a cache entry. The read/write paths touch RULES_CACHE directly.
#[cfg(test)]
fn rules_cache() -> &'static std::sync::Mutex<Option<CacheEntry>> {
    RULES_CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Drop any cached listing so the next read re-enumerates the firewall.
/// pub(crate): called from run_op on every successful firewall write.
fn invalidate_rules_cache() {
    if let Some(m) = RULES_CACHE.get() {
        if let Ok(mut guard) = m.lock() {
            *guard = None;
        }
    }
}

/// One PowerShell call lists every FlyGo-* rule with its port/address
/// filters (object output, locale-independent - unlike netsh text).
/// Filters share the rule's InstanceID, so fetch each filter type once and
/// join in-app: per-rule pipeline calls cost ~0.5s each (34 rules ≈ 17s).
fn list_rules() -> Vec<AccessRule> {
    let script = r#"
$rules = @(Get-NetFirewallRule -DisplayName 'FlyGo-*' -ErrorAction SilentlyContinue)
$pfMap = @{}; Get-NetFirewallPortFilter -ErrorAction SilentlyContinue | ForEach-Object { $pfMap[$_.InstanceID] = $_ }
$afMap = @{}; Get-NetFirewallAddressFilter -ErrorAction SilentlyContinue | ForEach-Object { $afMap[$_.InstanceID] = $_ }
foreach ($r in $rules) {
  $pf = $pfMap[$r.InstanceID]
  $af = $afMap[$r.InstanceID]
  $proto = if ($pf) { $pf.Protocol } else { '' }
  $ports = if ($pf) { ($pf.LocalPort -join ',') } else { '' }
  $remote = if ($af) { ($af.RemoteAddress -join ',') } else { '' }
  Write-Output ("RULE=" + $r.DisplayName + "|" + $r.Enabled + "|" + $r.Direction + "|" + $proto + "|" + $ports + "|" + $remote)
}
"#;
    let mut rules = Vec::new();
    for line in ps(script).lines() {
        let Some(rest) = line.strip_prefix("RULE=") else {
            continue;
        };
        let p: Vec<&str> = rest.splitn(6, '|').collect();
        if p.len() < 6 {
            continue;
        }
        let name = p[0].trim().to_string();
        let remote = norm_remote(p[5]);
        let scope = detect_scope(&name, &remote);
        rules.push(AccessRule {
            scope,
            name,
            enabled: p[1].trim().eq_ignore_ascii_case("True"),
            direction: p[2].trim().to_string(),
            protocol: p[3].trim().to_uppercase(),
            port: p[4].trim().to_string(),
            remote,
        });
    }
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    rules
}

/// GET /api/access/rules
///
/// The firewall is still the durable source of truth. Consecutive rapid reads
/// are served from a short-lived (RULES_CACHE_TTL) process-local cache so the
/// access-control page opens instantly; the cache is invalidated by every
/// successful write (run_op) and expires quickly, so it can never hide an
/// external firewall change for long.
pub async fn rules() -> ApiResult<Json<RulesOut>> {
    if let Some(Ok(guard)) = RULES_CACHE.get().map(|c| c.lock()) {
        if let Some(entry) = guard.as_ref() {
            if entry.at.elapsed() < RULES_CACHE_TTL {
                return Ok(Json(RulesOut {
                    ok: true,
                    elevated: entry.elevated,
                    rules: entry.rules.clone(),
                }));
            }
        }
    }
    let (elevated, items) = tokio::task::spawn_blocking(|| (is_elevated(), list_rules()))
        .await
        .unwrap_or((false, Vec::new()));
    if let Some(Ok(mut guard)) = RULES_CACHE.get().map(|c| c.lock()) {
        *guard = Some(CacheEntry {
            at: Instant::now(),
            elevated,
            rules: items.clone(),
        });
    }
    Ok(Json(RulesOut {
        ok: true,
        elevated,
        rules: items,
    }))
}

// ---------- writes (add / delete) ----------

/// Read a RESULT= line written by the elevated helper script. The script
/// writes with [IO.File]::WriteAllText(..., UTF8) which emits a BOM; strip
/// it so the RESULT= prefix matches.
fn read_op_result(path: &std::path::Path) -> Option<(bool, String)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let text = raw.trim_start_matches('\u{feff}');
    let line = text.lines().find(|l| l.starts_with("RESULT="))?.to_string();
    let rest = line.trim_start_matches("RESULT=").trim();
    if let Some(msg) = rest.strip_prefix("OK ") {
        Some((true, msg.to_string()))
    } else if let Some(msg) = rest.strip_prefix("ERROR ") {
        Some((false, msg.to_string()))
    } else {
        Some((false, rest.to_string()))
    }
}

/// Execute an admin firewall op. Builds one PS script, then either runs it
/// inline (panel elevated) or spawns it elevated via UAC and polls the
/// result file. Returns (ok, user-facing message).
/// pub(crate): dockernet.rs applies the one system-level Docker rule set
/// through the same elevation path.
pub(crate) fn run_op(script: &str) -> (bool, String) {
    let dir = std::env::temp_dir();
    let script_path = dir.join(OP_SCRIPT);
    let result_path = dir.join(OP_RESULT);
    if std::fs::write(&script_path, script).is_err() {
        return (false, "无法写入临时脚本".into());
    }
    let _ = std::fs::remove_file(&result_path); // drop any stale result

    if is_elevated() {
        // Already admin: run the same script inline and wait for it.
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-WindowStyle",
                "Hidden",
                "-File",
            ])
            .arg(&script_path)
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        if out.is_err() {
            return (false, "PowerShell 启动失败".into());
        }
        let r = read_op_result(&result_path).unwrap_or((false, "操作未返回结果".into()));
        if r.0 {
            invalidate_rules_cache();
        }
        return r;
    }

    // Non-elevated panel: UAC prompt via ShellExecuteW runas, then poll the
    // result file until the user answers (or we give up).
    // SW_HIDE (not SHOWNORMAL): the elevated powershell would otherwise flash
    // a visible console despite -WindowStyle Hidden (show-cmd race).
    let file = wstr("powershell.exe");
    let args = wstr(&format!(
        "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\"",
        script_path.display()
    ));
    unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(file.as_ptr()),
            PCWSTR(args.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(UAC_WAIT_SECS);
    loop {
        if let Some(r) = read_op_result(&result_path) {
            if r.0 {
                invalidate_rules_cache();
            }
            return r;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    (
        false,
        "等待管理员确认超时（UAC 未确认？），规则未变更".into(),
    )
}

/// The helper script body for one op. All interpolated values pass strict
/// charset validation upstream (no spaces, no quotes), so no escaping is
/// needed; the rule name is additionally single-quoted via sq().
fn build_add_script(entries: &[(String, String)]) -> String {
    let result = sq(&std::env::temp_dir().join(OP_RESULT).to_string_lossy());
    let mut script = format!(
        "$ErrorActionPreference = 'Continue'\n$resultFile = '{result}'\n$failed = @()\n",
        result = result
    );
    for (name, extra) in entries {
        script.push_str(&format!(
            "$name = '{name}'\n$null = netsh advfirewall firewall delete rule name=$name 2>&1\n$out = netsh advfirewall firewall add rule name=$name {extra} 2>&1\n$code = $LASTEXITCODE\n$exists = [bool](Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue)\nif (($code -ne 0) -or (-not $exists)) {{ $failed += $name }}\n",
            name = sq(name),
            extra = extra
        ));
    }
    script.push_str("if ($failed.Count -eq 0) { $res = 'RESULT=OK rules-added' } else { $res = 'RESULT=ERROR failed=' + ($failed -join ',') }\n[IO.File]::WriteAllText($resultFile, $res, [Text.Encoding]::UTF8)\n");
    script
}

/// Helper script body for one delete operation.
fn build_delete_script(name: &str) -> String {
    format!(
        r#"
$ErrorActionPreference = 'Continue'
$name = '{name}'
$resultFile = '{result}'
$null = netsh advfirewall firewall delete rule name=$name 2>&1
$exists = [bool](Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue)
if (-not $exists) {{ $res = 'RESULT=OK rule=' + $name }}
else {{ $res = 'RESULT=ERROR rule=' + $name + ' still present' }}
[IO.File]::WriteAllText($resultFile, $res, [Text.Encoding]::UTF8)
"#,
        name = sq(name),
        result = sq(&std::env::temp_dir().join(OP_RESULT).to_string_lossy())
    )
}

/// POST /api/access/allow  body: {port, scope, cidr?, name?, explicit?}
pub async fn allow(body: Json<AllowReq>) -> ApiResult<Json<AccessCtl>> {
    let port = body.port.trim().to_string();
    let cidr = body.cidr.trim().to_string();
    let label = body.name.trim().to_string();
    let mut raw_scopes = if !body.scopes.is_empty() {
        body.scopes.clone()
    } else {
        body.scope.split(',').map(str::to_string).collect()
    };
    if raw_scopes.is_empty() {
        raw_scopes.push("local".into());
    }
    let mut scopes = Vec::new();
    for raw in raw_scopes {
        let scope = raw.trim().to_lowercase();
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    // Validate the port before the local-only no-op so malformed requests never
    // receive a successful response, even when no firewall rule is needed.
    if !valid_port_spec(&port) {
        return Ok(Json(AccessCtl {
            ok: false,
            message: "端口格式不对（如 8388 / 8388,8788 / 8000-9000）".into(),
        }));
    }
    if scopes.iter().any(|s| s == "local") && scopes.len() == 1 {
        return Ok(Json(AccessCtl {
            ok: true,
            message: "仅本机（127.0.0.1）访问默认放行，无需创建防火墙规则".into(),
        }));
    }
    scopes.retain(|s| s != "local"); // local is a safe no-op when combined with network scopes.
    if !valid_port_spec(&port) {
        return Ok(Json(AccessCtl {
            ok: false,
            message: "端口格式不对（如 8388 / 8388,8788 / 8000-9000）".into(),
        }));
    }
    if !label.is_empty() && !valid_label(&label) {
        return Ok(Json(AccessCtl {
            ok: false,
            message: "名称只能含字母数字和 . _ -（最多 24 字符）".into(),
        }));
    }
    if scopes.iter().any(|s| s == "lan") && !cidr.is_empty() && !valid_cidr(&cidr) {
        return Ok(Json(AccessCtl {
            ok: false,
            message: "网段格式不对（如 192.168.1.0/24）".into(),
        }));
    }
    if scopes
        .iter()
        .any(|s| s != "tailscale" && s != "lan" && s != "all")
    {
        return Ok(Json(AccessCtl {
            ok: false,
            message: "非法的访问范围（应为 local|tailscale|lan|all）".into(),
        }));
    }
    if scopes.iter().any(|s| s == "all") && !body.explicit {
        return Ok(Json(AccessCtl {
            ok: false,
            message: "「所有人」范围必须显式确认（explicit=true）才能开放".into(),
        }));
    }
    let mut entries = Vec::new();
    for scope in &scopes {
        let tag = scope_tag(scope).unwrap();
        let name = rule_name(tag, &port, &label);
        let remote = remote_for(scope, &cidr);
        entries.push((
            name,
            format!("dir=in action=allow protocol=TCP localport={port} remoteip={remote}"),
        ));
    }
    let script = build_add_script(&entries);
    let (ok, message) = tokio::task::spawn_blocking(move || run_op(&script))
        .await
        .unwrap_or((false, "操作执行失败".into()));
    // run_op invalidates the rules cache on success so the next read reflects
    // the new rule immediately.
    let message = if ok {
        format!(
            "已添加规则：{}",
            scopes
                .iter()
                .map(|s| scope_msg(s))
                .collect::<Vec<_>>()
                .join("、")
        )
    } else {
        message.clone()
    };
    if ok {
        log_info!(
            "access.allow",
            "added rule(s) port={port} scopes={:?} label={label:?} -> {message}",
            scopes
        );
    } else {
        log_error!(
            "access.allow",
            "E add rule port={port} scopes={:?} label={label:?} -> {message}",
            scopes
        );
    }
    Ok(Json(AccessCtl { ok, message }))
}

fn scope_msg(scope: &str) -> &'static str {
    match scope {
        "tailscale" => "仅 Tailscale 网络",
        "lan" => "局域网",
        "all" => "所有人（0.0.0.0/0）",
        _ => "未知范围",
    }
}

/// DELETE /api/access/allow  body: {name}
pub async fn remove(body: Json<DelReq>) -> ApiResult<Json<AccessCtl>> {
    let name = body.name.trim().to_string();
    if !name.to_ascii_uppercase().starts_with("FLYGO-")
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Ok(Json(AccessCtl {
            ok: false,
            message: "只能删除 FlyGo- 前缀的规则".into(),
        }));
    }
    let script = build_delete_script(&name);
    let (ok, message) = tokio::task::spawn_blocking(move || run_op(&script))
        .await
        .unwrap_or((false, "操作执行失败".into()));
    // run_op invalidates the rules cache on success so the next read reflects
    // the removal immediately.
    let message = if ok {
        format!("已删除规则 {name}")
    } else {
        message.clone()
    };
    if ok {
        log_info!("access.remove", "removed rule {name} -> {message}");
    } else {
        log_error!("access.remove", "E remove rule {name} -> {message}");
    }
    Ok(Json(AccessCtl { ok, message }))
}

// ---------- private string helpers ----------

/// PowerShell single-quote escape: only ' needs doubling.
fn sq(s: &str) -> String {
    s.replace('\'', "''")
}

/// Wide-string helper for ShellExecuteW args.
fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_only_allow_still_validates_port() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(allow(Json(AllowReq {
            port: "not-a-port".into(),
            scope: "local".into(),
            scopes: Vec::new(),
            cidr: String::new(),
            name: String::new(),
            explicit: false,
        })));
        match result {
            Ok(json) => assert!(!json.0.ok),
            Err(_) => panic!("local validation unexpectedly returned an API error"),
        }
    }

    #[test]
    fn port_and_cidr_validation_reject_injection() {
        assert!(!valid_port_spec("8388; whoami"));
        assert!(valid_port_spec("8388,8788"));
        assert!(!valid_cidr("192.168.1.1/33"));
        assert!(valid_cidr("192.168.1.0/24"));
    }

    #[test]
    fn rules_cache_populate_and_invalidate() {
        // Start clean.
        invalidate_rules_cache();
        assert!(rules_cache().lock().unwrap().is_none());

        // A fresh listing stored in the cache is served while under TTL.
        *rules_cache().lock().unwrap() = Some(CacheEntry {
            at: Instant::now(),
            elevated: true,
            rules: vec![AccessRule {
                name: "FlyGo-LAN-8388-Panel".into(),
                scope: "lan".into(),
                protocol: "TCP".into(),
                port: "8388".into(),
                remote: "LocalSubnet".into(),
                enabled: true,
                direction: "Inbound".into(),
            }],
        });
        let guard = rules_cache().lock().unwrap();
        let entry = guard.as_ref().expect("cache should hold an entry");
        assert!(entry.elevated);
        assert_eq!(entry.rules.len(), 1);
        assert!(entry.at.elapsed() < RULES_CACHE_TTL);
        drop(guard);

        // A write must invalidate so the next read re-enumerates the firewall.
        invalidate_rules_cache();
        assert!(rules_cache().lock().unwrap().is_none());
    }
}
