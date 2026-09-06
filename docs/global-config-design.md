# FlyGo 整体配置文件设计（global-config-design.md）

> 目标：**系统有一个整体配置文件（用户可编辑，也支持图形界面点点选），数字类的东西（端口/超时/间隔/阈值）不硬编码**。
> 本文档只负责「全部配置参数盘点 + 整体配置文件 schema」；UI 组件/页面的编辑模型由 `docs/editable-components-design.md`（并行 Agent）负责，本文只在其间留引用、不重复设计。
> 盘点范围：`panel/src/`、`panel/static/index.html`、`webcli/`（含 `scripts/` 测试脚本；`control-plane/` 是另一独立服务，仅备注）。

---

## 一、盘点统计（速览）

| 类别 | 数量 | 主要位置 |
|---|---|---|
| 端口（监听/连接/子进程） | 15 处 + 前端 3 处 + control-plane 1 处 | main.rs / daemon.rs / term.rs / sync*.rs / onboarding.rs / launcher.rs / config.rs / webcli main.rs / index.html |
| 超时（HTTP/REST/子进程/等待） | 26 处 | health.rs / sync.rs / syncview.rs / onboarding.rs / wsl.rs / dockerctl.rs / access.rs / daemon.rs / launcher.rs / main.rs / webcli main.rs |
| 轮询/保活/缓存间隔 | 后端 8 处 + 前端 8 处 | daemon.rs / wsl.rs / system.rs / autostart.rs / access.rs / health.rs / index.html |
| 阈值/限制（重试次数/max/buffer/告警线） | 后端 14 处 + 前端 6 处 | main.rs / health.rs / stats.rs / term.rs / access.rs / syncview.rs / index.html |
| 内部常量（保留不配置） | ~10 类 | Win32 常量、pty 默认尺寸、通道容量等 |

**合计约 90+ 处数字硬编码**，其中**应进配置的约 70 项**（端口 18、超时 26、间隔 16、阈值 14 中的绝大部分），其余为内部常量/协议常量。

---

## 二、完整盘点表

### 2.1 端口（监听 / 连接 / 子进程参数）

| 文件:行号 | 当前值 | 用途 | 应进配置 |
|---|---|---|---|
| panel/src/main.rs:249 | `0.0.0.0:8388` | 面板监听地址（已有 FENGPAGE_ADDR env 覆盖） | ✅ [server].addr |
| panel/src/main.rs:246 | FENGPAGE_ADDR env | 面板监听地址 env 层 | ✅ 保留为 env 层 |
| panel/src/main.rs:362 | `http://127.0.0.1:8388` | 托盘「打开面板」浏览器 URL | ✅ 引用 [server].addr |
| panel/src/bin/launcher.rs:15 | `127.0.0.1:8388` | launcher 探测面板端口（独立二进制，硬编码） | ✅ 读同一配置文件（或 FENGPAGE_ADDR） |
| panel/src/bin/launcher.rs:87 | `http://127.0.0.1:8388` | launcher 打开浏览器 | ✅ 同上 |
| panel/src/daemon.rs:40-42 | webcli=8788, syncthing=8384, homepage=9199 | `port_of()`：子进程端口表（停止时按端口杀） | ✅ [services].ports |
| panel/src/daemon.rs:599 | `WEBPORT=8788` | spawn webcli 时注入 env | ✅ [webcli].port（同源） |
| panel/src/daemon.rs:622 | `--gui-address=0.0.0.0:8384` | spawn syncthing GUI 地址 | ✅ [services].ports.syncthing |
| panel/src/daemon.rs:643 | `PORT=9199` | spawn homepage 时注入 env | ✅ [services].ports.homepage |
| panel/src/daemon.rs:872 | `["8788","8384","9199"]` | 启动时清理残留监听（与 port_of 重复定义，需同源合并） | ✅ 同 [services].ports |
| panel/src/api/term.rs:35 | `127.0.0.1:8788` | 终端 WS 反代目标 webcli | ✅ [term].webcli_addr 或引用 [webcli].port |
| panel/src/api/sync.rs:20 | `http://127.0.0.1:8384/...` | Syncthing REST | ✅ [sync].syncthing_addr |
| panel/src/api/syncview.rs:29 | `http://127.0.0.1:8384` | Syncthing REST | ✅ [sync].syncthing_addr（同源） |
| panel/src/api/onboarding.rs:22 | `http://127.0.0.1:8384` | Syncthing REST（第三处重复定义） | ✅ [sync].syncthing_addr（合并三处） |
| panel/src/config.rs:177 | `localhost:9199,127.0.0.1:9199` | homepage 允许 Host 默认值 | ✅ 引用 [services].ports.homepage |
| webcli/src/main.rs:343 | 8788 | webcli 监听端口（已有 WEBPORT env） | ✅ [webcli].port |
| webcli/src/main.rs:339 | `127.0.0.1` | webcli 监听 host（已有 WEBHOST env） | ✅ [webcli].host |
| panel/static/index.html:316/340/361 | `:5244`(alist)、`:8096`(jellyfin)、`:2283`(immich) | iframe 服务地址（容器端口来自 compose） | ⚠️ 联动项：应由 /api/docker 的 port 字段动态构建，不硬编码 |
| control-plane/src/main.rs（备注） | 9001 | 控制面端口（FENGDOG_PORT env，独立服务） | 另文处理，不进本 schema |

