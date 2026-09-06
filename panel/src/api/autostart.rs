// /api/autostart — 开机/登录自启动管理（计划任务 + 注册表 Run 项）
// 目标：把"默默自启、没被说清"的开机项交还给用户，在 Web 页面里可看可开关。
// 全零新依赖：沿用 system.rs 的 ps() 惯例调 PowerShell 采集；开关走 Enable/Disable-ScheduledTask 与 reg.exe。
// 安全：本模块只改计划任务的启用/停用与注册表 Run 值，不改 \Microsoft\* 系统任务、不删任何文件。
use crate::ApiResult;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// CREATE_NO_WINDOW: powershell.exe / reg.exe are console apps; spawned from
/// a GUI parent they flash a black window unless this flag is set.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------- 常量 ----------
/// "废弃候选"名单——命中即在前端打 ⚠️ 标记，方便用户识别该不该开。
/// （FengVault/OpenClaw/Hermes/百度/夸克 经用户确认为已弃用自启项；只标记不硬删，开关交还给用户。）
const DEPRECATED_HINTS: &[&str] = &["fengvault", "openclaw", "hermes", "baidu", "百度", "quark", "夸克"];
/// 注册表 Run 值被停用前，原命令备份到这里（路径由 config 解析，env
/// FENGPAGE_AUTOSTART_BACKUP 可覆盖；*.json 已被 .gitignore，命令行可能含路径但非密钥）。
const RUN_PATH: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn doomed(name: &str) -> bool {
    let lower = name.to_lowercase();
    DEPRECATED_HINTS.iter().any(|k| lower.contains(k))
}

// ---------- 数据结构 ----------
#[derive(Serialize, Clone)]
pub struct AutostartItem {
    pub kind: &'static str, // task | run
    pub name: String,
    /// task=计划任务路径(TaskPath)，run=注册表根(HKLM|HKCU)
    pub path: String,
    /// task=触发类型(Logon/Boot/Daily/...)，run=(空)
    pub trigger: String,
    /// task=任务状态(Ready/Running/Disabled)，run=是否启用(on/off)
    pub state: String,
    /// task=任务动作命令，run=注册表值原命令
    pub action: String,
    /// 是否命中"废弃候选"名单
    pub deprecated: bool,
    /// 是否属于开机/登录自启动
    pub autostart: bool,
}

#[derive(Serialize)]
pub struct AutostartOut {
    pub ok: bool,
    pub items: Vec<AutostartItem>,
}

