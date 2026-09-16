// /api/health — 主机健康大盘
use crate::ApiResult;
use axum::Json;
use serde::Serialize;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;

/// CREATE_NO_WINDOW: powershell.exe is a console app; spawned from a GUI
/// parent it flashes a black window unless this flag is set.
use crate::winproc::CREATE_NO_WINDOW;

// ---- /api/health/services ----
// Service card wall: TCP connectivity probe per service port, all probed
// concurrently (tokio join_all) so one round stays under the 3s timeout.
// Probe target is 127.0.0.1 -- the panel runs on the NAS itself, so localhost
// means the NAS; remote clients reach the panel over Tailscale but the probe
// still measures the NAS, which is the intent.
// Color thresholds (HomeDash-Win inspired): <200ms green, <1000ms yellow,
// timeout/refused red. Services without a host port are "gray" (not probed).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const COLOR_GREEN_MAX_MS: u64 = 200;
const COLOR_YELLOW_MAX_MS: u64 = 1000;

#[derive(Serialize)]
pub struct ServiceProbe {
    pub key: String,
    pub name: String,
    pub desc: String,
    pub port: u16,
    pub icon: String,
    /// Whether the container is up (same source as /api/docker)
    pub running: bool,
    /// TCP 端口是否可达
    pub reachable: bool,
    /// 探测延迟（毫秒；超时项约等于 PROBE_TIMEOUT）
    pub latency_ms: u64,
    /// green / yellow / red / gray（无端口未探测）
    pub color: String,
    /// 未连通原因：refused / timeout / no host port
    pub reason: String,
    /// User-overridable card URL (${FENG_IP} template); None = derive from port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// 单端口 TCP 探测：3s 超时，返回 (可达, 延迟毫秒, 原因)
async fn probe_tcp(port: u16) -> (bool, u64, &'static str) {
    let start = Instant::now();
    let t = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(("127.0.0.1", port))).await;
    let latency = start.elapsed().as_millis() as u64;
    match t {
        Ok(Ok(_)) => (true, latency, "ok"),
        Ok(Err(_)) => (false, latency, "refused"),
        Err(_) => (false, latency, "timeout"),
    }
}

/// 全部服务并发探测：探测列表直接复用 /api/docker 的 compose 解析结果
/// （名称/端口/图标同源，compose_list 为纯文件解析，毫秒级）；容器运行态查询
/// （docker-ctl status，可能慢）与 TCP 探测并行跑，单轮总耗时受最慢一项约束，
/// 正常 <3s。探测本身是纯 async 非阻塞。
pub async fn services_health() -> ApiResult<Json<Vec<ServiceProbe>>> {
    // Compose list merged with the user's card config; hidden cards are
    // dropped here (the overview card wall) but stay on the docker page.
    let list: Vec<_> = crate::api::docker::merged_service_list()
        .into_iter()
        .filter(|s| !s.hidden)
        .collect();
    let running = tokio::task::spawn_blocking(crate::api::docker::running_containers);
    let probed = futures::future::join_all(list.iter().map(|s| {
        let port = s.port;
        async move { (s, probe_tcp(port).await) }
    }))
    .await;
    let running = running.await.unwrap_or_default();
    let out = probed
        .into_iter()
        .map(|(s, (reach, lat, reason))| {
            let (color, reason) = if s.port == 0 {
                ("gray", "no host port")
            } else if reach && lat < COLOR_GREEN_MAX_MS {
                ("green", "ok")
            } else if reach && lat < COLOR_YELLOW_MAX_MS {
                ("yellow", "ok")
            } else {
                ("red", reason)
            };
            ServiceProbe {
                key: s.key.clone(),
                name: s.name.clone(),
                desc: s.desc.clone(),
                port: s.port,
                icon: s.icon.clone(),
                running: running.contains(&s.key) || running.contains(&s.ctn),
                reachable: reach,
                latency_ms: lat,
                color: color.to_string(),
                reason: reason.to_string(),
                url: s.url.clone(),
            }
        })
        .collect();
    Ok(Json(out))
}

#[derive(Serialize)]
pub struct DiskInfo {
    pub name: String,
    pub mount: String,
    pub total_gb: f64,
    pub free_gb: f64,
}

#[derive(Serialize)]
pub struct Health {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub cpu_usage_percent: f64,
    pub mem_total_gb: f64,
    pub mem_used_gb: f64,
    pub mem_usage_percent: f64,
    pub disks: Vec<DiskInfo>,
    pub uptime_secs: u64,
    // 服务自身健康（错误率自检，贯通 /api/stats）：让健康大盘也反映服务自己有没有报错
    pub requests_total: u64,
    pub errors_total: u64,
    pub error_rate_percent: f64,
    pub healthy: bool,
}

/// 采集健康数据（含 CPU 采样 sleep 与 sysinfo 调用，属阻塞，放 spawn_blocking）
/// pub(crate): also reused by the /ws/status push provider (api/wsstatus.rs).
pub(crate) fn collect_health() -> Health {
    let hostname = hostname();
    let (mem_total, mem_used) = memory();
    let cpu = cpu_usage();
    let disks = disks();
    // 服务自身错误率自检（读 /api/stats 的 atomic 计数）
    let req = crate::api::stats::REQUESTS.load(std::sync::atomic::Ordering::Relaxed);
    let err = crate::api::stats::ERRORS.load(std::sync::atomic::Ordering::Relaxed);
    let error_rate = if req > 0 { (err as f64 / req as f64) * 100.0 } else { 0.0 };

    Health {
        hostname,
        os: std::env::consts::OS.to_string(),
        kernel: std::env::consts::ARCH.to_string(),
        cpu_usage_percent: cpu,
        mem_total_gb: bytes_to_gb(mem_total),
        mem_used_gb: bytes_to_gb(mem_used),
        mem_usage_percent: if mem_total > 0 {
            (mem_used as f64 / mem_total as f64) * 100.0
        } else {
            0.0
        },
        disks,
        uptime_secs: uptime(),
        requests_total: req,
        errors_total: err,
        error_rate_percent: (error_rate * 100.0).round() / 100.0,
        // 服务自身健康判定：错误率 < 5% 且没有连接层错误算健康
        healthy: error_rate < 5.0
            && crate::api::stats::CONN_ERRORS.load(std::sync::atomic::Ordering::Relaxed) == 0,
    }
}

