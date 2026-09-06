//! FlyGo webcli — Rust 单二进制版。
//!
//! - GET /     : 由 rust-embed 内嵌 static/ 目录（相对 Cargo.toml），返回 index.html
//! - GET /term : WebSocket 升级；每连接启动一个全新 ConPTY 会话（portable-pty）
//! - 可选鉴权   : 环境变量 WEBTOKEN 非空时，ws 查询参数 token 必须匹配，否则 close 1008
//! - 审计日志   : 会话结束时追加一行 JSON 到 CWD/audit.log
//!
//! 环境变量：
//!   WEBHOST    监听地址，默认 127.0.0.1
//!   WEBPORT    监听端口，默认 8788
//!   WEBTOKEN   非空则启用 ws 鉴权
//!   TERM_SHELL 会话 shell，默认 cmd.exe
//!   TERM_CWD   会话工作目录，默认用户主目录
//!
//! 入消息（文本帧 JSON）：{"type":"input","data":"..."}、{"type":"resize","cols":N,"rows":N}
//! 出消息：exit 仍为文本帧 JSON {"type":"exit","code":N}；output 改为 Binary 帧
//!   （pty 原始字节透传，UTF-8，前端负责解码）—— 避免 GBK 字节 / 跨块多字节
//!   字符被 from_utf8_lossy 破坏导致 TUI 网格漂移黑屏。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Instant;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rust_embed::RustEmbed;
use serde_json::{json, Value};

/// 内嵌 static/ 目录（folder 相对 Cargo.toml），编译期打包进二进制。
#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

/// pty 读线程 → ws 任务的通道事件。
enum PtyEvent {
    Output(Vec<u8>),
    Eof,
}

/// 用户主目录（Windows 取 USERPROFILE，Unix 取 HOME）。
fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// GET / —— 返回内嵌的 index.html。
async fn index() -> Response {
    match Assets::get("index.html") {
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(file.data.into_owned()))
            .unwrap(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 按扩展名给静态文件一个浏览器能识别的 Content-Type。
fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "map" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// GET /{*path} —— 其余路径从内嵌 static/ 取静态文件（xterm.js 等）。
/// 未命中返回 404；index.html 已由 / 路由接管，这里不重复兜底。
async fn static_file(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Assets::get(path) {
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime_for(path))
            .body(Body::from(file.data.into_owned()))
            .unwrap(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// GET /term —— WebSocket 升级 + 可选鉴权（WEBTOKEN 非空时校验 ?token=）。
async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let token = std::env::var("WEBTOKEN").unwrap_or_default();
    if !token.is_empty() && params.get("token").map(String::as_str) != Some(token.as_str()) {
        tracing::warn!(%addr, "ws rejected: invalid token");
        return ws.on_upgrade(|mut socket| async move {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 1008,
                    reason: "invalid token".into(),
                })))
                .await;
        });
    }
    let shell = std::env::var("TERM_SHELL").unwrap_or_else(|_| "cmd.exe".to_string());
    let cwd = std::env::var_os("TERM_CWD")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(home_dir);
    ws.on_upgrade(move |socket| async move {
        run_session(socket, addr, shell, cwd).await;
    })
}