### 2.2 超时

| 文件:行号 | 当前值 | 用途 | 应进配置 |
|---|---|---|---|
| panel/src/main.rs:254 | 350ms | 端口占用自愈重试间隔 | ✅ [server].bind_retry_ms |
| panel/src/main.rs:260 | 10 次 | bind 重试上限 | ✅ [server].bind_retries |
| panel/src/main.rs:160 | 600ms | 托盘重启 webcli 的停止→启动间隔 | ✅ 内部小延迟，可留 |
| panel/src/main.rs:174 | 2s | 弹出外置盘前等 docker 释放挂载 | ✅ [usb].eject_grace_secs |
| panel/src/main.rs:304 | 400ms | 退出前等托盘图标消失 | ✅ 内部小延迟，可留 |
| panel/src/api/health.rs:23 | 3s | 服务 TCP 探测超时 | ✅ [health].probe_timeout_secs |
| panel/src/api/sync.rs:21 | 2s | /api/sync 的 Syncthing REST 超时 | ✅ [sync].timeout_secs |
| panel/src/api/syncview.rs:30 | 3s | syncview REST 超时 | ✅ [sync].timeout_secs |
| panel/src/api/onboarding.rs:26 | 5s | onboarding REST 超时（慢扫描余量） | ✅ [sync].onboarding_timeout_secs |
| panel/src/api/wsl.rs:352/355 | 15s/30s | sc query / net start 超时 | ✅ [wsl].cmd_timeout_secs |
| panel/src/api/wsl.rs:390 | 15s | netsh portproxy show 超时 | ✅ [wsl].cmd_timeout_secs |
| panel/src/api/wsl.rs:464 | 15s | 等 WSL VM 起来的最长等待 | ✅ [wsl].vm_wait_secs |
| panel/src/api/wsl.rs:467 | 30s | systemctl is-active 超时 | ✅ [wsl].cmd_timeout_secs |
| panel/src/api/wsl.rs:713 | 10s | PowerShell 查询超时（ps_query） | ✅ [wsl].ps_timeout_secs |
| panel/src/api/wsl.rs:656 | 2s | 子进程输出收尾等待 | ✅ 内部小延迟，可留 |
| panel/src/api/wsl.rs:745 | 2s | VM 轮询步长 | ✅ [wsl].vm_poll_step_secs |
| panel/src/api/dockerctl.rs:28 | 15s | 状态查询超时 | ✅ [docker].status_timeout_secs |
| panel/src/api/dockerctl.rs:30 | 120s | 启停类动作超时（冷启动 dockerd） | ✅ [docker].action_timeout_secs |
| panel/src/api/dockerctl.rs:116 | 2s | 输出收尾等待 | ✅ 内部小延迟，可留 |
| panel/src/api/access.rs:48 | 12s | UAC 确认等待 | ✅ [access].uac_wait_secs |
| panel/src/api/access.rs:419 | 300ms | UAC 结果轮询步长 | ✅ 内部小延迟，可留 |
| panel/src/daemon.rs:529/744 | 10s | Parsec/USB PowerShell 探测超时 | ✅ [daemon].probe_timeout_secs |
| panel/src/daemon.rs:545/760 | 1s | 探测输出收尾等待 | ✅ 内部小延迟，可留 |
| panel/src/bin/launcher.rs:16-18 | 30 次 × 1s + 500ms | launcher 等面板就绪预算/连接超时 | ✅ [launcher].wait_secs |
| panel/src/api/term.rs:76 | 12h | 终端解锁门票 TTL | ✅ [term].session_ttl_hours |
| panel/src/api/term.rs:155 | 43200s | cookie Max-Age（与上同源） | ✅ 引用 session_ttl |
| webcli/src/main.rs:203/207 | 通道 256 / 缓冲 8192 | pty 输出通道与读缓冲 | ❌ 内部缓冲，留 |
| panel/src/tray.rs:155 | 6000ms | 托盘气泡显示时长 | ✅ [tray].balloon_ms |