/// Fallback health object when the blocking collection fails or panics
/// (marks the node unhealthy instead of erroring the endpoint).
/// pub(crate): shared by the REST handler and the /ws/status provider.
pub(crate) fn fallback_health() -> Health {
    Health {
        hostname: "?unknown".into(),
        os: std::env::consts::OS.to_string(),
        kernel: std::env::consts::ARCH.to_string(),
        cpu_usage_percent: 0.0,
        mem_total_gb: 0.0,
        mem_used_gb: 0.0,
        mem_usage_percent: 0.0,
        disks: Vec::new(),
        uptime_secs: 0,
        requests_total: 0,
        errors_total: 0,
        error_rate_percent: 0.0,
        healthy: false,
    }
}

pub async fn health() -> ApiResult<Json<Health>> {
    let h = tokio::task::spawn_blocking(collect_health)
        .await
        .unwrap_or_else(|_| fallback_health());
    Ok(Json(h))
}

fn bytes_to_gb(b: u64) -> f64 {
    (b as f64) / 1024.0 / 1024.0 / 1024.0
}

fn hostname() -> String {
    use sysinfo::System;
    // sysinfo 0.30：host_name 为关联函数
    System::host_name().unwrap_or_else(|| "?unknown".into())
}

fn memory() -> (u64, u64) {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_memory();
    // sysinfo 0.30 内存单位是 bytes
    (sys.total_memory(), sys.used_memory())
}

fn cpu_usage() -> f64 {
    // 用 PowerShell Get-Counter 的 \Processor(_Total)\% Processor Time——与「任务管理器」同源。
    // 跨 1s 取 2 点再取末值，最后过一次 EMA 平滑，避免瞬时尖峰把读数从 20 跳到 90。
    // 调用阻塞，外层 collect_health 已放 spawn_blocking，不卡 tokio 工作线程。
    let raw = {
        let out = Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", r#"try { [double](Get-Counter '\Processor(_Total)\% Processor Time' -SampleInterval 1 -MaxSamples 2 -ErrorAction Stop).CounterSamples[-1].CookedValue } catch { 0 }"#])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().unwrap_or(0.0),
            Err(_) => 0.0,
        }
    };
    // EMA 平滑（alpha=0.3）：跨多次调用取指数移动平均，读数更贴近"任务管理器那种稳"
    use std::sync::atomic::{AtomicU64, Ordering};
    static EMA: AtomicU64 = AtomicU64::new(0); // 存 f64 bits
    let prev = f64::from_bits(EMA.load(Ordering::Relaxed));
    let smoothed = if prev == 0.0 { raw } else { 0.3 * raw + 0.7 * prev };
    EMA.store(smoothed.to_bits(), Ordering::Relaxed);
    smoothed
}

fn disks() -> Vec<DiskInfo> {
    use sysinfo::{Disks, DiskKind};
    let d = Disks::new_with_refreshed_list();
    let mut out = Vec::new();
    for disk in d.list() {
        // 只看本地文件系统盘（过滤掉光驱等）
        if disk.kind() == DiskKind::Unknown(-1) {
            continue;
        }
        out.push(DiskInfo {
            name: disk.name().to_string_lossy().into_owned(),
            mount: disk.mount_point().to_string_lossy().into_owned(),
            total_gb: (disk.total_space() as f64) / 1024.0 / 1024.0 / 1024.0,
            free_gb: (disk.available_space() as f64) / 1024.0 / 1024.0 / 1024.0,
        });
    }
    out
}

/// uptime() — physical machine uptime in seconds.
///
/// Source: Win32_OperatingSystem.LastBootUpTime (PowerShell/CIM), queried
/// once per UPTIME_CACHE_TTL and cached as UTC epoch seconds. Uptime is then
/// computed per request as now - boot_time, so DST / local-timezone changes
/// cannot skew the value and it stays fresh between cache refreshes.
/// sysinfo's System::uptime() (GetTickCount64) is kept only as a fallback:
/// it undercounts on S3 sleep and can drift after hibernate/clock changes,
/// which is why LastBootUpTime is the authoritative source here.
fn uptime() -> u64 {
    const TTL: Duration = Duration::from_secs(60);
    static CACHE: Mutex<Option<(Instant, u64)>> = Mutex::new(None);

    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let boot_epoch = match *guard {
        Some((at, epoch)) if at.elapsed() < TTL => epoch,
        _ => {
            let epoch = last_boot_epoch_cim().unwrap_or_else(fallback_boot_epoch);
            *guard = Some((Instant::now(), epoch));
            epoch
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(boot_epoch)
}

/// Win32_OperatingSystem.LastBootUpTime as UTC epoch seconds via one
/// PowerShell/CIM call (blocking; callers run inside spawn_blocking).
fn last_boot_epoch_cim() -> Option<u64> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "[long]([DateTimeOffset]((Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime())).ToUnixTimeSeconds()",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

/// Fallback boot epoch: now minus sysinfo's GetTickCount64-based uptime.
fn fallback_boot_epoch() -> u64 {
    use sysinfo::System;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(System::uptime())
}
