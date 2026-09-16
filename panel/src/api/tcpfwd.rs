// 本机 TCP 转发层：让 compose 容器端口经 Tailscale IP / 局域网 IP 直达。
//
// 背景：容器活在 WSL2 里，Windows 侧 Docker Desktop 只监听 127.0.0.1:P。
// 历史两条路都有坑：
//   1) netsh portproxy（100.x:P -> 127.0.0.1:P）对 DERP 中继包无效；
//   2) tailscale serve 只认 `.ts.net` 域名不认 IP，远程经 IP 必 404。
// 本模块让面板进程自己监听通用端口全集（compose 声明 ∪ docker 实际发布，
// 加容器零配置自动接入）并中继到 127.0.0.1:P，
// 与面板 8388 的监听同原理，成为这批端口的唯一 owner：
//   - 监听地址 = Tailscale IP + 默认出口局域网 IP（不能绑 0.0.0.0，
//     127.0.0.1:P 已被 Docker Desktop 占用，0.0.0.0 会bind冲突）；
//   - 同端口的 tailscale serve 条目会被逐出（面板 443/8443 不动）；
//   - 旧 portproxy 规则保留不动，作同网段直连兜底（NAT，不占端口，无冲突）。
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::winproc::hidden_command;
use tokio::net::{TcpListener, TcpStream};

/// 调和间隔：IP 变化（DHCP / tailnet 重连）与端口占用恢复都靠下一轮自愈。
const RECONCILE_SECS: u64 = 30;

/// 运行中的监听任务：(listen_ip, port) -> JoinHandle。
type FwdMap = HashMap<(String, u16), tokio::task::JoinHandle<()>>;
static REG: OnceLock<Mutex<FwdMap>> = OnceLock::new();

fn reg() -> &'static Mutex<HashMap<(String, u16), tokio::task::JoinHandle<()>>> {
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 默认出口的局域网 IPv4（UDP connect 是惰性的，不发包，离线也可用）。
fn default_lan_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => {
            Some(v4.to_string())
        }
        _ => None,
    }
}

/// 本轮期望的监听集合：通用端口全集（compose 声明 ∪ docker 实际发布，
/// 加容器零配置自动接入）× 可达 IP 全集。
/// pub(crate)：自检步比对"应跑/在跑"用同一来源。
pub(crate) fn desired_listeners() -> Vec<(String, u16)> {
    desired_listeners_for(&crate::api::wsl::all_forward_ports(), &listen_ips())
}

/// 可达 IP：tailnet IP + 默认出口 IP + 本机全部站点内网 IP（去重）。
/// 默认出口可能走 VPN/TUN（拿不到 192.168.x），所以再扫一遍 ipconfig，
/// 多网卡 / 换网段后下一轮调和自动跟上。
fn listen_ips() -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |ip: String| {
        if !out.contains(&ip) {
            out.push(ip);
        }
    };
    if let Some(ip) = crate::api::wsl::tailnet_ip() {
        push(ip);
    }
    if let Some(ip) = default_lan_ip() {
        push(ip);
    }
    for ip in ipconfig_ipv4s() {
        push(ip);
    }
    out
}

/// ipconfig 输出里的全部 IPv4（行内含 "IPv4" 的才认，中英文系统通用；
/// 跳过回环/链路本地；100.x 的 tailnet 段由 tailnet_ip() 专管，此处照收，
/// 去重后无害）。
fn ipconfig_ipv4s() -> Vec<String> {
    let out = hidden_command("ipconfig")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    let ips = Vec::new();
    let Ok(out) = out else { return ips };
    parse_ipconfig_ipv4s(&String::from_utf8_lossy(&out.stdout))
}

/// ipconfig 文本 -> IPv4 集合（纯字符串逻辑，可单测）。
fn parse_ipconfig_ipv4s(text: &str) -> Vec<String> {
    let mut ips = Vec::new();
    for line in text.lines() {
        if !line.contains("IPv4") {
            continue;
        }
        let Some(v4) = line
            .split_whitespace()
            .rev()
            .find_map(|t| t.parse::<std::net::Ipv4Addr>().ok())
        else {
            continue;
        };
        if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
            continue;
        }
        let s = v4.to_string();
        if !ips.contains(&s) {
            ips.push(s);
        }
    }
    ips
}

