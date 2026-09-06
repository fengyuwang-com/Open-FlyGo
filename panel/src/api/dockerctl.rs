// /api/docker/control — Docker 控制
// 调用 scripts/docker-ctl.cmd：
//   op=engine|compose|portainer  -> 三大类开关
//   op=<任意容器名>              -> 单个容器启停（单个容器走 compose up/stop）
//   op=status                    -> 当前容器状态
// 历史坑：docker-ctl.cmd 必须纯 ASCII（中文注释在 GBK 控制台会被吃掉 rem）。
// 已知 bug（本修复）：docker-ctl.cmd 里 `( wsl ... 2>&1 & echo ... )` 的
// 括号块在 stdout 是管道时会把整块输出吞成空，run_ctl_capture_to 把空输出
// 当成"探测超时"——导致 :container/:portainer 分支永远报
// "状态探测超时（引擎可能正在启动）"。修复：单个容器/portainer 不再走
// docker-ctl.cmd，改用直接 `wsl -d <distro> -u root -- bash -lc "...compose..."`
// （2>&1 在 bash 串内、不在 cmd 括号里，输出稳定；再附加退出码标记判成败）。
// 另：compose 里 16 个服务从未被创建过，`docker start <svc>` 会报
// "No such container"——所以启动统一用 `docker compose up -d --no-recreate
// <svc>`（已存在则只启动，不存在则创建）。
// 不唤醒原则：WSL VM 停机时，查询/轮询一律先查 vmmemWSL 进程做前置判断，
// 绝不调用 `wsl -d <distro> -- ...`（那会冷启动 VM、让容器常驻吃内存）；
// 只有用户显式点"引擎 开"才允许唤醒。
use crate::ApiResult;
use axum::Json;
use serde::Serialize;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

/// CREATE_NO_WINDOW: cmd.exe / tasklist.exe / taskkill.exe are console apps;
/// spawned from a GUI parent they flash a black window unless this flag is set.
/// docker-ctl.cmd's wsl.exe children inherit this hidden console.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 开关动作的串行锁：避免多个按钮同时点导致 wsl 命令并发打架。
pub static LOCK: Mutex<()> = Mutex::new(());

/// 状态/查询类调用超时：dockerd 冷启动或卡死时，不让前端/后续操作永久等待。
const STATUS_TIMEOUT_SECS: u64 = 15;
/// 启停类（engine on / compose up 等）超时：冷启动 dockerd、拉起一批容器较慢，放宽到 120s。
pub(crate) const ACTION_TIMEOUT_SECS: u64 = 120;

#[derive(Serialize)]
pub struct DockerCtlResult {
    pub op: String,
    pub action: String,
    pub ok: bool,
    pub message: String,
}

