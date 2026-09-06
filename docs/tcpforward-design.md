# 容器端口直连方案：面板内建 TCP 转发层（tcpfwd）

> 生效：2026-09-05 · 实现：`panel/src/api/tcpfwd.rs` · 自检第 7 步 `tcpforward`
> 访问形态：`http://<100.x>:<端口>/` / `http://<局域网IP>:<端口>/`（无需域名、无需证书）

## 一、为什么需要它

容器活在 WSL2 里，Windows 侧 Docker Desktop 只监听 `127.0.0.1:P`。
历史两条路在远程场景下全灭：

1. **netsh portproxy**（`100.x:P -> 127.0.0.1:P`）：对 DERP 中继包无效。
   同 WiFi 直连时包走真实网卡，portproxy 能拦截；手机走移动数据时包经
   tailscaled 用户态/Wintun 到达，portproxy 的钩子看不到，直接丢包。
   实测：serve 摘掉后 `curl http://100.101.102.103:5244/` → 000。
2. **tailscale serve**：官方设计只认 `.ts.net` 域名不认 IP（上游 issue
   #11301 原话 "we currently only support content served over `.ts.net`
   domains"）。经 IP 访问必 404；且 serve 与 portproxy 抢同一批端口，
   serve 赢、portproxy 规则变死配置，而自检还报 portproxy"全绿"——
   状态撒谎，排障走弯路。另外 serve 的 http/https 模式必须与容器协议
   一致，无人强制时就会出现 "Client sent an HTTP request to an HTTPS
   server" 这类迷惑报错。

面板 8388 本身经 100.x 一直可达（`HTTP 200`），因为它是 Windows 侧
真正的 `0.0.0.0` 监听。结论：**让面板自己成为容器端口的唯一 owner**，
用同样的原理把每个容器端口"钉"在 Windows 网络栈上。

## 二、否决过的备选

- **子路径反向代理**（容器挂到 8388/xxx 下）：L7 方案，每个应用都要配合
  base-path（Jellyfin 需设 baseurl，Immich 官方不支持子路径部署），加一个
  容器配一次，还有 WebSocket 转发/绝对路径逃逸/cookie Path 三件套。
  与"加容器零配置"的通用性要求相冲，否决。
- **Docker Desktop 绑 0.0.0.0**：版本行为不可控，FlyGo 无法程序化保证，否决。
- **继续修 portproxy**：中继包不经过它，无解，保留作同网段直连兜底。

## 三、设计（L4，应用无感知）

- **通用端口全集**（`wsl::all_forward_ports`）：compose 声明 ∪
  `docker ps` 实际发布（TCP only）。新容器只要发布了端口，30 秒内自动
  接入，**不改 FlyGo 任何配置**；容器 down 了端口还占着（连上即断），
  起来立刻通。
- **监听地址**：Tailscale IP + 默认出口 IP + ipconfig 全量站点内网 IP。
  不能绑 `0.0.0.0`（`127.0.0.1:P` 已被 Docker Desktop 占用会冲突）；
  默认出口可能走 VPN/TUN，所以要全量扫（`parse_ipconfig_ipv4s`，中英文
  系统通用）。IP 变化（DHCP/tailnet 重连）下一轮调和自动跟上。
- **唯一 owner**：同端口的 serve 条目在 bind 冲突时懒逐出（只清容器端口，
  面板 443/8443 不动）；旧 portproxy 规则保留，不占端口，无冲突。
- **自检**：第 7 步 `tcpforward` 只读比对"应跑/在跑"，变更归后台
  `forward_task`（30s 调和），`main.rs` 随面板常驻。
- **安静原则**：全部子进程（tailscale/ipconfig/docker ps）带
  `CREATE_NO_WINDOW`；转发只记监听变更与连接开关，不刷屏。
  （教训：首版 `evict_serve` 漏 flag，每轮调和闪黑窗口。）

## 四、访问矩阵（2026-09-05 实测）

| 路径 | 100.x | 局域网 IP | 域名 |
|---|---|---|---|
| 容器（5244/8096/2283…）经 tcpfwd | `http://100.x:P/` 200 | `http://192.168.1.50:P/` 200 | 不需要 |
| 面板 | :8388 直连 | :8388 直连 | `https://<node>.ts.net/`（serve 443） |
| 控制面 | — | — | `https://<node>.ts.net/ctrl`（serve 443） |

## 五、已知边界

- TCP-only（UDP 端口跳过，与 portproxy 同限；影视/相册/Web 全是 TCP）。
- 手机浏览器**直接打开 IP 地址**时不受面板页逻辑保护：全屏方向锁
  （见 `alist-fullscreen-report.md`）等面板侧修复都不生效——竖屏全屏请
  走面板页内的 ⛶，直接 URL 全屏即浏览器默认行为（视频横屏）。
- 远程断连排障（2026-09-06 定因方法）：转发层每条连接记开关日志，同时
  进 stderr 与文件日志（`tcpfwd.conn`，含时长/上下行字节/断侧）：
  1. 先看客户端 Tailscale 在不在线（`tailscale status` / ping）：手机
     offline（本机实测红米 offline 7h、ping 超时）= 手机侧 Tailscale 被
     省电杀掉/断网，与服务端无关；
  2. 在线但打不开：看 `tcpfwd.conn` 有无该客户端 IP 的 `upstream refused`
     （容器掉了）或 `lived` 极短的 `c-err`（DERP 抖动/手机休眠）；
  3. Mac 经 `DERP(hkg)` 中继 115ms 属正常（打洞失败时必然中继，不影响用）。
- USB 热插拔闭环（2026-09-06）：alist 挂载整 `/mnt` 但容器 mount 表在
  创建时冻结（rprivate），新盘必须重建容器才可见。`usb_guard` 在插入时
  逐盘 `mount -t drvfs` + `compose --force-recreate alist`；拔盘时重建掉
   stale 绑定但保持 C/D 服务（不再整停）；显式安全弹出仍先停 alist 放
  句柄。应用层存储由 openlist 同步兜底（见 §六）。
- Defender `Bearfoos.A!ml` 系 ML 启发式误报（新 hash + 无流行度 + 行为
  像木马：计划任务/多端口监听/拉起 powershell）。见 `defender-false-positive.md`。