/// 执行 powershell 并返回 stdout（照抄 system.rs 惯例，UTF-8 输出、去 BOM）。
fn ps(script: &str) -> String {
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

/// 一次 PowerShell 调用同时列出：非 Microsoft 计划任务 + HKLM/HKCU 的 Run 值。
/// 输出约定：`TASK=名称|路径|触发,触发|状态|动作`、`RUN=根|值名|命令`。
fn collect() -> AutostartOut {
    let mut items = Vec::new();
    let script = r#"
$out = ''
# 1) 计划任务：排除系统 \Microsoft\*，其余全列（含开机/登录/定时）
Get-ScheduledTask | Where-Object { $_.TaskPath -notlike '\Microsoft\*' } | ForEach-Object {
  $tr = @()
  foreach ($x in $_.Triggers) {
    $c = $x.CimClass.CimClassName
    if     ($c -match 'Logon')  { $tr += 'Logon' }
    elseif ($c -match 'Boot')   { $tr += 'Boot' }
    elseif ($c -match 'Daily')  { $tr += 'Daily' }
    elseif ($c -match 'Time')   { $tr += 'Time' }
    elseif ($c -match 'Event')  { $tr += 'Event' }
    elseif ($c -match 'AtStartup') { $tr += 'AtStartup' }
    else { $tr += ($c -replace 'MSFT_Task','') }
  }
  $ac = @($_.Actions | ForEach-Object { (($_.Execute + ' ' + $_.Arguments).Trim()) }) -join ' ; '
  if ($_.Actions) { } else { $ac = '(无动作)' }
  if (-not $ac) { $ac = '(无动作)' }
  Write-Output ("TASK=" + $_.TaskName + "|" + $_.TaskPath + "|" + ($tr -join ',') + "|" + $_.State + "|" + $ac)
}
# 2) 注册表 Run（HKLM + HKCU）
foreach ($h in @(@('HKLM','HKLM:\Software\Microsoft\Windows\CurrentVersion\Run'),
                 @('HKCU','HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'))) {
  $item = Get-ItemProperty -Path $h[1] -ErrorAction SilentlyContinue
  if ($item) {
    foreach ($p in $item.PSObject.Properties) {
      if ($p.Name -in @('PSPath','PSParentPath','PSChildName','PSDrive','PSProvider')) { continue }
      Write-Output ("RUN=" + $h[0] + "|" + $p.Name + "|" + $p.Value)
    }
  }
}
"#;
    let out = ps(script);
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("TASK=") {
            let p: Vec<&str> = rest.splitn(5, '|').collect();
            if p.len() >= 4 {
                let name = p[0].trim().to_string();
                let trigger = p[2].trim().to_string();
                let state = p[3].trim().to_string();
                let action = p.get(4).cloned().unwrap_or("").trim().to_string();
                let is_start = trigger.contains("Logon") || trigger.contains("Boot");
                items.push(AutostartItem {
                    kind: "task",
                    name: name.clone(),
                    path: p[1].trim().to_string(),
                    trigger,
                    state,
                    action,
                    deprecated: doomed(&name),
                    autostart: is_start,
                });
            }
        } else if let Some(rest) = line.strip_prefix("RUN=") {
            let p: Vec<&str> = rest.splitn(3, '|').collect();
            if p.len() >= 2 {
                let name = p[1].trim().to_string();
                items.push(AutostartItem {
                    kind: "run",
                    name: name.clone(),
                    path: p[0].trim().to_string(),
                    trigger: String::new(),
                    state: "on".into(),
                    action: p.get(2).cloned().unwrap_or("").trim().to_string(),
                    deprecated: doomed(&name),
                    autostart: true, // Run 值都是开机自启
                });
            }
        }
    }
    // 排序：自启动在前、废弃候选置顶，方便一眼看到该处理谁
    items.sort_by(|a, b| {
        (b.autostart as u8).cmp(&(a.autostart as u8))
            .then((b.deprecated as u8).cmp(&(a.deprecated as u8)))
            .then(a.name.cmp(&b.name))
    });
    AutostartOut { ok: true, items }
}

// ---------- 缓存（沿用 system.rs 的 12s TTL 惯例，避免 15s 轮询每轮都跑慢 PowerShell） ----------
static CACHE: LazyLock<Mutex<(Instant, Vec<AutostartItem>)>> =
    LazyLock::new(|| Mutex::new((Instant::now() - Duration::from_secs(12), Vec::new())));
const CACHE_TTL: Duration = Duration::from_secs(12);

/// GET /api/autostart — 列出自启动项。慢采集放 spawn_blocking + 12s 缓存。
pub async fn autostart() -> ApiResult<Json<AutostartOut>> {
    {
        let g = CACHE.lock().unwrap();
        if g.0.elapsed() < CACHE_TTL {
            return Ok(Json(AutostartOut { ok: true, items: g.1.clone() }));
        }
    }
    let info = tokio::task::spawn_blocking(collect)
        .await
        .unwrap_or(AutostartOut { ok: true, items: Vec::new() });
    *CACHE.lock().unwrap() = (Instant::now(), info.items.clone());
    Ok(Json(info))
}

// ---------- 开关 ----------
/// 计划任务启停；开关放一个锁串行，避免并发改错位。
static TLOCK: Mutex<()> = Mutex::new(());
static ALOCK: Mutex<()> = Mutex::new(());