/// WSL2 VM 是否在运行 —— 只查进程、绝不调用 wsl（`wsl -d <distro> -- ...` 会冷启动 VM）。
/// vmmemWSL 是 WSL2 的虚拟机进程：存在 = VM 在跑；不存在 = VM 已停机。
/// NOTE: 本机（Store 版 WSL2）该进程镜像名是 vmmemWSL，无 .exe 后缀。
/// 实测 tasklist 过滤器带 .exe 时输出"没有运行的任务匹配"、永不命中（曾导致状态误报），
/// 故此处必须用不带后缀的 "vmmemWSL"。
/// tasklist 是纯本地进程查询，不启动任何东西；查询失败按"未运行"处理（宁可显示停机，也不唤醒）。
pub(crate) fn wsl_vm_running() -> bool {
    let out = Command::new("tasklist")
        .args(["/fi", "imagename eq vmmemWSL", "/fo", "csv", "/nh"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_lowercase().contains("vmmemwsl"),
        Err(_) => false,
    }
}

/// 执行 docker-ctl.cmd 并返回原始输出（含状态行）。供本模块与 docker.rs 复用。
/// 默认按状态查询的 15s 超时。
pub(crate) fn run_ctl_capture(args: &[&str]) -> String {
    run_ctl_capture_to(args, STATUS_TIMEOUT_SECS)
}

/// 带超时的执行：轮询等待子进程，超时则杀掉整个进程树（含 wsl.exe 后代），
/// 确保全局锁及时释放、前端不会永久卡"—…"。纯 std，零新依赖。
pub(crate) fn run_ctl_capture_to(args: &[&str], timeout_secs: u64) -> String {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // docker-ctl.cmd path is user-configurable; missing = Docker not installed.
    let Some(ctl) = crate::config::get().docker_ctl.clone() else {
        return "error: docker-ctl.cmd not found; Docker not installed? set FENGPAGE_DOCKER_CTL"
            .to_string();
    };
    let mut child = match Command::new("cmd")
        .arg("/c")
        .arg(&ctl)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("error: 无法启动 cmd: {e}"),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // 读输出线程：子进程退出、管道 EOF 后自然结束，不阻塞主流程
    let (tx, rx) = mpsc::channel::<(String, String)>();
    std::thread::spawn(move || {
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut s) = stdout { let _ = s.read_to_string(&mut out); }
        if let Some(mut s) = stderr { let _ = s.read_to_string(&mut err); }
        let _ = tx.send((out, err));
    });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break, // 已退出，下面收读线程的输出
            Ok(None) => {
                if Instant::now() >= deadline {
                    let pid = child.id();
                    let _ = child.kill();
                    let _ = child.wait();
                    // 杀进程树：cmd 的后代 wsl.exe 可能还在跑，一并结束
                    let _ = Command::new("taskkill")
                        .args(["/pid", &pid.to_string(), "/t", "/f"])
                        .creation_flags(CREATE_NO_WINDOW)
                        .output();
                    return "error: 状态探测超时（引擎可能正在启动），请稍候刷新".to_string();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return "error: 等待子进程失败".to_string(),
        }
    }
    let (out, err) = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    let stdout = out.trim().to_string();
    let stderr = err.trim().to_string();
    // stdout/stderr 全空：按"探测超时/无响应"处理，避免前端把空输出当成"没有 Up 容器"而误判停机
    if stdout.is_empty() && stderr.is_empty() {
        return "error: 状态探测超时（引擎可能正在启动），请稍候刷新".to_string();
    }
    if stdout.is_empty() { stderr } else { stdout }
}

// ---------------- 单个容器：直接 WSL + docker compose ----------------
// 见文件头注释：绕过 docker-ctl.cmd 的括号空输出 bug；同时用
// `up -d --no-recreate` 兼顾"从未创建"与"已存在仅启动"两种情况。

/// 把宿主机路径（docker-compose.yml 或其目录）映射为 WSL 里的目录：
/// `C:/FengNAS`            -> `/mnt/c/FengNAS`
/// `C:\FengNAS\compose.yml`-> `/mnt/c/FengNAS`（带点 basename 视为文件，取其父目录）
/// `/mnt/c/FengNAS`        -> 原样返回
/// 纯字符串逻辑，可单测；映射不了返回 None（上层回退默认 /mnt/c/FengNAS）。
fn wsl_dir_of(host: &Path) -> Option<String> {
    let raw = host.to_string_lossy().replace('\\', "/");
    let mut p = raw.trim_end_matches('/').to_string();
    if let Some(base) = p.rsplit('/').next() {
        if base.contains('.') && !p.is_empty() {
            // basename 带点视为文件（docker-compose.yml），取其父目录
            p = p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        }
    }
    if p.starts_with("/mnt/") || (p.starts_with('/') && !p.starts_with("//")) {
        return Some(p);
    }
    let bytes = p.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        let drive = p[0..1].to_ascii_lowercase();
        let rest = p[2..].trim_start_matches('/');
        if rest.is_empty() {
            return Some(format!("/mnt/{drive}"));
        }
        return Some(format!("/mnt/{drive}/{rest}"));
    }
    None
}

/// compose 项目目录（WSL 路径）。以 docker-compose.yml 所在目录为准；
/// 未配置时返回 Err 并指引设置项，绝不静默回退到某台机器的旧路径。
fn compose_dir_wsl() -> Result<String, String> {
    match crate::config::get().docker_compose.clone() {
        Some(p) => wsl_dir_of(&p).ok_or_else(|| {
            format!(
                "docker-compose.yml 路径无法映射到 WSL ({}), 请检查 FENGPAGE_DOCKER_COMPOSE)",
                p.display()
            )
        }),
        None => Err("docker-compose.yml 未配置 (FENGPAGE_DOCKER_COMPOSE 或安装目录)，无法执行 compose".into()),
    }
}

