# FlyGo webcli

[![License](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2021%20Edition-orange.svg)](#环境要求)
[![Platform](https://img.shields.io/badge/Platform-Windows-black.svg)](#windows-兼容说明)
[![WebSocket](https://img.shields.io/badge/API-WebSocket%20JSON-8A2BE2.svg)](#接口契约)

一个基于 **axum + WebSocket + ConPTY** 的网页命令行应用：在浏览器里打开一个**真·交互式终端**，
连接远端 Windows 机器的 shell。前端为 xterm.js 完整终端仿真，后端经 Windows ConPTY 直通，
因此可以运行 `python` REPL、`vim`、`opencode` 等需要全屏/原始模式的全屏 TUI 程序——
不是"提交一条命令看一行输出"，而是像坐在那台机器前一样。

整个应用编译为**单个 exe**（静态资源由 rust-embed 在编译期内嵌），拷贝即用。

> ⚠️ 安全提示：`/term` 端点会打开一个可执行任意命令的交互式 shell。可选设置环境变量
> `WEBTOKEN` 启用令牌鉴权（默认关闭）；访问控制主要依赖**网络层绑定**：默认只绑
> `127.0.0.1`，生产环境请按[部署到 Tailscale](#部署到-tailscale) 绑定私网 IP，
> **切勿暴露公网**。注意：真终端模式下**无法**逐条拦截高危命令（详见[安全模型](#安全模型)）。

---

## 项目简介

webcli 是 FlyGo 仓库中的一个独立模块，定位为"干事机器"的可视化操作入口。
它由两部分组成：

- **后端**（`src/main.rs`，唯一入口）：axum 应用，`GET /term` WebSocket 端点，每连接
  用 portable-pty 启动一个全新 ConPTY 会话，双向转发数据，会话结束时写审计日志。
- **前端**（`static/index.html`，单页）：xterm.js 真终端（含 `addon-fit`），深色终端
  UI，开箱即用，无任何构建步骤；静态资源编译期内嵌进二进制。

任何能够连到服务端口的客户端（网页或原始 WebSocket 客户端）都能在该机器上执行命令，
因此**部署时的网络隔离是本项目的首要安全责任**，详见[安全模型](#安全模型)。

## 功能特性

- **真终端交互**：xterm.js + ConPTY 直通，完整支持 ANSI 转义序列、原始模式、
  全屏 TUI——`python` REPL、`vim`、`opencode`、`htop` 风格程序均可直接运行。
- **流式输出**：PTY 输出实时推送到浏览器，无逐行缓冲延迟。
- **可选 WEBTOKEN 鉴权**：设置环境变量 `WEBTOKEN` 后，`/term` 连接必须携带
  `?token=<值>`，否则以 1008 拒绝（默认关闭）。
- **审计日志**：每个会话结束时追加一行 JSON 到 `audit.log`，记录来源 IP、shell、
  时长与退出码，仅追加不覆盖。
- **断线自动重连**：网络抖动后前端自动指数退避重连，最大间隔 10 秒。
- **全屏适配**：前端窗口尺寸变化时通过 `resize` 消息同步 PTY 行列数，
  xterm.js `addon-fit` 自动铺满视口。
- **单二进制分发**：rust-embed 将 `static/` 内嵌进 exe，发布只需一个文件，
  无 Python 运行时、无 node_modules、无额外静态资源目录。

## 快速开始

### 环境要求

- Windows 10 1809+（ConPTY 要求，见 [Windows 兼容说明](#windows-兼容说明)）
- Rust stable 工具链（`rustup` 安装即可）
- 无其他运行时依赖

### 构建与启动

```bash
cd webcli
cargo build --release
WEBHOST=127.0.0.1 ./target/release/..\bin\flygo-webcli.exe
```

看到日志输出（`listening on ...` 级别信息）即启动成功，默认监听 `127.0.0.1:8788`
（安全默认，不对外）。

### 访问

浏览器打开 <http://127.0.0.1:8788/>，页面自动通过 `ws://127.0.0.1:8788/term`
建立 WebSocket 连接，直接进入交互式 shell。

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `WEBHOST` | `127.0.0.1` | 监听地址（安全默认本机，勿改 `0.0.0.0`） |
| `WEBPORT` | `8788` | 监听端口 |
| `WEBTOKEN` | 空（不启用） | 非空则启用鉴权，`/term` 连接须带 `?token=<值>`，否则 close 1008 |
| `TERM_SHELL` | `cmd.exe` | 会话 shell（可换 `powershell.exe`、`pwsh`、`bash` 等） |
| `TERM_CWD` | 用户主目录 | 会话初始工作目录 |

```bash
# 自定义端口 + 令牌
WEBHOST=127.0.0.1 WEBPORT=9000 WEBTOKEN=<随机长串> ./target/release/..\bin\flygo-webcli.exe
# 生产：绑定 Tailscale 私网 IP（见部署章节）
WEBHOST=100.101.102.103 ./target/release/..\bin\flygo-webcli.exe
```

## 使用说明

1. **直接操作 shell**：打开页面即进入 `cmd.exe`（或 `TERM_SHELL` 指定的 shell），
   输入命令回车执行，与本地终端体验一致。
2. **全屏 TUI 程序**：直接运行 `python`（进入 REPL）、`vim`、`opencode` 等，
   xterm.js 完整渲染光标移动、全屏切换、颜色与原始模式输入。
3. **中断/退出**：`Ctrl+C` 中断前台程序，`exit` 退出 shell（会话结束，前端显示退出码）。
4. **全屏适配**：浏览器窗口缩放时终端自动跟随，PTY 行列数同步调整。
5. **连接状态**：顶部状态栏圆点——绿=已连接、黄=重连中、红=断开；断线后自动
   指数退避重连（最大间隔 10 秒），重连后新建 shell 会话。

## 接口契约

端点：`ws://<host>:<port>/term`，文本帧 JSON（UTF-8）。鉴权：`WEBTOKEN` 非空时
必须带 `?token=<值>` 查询参数，不匹配则以 **1008** 关闭。

**入消息（客户端 → 后端）：**

| type | 字段 | 说明 |
|------|------|------|
| `input` | `data`(string) | 键盘输入（含粘贴文本、控制序列、原始模式按键） |
| `resize` | `cols`(int), `rows`(int) | 调整 PTY 尺寸（连接后前端应立即按实际视口发送一次） |

**出消息（后端 → 客户端）：**

| type | 字段 | 说明 |
|------|------|------|
| `output` | `data`(string) | PTY 输出流（UTF-8 文本，含 ANSI 转义序列，前端直喂 xterm.js） |
| `exit` | `code`(int) | 会话结束；`0` 正常退出，`1` 内部错误/PTY 启动失败，其他为 shell 真实退出码 |

协议流程：

```
客户端 ── {"type":"input","data":"dir\r"} ──────────────────────────▶ 后端
后端   ◀─ {"type":"output","data":"...（回显+输出，ANSI 序列）"} ──── 客户端
后端   ◀─ {"type":"exit","code":0}            （会话结束）──────────▶ 客户端
```

补充约定：

- 消息字段名（`input` / `resize` / `output` / `exit`）是**稳定契约**，勿改字段名，
  参见 `src/main.rs` 头部注释。
- 未知 `type` 的消息被忽略；`resize` 缺省字段按 80x24 兜底。
- 每个 WebSocket 连接对应一个**全新** ConPTY 会话，连接断开即销毁，互不影响。
- 客户端应按 `output → exit` 顺序消费。

## 安全模型

> 本项目奉行"**网络层隔离为主、应用层令牌为辅**"模型，请务必完整阅读本节，
> 尤其是与旧版差异的说明。

**第一层：网络隔离（主要的访问控制，必须由部署者执行）**

- 默认绑定 `127.0.0.1:8788`，仅本机可访问——这是出厂安全默认，**不要改成 `0.0.0.0`**。
- 生产环境推荐绑定 Tailscale 私网 IP（见下文），让访问面收敛到自己的设备网络。
- WebSocket 不受浏览器同源策略约束，因此不要指望"前端只走同源"来限制访问。

**第二层：可选令牌 + 审计（内置，需配置/自动生效）**

- 可选 WEBTOKEN 鉴权：设置环境变量 `WEBTOKEN` 后，`/term` 连接必须带 `?token=<值>`
  查询参数，否则以 1008 拒绝；未设置则仅靠网络层隔离。**建议生产环境务必设置。**
- 审计日志：每个会话结束时追加一行 JSON 到 `audit.log`（来源 IP / shell / 时长 /
  退出码），仅追加不覆盖，供事后审计与取证。

**⚠️ 与旧版（Python 单命令模式）的关键差异：**

旧版以 `cmd /c <单条命令>` 方式执行，因此能在执行前对命令逐条做高危关键词判定并
二次确认。0.3.0 起改为 **ConPTY 真终端直通**——输入原样进入交互式 shell，后端
**无法**逐条拦截高危命令（如 `del /s /q`、`format` 等），也没有二次确认机制。
安全完全依赖**网络层隔离 + WEBTOKEN 令牌**：未设置 WEBTOKEN 时，任何能连到 `/term`
的人都能像坐在机器前一样操作主机。部署者必须自行保持操作纪律，并严格收敛访问面。

## 部署到 Tailscale

Tailscale（WireGuard 私网）是本项目推荐的生产网络方案：服务只出现在你自己的
设备网络中，公网完全不可达。

```bash
# 1. 在目标 Windows 机器上安装并登录 Tailscale
tailscale up

# 2. 查看该机器的 Tailscale 私网 IP（100.x.y.z）
tailscale ip -4

# 3. 绑定该 IP 启动服务（不要绑 0.0.0.0），并设置令牌
cd webcli
WEBHOST=<你的-tailscale-ip> WEBTOKEN=<随机长串> ./target/release/..\bin\flygo-webcli.exe

# 4. 在另一台已登录同一 Tailscale 网络的设备浏览器中访问
#    http://<你的-tailscale-ip>:8788/?token=<随机长串>
```

加固建议：

- **最小化 ACL**：在 Tailscale 管理后台配置 ACL，只允许指定的节点访问
  `<你的-tailscale-ip>:8788`，不要对所有节点放行。
- **务必设置 WEBTOKEN**：即使有 Tailscale ACL，也建议叠加令牌层，纵深防御。
- **定期审计**：查看 `webcli/audit.log`，核对每个会话的来源 IP、时长与退出码。
- **随用随关**：不需要远程操作时停掉服务进程，进一步缩小暴露窗口。
- **切勿**将 `WEBHOST` 设为 `0.0.0.0` 或公网 IP——该服务被公网访问即等于
  把你的 Windows 机器完全交给对方。

## 项目结构

```
webcli/
├── Cargo.toml / Cargo.lock      # 依赖与 flygo-webcli 二进制定义（publish = false）
├── src/main.rs                  # 全部后端：axum 路由 + WS /term + ConPTY 会话 + 审计
├── static/
│   ├── index.html               # xterm.js 单页前端（真终端 UI，原生 JS 零构建）
│   └── vendor/                  # xterm.js 与 addon-fit（本地 vendored，内嵌进二进制）
├── .github/workflows/ci.yml     # cargo build --release + clippy + test 双平台 CI
├── README.md / LICENSE / CONTRIBUTING.md / SECURITY.md / CHANGELOG.md
└── .github/PULL_REQUEST_TEMPLATE.md
（audit.log 为运行时自动生成，不入库）
```

## 测试

```bash
cd webcli
cargo test                 # 单元/集成测试
cargo clippy -- -D warnings  # lint，要求零告警
cargo build --release      # 构建发布二进制
```

CI（`.github/workflows/ci.yml`）在 ubuntu-latest 与 windows-latest 双平台 × stable
toolchain 上执行上述全部检查。

## Windows 兼容说明

- **ConPTY**（Windows 伪终端，Win10 1809 / 2018-10 更新起提供）：经 `portable-pty`
  crate 驱动，无需安装任何额外组件；这是真终端模式（全屏 TUI）的技术基础。
- 默认 shell 为 `cmd.exe`；`TERM_SHELL` 可换 `powershell.exe`、`pwsh`、`bash` 等。
- 单 exe 分发，无 Python 运行时依赖，拷贝到目标机器即可运行。

## 已知限制

- **仅 Windows 完善适配**（ConPTY）；非 Windows 平台未做完整验证，`portable-pty`
  虽跨平台但本项目的终端行为、默认 shell 均以 Windows 为准。
- **无高危命令拦截**：真终端直通模式下无法逐条拦截高危命令（与旧版单命令模式
  不同），安全依赖网络层隔离 + WEBTOKEN，见[安全模型](#安全模型)。
- **无内置 TLS**：鉴权令牌经查询参数传输，注意 URL 日志泄露面；跨公网传输
  建议前置 HTTPS 反代或直接走 Tailscale 加密网络。
- **审计为会话级**：`audit.log` 记录每个连接的来源 IP / shell / 时长 / 退出码，
  不记录逐条命令内容（真终端模式下无法可靠切分命令边界）。

## 许可证

[Apache-2.0](LICENSE) © 2026 FlyGo Contributors
