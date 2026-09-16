// /api/system — 定时任务 / 硬盘(USB) / 网盘(SMB) 状态大盘
// 后端以用户身份运行（scheduled task），可调用 PowerShell 采集系统信息。
use crate::ApiResult;
use axum::Json;
use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::process::Command;

/// CREATE_NO_WINDOW: keep the spawned console app (powershell.exe) from
/// flashing a black window. GUI parent + console child == visible console
/// unless this flag is set.
use crate::winproc::CREATE_NO_WINDOW;

#[derive(Serialize, Clone)]
pub struct SysTask {
    pub name: String,
    pub state: String,
    pub trigger: String,
    pub action: String,
}

#[derive(Serialize, Clone)]
pub struct Disk {
    pub drive: String,
    pub fs: String,
    pub free_gb: f64,
    pub total_gb: f64,
    pub label: String,
    pub model: String,
    pub bus: String,
    pub is_usb: bool,
    pub is_removable: bool,
    /// 硬盘健康（Get-StorageReliabilityCounter，in-box）：温度、磨损、累计读错误。取不到则 null 或 -1。
    pub smart_temp_c: Option<f64>,
    pub smart_wear: Option<f64>,
    pub smart_errors: Option<u64>,
}

#[derive(Serialize, Clone)]
pub struct SmbShare {
    pub name: String,
    pub path: String,
    pub access: String,
}

#[derive(Serialize, Clone)]
pub struct SystemInfo {
    pub tasks: Vec<SysTask>,
    pub disks: Vec<Disk>,
    pub shares: Vec<SmbShare>,
    /// ACPI thermal-zone reading in Celsius; approximate and not CPU/GPU-specific.
    pub temp_c: Option<f64>,
    pub temp_status: String,
    pub network: NetworkThroughput,
}

/// 读 ACPI 主板热区温度（摄氏度）。返回 None 表示未暴露或不可读。
fn motherboard_temp() -> Option<f64> {
    let out = ps(
        r#"$t = Get-CimInstance -Namespace root/wmi -ClassName MSAcpi_ThermalZoneTemperature -ErrorAction SilentlyContinue | Select-Object -First 1; if ($t) { [math]::Round(($t.CurrentTemperature/10) - 273.15, 1) } else { 'n/a' }"#,
    );
    out.split_whitespace().next()?.parse::<f64>().ok()
}

/// 执行 powershell 并返回 stdout。
/// 前置 `[Console]::OutputEncoding=UTF8`，让中文以 UTF-8 写出，避免 GBK 乱码。
fn ps(script: &str) -> String {
    let exp = format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; {script}");
    let out = Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &exp])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let s = s.trim_start_matches('\u{feff}');
            s.trim().to_string()
        }
        Err(_) => String::new(),
    }
}

/// 硬盘健康探测：Get-StorageReliabilityCounter（Windows in-box），返回 (物理盘号 -> (温度, 磨损, 读错误))。
/// 取不到的盘不放入 map（默认缺省即无 SMART 数据）。仅一次阻塞调用。
type SmartMap = std::collections::HashMap<u32, (Option<f64>, Option<f64>, Option<u64>)>;

fn disk_smart() -> SmartMap {
    let mut map = std::collections::HashMap::new();
    let out = ps(
        r#"Get-PhysicalDisk | ForEach-Object { $n=$_.DeviceId; $r=$_ | Get-StorageReliabilityCounter -ErrorAction SilentlyContinue; if($r){ Write-Output ("SMART=$n|" + $r.Temperature + "|" + $r.Wear + "|" + $r.ReadErrorsTotal) } }"#,
    );
    for line in out.lines() {
        let p: Vec<&str> = line.split('|').collect();
        if p.len() >= 4 && p[0].starts_with("SMART=") {
            let idx: u32 = p[0].trim_start_matches("SMART=").trim().parse().unwrap_or(u32::MAX);
            let temp = p[1].trim().parse::<f64>().ok();
            let wear = p[2].trim().parse::<f64>().ok();
            let errs = p[3].trim().parse::<u64>().ok();
            map.insert(idx, (temp, wear, errs));
        }
    }
    map
}