### 2.3 轮询 / 保活 / 缓存间隔

| 文件:行号 | 当前值 | 用途 | 应进配置 |
|---|---|---|---|
| panel/src/daemon.rs:51 | 600s（10 分钟） | 镜像同步定时器间隔 | ✅ [sync].interval_secs |
| panel/src/daemon.rs:53 | 15s | 外置盘存在性轮询 | ✅ [daemon].usb_poll_secs |
| panel/src/api/wsl.rs:37 | 30s | WSL 保活线程轮询 | ✅ [wsl].keepalive_poll_secs |
| panel/src/api/system.rs:213 | 12s | /api/system 慢采集缓存 TTL（设计约束：必须 < 前端 15s 轮询） | ✅ [system].cache_ttl_secs |
| panel/src/api/autostart.rs:158 | 12s | 自启动列表缓存 TTL（同上约束） | ✅ [system].cache_ttl_secs |
| panel/src/api/access.rs:50 | 12s | 防火墙规则列表缓存 TTL | ✅ [system].cache_ttl_secs |
| panel/src/api/health.rs:252 | 60s | uptime 采集缓存 | ✅ [health].uptime_cache_secs |
| panel/src/daemon.rs:436 | 60s | Parsec 虚拟显示探测缓存 | ✅ [daemon].vdisp_cache_secs |
| panel/static/index.html:1390-1391 | 15000ms | 总览/系统大轮询（联动上面的 12s 缓存） | ✅ [ui].poll_ms |
| panel/static/index.html:1392 | 5000ms | 托盘卡片轮询 | ✅ [ui].tray_poll_ms |
| panel/static/index.html:1303 | 5000ms | 手机接入页状态轮询 | ✅ [ui].phone_poll_ms |
| panel/static/index.html:658 | 8000ms | iframe 加载失败判定（EMBED_TIMEOUT） | ✅ [ui].embed_timeout_ms |
| panel/static/index.html:659 | 2500ms / 90000ms | 一键启动轮询步长 / 冷启动上限 | ✅ [ui].boot_poll_ms / boot_timeout_ms |
| panel/static/index.html:703 | 5000ms | iframe probe 兜底延时 | ✅ [ui].embed_probe_ms |
| webcli/static/index.html:128-129 | 1s → 10s | WS 重连退避（指数，1s 起步 10s 封顶） | ✅ [ui].ws_reconnect_ms |