/// 单引号包裹并转义（'' 是 POSIX 里单引号内的转义表示），用于拼进 bash -lc 串。
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 拼 compose 执行串：`cd <dir> && docker compose <args...> 2>&1; echo [[exit=$?]]`。
/// 2>&1 在 bash 串内（cmd 看不到），退出码用 ``[[exit=N]]`` 标记带回，据此判成败，
/// 不依赖输出是否为空（compose stop 成功时可能无输出）。
fn build_compose_inner(dir: &str, args: &[&str]) -> String {
    let quoted: Vec<String> = args.iter().map(|a| sq(a)).collect();
    format!(
        "cd {} && docker compose {} 2>&1; echo [[exit=$?]]",
        sq(dir),
        quoted.join(" ")
    )
}

/// 在全局 LOCK 下直接 `wsl -d <distro> -u root -- bash -lc "<inner>"`，
/// 带超时+杀进程树，返回 (是否成功, 原始输出，已剔除退出码标记行)。
/// 不经过 cmd.exe —— 规避 docker-ctl.cmd 括号空输出 bug。
fn compose_run(args: &[&str], timeout_secs: u64) -> (bool, String) {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = match compose_dir_wsl() {
        Ok(d) => d,
        Err(e) => return (false, format!("error: {e}")),
    };
    let d = crate::api::wsl::distro();
    let inner = build_compose_inner(&dir, args);
    let mut child = match Command::new("wsl")
        .args([
            "-d",
            &d,
            "-u",
            "root",
            "--",
            "bash",
            "-lc",
            &inner,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, format!("error: wsl 启动失败: {e}")),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel::<(String, String)>();
    std::thread::spawn(move || {
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut s) = stdout { let _ = s.read_to_string(&mut out); }
        if let Some(mut s) = stderr { let _ = s.read_to_string(&mut err); }
        let _ = tx.send((out, err));
    });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let pid = child.id();
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = Command::new("taskkill")
                        .args(["/pid", &pid.to_string(), "/t", "/f"])
                        .creation_flags(CREATE_NO_WINDOW)
                        .output();
                    return (false, format!("error: compose 操作超时（{timeout_secs}s），进程树已结束"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return (false, "error: 等待 wsl 子进程失败".to_string()),
        }
    }
    let (out, _err) = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    flag_output(&out)
}

/// 解析 compose 输出的退出码标记 ``[[exit=N]]``（compose_run 自己拼的）判成败。
/// 无标记时（WSL 层异常吞掉末尾行）按内容启发式兜底：非空且无错误特征即视为成功
/// —— 保证 `compose stop` 对已停止容器这种"成功但可能无输出"的情况也能判定成功。
fn flag_output(out: &str) -> (bool, String) {
    let mut exit_ok = false;
    let mut has_marker = false;
    let mut lines: Vec<&str> = out.lines().collect();
    if let Some(last) = lines.pop() {
        let t = last.trim();
        if let Some(rest) = t.strip_prefix("[[exit=") {
            if let Some(n) = rest.strip_suffix("]]") {
                has_marker = true;
                exit_ok = n.trim() == "0";
            }
        } else {
            lines.push(last); // 没有标记：保留原样
        }
    }
    let message = lines.join("\n").trim().to_string();
    if has_marker && exit_ok {
        (true, if message.is_empty() { "ok".to_string() } else { message })
    } else if has_marker {
        (
            false,
            if message.is_empty() { "error: compose 命令失败".to_string() } else { message },
        )
    } else {
        let lower = message.to_lowercase();
        let bad = lower.contains("error")
            || lower.contains("no such")
            || lower.contains("not found")
            || lower.contains("failed")
            || lower.contains("unknown");
        (!message.is_empty() && !bad, message)
    }
}

/// 执行并返回 (ok, message)
fn run_cmd(args: &[&str]) -> (bool, String) {
    let msg = run_ctl_capture(args);
    (is_success(&msg), msg)
}

/// run_ctl_capture 返回的字符串里含 `[X] on/off` 成功标记，据此判断是否成功
fn is_success(msg: &str) -> bool {
    !msg.to_lowercase().contains("error") && !msg.is_empty()
}

/// 引擎（WSL VM）未启动时给前端的统一提示
fn engine_down_message() -> String {
    "Docker 未启动（WSL 已停机），点“引擎 开”启动".to_string()
}