/// 读注册表 Run 值备份（停用时存，启用时恢复原命令）
fn run_backup_path() -> std::path::PathBuf {
    crate::config::get().autostart_backup.clone()
}
fn run_backup_read() -> HashMap<String, serde_json::Value> {
    std::fs::read_to_string(run_backup_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn run_backup_write(map: &HashMap<String, serde_json::Value>) {
    if let Ok(s) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(run_backup_path(), s);
    }
}

#[derive(Deserialize)]
pub struct AutostartReq {
    pub kind: String,    // task | run
    pub name: String,
    #[serde(default)]
    pub path: String,    // task=TaskPath，run=HKLM|HKCU
    #[serde(default)]
    pub action: String,  // on | off
}

#[derive(Serialize)]
pub struct AutostartCtl {
    pub ok: bool,
    pub message: String,
}

/// 单引号字符串内的可靠转义：PowerShell 单引号里只有 ' 需要翻倍（''），
/// { } \ $ ` [ ] 等字符都是字面量、无需额外转义；配合"变量传参 + 精确匹配"
/// 后，任务名里的花括号等特殊字符不再参与命令拼接，也不会被当通配符解析。
fn sq(s: &str) -> String {
    s.replace('\'', "''")
}

/// POST /api/autostart  body: {kind,name,path?,action:"on"|"off"}
pub async fn toggle(body: axum::Json<AutostartReq>) -> ApiResult<Json<AutostartCtl>> {
    let kind = body.kind.to_lowercase();
    let name = body.name.trim().to_string();
    let act = body.action.to_lowercase();
    if name.is_empty() {
        return Ok(Json(AutostartCtl { ok: false, message: "缺少 name".into() }));
    }
    let (ok, message) = match kind.as_str() {
        "task" => {
            let _g = TLOCK.lock().unwrap_or_else(|e| e.into_inner());
            let path = if body.path.is_empty() { "\\".to_string() } else { body.path.clone() };
            toggle_task(&name, &path, &act)
        }
        "run" => {
            let _g = ALOCK.lock().unwrap_or_else(|e| e.into_inner());
            let hive = if body.path.is_empty() { "HKCU".to_string() } else { body.path.to_uppercase() };
            toggle_run(&name, &hive, &act)
        }
        _ => (false, format!("非法 kind: {kind}（应为 task|run）")),
    };
    // 开关成功即失效缓存，下次刷新拿到最新状态
    if ok {
        if let Ok(mut g) = CACHE.lock() {
            g.0 = Instant::now() - CACHE_TTL - Duration::from_secs(1); // 强制过期
        }
    }
    Ok(Json(AutostartCtl { ok, message }))
}

/// 计划任务启停（on/off）。
/// 旧实现把任务名直接拼进 `-TaskName '<name>' -TaskPath '<path>'` 命令行，
/// 特殊字符（花括号等）匹配不可靠；且成功判定只看 stdout 是否含 "error"，
/// 而 PowerShell 报错走 stderr（ps() 只收 stdout），导致"返回 ok 但没生效"。
/// 新实现：任务名/路径以单引号变量传入（sq() 翻倍 '），用 Get-ScheduledTask
/// 精确匹配（Where-Object -eq，不走 -TaskName 通配）后经 -InputObject 启停，
/// 再回读 State 验证真正生效（off -> Disabled；on -> 非 Disabled），只有
/// 实态变了才报 ok，否则明确报错。
fn toggle_task(name: &str, path: &str, act: &str) -> (bool, String) {
    let (verb, done_msg) = match act {
        "off" => ("Disable-ScheduledTask", "已停用计划任务"),
        "on" => ("Enable-ScheduledTask", "已启用计划任务"),
        _ => return (false, format!("非法动作: {act}（应为 on|off）")),
    };
    let name_q = sq(name);
    let path_q = sq(path);
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
$name = '{name_q}'
$path = '{path_q}'
try {{
    $t = Get-ScheduledTask | Where-Object {{ $_.TaskName -eq $name -and $_.TaskPath -eq $path }} | Select-Object -First 1
    if (-not $t) {{ Write-Output 'RESULT=NOTFOUND'; return }}
    if (('{act}' -eq 'off' -and $t.State -eq 'Disabled') -or ('{act}' -eq 'on' -and $t.State -ne 'Disabled')) {{
        Write-Output ('RESULT=ALREADY ' + $t.State); return
    }}
    {verb} -InputObject $t | Out-Null
    $t2 = Get-ScheduledTask | Where-Object {{ $_.TaskName -eq $name -and $_.TaskPath -eq $path }} | Select-Object -First 1
    $ok = $false
    if ('{act}' -eq 'off' -and $t2.State -eq 'Disabled') {{ $ok = $true }}
    if ('{act}' -eq 'on' -and $t2.State -ne 'Disabled') {{ $ok = $true }}
    if ($ok) {{ Write-Output ('RESULT=OK ' + $t2.State); return }}
    Write-Output ('RESULT=FAIL state=' + $t2.State)
}} catch {{
    Write-Output ('RESULT=ERROR ' + $_.Exception.Message)
}}
"#
    );
    let out = ps(&script);
    let line = out
        .lines()
        .find(|l| l.starts_with("RESULT="))
        .map(|l| l.to_string())
        .unwrap_or_default();
    if line.is_empty() {
        return (false, format!("操作未返回结果（PowerShell 无输出），任务 {name} 状态未确认"));
    }
    let rest = &line["RESULT=".len()..];
    if let Some(state) = rest.strip_prefix("OK ") {
        return (true, format!("{done_msg} {name}（State 已变更为 {state}）"));
    }
    if let Some(state) = rest.strip_prefix("ALREADY ") {
        let target = if act == "off" { "停用" } else { "启用" };
        return (true, format!("{name} 本就处于{target}状态（State={state}），无需操作"));
    }
    if rest.starts_with("NOTFOUND") {
        return (false, format!("未找到计划任务 {name}（TaskPath={path}）"));
    }
    if let Some(err) = rest.strip_prefix("ERROR ") {
        return (false, format!("操作失败: {err}"));
    }
    if let Some(fail) = rest.strip_prefix("FAIL ") {
        return (false, format!("操作未生效（{fail}），任务 {name} 状态未改变"));
    }
    (false, format!("未知结果: {rest}"))
}