/// 采集定时任务/硬盘/网盘/温度。全部走 PowerShell 子进程（阻塞），故用普通 fn 供 spawn_blocking 调用。
fn collect_system() -> SystemInfo {
    // 1) 定时任务：列出自定义任务（ps() 会自动 $out=@(); ... ; $out|Out-File）
    let task_script = r#"
$tasks = Get-ScheduledTask | Where-Object { $_.TaskName -match 'Sync|Vault|NetReset|Feng' -and $_.TaskPath -notlike '\Microsoft\*' }
foreach ($t in $tasks) {
  $tr = @()
  foreach ($x in $t.Triggers) {
    $c = $x.CimClass.CimClassName
    if ($c -match 'Time') { $tr += "Time rep=$($x.Repetition.Interval)" }
    elseif ($c -match 'Daily') { $tr += "Daily" }
    elseif ($c -match 'Logon') { $tr += "<at logon>" }
    else { $tr += $c }
  }
  $ac = @($t.Actions | ForEach-Object { ($_.Execute + ' ' + $_.Arguments).Trim() }) -join ' '
  Write-Output "TASK=$($t.TaskName)|$($t.State)|$($tr -join ',')|$ac"
}
"#;
    let task_out = ps(task_script);
    let mut tasks = Vec::new();
    for line in task_out.lines() {
        let parts: Vec<&str> = line.splitn(4, '|').collect();
        if parts.len() >= 3 {
            tasks.push(SysTask {
                name: parts[0].to_string(),
                state: parts[1].to_string(),
                trigger: if parts.len() > 2 { parts[2].to_string() } else { String::new() },
                action: if parts.len() > 3 { parts[3].to_string() } else { String::new() },
            });
        }
    }

    // 2) 硬盘：逻辑分区 + 物理磁盘(型号/接口)，识别 USB
    let disk_out = ps(
        r#"Get-CimInstance Win32_LogicalDisk -Filter "DriveType=3 OR DriveType=2" | ForEach-Object { $t=[math]::Round($_.Size/1GB,1); $f=[math]::Round($_.FreeSpace/1GB,1); Write-Output "DISK=$($_.DeviceID)|$($_.FileSystem)|$f|$t|$($_.VolumeName)|$($_.DriveType)" }"#,
    );
    let disk_out2 = ps(
        r#"Get-CimInstance Win32_DiskDrive | ForEach-Object { Write-Output "PDISK=$($_.Index)|$($_.Model)|$($_.InterfaceType)|$([math]::Round($_.Size/1GB,0))|$($_.BusType)" }"#,
    );

    let mut disks: Vec<Disk> = Vec::new();
    // 物理盘列表：(model, interface, size_gb, index)。index 用于查 SMART（keyed by 物理盘号）
    let mut phys: Vec<(String, String, f64, u32)> = Vec::new();
    for line in disk_out2.lines() {
        let p: Vec<&str> = line.split('|').collect();
        if p.len() >= 5 {
            phys.push((
                p[1].trim().to_string(),
                p[2].trim().to_string(),
                        p[3].parse().unwrap_or(0.0),
                        p[0].trim().trim_start_matches("PDISK=").parse().unwrap_or(0),
            ));
        }
    }
    // SMART 健康：Get-StorageReliabilityCounter（in-box，输出温度/磨损/错误），按物理盘号索引
    let smart_map = disk_smart();
    for line in disk_out.lines() {
        let p: Vec<&str> = line.split('|').collect();
        if p.len() >= 6 {
            let drive = p[0].to_string();
            let total = p[3].parse().unwrap_or(0.0);
            let removable = p[5] == "2";
            // 按总容量匹配物理盘（容量最接近者），识别其接口/型号，并取 SMART
            let matched = phys.iter().min_by(|a, b| {
                let da = (a.2 - total).abs();
                let db = (b.2 - total).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            }).cloned();
            let (model, iface, _m_size, index) = matched.unwrap_or_default();
            let is_usb = iface.to_uppercase().contains("USB") || model.to_uppercase().contains("USB") || removable;
            let smart = smart_map.get(&index).copied().unwrap_or_default();
            disks.push(Disk {
                drive: drive.clone(),
                fs: p[1].to_string(),
                free_gb: p[2].parse().unwrap_or(0.0),
                total_gb: total,
                label: p[4].to_string(),
                model,
                bus: if is_usb { "USB".into() } else { "NVMe/内置".into() },
                is_usb,
                is_removable: removable,
                smart_temp_c: smart.0,
                smart_wear: smart.1,
                smart_errors: smart.2,
            });
        }
    }

    // 3) SMB 共享
    let share_script = r#"Get-SmbShare | Where-Object { $_.Name -notin 'IPC$','ADMIN$','C$','D$','E$' } | ForEach-Object { $acc = (Get-SmbShareAccess -Name $_.Name -ErrorAction SilentlyContinue | ForEach-Object { $_.AccountName }) -join ','; $acc = if ($acc) { $acc } else { 'n/a' }; Write-Output "SHARE=$($_.Name)|$($_.Path)|$acc" }"#;
    let share_out = ps(share_script);
    let mut shares = Vec::new();
    for line in share_out.lines() {
        let p: Vec<&str> = line.split('|').collect();
        if p.len() >= 3 {
            shares.push(SmbShare {
                name: p[0].to_string(),
                path: p[1].to_string(),
                access: p[2].to_string(),
            });
        }
    }

    let temp_c = motherboard_temp();
    let temp_status = if temp_c.is_some() { "approximate" } else { "unavailable" }.to_string();
    SystemInfo { tasks, disks, shares, temp_c, temp_status, network: network_throughput() }
}