fn do_ctl(op: &str, action: &str) -> DockerCtlResult {
    let target: (&str, &str) = match (op, action) {
        ("engine", "on") | ("engine", "off")
        | ("compose", "on") | ("compose", "off")
        | ("portainer", "on") | ("portainer", "off")
        // 任意容器名：op=<name>, action=on|off
        | (_, "on") | (_, "off") => (op, action),
        ("status", _) => {
            // 引擎停机时直接回"未启动"，不调 wsl（wsl -d 会冷启动 VM）
            if !wsl_vm_running() {
                return DockerCtlResult {
                    op: "status".into(), action: "".into(), ok: false, message: engine_down_message(),
                };
            }
            let (ok, msg) = run_cmd(&["status"]);
            return DockerCtlResult { op: "status".into(), action: "".into(), ok, message: msg };
        }
        _ => {
            return DockerCtlResult {
                ok: false, op: op.into(), action: action.into(),
                message: format!("非法操作: {op} {action}（应为容器名或 engine|compose|portainer + on|off）"),
            };
        }
    };
    // 引擎停机时：除"引擎 开"（用户显式唤醒）外，其余启停一律不碰 wsl，提示先开引擎
    if !wsl_vm_running() {
        if target == ("engine", "on") {
            // 用户显式开引擎：允许唤醒 WSL
        } else if target == ("engine", "off") {
            // 引擎已停机：仍按 engine off 联动（清目标态 + 杀保活会话）
            link_engine_state(target, true);
            return DockerCtlResult {
                ok: true, op: "engine".into(), action: "off".into(), message: "Docker 已停机".to_string(),
            };
        } else {
            return DockerCtlResult {
                ok: false, op: target.0.into(), action: target.1.into(), message: engine_down_message(),
            };
        }
    }
    // engine/compose 走 docker-ctl.cmd 批量开关（它们输出稳定、无空输出 bug）；
    // 单个容器与 portainer 走直连 wow compose（见文件头注释的修复说明）。
    let (ok, msg) = match (target.0, target.1) {
        ("engine", _) | ("compose", _) => {
            let m = run_ctl_capture_to(&[target.0, target.1], ACTION_TIMEOUT_SECS);
            (is_success(&m), m)
        }
        // 单个容器 / portainer：up --no-recreate 创建缺失的、启动已存在的
        (_, "on") => compose_run(&["up", "-d", "--no-recreate", target.0], ACTION_TIMEOUT_SECS),
        // 停止单个容器：stop 保留容器（下次 up 秒启）
        _ => compose_run(&["stop", target.0], ACTION_TIMEOUT_SECS),
    };
    // WSL keepalive linkage (guard-design.md stage 1): engine on arms the
    // keepalive thread; engine off clears the target and drops the keepalive
    // session (docker-ctl.cmd already runs `wsl --terminate`).
    link_engine_state(target, ok);
    DockerCtlResult { op: target.0.into(), action: target.1.into(), ok, message: msg }
}

/// WSL keepalive linkage: engine on success sets the persisted target ON and
/// pulls a keepalive session immediately; engine off clears the target and
/// kills the keepalive sessions (wsl --terminate is inside docker-ctl.cmd).
/// Other ops are untouched.
fn link_engine_state(target: (&str, &str), ok: bool) {
    if target.0 != "engine" {
        return;
    }
    match target.1 {
        "on" => {
            if ok {
                crate::api::wsl::set_target(true);
                crate::api::wsl::kick_keepalive();
            }
        }
        "off" => {
            crate::api::wsl::set_target(false);
            let killed = crate::api::wsl::kill_keepalive_sessions();
            if killed > 0 {
                eprintln!("[wsl-keepalive] engine off: dropped {killed} keepalive session(s)");
            }
        }
        _ => {}
    }
}

/// Recreate one compose service so fresh host binds are picked up.
/// Used by the USB guard after (re)insertion: /mnt is bind-mounted as a
/// whole, but the mount table is frozen at container creation (rprivate),
/// so a plain `docker start` never sees a newly inserted drive - only a
/// recreate re-binds the new /mnt/<letter>. Also creates+starts when the
/// container is missing entirely. Best-effort: returns (ok, message),
/// callers log only. pub(crate): daemon.rs USB guard consumes it.
pub(crate) fn recreate_container(name: &str) -> (bool, String) {
    if !wsl_vm_running() {
        return (false, "WSL 未运行，跳过重建".to_string());
    }
    compose_run(&["up", "-d", "--force-recreate", name], ACTION_TIMEOUT_SECS)
}