fn reg_root(hive: &str) -> &'static str {
    match hive {
        "HKLM" => "HKLM",
        "HKCU" => "HKCU",
        _ => "HKCU",
    }
}

fn toggle_run(name: &str, hive: &str, act: &str) -> (bool, String) {
    let root = reg_root(hive);
    let full = format!("{root}\\{RUN_PATH}");
    match act {
        "off" => {
            // 先备份原命令，再删值，保证 Web 里可一键恢复
            if let Some(cmd) = reg_query_value(&full, name) {
                let mut map = run_backup_read();
                map.insert(format!("{root}::{name}"), serde_json::json!(cmd));
                run_backup_write(&map);
            }
            let out = Command::new("reg")
                .args(["delete", &full, "/v", name, "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            match out {
                Ok(o) if o.status.success() => (true, format!("已停用自启动项 {name}（原命令已备份，可在 Web 开关里恢复）")),
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    (false, format!("停用失败: {err}"))
                }
                Err(e) => (false, format!("停用失败: {e}")),
            }
        }
        "on" => {
            // 从备份恢复原命令；无备份则报错
            let map = run_backup_read();
            let key = format!("{root}::{name}");
            let cmd = map.get(&key).and_then(|v| v.as_str());
            let cmd = match cmd {
                Some(c) => c.to_string(),
                None => return (false, format!("无法启用 {name}：未找到已备份的原命令（之前没有停用记录）")),
            };
            let out = Command::new("reg")
                .args(["add", &full, "/v", name, "/t", "REG_SZ", "/d", &cmd, "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    // 启用成功后清掉备份
                    let mut map = run_backup_read();
                    map.remove(&key);
                    run_backup_write(&map);
                    (true, format!("已启用自启动项 {name}"))
                }
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    (false, format!("启用失败: {err}"))
                }
                Err(e) => (false, format!("启用失败: {e}")),
            }
        }
        _ => (false, format!("非法动作: {act}（应为 on|off）")),
    }
}

/// reg query 单个值，返回其数据（REG_SZ 等）
fn reg_query_value(full: &str, name: &str) -> Option<String> {
    let out = Command::new("reg")
        .args(["query", full, "/v", name])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    // 输出形如：... 值名     REG_SZ    数据
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("HKEY") { continue; }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            return Some(parts[2..].join(" "));
        }
    }
    None
}