/// /api/system 结果缓存：把昂贵的 PowerShell 采集（实测 10~15s）限制为每 ~12s 最多跑一次。
/// 轮询窗口内命中缓存直接返回，避免每 15s 轮询都重跑一遍、拖满自己 CPU。
/// 用 SystemTime 而非 Instant（Instant 在进程内单调，够用；SystemTime 便于理解语义）。
use std::sync::{OnceLock, Mutex};
use std::time::{Instant, Duration};
static SYSTEM_CACHE: OnceLock<Mutex<(Instant, SystemInfo)>> = OnceLock::new();
static NETWORK_SAMPLE: OnceLock<Mutex<Option<(Instant, u64, u64)>>> = OnceLock::new();

#[derive(Serialize, Clone, Default)]
pub struct NetworkThroughput {
    /// Receive rate in megabits per second, calculated from Windows byte counters.
    pub rx_mbps: Option<f64>,
    /// Send rate in megabits per second, calculated from Windows byte counters.
    pub tx_mbps: Option<f64>,
    /// `ok`, `warming_up`, or `unavailable`.
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::parse_network_counters;
    #[test]
    fn parses_windows_counters() { assert_eq!(parse_network_counters("NET=123|456"), Some((123, 456))); }
    #[test]
    fn unavailable_output_is_graceful() { assert_eq!(parse_network_counters(""), None); }

    #[test]
    fn parses_usb_status_lines() {
        use super::parse_usb_status;
        let out = "=== Volumes ===\nVOL=D|4T|NTFS|3726.0\nVOL=C|OS|NTFS|475.2\n=== Disks ===\nDISK=0|HFM512GD3JX013N||False|Online|476.9|True\nDISK=1|ST4000VM 000-2AF166|USB|True|Offline|3726.0|False\n=== Partitions ===\nPDVOL=0|C\nPDVOL=1|D\n";
        let (vols, disks, pdvol) = parse_usb_status(out);
        assert_eq!(vols.len(), 2);
        assert_eq!(vols[0].letter, "C");
        assert_eq!(vols[1].letter, "D");
        assert_eq!(disks.len(), 2);
        assert!(disks[0].is_system);
        assert!(!disks[0].offline);
        assert!(disks[1].offline);
        assert_eq!(disks[1].number, 1);
        assert_eq!(pdvol.get("C"), Some(&0));
        assert_eq!(pdvol.get("D"), Some(&1));
    }

    #[test]
    fn rejects_usb_status_junk() {
        use super::parse_usb_status;
        let (vols, disks, pdvol) = parse_usb_status("garbage\nVOL=toolongname|X|Y|1\nDISK=x|y\nPDVOL=x|toolong\n");
        assert!(vols.is_empty());
        assert!(disks.is_empty());
        assert!(pdvol.is_empty());
    }
}

fn parse_network_counters(output: &str) -> Option<(u64, u64)> {
    let line = output.lines().find(|line| line.trim_start().starts_with("NET="))?;
    let mut fields = line.trim().trim_start_matches("NET=").split('|');
    Some((fields.next()?.trim().parse().ok()?, fields.next()?.trim().parse().ok()?))
}