/// 单个 WebSocket 连接的完整会话生命周期：开 ConPTY → 双向转发 → 收尾审计。
async fn run_session(mut socket: WebSocket, addr: SocketAddr, shell: String, cwd: PathBuf) {
    let start = Instant::now();
    let mut exit_code: u32 = 0;

    // 1. 打开全新 ConPTY（默认 80x24，前端会随 resize 消息调整）
    let pty = match native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(%addr, error = %e, "openpty failed");
            let _ = socket.send(Message::Text(json!({"type": "exit", "code": 1}).to_string())).await;
            return;
        }
    };

    // 2. 启动 shell
    let mut cmd = CommandBuilder::new(shell.as_str());
    cmd.cwd(&cwd);
    // Prefer 256-color/truecolor so vim and other TUI apps render correctly
    // through xterm.js instead of falling back to 16 colors.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let mut child = match pty.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(%addr, error = %e, "spawn shell failed");
            let _ = socket.send(Message::Text(json!({"type": "exit", "code": 1}).to_string())).await;
            return;
        }
    };
    drop(pty.slave);

    // 3. 取 pty 读写端
    let mut reader = match pty.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(%addr, error = %e, "pty reader failed");
            let _ = child.kill();
            let _ = socket.send(Message::Text(json!({"type": "exit", "code": 1}).to_string())).await;
            return;
        }
    };
    let mut writer = match pty.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(%addr, error = %e, "pty writer failed");
            let _ = child.kill();
            let _ = socket.send(Message::Text(json!({"type": "exit", "code": 1}).to_string())).await;
            return;
        }
    };

    // The console codepage switch to UTF-8 is deferred until the first user
    // input instead of being injected right after spawn: running "chcp" on a
    // live ConPTY screen buffer can trigger full-screen redraws that corrupt
    // alternate-screen TUI apps (microsoft/terminal#9446). By the time the
    // user's first command runs the session is already on codepage 65001, so
    // vim/htop never see a mid-session codepage switch. The host system
    // codepage is GBK(936) whose bytes are not valid UTF-8 and would render as
    // '?' after the frontend decodes the byte stream. cmd/powershell both
    // honor "chcp 65001" inside the session; other shells (msys bash etc.)
    // already emit UTF-8 bytes, so skip them to avoid a stray "chcp not found".
    let shell_lower = shell.to_lowercase();
    let mut chcp_pending = shell_lower.contains("cmd") || shell_lower.contains("powershell");

    tracing::info!(%addr, %shell, "ws session opened");

    // 4. Reader thread: raw pty bytes -> channel; EOF -> Eof event. No UTF-8
    //    decoding here: chunks are forwarded verbatim as Binary frames.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PtyEvent>(256);
    {
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF：会话结束
                    Ok(n) => {
                        if tx.blocking_send(PtyEvent::Output(buf[..n].to_vec())).is_err() {
                            return; // ws 侧已关闭
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%addr, error = %e, "pty read error");
                        break;
                    }
                }
            }
            let _ = tx.blocking_send(PtyEvent::Eof);
        });
    }

    // 5. 主循环：pty 输出 → ws；ws 消息 → pty（input 写入 / resize 改尺寸）
    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(PtyEvent::Output(bytes)) => {
                        // Send raw pty bytes as a Binary frame. This bypasses
                        // String::from_utf8_lossy entirely: a multi-byte UTF-8
                        // char split across two 8KB chunks stays intact because
                        // the frontend joins bytes before decoding. GBK bytes
                        // are also no longer lossy-converted here (they are
                        // handled by the chcp 65001 session setup instead).
                        if socket.send(Message::Binary(bytes)).await.is_err() {
                            let _ = child.kill();
                            break;
                        }
                    }
                    Some(PtyEvent::Eof) => {
                        exit_code = match child.try_wait() {
                            Ok(Some(st)) => st.exit_code(),
                            Ok(None) => {
                                let _ = child.kill();
                                child.wait().map(|st| st.exit_code()).unwrap_or(1)
                            }
                            Err(_) => 1,
                        };
                        let _ = socket.send(Message::Text(json!({"type": "exit", "code": exit_code}).to_string())).await;
                        break;
                    }
                    None => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let v: Value = match serde_json::from_str(text.as_str()) {
                            Ok(v) => v,
                            Err(_) => continue, // 忽略坏帧
                        };
                        match v.get("type").and_then(Value::as_str) {
                            Some("input") => {
                                if let Some(data) = v.get("data").and_then(Value::as_str) {
                                    // Flush the deferred codepage switch before
                                    // the first user keystroke, so the session
                                    // is UTF-8 before any TUI can start.
                                    if chcp_pending {
                                        chcp_pending = false;
                                        if let Err(e) = writer.write_all(b"chcp 65001>nul\r") {
                                            tracing::warn!(%addr, error = %e, "chcp write failed");
                                        }
                                    }
                                    // Ctrl+C arrives as "\x03" and is forwarded as-is.
                                    // Known limitation: portable-pty 0.9.0 spawns
                                    // ConPTY children WITHOUT CREATE_NEW_PROCESS_GROUP
                                    // and this daemon owns no console, so
                                    // GenerateConsoleCtrlEvent cannot interrupt the
                                    // child; a real Ctrl+C requires a custom ConPTY
                                    // spawn (CreateProcess + PROC_THREAD_ATTRIBUTE_
                                    // PSEUDOCONSOLE). Kept as-is on purpose.
                                    if let Err(e) = writer.write_all(data.as_bytes()) {
                                        tracing::warn!(%addr, error = %e, "pty write failed");
                                    }
                                }
                            }
                            Some("resize") => {
                                let cols = v.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
                                let rows = v.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                                if let Err(e) = pty.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }) {
                                    tracing::warn!(%addr, error = %e, "pty resize failed");
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        // ws 断开 → 杀掉会话进程
                        let _ = child.kill();
                        if let Ok(Some(st)) = child.try_wait() {
                            exit_code = st.exit_code();
                        }
                        break;
                    }
                    Some(Ok(_)) => {} // ping/pong/binary：忽略
                }
            }
        }
    }

    // 6. 审计 + 会话关闭日志
    let duration_s = (start.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
    write_audit(&addr, &shell, duration_s, exit_code);
    tracing::info!(%addr, exit_code, duration_s, "ws session closed");
}

/// 会话结束：追加一行 JSON 到 CWD/audit.log（本地时间、来源 IP、shell、时长秒、退出码）。
fn write_audit(addr: &SocketAddr, shell: &str, duration_s: f64, exit_code: u32) {
    let record = json!({
        "ts": chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        "ip": addr.ip().to_string(),
        "shell": shell,
        "duration_s": duration_s,
        "exit_code": exit_code,
    });
    match std::fs::OpenOptions::new().create(true).append(true).open("audit.log") {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{record}") {
                tracing::warn!(error = %e, "audit write failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "audit.log open failed"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let host = std::env::var("WEBHOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("WEBPORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8788);

    let app = Router::new()
        .route("/", get(index))
        .route("/term", get(ws_handler))
        .fallback(static_file);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!(%host, port, "flygo-webcli listening");

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}
