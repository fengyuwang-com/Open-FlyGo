// /api/term* + /term-ws — 终端页：0909 密码锁 + WebSocket 反向代理到本机 webcli(127.0.0.1:8788)
//
// 安全模型（Tailscale 内网场景，面板本身无鉴权，终端页单独上锁）：
//   - 密钥文件 term_token.env（FENGPAGE_TERM_ENV 可覆盖路径）：
//       PASSWORD 每次校验时重新读文件（改密码即时生效，无需重启）
//       TOKEN    进程内只读一次缓存（webcli 的 WEBTOKEN）
//   - 解锁成功签发 HttpOnly cookie 门票（内存 map 记过期时刻，12 小时）
//   - /term-ws 必须带有效门票；无门票一律 403，不升级 WS
//   - 修改密码：原子更新文件（临时文件 + rename 覆盖，TOKEN 行不动），
//     改完清空整张门票 map —— 所有会话立即失效，需重新解锁
//   - 浏览器侧 Ping 显式回 Pong（心跳保活），webcli 侧由 tungstenite 自动处理
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Path,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_tungstenite::{connect_async, tungstenite as tw, MaybeTlsStream, WebSocketStream};

// ---------- 配置 ----------
/// 本机 webcli（前端资源同源的终端网关）。目标地址来自配置文件
/// [term].webcli_addr，缺省时由 [webcli] host:port 推导（settings.rs），
/// 保证改端口时反代目标与 daemon spawn 同源。
fn web_cli_addr() -> String {
    crate::settings::get().term_webcli_addr()
}

/// 密钥文件路径由 config 解析（FENGPAGE_TERM_ENV 可覆盖，见 config.rs）。
fn env_path() -> String {
    crate::config::get().term_env.to_string_lossy().into_owned()
}

/// 读密钥文件（K=V 行，跳过空行/注释）。PASSWORD 每次校验都重读。
fn read_env() -> Result<HashMap<String, String>, String> {
    let p = env_path();
    let content = std::fs::read_to_string(&p).map_err(|e| format!("读取 {p} 失败: {e}"))?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(map)
}

/// TOKEN 进程内缓存一次即可（webcli 的 WEBTOKEN 不会热更新）
static TOKEN_CACHE: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));
fn token_cached() -> Option<String> {
    let mut g = TOKEN_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = read_env().ok().and_then(|m| m.get("TOKEN").cloned());
    }
    g.clone()
}

fn current_password() -> Option<String> {
    read_env().ok().and_then(|m| m.get("PASSWORD").cloned())
}

// ---------- 会话锁：内存 ticket + HttpOnly cookie（12h） ----------
static SESSIONS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const COOKIE_NAME: &str = "feng_term";

/// ticket 无需密码学强随机（Tailscale-only）：时间纳秒 + 进程 id + 递增计数器
fn gen_ticket() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:020x}{pid:08x}{c:08x}")
}

fn unlocked(ticket: &str) -> bool {
    let mut g = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    match g.get(ticket) {
        Some(exp) if *exp > Instant::now() => true,
        _ => {
            g.remove(ticket);
            false
        }
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for v in headers.get_all(header::COOKIE) {
        if let Ok(s) = v.to_str() {
            for part in s.split(';') {
                if let Some((k, val)) = part.trim().split_once('=') {
                    if k.trim() == name {
                        return Some(val.trim().to_string());
                    }
                }
            }
        }
    }
    None
}

fn locked(headers: &HeaderMap) -> bool {
    match cookie_value(headers, COOKIE_NAME) {
        Some(t) => !unlocked(&t),
        None => true,
    }
}

fn ok_json(ok: bool, message: &str) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": ok, "message": message }))
}

// ---------- 端点 ----------
/// GET /api/term/status — 前端进入终端页时查锁状态
pub async fn status(headers: HeaderMap) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "locked": locked(&headers) }))
}

#[derive(Deserialize)]
pub struct UnlockReq {
    pub password: String,
}

/// POST /api/term/unlock body:{"password":"..."} — 成功签发门票 + Set-Cookie
pub async fn unlock(Json(body): Json<UnlockReq>) -> Response {
    let cur = match current_password() {
        Some(p) => p,
        None => return ok_json(false, "term_token.env 缺失或格式错误").into_response(),
    };
    if body.password.trim() != cur.trim() {
        return ok_json(false, "密码错误").into_response();
    }
    let ticket = gen_ticket();
    SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ticket.clone(), Instant::now() + SESSION_TTL);
    let mut res = ok_json(true, "已解锁").into_response();
    if let Ok(v) = HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={ticket}; HttpOnly; SameSite=Lax; Max-Age=43200; Path=/"
    )) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

#[derive(Deserialize)]
pub struct PassReq {
    pub old: String,
    pub new: String,
}

/// POST /api/term/password body:{"old","new"} — 校验旧密码 -> 原子更新文件 -> 清空门票
pub async fn password(Json(body): Json<PassReq>) -> Json<serde_json::Value> {
    let cur = match current_password() {
        Some(p) => p,
        None => return ok_json(false, "term_token.env 缺失或格式错误"),
    };
    if body.old.trim() != cur.trim() {
        return ok_json(false, "旧密码错误");
    }
    let newpw = body.new.trim().to_string();
    if newpw.is_empty() {
        return ok_json(false, "新密码不能为空");
    }
    if newpw.chars().count() < 4 {
        return ok_json(false, "新密码至少 4 个字符");
    }
    match atomic_update_password(&newpw) {
        Ok(()) => {
            // 改密码后所有门票立即失效，需重新解锁
            SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).clear();
            ok_json(true, "密码已修改")
        }
        Err(e) => ok_json(false, &e),
    }
}

