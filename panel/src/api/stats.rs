// /api/stats + /api/flags — 服务自身统计与特性开关
// 目标：让 /api/health 能反映"服务自己健不健康"（错误率），并支持运行时开关特性。
// 全零新依赖：用 std atomic + Mutex<HashMap>。
use crate::ApiResult;
use axum::Json;
use serde::Serialize;
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// CREATE_NO_WINDOW: keep the restarted instance from flashing a console.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 总请求数 / 总错误数（>=400 计为错误，连接层错误单独计）
pub static REQUESTS: AtomicU64 = AtomicU64::new(0);
pub static ERRORS: AtomicU64 = AtomicU64::new(0);
pub static CONN_ERRORS: AtomicU64 = AtomicU64::new(0);

/// 控制面连通性计数（/api/control/* 的注册/激活/心跳/直播状态调用结果）。
/// /api/stats 暴露它们，让监控能看出"控制面不可达是持续多久的问题"。
pub static CONTROL_OK: AtomicU64 = AtomicU64::new(0);
pub static CONTROL_FAIL: AtomicU64 = AtomicU64::new(0);

/// 记录一次控制面调用结果（ok=true 表示 HTTP 200 且业务 ok:true）。
/// 由 api/control.rs 与 api/pair.rs 的 plane 调用路径调用。
pub fn record_control(ok: bool) {
    if ok {
        CONTROL_OK.fetch_add(1, Ordering::Relaxed);
    } else {
        CONTROL_FAIL.fetch_add(1, Ordering::Relaxed);
    }
}

/// 按 `方法 路径` 计的请求计数（只保留出现过的最新路径，容量上限防内存涨）
static BY_ROUTE: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 特性开关（运行时可达 /api/flags 读/写，默认值由 init_flags 注入）
static FLAGS: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 在请求日志中间件里调用，逐请求累加统计。零锁热路径只碰两个 atomic。
pub fn record(method: &str, path: &str, status: u16) {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    if status >= 400 {
        ERRORS.fetch_add(1, Ordering::Relaxed);
    }
    // 按路径计数：只更新，锁不常hot（请求日志是 eprintln，本就慢）
    if let Ok(mut map) = BY_ROUTE.lock() {
        if map.len() < 512 {
            *map.entry(format!("{method} {path}")).or_insert(0) += 1;
        }
    }
}

/// Connection-layer error counter hook. Reserved for a future "failed before
/// reaching the app handler" signal; currently no caller (kept, not removed).
#[allow(dead_code)]
pub fn conn_error() {
    CONN_ERRORS.fetch_add(1, Ordering::Relaxed);
}

/// 默认特性开关。KEY 语义：`feature.<名字>`。默认全开，可到 /api/flags 关。
fn default_flags() -> HashMap<String, bool> {
    let mut m = HashMap::new();
    // 示例：feature.auto_retry 由前端读取、feature.usb_eject 控制高危弹出
    m.insert("feature.auto_retry".to_string(), true);
    m.insert("feature.usb_eject".to_string(), true);
    m
}

pub fn flag_enabled(key: &str) -> bool {
    FLAGS.lock().map(|m| *m.get(key).unwrap_or(&true)).unwrap_or(true)
}

/// 特性开关统一入口：GET 读全量，POST 写单个
/// 用单个 handler 收 GET+POST（避免 .get().post() 两个不同 extractor 类型无法 chain 的问题）。
/// 返回：FlagsOut
#[derive(Serialize)]
pub struct FlagsOut {
    pub ok: bool,
    pub flags: HashMap<String, bool>,
}

pub async fn flags(req: axum::extract::Request) -> ApiResult<Json<FlagsOut>> {
    // POST 时读 JSON body 设置开关
    if req.method() == axum::http::Method::POST {
        let bytes = axum::body::to_bytes(req.into_body(), 1024 * 8)
            .await
            .unwrap_or_default();
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let (Some(key), Some(val)) = (
                v.get("key").and_then(|x| x.as_str()).map(|s| s.to_string()),
                v.get("value").and_then(|x| x.as_bool()),
            ) {
                FLAGS.lock().unwrap_or_else(|e| e.into_inner()).insert(key, val);
            }
        }
    }
    let flags = FLAGS.lock().map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())
        .unwrap_or_default();
    Ok(Json(FlagsOut { ok: true, flags }))
}