fn network_throughput() -> NetworkThroughput {
    let output = ps(r#"$rx=0; $tx=0; Get-NetAdapterStatistics -ErrorAction SilentlyContinue | ForEach-Object { $rx += [uint64]$_.ReceivedBytes; $tx += [uint64]$_.SentBytes }; if($rx -gt 0 -or $tx -gt 0) { Write-Output \"NET=$rx|$tx\" }"#);
    let Some((rx, tx)) = parse_network_counters(&output) else {
        return NetworkThroughput { status: "unavailable".into(), ..Default::default() };
    };
    let now = Instant::now();
    let state = NETWORK_SAMPLE.get_or_init(|| Mutex::new(None));
    let mut previous = state.lock().unwrap();
    let result = match *previous {
        Some((at, old_rx, old_tx)) => {
            let seconds = now.duration_since(at).as_secs_f64();
            if seconds > 0.0 && rx >= old_rx && tx >= old_tx {
                NetworkThroughput { rx_mbps: Some((rx - old_rx) as f64 * 8.0 / seconds / 1_000_000.0), tx_mbps: Some((tx - old_tx) as f64 * 8.0 / seconds / 1_000_000.0), status: "ok".into() }
            } else { NetworkThroughput { status: "warming_up".into(), ..Default::default() } }
        }
        None => NetworkThroughput { status: "warming_up".into(), ..Default::default() },
    };
    *previous = Some((now, rx, tx));
    result
}
const SYSTEM_CACHE_TTL: Duration = Duration::from_secs(12);

/// GET /api/system — PowerShell 采集慢，放 spawn_blocking；并用 12s 缓存抑制自耗。
/// The cached variant is also the /ws/status system provider source, so
/// the push feed and the REST endpoint share one pacing (api/wsstatus.rs).
pub async fn cached_system() -> SystemInfo {
    // 命中缓存且未过期 -> 直接返回（不重跑慢 PowerShell）
    let lock = SYSTEM_CACHE.get_or_init(|| Mutex::new((Instant::now() - SYSTEM_CACHE_TTL, SystemInfo {
        tasks: Vec::new(), disks: Vec::new(), shares: Vec::new(), temp_c: None, temp_status: "unavailable".into(), network: NetworkThroughput { status: "unavailable".into(), ..Default::default() },
    })));
    {
        let g = lock.lock().unwrap();
        if g.0.elapsed() < SYSTEM_CACHE_TTL {
            return g.1.clone();
        }
    }
    // 未命中 -> spawn_blocking 重采集（慢），落缓存后返回
    let info = tokio::task::spawn_blocking(collect_system)
        .await
        .unwrap_or(SystemInfo { tasks: Vec::new(), disks: Vec::new(), shares: Vec::new(), temp_c: None, temp_status: "unavailable".into(), network: NetworkThroughput { status: "unavailable".into(), ..Default::default() } });
    *lock.lock().unwrap() = (Instant::now(), info.clone());
    info
}

pub async fn system() -> ApiResult<Json<SystemInfo>> {
    Ok(Json(cached_system().await))
}

#[derive(Serialize)]
pub struct UsbResult {
    pub ok: bool,
    pub message: String,
}

/// 运行 usb-ctl.ps1 的某个动作，可附带盘符/盘号参数，返回其输出
fn run_usb(action: &str, target: &str) -> String {
    // Script path is user-configurable; missing = feature not installed.
    let Some(script) = crate::config::get().usb_ctl.clone() else {
        return "error: usb-ctl.ps1 not found; set FENGPAGE_USB_CTL".to_string();
    };
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-WindowStyle",
        "Hidden",
        "-File",
    ])
    .arg(&script)
    .arg(action);
    if !target.trim().is_empty() {
        cmd.arg(target.trim());
    }
    let out = cmd.creation_flags(CREATE_NO_WINDOW).output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim_start_matches('\u{feff}').trim().to_string()
        }
        Err(e) => format!("error: {e}"),
    }
}

/// POST /api/system/usb  body: {"action":"status|eject|replug", "letter":"D"}
/// letter/target: eject/replug 必须给（盘符如 D，或 replug 时盘号如 1）；
/// status 可省略（返回全部卷+盘）。
/// eject 会关闭 SMB 会话并让磁盘脱机（谨慎：连着的设备会被断开）。
pub async fn usb(body: axum::Json<serde_json::Value>) -> ApiResult<Json<UsbResult>> {
    let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("status").to_string();
    let target = body
        .get("letter")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("target").and_then(|v| v.as_str()))
        .unwrap_or("")
        .trim()
        .to_string();
    // Validate the target: single A-Z letter (with optional colon) or a disk
    // number for replug. Never pass raw user text to PowerShell unchecked.
    let target_ok = target.is_empty()
        || {
            let t = target.trim_end_matches(':').to_uppercase();
            (t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic()))
                || t.chars().all(|c| c.is_ascii_digit())
        };
    if !target_ok {
        return Ok(Json(UsbResult { ok: false, message: format!("非法目标: {target}（应为盘符如 D，或盘号如 1）") }));
    }
    if (action == "eject" || action == "replug") && target.is_empty() {
        return Ok(Json(UsbResult { ok: false, message: format!("{action} 需要指定盘符（如 D）") }));
    }
    match action.as_str() {
        "status" | "eject" | "replug" => {
            // PowerShell 子进程阻塞调用，放 spawn_blocking 不卡 tokio 工作线程
            let (action2, target2) = (action.clone(), target.clone());
            let msg = tokio::task::spawn_blocking(move || run_usb(&action2, &target2))
                .await
                .unwrap_or_else(|_| "error: spawn_blocking failed".to_string());
            // 判断是否成功：输出里不包含 error / 无法 / failed
            let lower = msg.to_lowercase();
            let failed = lower.contains("error") || lower.contains("无法")
                || (action == "status" && msg.trim().is_empty());
            Ok(Json(UsbResult { ok: !failed, message: msg }))
        }
        _ => Ok(Json(UsbResult { ok: false, message: format!("非法动作: {action}（应为 status|eject|replug）") })),
    }
}