/// Toggle the docker engine (used by the tray menu). Reuses the global LOCK
/// serialization and the 120s action timeout inside do_ctl. Blocks; call from
/// spawn_blocking so the menu handler stays responsive.
pub(crate) fn engine_toggle(action: &str) -> DockerCtlResult {
    do_ctl("engine", action)
}

/// POST /api/docker/control  body: {"op":"engine|compose|portainer|<container>|status","action":"on|off"}
pub async fn control(body: axum::Json<serde_json::Value>) -> ApiResult<Json<DockerCtlResult>> {
    let op = body.get("op").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
    // WSL/CMD 子进程是阻塞调用，放 spawn_blocking 避免卡住 tokio 工作线程
    let (op_c, action_c) = (op.clone(), action.clone());
    let result = tokio::task::spawn_blocking(move || do_ctl(&op_c, &action_c))
        .await
        .unwrap_or(DockerCtlResult {
            ok: false, op: op.clone(), action: action.clone(),
            message: "spawn_blocking 执行失败".to_string(),
        });
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Host path -> WSL dir mapping used to run compose in the right place.
    #[test]
    fn wsl_dir_mapping() {
        let cases: Vec<(&str, &str)> = vec![
            (r"C:\FengNAS", "/mnt/c/FengNAS"),
            (r"C:\FengNAS\docker-compose.yml", "/mnt/c/FengNAS"),
            ("C:/FengNAS/docker-compose.yml", "/mnt/c/FengNAS"),
            (r"D:\data", "/mnt/d/data"),
            (r"E:\", "/mnt/e"),
            ("/mnt/c/FengNAS", "/mnt/c/FengNAS"),
            ("/mnt/d/data/compose.yaml", "/mnt/d/data"),
        ];
        for (input, want) in cases {
            assert_eq!(
                wsl_dir_of(Path::new(input)).as_deref(),
                Some(want),
                "input {input:?}"
            );
        }
        // 映射不了（UNC / 裸字符串）-> None
        assert!(wsl_dir_of(Path::new(r"\\server\share")).is_none());
        assert!(wsl_dir_of(Path::new("")).is_none());
    }

    /// Compose command string: cds into the dir, runs compose with quoted args,
    /// redirects stderr inside the bash string (never at cmd level), and tags
    /// the exit code for reliable success detection.
    #[test]
    fn compose_inner_builds_quoted_command() {
        let inner = build_compose_inner("/mnt/c/FengNAS", &["up", "-d", "--no-recreate", "alist"]);
        assert!(inner.starts_with("cd '/mnt/c/FengNAS' && docker compose "), "{inner}");
        assert!(inner.contains("'--no-recreate'"), "{inner}");
        assert!(inner.contains("2>&1; echo [[exit=$?]]"), "{inner}");
        // 服务名里的单引号必须被转义，避免注入
        let evil = build_compose_inner("/mnt/c/x", &["up", "-d", "a'b"]);
        assert!(!evil.contains("a'b' "), "single-quote must be escaped: {evil}");
    }

    /// compose_run 解析 `[[exit=N]]` 标记：成功但无输出 -> ok=true；失败 -> false。
    #[test]
    fn compose_success_detection_from_exit_marker() {
        // 模拟 _err 无关、stdout 以 [[exit=0]] 结尾且前面有内容
        let out_ok = "\u{1b}[32m Container alist  Started\u{1b}[0m\n[[exit=0]]";
        let (ok, msg) = flag_output(out_ok);
        assert!(ok);
        assert!(msg.contains("alist"));
        // 成功但无输出
        let (ok2, _) = flag_output("[[exit=0]]");
        assert!(ok2);
        // 失败（exit=1）
        let (ok3, _) = flag_output("service not found\n[[exit=1]]");
        assert!(!ok3);
        // 无标记兜底：非空输出视为成功
        let (ok4, _) = flag_output("Container alist Stopped");
        assert!(ok4);
        // 无标记 + error 特征 -> 失败
        let (ok5, _) = flag_output("Error response from daemon");
        assert!(!ok5);
        // 空输出兜底 -> 失败（不误报成功）
        let (ok6, _) = flag_output("");
        assert!(!ok6);
    }
}