/// 启动时注入默认开关（main 里调用一次）
pub fn init_flags() {
    let mut m = FLAGS.lock().unwrap_or_else(|e| e.into_inner());
    *m = default_flags();
}

/// 计数测试：record_control 累加控制面连通性计数，stats() 原样暴露。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_counters_are_exposed_via_stats() {
        // Fresh counters: the atomics are process-wide, so snapshot and add.
        let before_ok = CONTROL_OK.load(Ordering::Relaxed);
        let before_fail = CONTROL_FAIL.load(Ordering::Relaxed);
        record_control(true);
        record_control(true);
        record_control(false);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = match rt.block_on(stats()) {
            Ok(o) => o.0,
            Err(_) => panic!("stats handler must not fail"),
        };
        assert_eq!(out.control_ok_total, before_ok + 2);
        assert_eq!(out.control_fail_total, before_fail + 1);
    }
}

/// GET /api/stats -> 请求/错误统计
#[derive(Serialize)]
pub struct StatsOut {
    pub ok: bool,
    pub requests_total: u64,
    pub errors_total: u64,
    pub conn_errors_total: u64,
    pub error_rate_percent: f64,
    pub by_route: Vec<RouteStat>,
    /// 控制面调用成功/失败累计（/api/control/* 的 plane 调用）。
    pub control_ok_total: u64,
    pub control_fail_total: u64,
}

#[derive(Serialize)]
pub struct RouteStat {
    pub route: String,
    pub count: u64,
}

pub async fn stats() -> ApiResult<Json<StatsOut>> {
    let req = REQUESTS.load(Ordering::Relaxed);
    let err = ERRORS.load(Ordering::Relaxed);
    let conn = CONN_ERRORS.load(Ordering::Relaxed);
    let rate = if req > 0 { (err as f64 / req as f64) * 100.0 } else { 0.0 };
    let by_route = BY_ROUTE.lock()
        .map(|m| {
            let mut v: Vec<RouteStat> = m.iter().map(|(k, c)| RouteStat { route: k.clone(), count: *c }).collect();
            v.sort_by(|a, b| b.count.cmp(&a.count));
            v
        })
        .unwrap_or_default();
    Ok(Json(StatsOut {
        ok: true,
        requests_total: req,
        errors_total: err,
        conn_errors_total: conn,
        error_rate_percent: (rate * 100.0).round() / 100.0,
        by_route,
        control_ok_total: CONTROL_OK.load(Ordering::Relaxed),
        control_fail_total: CONTROL_FAIL.load(Ordering::Relaxed),
    }))
}

/// POST /api/restart — 一键自重启
/// Spawn the new instance directly by exe path (no `start` wrapper, which relied
/// on `FlyGo` being on PATH and failed silently), then exit this process
/// after a short delay so the port gets released. The new instance's bind-retry
/// self-heal (main.rs, 10 x 350ms) absorbs the brief EADDRINUSE overlap.
#[derive(Serialize)]
pub struct RestartOut {
    pub ok: bool,
    pub message: String,
}

pub async fn restart() -> ApiResult<Json<RestartOut>> {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return Ok(Json(RestartOut { ok: false, message: format!("无法定位本程序路径: {e}") })),
    };
    let spawned = std::process::Command::new(&exe)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let (ok, msg) = match spawned {
        Ok(_) => (true, format!("已启动新实例，本进程 {} 即将退出并重启...", exe.to_string_lossy())),
        Err(e) => (false, format!("spawn 失败: {e}")),
    };
    // 稍等让端口释放 + 新实例接管，再退出。本次响应先返回，客户端会断开连接。
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    if ok {
        // 走优雅关停：释放单实例互斥锁、停止后台子进程、清理托盘图标后再退出。
        crate::request_shutdown();
    }
    Ok(Json(RestartOut { ok, message: msg }))
}