### 2.4 阈值 / 限制

| 文件:行号 | 当前值 | 用途 | 应进配置 |
|---|---|---|---|
| panel/src/main.rs:260 | 10 | bind 重试次数上限 | ✅ [server].bind_retries |
| panel/src/api/health.rs:24-25 | 200ms / 1000ms | 服务卡片绿/黄阈值（前端说明文字同值，需联动） | ✅ [health].color_green_max_ms / color_yellow_max_ms |
| panel/src/api/health.rs:155 | <5% | 服务健康判定错误率 | ✅ [health].error_rate_healthy_pct |
| panel/src/api/stats.rs:37 | 512 | 按路由计数上限（防内存涨） | ✅ [system].route_stats_max |
| panel/src/api/stats.rs:73 | 8KB | /api/flags body 上限 | ✅ [system].api_body_max_kb |
| panel/src/api/term.rs:181 | 4 字符 | 终端密码最小长度 | ✅ [term].password_min_len |
| panel/src/api/access.rs:139 | 64 字符 | 端口规格串长度上限 | ✅ [access].port_spec_max_len |
| panel/src/api/access.rs:181 | 24 字符 | 规则标签长度上限 | ✅ [access].label_max_len |
| panel/src/api/access.rs:549 | 64 字符 | 规则名长度上限 | ✅ [access].name_max_len |
| panel/src/api/syncview.rs:123 | 7 字符 | 设备短 ID 前缀 | ❌ 展示约定，留 |
| panel/src/daemon.rs:833 | 15 次 × 200ms | 单实例互斥重试 | ✅ [daemon].instance_retries / retry_ms |
| panel/src/api/stats.rs:49-55 | feature.auto_retry / feature.usb_eject = true | 特性开关默认值 | ✅ [flags] 节（从代码迁入） |
| panel/static/index.html:826 | CPU>85% / 内存>90% | 资源条告警线 | ✅ [ui].cpu_warn_pct / mem_warn_pct |
| panel/static/index.html:886 | 温度 ≥70 err / ≥60 warn | 主板温度告警线 | ✅ [ui].temp_err_c / temp_warn_c |
| panel/static/index.html:888 | SMART ≥55°C err / 磨损 >60% err | 硬盘健康告警线 | ✅ [ui].smart_temp_err_c / smart_wear_err_pct |
| panel/static/index.html:638 | 重试 3 次，400/600ms | 前端请求重试（联动 feature.auto_retry） | ✅ [ui].fetch_retries |

### 2.5 内部常量（保留不配置）

- Win32/系统协议常量：`CREATE_NO_WINDOW=0x08000000`、`EADDRINUSE=10048`、`STILL_ACTIVE=259`、`SC_MONITORPOWER` wParam（-1/2）、Tailscale CGNAT `100.64.0.0/10`（RFC 6598 规范值）
- 托盘 Win32 消息/菜单 ID（WM_APP+1/2、IDM_*）、单实例互斥名 `FlyGoTray`
- pty 默认 80x24、mpsc 通道 256、读缓冲 8192（前端连接后立即 resize，80x24 仅是兜底）
- 自检六步结构、组件键名（webcli/syncthing/homepage）、前端 CSS/动画数值、`FENG_IP` 推导逻辑
- scripts/ 下的测试脚本常量与开发机路径（测试工具，不进运行时配置）

### 2.6 需要与后端联动的前端数字（重点）