fn desired_listeners_for(ports: &[u16], ips: &[String]) -> Vec<(String, u16)> {
    let mut set = HashSet::new();
    for p in ports {
        for ip in ips {
            set.insert((ip.clone(), *p));
        }
    }
    let mut out: Vec<(String, u16)> = set.into_iter().collect();
    out.sort();
    out
}

/// 逐出 tailscale serve 对该端口的占用（http/https 两种模式都清；
/// 面板 443/8443 不在 compose 端口集里，不会误伤）。
/// 同步阻塞调用：只在调和线程（spawn_blocking 上下文）里用。
/// CREATE_NO_WINDOW：面板是 GUI 进程，子进程不带此标志会闪黑窗口。
fn evict_serve(port: u16) {
    for mode in ["--https", "--http"] {
        let _ = hidden_command("tailscale")
            .args(["serve", &format!("{mode}={port}"), "off"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// 单条连接：client <-> 127.0.0.1:port 双向拷贝。
/// 只记开关两行（断连排障用：lived 时长 + 上下行字节 + 哪一侧先断）。
async fn pump(client: TcpStream, peer: SocketAddr, listen: SocketAddr, port: u16) {
    let start = std::time::Instant::now();
    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let upstream = match TcpStream::connect(target).await {
        Ok(s) => s,
        Err(e) => {
            let line = format!("conn {peer} via {listen} p{port}: upstream refused ({e})");
            eprintln!("[tcpfwd] {line}");
            crate::log::log("WARN", "tcpfwd.conn", &line);
            return;
        }
    };
    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();
    let (c2u, u2c) = tokio::join!(
        tokio::io::copy(&mut cr, &mut uw),
        tokio::io::copy(&mut ur, &mut cw),
    );
    // Dual-sink: stderr (live console) + file log (post-mortem disconnect
    // forensics; eprintln alone is lost under the scheduled task).
    let line = format!(
        "conn {peer} via {listen} p{port}: lived {:.1}s up={}B down={}B end={}/{}",
        start.elapsed().as_secs_f32(),
        c2u.as_ref().copied().unwrap_or(0),
        u2c.as_ref().copied().unwrap_or(0),
        if c2u.is_ok() { "c-eof" } else { "c-err" },
        if u2c.is_ok() { "u-eof" } else { "u-err" },
    );
    eprintln!("[tcpfwd] {line}");
    crate::log::log("INFO", "tcpfwd.conn", &line);
}

/// 单个监听任务：bind 失败（多半是 serve 还没逐出）则退出，
/// 下一轮调和重试；accept 循环永不主动退出。
async fn listen_task(ip: String, port: u16) {
    let addr: SocketAddr = match format!("{ip}:{port}").parse() {
        Ok(a) => a,
        Err(_) => return,
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        // 被占位多半是残留的 serve 条目：逐出后重试一次，保住唯一 owner；
        // 仍失败则退出等下一轮（is_finished 触发调和重建）。
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("[tcpfwd] {addr} in use, evicting serve and retrying ({e})");
            tokio::task::spawn_blocking(move || evict_serve(port))
                .await
                .ok();
            match TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e2) => {
                    eprintln!("[tcpfwd] listen {addr} failed after evict: {e2}");
                    return;
                }
            }
        }
        Err(e) => {
            eprintln!("[tcpfwd] listen {addr} failed: {e}");
            return;
        }
    };
    eprintln!("[tcpfwd] listening on {addr} -> 127.0.0.1:{port}");
    loop {
        match listener.accept().await {
            Ok((client, peer)) => {
                tokio::spawn(pump(client, peer, addr, port));
            }
            Err(e) => {
                eprintln!("[tcpfwd] accept on {addr} failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// 调和一轮：补齐缺失的监听，收掉已过期（IP 变了 / 端口没了）的。
/// 返回 (期望数, 在跑数)。
fn reconcile() -> (usize, usize) {
    let desired = desired_listeners();
    let want: HashSet<(String, u16)> = desired.into_iter().collect();
    let mut r = reg().lock().unwrap_or_else(|e| e.into_inner());
    // 收掉过期项（先 abort，下轮按需重建；bind 冲突靠逐出 serve 解决）。
    r.retain(|k, h| {
        if want.contains(k) && !h.is_finished() {
            true
        } else {
            h.abort();
            false
        }
    });
    // 补齐缺失项（监听任务自己负责 bind；被 serve 占位时任务内懒逐出，
    // 首轮调和零等待，大全集端口也不卡）。
    for (ip, port) in &want {
        if r.contains_key(&(ip.clone(), *port)) {
            continue;
        }
        let (ip2, p2) = (ip.clone(), *port);
        r.insert(
            (ip.clone(), *port),
            tokio::spawn(async move { listen_task(ip2, p2).await }),
        );
    }
    let running = r.len();
    (want.len(), running)
}

/// 常驻后台任务：main.rs 里 tokio::spawn，永不返回，失败只记日志。
pub async fn forward_task() {
    loop {
        let (want, running) =
            tokio::task::spawn_blocking(reconcile).await.unwrap_or((0, 0));
        eprintln!("[tcpfwd] reconcile: {running}/{want} listeners running");
        tokio::time::sleep(Duration::from_secs(RECONCILE_SECS)).await;
    }
}

/// 自检步用的同步状态：只读注册表，不做任何变更（变更归 forward_task）。
/// pub(crate)：wsl.rs 自检第 7 步调用。
pub(crate) fn status_for_selfcheck() -> (bool, String) {
    let want: HashSet<(String, u16)> = desired_listeners().into_iter().collect();
    if want.is_empty() {
        return (
            true,
            "no published TCP ports (compose + docker ps); nothing to forward".to_string(),
        );
    }
    let r = reg().lock().unwrap_or_else(|e| e.into_inner());
    let running: HashSet<(String, u16)> = r
        .iter()
        .filter(|(_, h)| !h.is_finished())
        .map(|(k, _)| k.clone())
        .collect();
    let missing: Vec<String> = want
        .difference(&running)
        .map(|(ip, p)| format!("{ip}:{p}"))
        .collect();
    if missing.is_empty() {
        (
            true,
            format!("all {} forwarder(s) running ({})", want.len(), describe(&want)),
        )
    } else {
        (
            false,
            format!(
                "{}/{} running, missing: {} (forward_task 调和中，30s 后复查)",
                running.len(),
                want.len(),
                missing.join(",")
            ),
        )
    }
}

fn describe(set: &HashSet<(String, u16)>) -> String {
    let mut v: Vec<String> = set.iter().map(|(ip, p)| format!("{ip}:{p}")).collect();
    v.sort();
    let shown: Vec<String> = v.into_iter().take(6).collect();
    shown.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipconfig_parser_takes_all_site_local_v4() {
        let text = "   IPv4 地址 . . . . . . . . . . . . : 192.168.1.50\n\
                    \x20  IPv4 Address. . . . . . . . . . . . : 100.101.102.103\n\
                    \x20  IPv4 地址 . . . . . . . . . . . . : 127.0.0.1\n\
                    \x20  IPv4 地址 . . . . . . . . . . . . : 169.254.1.2\n\
                    \x20  子网掩码  . . . . . . . . . . . . : 255.255.255.0\n";
        assert_eq!(
            parse_ipconfig_ipv4s(text),
            vec!["192.168.1.50".to_string(), "100.101.102.103".to_string()]
        );
    }

    #[test]
    fn desired_set_dedups_ips_and_ports() {        let ports = vec![5244, 8096, 5244];
        let ips = vec!["100.1.2.3".to_string(), "100.1.2.3".to_string()];
        let got = desired_listeners_for(&ports, &ips);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&("100.1.2.3".to_string(), 5244)));
        assert!(got.contains(&("100.1.2.3".to_string(), 8096)));
    }

    /// 回环实测：echo 后端 + 一次性转发 + 客户端往返。
    #[tokio::test]
    async fn loopback_forward_roundtrip() {        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_addr = front.local_addr().unwrap();
        let target_port = echo_addr.port();
        tokio::spawn(async move {
            let (client, peer) = front.accept().await.unwrap();
            pump(client, peer, front_addr, target_port).await;
        });
        let mut c = TcpStream::connect(front_addr).await.unwrap();
        let (mut r, mut w) = c.split();
        tokio::io::AsyncWriteExt::write_all(&mut w, b"flygo-fwd")
            .await
            .unwrap();
        let mut buf = [0u8; 9];
        tokio::io::AsyncReadExt::read_exact(&mut r, &mut buf)
            .await
            .unwrap();
        assert_eq!(&buf, b"flygo-fwd");
    }
}