/// One mounted volume line from usb-ctl.ps1 status (VOL=<letter>|<label>|<fs>|<sizeGB>).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct UsbVolume {
    pub letter: String,
    pub label: String,
    pub fs: String,
    pub size_gb: f64,
}

/// One physical disk line (DISK=<num>|<model>|<bus>|<offline>|<status>|<sizeGB>|<issys>).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct UsbDisk {
    pub number: u32,
    pub model: String,
    pub bus: String,
    pub offline: bool,
    pub status: String,
    pub size_gb: f64,
    pub is_system: bool,
}

/// Parse VOL=/DISK= machine lines from usb-ctl.ps1 status output. Pure.
fn parse_usb_status(out: &str) -> (Vec<UsbVolume>, Vec<UsbDisk>, std::collections::HashMap<String, u32>) {
    let mut vols = Vec::new();
    let mut disks = Vec::new();
    let mut pdvol = std::collections::HashMap::new();
    for line in out.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("VOL=") {
            let p: Vec<&str> = rest.split('|').collect();
            if p.len() >= 4 {
                let letter = p[0].trim().trim_end_matches(':').to_uppercase();
                if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_alphabetic()) {
                    vols.push(UsbVolume {
                        letter,
                        label: p[1].trim().to_string(),
                        fs: p[2].trim().to_string(),
                        size_gb: p[3].trim().parse().unwrap_or(0.0),
                    });
                }
            }
        } else if let Some(rest) = t.strip_prefix("DISK=") {
            let p: Vec<&str> = rest.split('|').collect();
            if p.len() >= 7 {
                if let Ok(number) = p[0].trim().parse::<u32>() {
                    disks.push(UsbDisk {
                        number,
                        model: p[1].trim().to_string(),
                        bus: p[2].trim().to_string(),
                        offline: p[3].trim().eq_ignore_ascii_case("true"),
                        status: p[4].trim().to_string(),
                        size_gb: p[5].trim().parse().unwrap_or(0.0),
                        is_system: p[6].trim().eq_ignore_ascii_case("true"),
                    });
                }
            }
        } else if let Some(rest) = t.strip_prefix("PDVOL=") {
            // PDVOL=<diskNumber>|<letter>: which volume lives on which disk.
            let p: Vec<&str> = rest.split('|').collect();
            if p.len() >= 2 {
                if let Ok(n) = p[0].trim().parse::<u32>() {
                    let letter = p[1].trim().trim_end_matches(':').to_uppercase();
                    if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_alphabetic()) {
                        pdvol.insert(letter, n);
                    }
                }
            }
        }
    }
    vols.sort_by_key(|d| d.letter.clone());
    disks.sort_by_key(|d| d.number);
    (vols, disks, pdvol)
}

/// GET /api/system/usb/list — machine-readable volumes + disks for the
/// per-drive eject/replug UI (mounted volumes ∪ offline disks, no hardcoded
/// letters; ejected drives have no volume so they only appear as disks).
pub async fn usb_list() -> ApiResult<Json<serde_json::Value>> {
    let msg = tokio::task::spawn_blocking(|| run_usb("status", ""))
        .await
        .unwrap_or_else(|_| "error: spawn_blocking failed".to_string());
    let (volumes, disks, pdvol) = parse_usb_status(&msg);
    Ok(Json(serde_json::json!({
        "ok": true,
        "volumes": volumes,
        "disks": disks,
        "pdvol": pdvol,
        "raw": msg.split('\n').take(30).collect::<Vec<_>>().join("\n"),
    })))
}