1. **`setInterval(15000)`（index.html:1390-1391）↔ 12s 缓存 TTL（system/autostart/access.rs）**：注释明确「缓存 TTL 必须小于轮询间隔」。若 [ui].poll_ms 可调，后端缓存 TTL 必须保持 `< poll_ms`，建议 schema 校验或由后端把 cache_ttl 也一并导出、前端只读一个来源。
2. **「<200ms 绿 / <1000ms 黄」文案（index.html:302）↔ health.rs:24-25**：文案写死，阈值改配置后文案会失真 → 前端应从 /api/config 或 /api/health 读阈值渲染文案。
3. **iframe 端口 5244/8096/2283（index.html:316/340/361）↔ docker-compose.yml**：容器端口由 compose 决定（面板刻意不硬编码清单，见 docker.rs 注释），前端却硬编码了三个 → 应从 /api/docker 返回的 `port` 字段动态拼 URL（loadSvcWall 已这么做，iframe 也应如此）。
4. **前端请求重试 3 次（index.html:638）↔ stats.rs `feature.auto_retry`**：已联动（G_FLAG），重试次数本身可进 [ui].fetch_retries。
5. **launcher（独立二进制）硬编码 8388**：必须与 [server].addr 同源，否则改端口后 launcher 失联。

---

## 三、参考成熟做法（调研结论）

- **Rust 生态配置分层事实标准**：Figment / config-rs 均支持「默认值 → 文件 → 环境变量」多层合并，后者覆盖前者（merge 语义：后加入者胜；env 可用前缀如 `FENGPAGE_` 自动映射成嵌套键）。FlyGo 现状（config.rs 的 env > 安装根 > legacy 三级）正是这个模式，**方案应沿用并扩展，不推翻**。
- **格式**：TOML 是 Rust 生态事实标准（Cargo.toml 同款、`toml` crate 由 serde 团队维护、40+ 语言实现、支持注释、错误信息带行号）；YAML 缩进敏感、用户手改易错；JSON 无注释、不适合人类编辑。
- **热加载**：Figment/config-rs 本身是「启动时读一次」的静态模型，无内置热加载；成熟做法是「文件 mtime 轮询 + 运行时代码显式重读」或信号触发（Windows 无 SIGHUP，用 mtime/API 更顺）。

---

## 四、Schema 设计

### 4.1 格式：TOML

理由：
1. 与 Cargo 同款、`toml` crate 极轻（parse-only，无 C/网络依赖），贴合「内存第一、少依赖」哲学；
2. 支持注释 → 用户可编辑性好（配置项旁写中文说明）；
3. 类型明确、错误带行列号 → UI 校验好做；
4. YAML 缩进错误是手改最大痛点，JSON 无注释无法写说明。

依赖影响：panel 新增 `toml = "0.8"`（与 serde 天然集成，预计纯 Rust、无传递依赖）；webcli 不动（由面板注入 env）。

### 4.2 文件位置与解析链

```
FENGPAGE_CONFIG（env，显式指定文件）        ← 最高优先
  > 安装根/config/flygo.toml                ← 新默认位置（config.rs 已能 detect_root）
  > legacy C:/FengNAS/flygo.toml（存在则读） ← 老部署无感迁移
  > 无文件 → 全部用内置默认值（= 当前硬编码）
```

- 选 `config/` 而非 `data/`：`data/` 留作运行时数据（term_token.env、autostart_backup.json、WSL state.json 等已分散在根/panel 级）；用户配置与运行时数据分离。
- 与 config.rs 现有三级解析完全同构：`env 覆盖 > 配置文件 > 内置默认值`，FENGPAGE_* 环境变量**保留并永远压过配置文件**（env 适合临时调试/容器化部署），配置文件名对应关系：
  - `[server].addr` ↔ FENGPAGE_ADDR（已有）
  - `[webcli].port` ↔ WEBPORT / FENGPAGE_WEB_CLI_*（spawn 时注入）
  - `[sync].interval_secs` ↔ FENGPAGE_SYNC_SCRIPT 类（脚本路径已可配，间隔本次新配）
  - 其余 FENGPAGE_* 路径类变量继续留在 config.rs，不重复进配置文件（路径仍走 env，端口/超时/间隔走配置文件——路径已在「设备无关改造」里做完了）。

### 4.3 配置节划分（草案）