/// 原子更新：读原内容 -> 替换 PASSWORD 行 -> 写临时文件 -> rename 覆盖（TOKEN 行保留不动）
fn atomic_update_password(newpw: &str) -> Result<(), String> {
    let p = env_path();
    let content = std::fs::read_to_string(&p).map_err(|e| format!("读取 {p} 失败: {e}"))?;
    let eol = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::new();
    let mut replaced = false;
    for line in content.lines() {
        if line.trim_start().starts_with("PASSWORD=") {
            out.push_str(&format!("PASSWORD={newpw}"));
            replaced = true;
        } else {
            out.push_str(line);
        }
        out.push_str(eol);
    }
    if !replaced {
        return Err("term_token.env 缺少 PASSWORD 行".into());
    }
    let tmp = format!("{p}.tmp");
    std::fs::write(&tmp, out).map_err(|e| format!("写临时文件失败: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("覆盖 {p} 失败: {e}"))?;
    Ok(())
}

// ---------- WS 反向代理 ----------
type TwSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// GET /term-ws — 门票校验通过后连 webcli 并双向转发
pub async fn term_ws(ws: WebSocketUpgrade, headers: HeaderMap) -> Response {
    if locked(&headers) {
        return (StatusCode::FORBIDDEN, "未解锁终端，请先解锁").into_response();
    }
    let token = match token_cached() {
        Some(t) => t,
        None => {
            eprintln!("[term] TOKEN 缺失（term_token.env 未配置），拒绝 WS 连接");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "term_token.env 未配置 TOKEN",
            )
                .into_response();
        }
    };
    // 先连 webcli 再升级：连不上直接 502，前端能感知失败
    let addr = web_cli_addr();
    let url = format!("ws://{addr}/term?token={token}");
    match connect_async(&url).await {
        Ok((tw_sock, _)) => {
            ws.on_upgrade(move |ax_sock| proxy(ax_sock, tw_sock)).into_response()
        }
        Err(e) => {
            eprintln!("[term] webcli({addr}) 连接失败: {e}");
            (StatusCode::BAD_GATEWAY, "终端后端(webcli)不可达").into_response()
        }
    }
}

/// 双向转发核心：单循环 select 两侧读端，任一侧结束/出错即关闭另一侧。
/// 浏览器 Ping 显式回 Pong（tungstenite 底层也会自动回，双保险，浏览器忽略多余 Pong）。
async fn proxy(ax_sock: WebSocket, tw_sock: TwSocket) {
    let (mut ax_tx, mut ax_rx) = ax_sock.split();
    let (mut tw_tx, mut tw_rx) = tw_sock.split();
    loop {
        tokio::select! {
            msg = ax_rx.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break, // 浏览器侧关闭/出错
                };
                match msg {
                    Message::Ping(p) => {
                        // axum 不自动回 Pong，不回浏览器 30s 心跳会断开
                        if ax_tx.send(Message::Pong(p)).await.is_err() { break; }
                    }
                    Message::Close(_) => break,
                    other => {
                        if let Some(tm) = ax2tw(other) {
                            if tw_tx.send(tm).await.is_err() { break; }
                        }
                    }
                }
            }
            msg = tw_rx.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break, // webcli 侧关闭/出错
                };
                match msg {
                    tw::Message::Close(_) => break,
                    // ping/pong 已由 tungstenite 客户端自动处理，无需转发
                    tw::Message::Ping(_) | tw::Message::Pong(_) => {}
                    other => {
                        if let Some(am) = tw2ax(other) {
                            if ax_tx.send(am).await.is_err() { break; }
                        }
                    }
                }
            }
        }
    }
    // 收尾：给另一侧发 Close（对端可能已断开，失败忽略）
    let _ = tw_tx.close().await;
    let _ = ax_tx.close().await;
}

fn ax2tw(m: Message) -> Option<tw::Message> {
    Some(match m {
        Message::Text(s) => tw::Message::Text(s),
        Message::Binary(b) => tw::Message::Binary(b),
        Message::Ping(p) => tw::Message::Ping(p),
        Message::Pong(p) => tw::Message::Pong(p),
        Message::Close(Some(c)) => tw::Message::Close(Some(tw::protocol::CloseFrame {
            code: tw::protocol::frame::coding::CloseCode::from(c.code),
            reason: c.reason,
        })),
        Message::Close(None) => tw::Message::Close(None),
    })
}

fn tw2ax(m: tw::Message) -> Option<Message> {
    Some(match m {
        tw::Message::Text(s) => Message::Text(s),
        tw::Message::Binary(b) => Message::Binary(b),
        tw::Message::Ping(p) => Message::Ping(p),
        tw::Message::Pong(p) => Message::Pong(p),
        tw::Message::Close(Some(c)) => Message::Close(Some(CloseFrame {
            code: c.code.into(),
            reason: c.reason,
        })),
        tw::Message::Close(None) => Message::Close(None),
        tw::Message::Frame(_) => return None, // 原始帧直接跳过（与 axum 自身处理一致）
    })
}

// ---------- 静态资源（xterm/addon-fit，include_str 内嵌，exe 自包含） ----------
const XTERM_JS: &str = include_str!("../../static/vendor/xterm/xterm.js");
const XTERM_CSS: &str = include_str!("../../static/vendor/xterm/xterm.css");
const FITADDON_JS: &str = include_str!("../../static/vendor/addon-fit/addon-fit.js");

/// GET /vendor/{file} — 只放行 xterm.js / xterm.css / addon-fit.js
pub async fn vendor(Path(file): Path<String>) -> Response {
    let (ct, body) = match file.as_str() {
        "xterm.js" => ("text/javascript", XTERM_JS),
        "addon-fit.js" => ("text/javascript", FITADDON_JS),
        "xterm.css" => ("text/css", XTERM_CSS),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, ct)], body).into_response()
}