```toml
# config/flygo.toml — FlyGo 整体配置文件（示例）
# 解析顺序：FENGPAGE_* 环境变量 > 本文件 > 内置默认值。
# 默认值 = 当前代码里的硬编码值；删掉本文件的任何一节，行为回到默认。
# 由面板 GET/PUT /api/config 读写；直接改文件同样生效（下次热加载或重启）。

[server]
addr = "0.0.0.0:8388"        # 面板监听地址（FENGPAGE_ADDR 可覆盖）
bind_retries = 10            # 端口占用自愈重试次数
bind_retry_ms = 350          # 重试间隔

[webcli]                     # 面板托管的终端网关（面板 spawn 时注入 env）
host = "127.0.0.1"
port = 8788
shell = "cmd.exe"            # 会话 shell（TERM_SHELL）

[term]
session_ttl_hours = 12       # 解锁门票有效期（cookie Max-Age 同源）
password_min_len = 4
webcli_addr = "127.0.0.1:8788"   # WS 反代目标（默认引用 [webcli]）

[sync]
interval_secs = 600          # 镜像同步定时器（分钟级）
syncthing_addr = "http://127.0.0.1:8384"   # REST 基址（sync/syncview/onboarding 三处合并）
timeout_secs = 3             # syncview 用
sync_timeout_secs = 2        # /api/sync 用
onboarding_timeout_secs = 5  # onboarding 用（扫描慢余量）

[daemon]
usb_poll_secs = 15           # 外置盘守卫轮询
probe_timeout_secs = 10      # Parsec/USB PowerShell 探测超时
vdisp_cache_secs = 60
instance_retries = 15        # 单实例互斥重试
instance_retry_ms = 200
eject_grace_secs = 2         # 弹出前等 docker 释放挂载

[wsl]
distro = "Ubuntu-24.04"
keepalive_poll_secs = 30
cmd_timeout_secs = 30        # wsl/sc/netsh 类命令超时
ps_timeout_secs = 10
vm_wait_secs = 15
vm_poll_step_secs = 2

[docker]
status_timeout_secs = 15
action_timeout_secs = 120    # 冷启动 dockerd 放宽

[health]
probe_timeout_secs = 3
color_green_max_ms = 200     # 服务卡片绿/黄阈值
color_yellow_max_ms = 1000
error_rate_healthy_pct = 5
uptime_cache_secs = 60

[system]
cache_ttl_secs = 12          # 慢采集缓存（system/autostart/access 共用；必须 < [ui].poll_ms）
route_stats_max = 512
api_body_max_kb = 8

[access]
uac_wait_secs = 12
cache_ttl_secs = 12
port_spec_max_len = 64
label_max_len = 24
name_max_len = 64
tailscale_cidr = "100.64.0.0/10"   # 默认 Tailscale 网段策略

[services]                   # 组件层：只留端口 + 默认启动清单，组件模型归组件编辑系统
default_start = ["webcli"]   # 开机自动启动的组件（现 DEFAULT_COMPONENTS）
[services.ports]
webcli = 8788
syncthing = 8384
homepage = 9199
# 组件卡片/导航结构 → 见 data/components.json（组件编辑系统，docs/editable-components-design.md），
# 本文件不重复设计组件模型，只引用其产物。

[ui]                         # 前端轮询/阈值（经 GET /api/config 下发，前端不再写死）
poll_ms = 15000
tray_poll_ms = 5000
phone_poll_ms = 5000
embed_timeout_ms = 8000
boot_poll_ms = 2500
boot_timeout_ms = 90000
embed_probe_ms = 5000
fetch_retries = 3
cpu_warn_pct = 85
mem_warn_pct = 90
temp_err_c = 70
temp_warn_c = 60
smart_temp_err_c = 55
smart_wear_err_pct = 60
ws_reconnect_ms = [1000, 10000]   # webcli 页重连退避 [起步, 封顶]

[launcher]                   # 独立启动器（探测面板是否就绪；需读同一配置文件）
wait_secs = 30               # 等面板就绪预算
poll_interval_secs = 1
connect_timeout_ms = 500

[flags]                      # 特性开关默认值（从 stats.rs::default_flags 迁入）
feature.auto_retry = true
feature.usb_eject = true

[tray]
balloon_ms = 6000            # 气泡通知时长

[control]                    # FlyGo 控制面客户端（面板作为 computer 节点）
enabled = false              # 是否上报心跳（默认关；开启后按 heartbeat_secs 周期上报）
base_url = "http://127.0.0.1:9001"  # 控制面基址（本机 MVP；正式 VPS 部署后改成公网地址，支持 https）
device_name = "flygo-panel"  # 注册到控制面的设备名（1-64 字符）
heartbeat_secs = 300         # 心跳间隔秒（10..840；控制面 15 分钟无心跳判离线，间隔必须低于 900）
upload_max_mb = 500          # POST /api/upload 大小上限（超限返回 413）
# 环境变量覆盖：FENGPAGE_CONTROL_ENABLED / BASE_URL / DEVICE_NAME / HEARTBEAT_SECS / UPLOAD_MAX_MB
# 设备 JWT 与激活码存 data/control-state.json（密钥不进本文件），注册/激活/状态端点见 /api/control/*
```

说明：
- **密钥不进本文件**：term_token.env（PASSWORD/TOKEN）保持独立且已有原子更新机制；配置文件只放非敏感参数。
- **`[services]` 与组件编辑系统的边界**：本文件只放「端口 + 默认启动」，组件卡片/导航/页面结构全部指向 `data/components.json`（另一 Agent 的产物），互不重复、端口单点定义（daemon.rs 的 `port_of` 与 `cleanup_stale` 两处重复也借此合并）。

### 4.4 默认值策略

- **无配置文件时行为完全不变**：每个配置项的默认值 = 现代码里的硬编码值；删除配置文件中任意一节/一项，该项回落到内置默认。
- 读入用 `serde(default)` + 全字段默认值结构体，缺失字段永不报错，只在「类型错误」时报错（报错带行列号，UI 可展示）。
- 新增配置项永远是「可选 + 有默认」，保证老部署零配置升级。

### 4.5 热加载 vs 重启生效

| 类别 | 生效方式 | 说明 |
|---|---|---|
| [ui] 全部、[health] 阈值、[system].cache_ttl、[access] 等待/长度、[term].session_ttl/password_min_len、[flags]、[sync].timeout_*、[wsl].poll 间隔 | **热加载** | 读时即取（访问路径已走 RwLock/原子读取），或 PUT /api/config 后主动刷新 |
| [server].addr / bind_retries | 重启生效 | 监听 socket 已 bind；改完 API 提示「需要重启」（/api/restart 已存在，一键重启） |
| [webcli].host/port/shell、[services].ports、[wsl].distro | 下次 spawn 生效（半热） | 子进程启动参数；改后重启对应组件即生效，无需重启面板 |
| [daemon].instance_retries 等启动期参数 | 重启生效 | 只影响启动序列 |

实现建议（零新依赖）：
- 配置拆两层：**静态层**（启动时解析一次，OnceLock，沿用现 config.rs）+ **动态层**（`RwLock<EffectiveCfg>`，PUT /api/config 后重载）。
- 文件 mtime 轮询挂在现有 ticker 上（如 [daemon].usb_poll 15s 或 [wsl].keepalive 30s 的循环里顺带 check，约每 15~30s 检测一次），用户手改文件也能被捡到——**不引入 notify crate**，符合少依赖原则。

### 4.6 与图形界面编辑的关系（single source of truth）

- **文件是唯一事实来源**：`config/flygo.toml`（或 FENGPAGE_CONFIG 指向的文件）。
- API 契约：
  - `GET /api/config` → `{ file: 实际文件路径, exists: bool, effective: {合并后全量配置}, default: {内置默认值} }`，前端「设置页」按 effective 渲染表单、并标注「当前被 env 覆盖的项」。
  - `PUT /api/config` → body 为完整 TOML 文本（或按 effective 结构提交 JSON）；后端 `toml` 解析校验 → 原子写盘（临时文件 + rename，沿用 term.rs 的 atomic_update_password 同款模式）→ 动态层热应用 → 返回校验错误（带行列号）或成功。
- **env 层永远压过文件层**：UI 上被 env 覆盖的项显示「由环境变量 FENGPAGE_* 控制，修改配置文件不生效」。
- 终端密码仍走 /api/term/password（密钥独立文件），不进本页。

### 4.7 与组件编辑系统（并行 Agent）的衔接

- 两个设计共用 `config/` 目录：本文件 `config/flygo.toml`（参数）+ `data/components.json`（组件/页面结构，编辑系统产物，另见 docs/editable-components-design.md）。
- 端口单点定义在 `[services].ports`，组件编辑系统渲染组件卡片时**引用**它（经 /api/config 或 /api/services），不各自硬编码。
- 前端轮询数字迁入 [ui] 后，组件卡片的刷新策略也读同一来源。

---

## 五、落地分期（每期改动小、可独立验收）

| 期 | 内容 | 验收标准 |
|---|---|---|
| **P1 schema + 读入** | 加 `toml` 依赖；config.rs 增加「env > 文件 > 默认」第三层（FENGPAGE_CONFIG + 安装根 config/flygo.toml + legacy 探测）；`[server].addr`、`[webcli].port`、`[sync].interval_secs` 三组接入；新增 GET /api/config（只读） | 无配置文件跑全量测试行为不变；放一个最小配置文件改端口/间隔生效；GET /api/config 返回 effective |
| **P2 全量迁移** | 2.1~2.4 全部「应进配置」项迁入 schema；daemon.rs `port_of`/`cleanup_stale` 同源合并；sync/syncview/onboarding 三处 syncthing_addr 合并；launcher 读同一文件 | 代码里不再有 2.1~2.4 表中的数字字面量（内部常量除外）；改任何一项无需改码 |
| **P3 阈值 + 热加载 + 前端联动** | [health]/[system]/[ui] 阈值迁入；动态层 RwLock + mtime 轮询热加载；PUT /api/config 写盘；前端 setInterval/阈值/文案改从 /api/config 读；iframe URL 改由 /api/docker 动态构建 | 前端零硬编码轮询/阈值；改配置文件 ≤30s 内生效（除重启类） |
| **P4 配置页 UI** | 「设置」页面：按 effective 渲染表单、校验错误展示、env 覆盖标注、重启提示（联动 /api/restart） | 图形界面点点选可改全部参数；与手改文件一致 |

每期结束时同步更新 todo.md；P1~P2 不涉及前端组件结构，与组件编辑系统并行不冲突。

---

## 六、风险与注意

1. **12s 缓存 ↔ 15s 轮询的约束**：若用户把 [ui].poll_ms 调小到 <12s，慢采集会每轮重跑（自耗 CPU，system.rs 注释明言）。schema 校验或后端下发时强制 `cache_ttl_secs < poll_ms`。
2. **端口改动的连锁面**：改 [webcli].port 需同时改 term.rs 反代地址、daemon.rs spawn env、stale cleanup 端口表——全部必须引用同一来源（[services].ports / [webcli].port），否则出现「面板通、终端断」的怪故障。
3. **launcher 是独立二进制**：与面板不共享进程内配置，P2 需给它加读文件能力（或至少读 FENGPAGE_ADDR）。
4. **热加载粒度**：P3 之前不要承诺「所有配置都能热加载」，重启类（[server].addr）明确标注，避免用户预期错位。
5. **不碰 `docs/editable-components-design.md`**：两设计在 `[services]` 引用处对接即可。
