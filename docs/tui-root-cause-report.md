# TUI 应用不可用 —— 根因诊断 + 开源成熟方案调研报告

- 日期：2026-08-16
- 范围：`C:\FlyGoLegacy\webcli`（独立 Rust crate，v0.3.1）、`panel/src/api/term.rs` 与面板前端、`panel/src/daemon.rs`（webcli 托管）、便携式依赖 `portable-pty 0.8.1`（本机 cargo registry 实读源码）
- 方式：纯静态读代码 + 官方文档/源码/issue 调研，未安装、未运行、未改动任何代码
- 现象：**TUI 类应用（htop、vim、btop、lazygit 等全屏交互式程序）在网页终端完全不能用，普通命令行正常**

---

## 一、现状链路（逐段读代码核实）

```
浏览器 xterm.js (panel 8388 页面)
  └─ /term-ws  WebSocket（0909 密码锁 + 门票 cookie）
       └─ panel: src/api/term.rs proxy() 双向转发（axum WS ↔ tungstenite）
            └─ 127.0.0.1:8788 /term（WEBTOKEN 鉴权）
                 └─ webcli: src/main.rs run_session()
                      └─ portable-pty 0.8.1 → 真实 ConPTY
                           └─ CreateProcessW(cmd.exe) 挂在伪控制台上
```

另有**直连路径**：浏览器直接开 `http://127.0.0.1:8788/`（webcli 自带独立页，start.bat / README 主推入口）。

### 1.1 webcli 的 PTY 实现（读 `webcli/src/main.rs` + `portable-pty 0.8.1/src/win/*.rs`）

**结论：是真正的 ConPTY，不是管道 + stdin/stdout 桥。**

- `native_pty_system().openpty(PtySize{80x24})` → `portable-pty` Windows 后端（`src/win/conpty.rs`）：
  - `CreatePseudoConsole(size, hInputPipe, hOutputPipe, flags=0x6, &hpc)`
  - flags = `PSEUDOCONSOLE_RESIZE_QUIRK(0x2) | PSEUDOCONSOLE_WIN32_INPUT_MODE(0x4)`
- 子进程创建（`src/win/psuedocon.rs` `spawn_command`）：
  - `STARTUPINFOEXW` + `InitializeProcThreadAttributeList` + `UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE)`
  - `CreateProcessW(..., EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT, ...)`
  - **没有 `CREATE_NEW_PROCESS_GROUP`**（webcli 注释也自知：Ctrl+C 无法走 `GenerateConsoleCtrlEvent` 中断子进程）
- 对任务提问的逐项回答：

| 检查项 | 结论 |
|---|---|
| 是否发送终端尺寸 rows/cols | 是：前端 fit 后发 `{type:resize}`，webcli 调 `ResizePseudoConsole`（main.rs 281-287 行） |
| 是否处理 resize | 是，且前端在 onResize / 窗口 resize / 会话打开时都会发（防抖 + 去重） |
| 子进程是否在伪控制台里创建 | 是：`PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` 正确挂接（不是 `CREATE_NEW_PROCESS_GROUP`，也没有 `EXTENDED_STARTUPINFO_PRESENT` 缺省问题） |
| 是否设置 raw mode / ENABLE_VIRTUAL_TERMINAL_INPUT | **没有也不需要由宿主设置**：ConPTY 输入处理由**客户端**（shell 及其子进程）在共享输入缓冲上的 `SetConsoleMode` 决定（见 §2.4） |
| stdin 是否转义 0x1A / 流控字节 | 无转义需求：输入按字节原样写入 ConPTY 输入管道；0x1A 只有在客户端 `ENABLE_LINE_INPUT` 时由 conhost 当作 EOF 处理（详见 §2.4） |

### 1.2 面板 /term-ws 代理（读 `panel/src/api/term.rs`）

**结论：没有丢二进制帧、没有按 UTF-8 字符串切割。**

- `ax2tw` / `tw2ax` 对 `Message::Binary` 原样映射（299-326 行），axum ↔ tungstenite 两侧 Binary 帧逐字节透传；
- Ping/Pong 处理正确，Close 正确转发；
- 面板前端（`panel/static/index.html` 1202-1211 行）：`ev.data instanceof Blob → arrayBuffer() → new Uint8Array(b) → TERM_TERM.write(...)` —— xterm.js 协议要求的二进制帧路径正确；
- resize 帧：`onResize` + 窗口 resize（防抖 200ms）+ 打开时强制首帧（`TERM_LAST_RESIZE=''`），均发 `{type:'resize',cols,rows}` —— 正确。

### 1.3 webcli 输出路径

- PTY 读线程按 8KB 块读取 → `Message::Binary(bytes)` 直接发给前端（main.rs 238 行），**不做 from_utf8_lossy、不做跨块字符拼接** —— 字节级正确。

---

## 二、根因诊断（Task A 结论）

### 2.1 【已证实缺陷 · 确定性】webcli 自带独立页（8788 直连）从 v0.3.0 起完全失明

`webcli/static/index.html`（232-237 行）：

```js
ws.onmessage = function (ev) {
  var msg;
  try { msg = JSON.parse(ev.data); }   // Binary 帧 → JSON.parse(Blob) 必抛异常
  catch (e) { return; }                 // ← 静默丢弃所有 Binary 帧
  handleServer(msg);
};
```

- 服务端自 v0.3.0 起输出**全部**走 `Message::Binary`（main.rs 238 行），但独立页把每条消息当 JSON 文本解析并吞掉异常 → **所有 PTY 输出（包括 cmd 提示符、任何命令回显、TUI 画面）全部丢失**，只有 `{"type":"exit"}` 文本帧能显示。
- 影响面：任何直连 `http://127.0.0.1:8788/` 的使用方式（start.bat / README 主推入口、干事机器直连）——终端永久黑屏。这是本次报告里**唯一能在代码层面 100% 证实的"TUI 不可用"缺陷**。
- 注：若用户测试普通命令也在该页面上，同样会看不到任何输出；"普通命令行正常"应与面板页（8388）观察一致。

### 2.2 面板链路（8388）——字节级正确，未发现代码缺陷

面板页 + term.rs 代理 + webcli ConPTY 三段逐一验证（§1.2、§1.3），二进制帧、resize、输入三条路径均正确。**在这条链路上，TUI 理论上应当可用**（与 Windows Terminal / wezterm 相同的 ConPTY 机制）。若用户实测面板页 TUI 仍不可用，根因落在 ConPTY 会话层行为（§2.3），属于"实现正确但受 OS/ConPTY 行为制约"的范畴。

### 2.3 高风险因素（面板链路 · 按可能性排序）

1. **`chcp 65001` 注入（main.rs 192-197 行）**：会话启动即向 ConPTY 输入管道写 `chcp 65001>nul\r`。
   - ConPTY 的输出翻译依赖控制台代码页（屏幕缓冲 → VT 序列）；在 ConPTY 会话内切换代码页是微软已知问题域：`microsoft/terminal#9446`（chcp 清屏，open/backlog）、`#1802`（应默认强制 UTF-8 而未做）等。
   - 全屏 TUI 依赖**备用屏幕缓冲 + 整屏重绘**，正是代码页翻译 bug 最容易炸的路径（画面错乱/黑屏/残影）。
   - 本机默认代码页 936(GBK)，webcli 为让 cmd 输出 UTF-8 而注入 chcp —— 方向对，但手段与 ConPTY 有历史冲突面。
2. **ConPTY 输入翻译（`WIN32_INPUT_MODE` 0x4 + 客户端输入模式）**：`src/host/VtInputThread.cpp` + `InputStateMachineEngine.cpp` 证实：宿主写入的输入字节先经 conhost 的 VT 状态机解析，再按客户端 `ENABLE_VIRTUAL_TERMINAL_INPUT` 决定透传或翻译成合成按键事件：
   - 客户端未启用 VT 输入时：方向键/Home/End 等被翻译成 `KEY_EVENT_RECORD`（带修饰键失真，`#12166` ESC[5D→ESC[D）；`#16343`（WIN32_INPUT_MODE 下 ReadConsoleInputW 应用收到"一按键几十个 INPUT_RECORD"）。
   - 现代 vim.exe / btop / lazygit（以及 cmd.exe 自身）会启用 VT 输入，通常没问题；但**老版本/未适配 ConPTY 的原生 Windows TUI 程序**会踩翻译失真。
3. **无 `CREATE_NEW_PROCESS_GROUP`**：Ctrl+C 字节(0x03)仍会被 conhost 合成 Ctrl+C 事件（`InputStateMachineEngine` 164-168 行），前台程序基本可用；但进程树级信号语义（Ctrl+Break、整树中断）不完整 —— 是已知限制而非"完全不能用"级缺陷。
4. **默认 shell = cmd.exe（daemon.rs 588-601 / start.bat 均未设 TERM_SHELL）**：
   - `htop` 只有 Linux 版，cmd 里根本跑不了 —— 只能经 WSL（本机有 Ubuntu-24.04 keepalive）或 msys/git-bash；`vim/btop/lazygit` 用原生 Windows 版可跑。
   - 若经 WSL：链路 cmd→wsl.exe→WSL2，与 Windows Terminal 相同（一般可用）；但 `WIN32_INPUT_MODE` 与 WSL .exe interop 有已知冲突（`#16343`）。

### 2.4 根因结论（3 行）

1. **链路本身是"真 ConPTY + 二进制帧端到端透传 + resize 全通"**，不存在"伪管道桥"或"按 UTF-8 字符串切割数据"这类架构缺陷；portable-pty 0.8.1 的 ConPTY 实现与 wezterm（可正常跑 TUI）同款，其 Windows 核心自 2024-02 起未变。
2. **已证实的确定性缺陷只有一个：webcli 自带独立页（8788）对所有 Binary 帧做 `JSON.parse` 并静默丢弃（v0.3.0 回归）**——凡直连 8788 的路径终端全盲，TUI 必黑屏；面板页（8388）三段链路字节级正确。
3. **若面板页实测仍不可用**，根因在 ConPTY 会话层行为：启动即注入 `chcp 65001`（代码页切换/备用屏幕缓冲重绘是 ConPTY 已知问题域，#9446 等）+ 客户端未启用 `ENABLE_VIRTUAL_TERMINAL_INPUT` 时 conhost 对 VT 输入的损失性翻译（#12166/#16343）+ 无 `CREATE_NEW_PROCESS_GROUP` 的信号语义缺失 + 默认 cmd.exe 对 htop 类程序根本不提供入口。

### 2.5 建议的复现/验证步骤（用户侧，无需安装）

1. 面板页（8388 → 终端）跑 `vim`（原生 Windows 版）→ 记录是"黑屏/花屏"还是"画面正常但按键失灵"。
2. 直连页（8788）跑同一命令 → 预期是什么都看不到（证明 §2.1）。
3. 在面板页会话内执行 `mode con` / `chcp` 确认代码页；执行 `wsl -d Ubuntu-24.04` 后跑 `htop`，看是否可用。
4. 会话内 `echo %TERM%` 确认 TERM=xterm-256color 是否被 cmd 环境继承。

---

## 三、开源成熟方案调研（Task B）

调研日期 2026-08-16，均未安装，仅资料调研。

| 方案 | 成熟度 | Windows 支持 | TUI 兼容性 | 嵌入难度 | 许可证 | 结论 |
|---|---|---|---|---|---|---|
| **ttyd** (tsl0922/ttyd) | 12.2k★，MIT，活跃（最近 push 2026-08-12，v1.7.7 于 2026-03 发布） | **原生 Windows 支持（v1.7.0 起，ConPTY，要求 Win10+）**；⚠️ Win11 25H2 上 MinGW 构建有已知问题（#1501 CreateProcessW 123 错误，#1502 已加 MSVC 原生构建修复），须用 MSVC 版 | 好（xterm.js + libuv + WebGL2，CJK/IME/ZMODEM/Sixel），Linux 上 htop/vim 实测良好；Windows 上与 ConPTY 同等水平 | 低-中：**独立 C 二进制（非库）**，按子进程托管（FlyGo 已有 daemon/tray 托管 webcli 的现成机制）+ WS 反向代理；自带鉴权（--credential/--token/--once） | MIT（AGPL v3 项目可内嵌/托管，无传染） | **第一备选**，但引入外部二进制与"单 exe"原则冲突 |
| **gotty** (yudai/gotty) | 19.5k★，MIT | **不支持**：README 明示 "Windows is not supported now" | 好（Linux/macOS，xterm.js） | 中 | MIT | **出局**（无 Windows 支持；长期仅零星维护，最后实质 release 2018 年） |
| **Wetty** (butlerx/wetty) | 5.4k★，MIT，维护缓慢（Node.js，需 node≥20） | 靠 node-pty 的 ConPTY 间接支持（Win10+ 可跑，但官方以 Unix 为目标，构建需 make/python） | 好（xterm.js 完整仿真） | 高：引入 Node 运行时 + node-pty 原生模块，与 FlyGo"单二进制、无额外运行时"原则冲突 | MIT | **不建议** |
| **tty-share** (elisescu/tty-share) | ~1k★，MIT，Go，更新停滞 | **无 Windows**（README：Linux/OSX） | 好（Linux） | 高（独立工具非库） | MIT | **出局** |
| **portable-pty**（Rust，现用 0.8.1） | wezterm 官方 pty 层；最新 0.9.0（2026-06）；ConPTY 核心 2024-02 后仅加 `INHERIT_CURSOR` | 完整 ConPTY（本报告已实读源码验证） | 好（wezterm 同款生产验证） | **低：已是本项目在用的库**，无需换架构 | MIT | **推荐继续使用**，升级到 0.9 即可 |
| **conpty crate**（Arnavion） | 0.7.0（2026-06），活跃发布 | ConPTY 底层封装（windows crate），x86_64-pc-windows-msvc | 取决于上层 | 中-高：低层 API，需自写 spawn/信号/尺寸管理（portable-pty 已封装好这些） | MIT（Apache-2.0 双许可） | 仅在需脱离 portable-pty 时考虑 |
| **xterm.js**（前端） | 事实标准（VS Code 同款），MIT | —— | 完整 VT/CSI/备用屏幕仿真 | 已在用 | MIT | 前端无需换 |

### 关键取舍点

- **ttyd 集成模式** = 面板 `daemon.rs` 增加托管 ttyd.exe 子进程（与现在托管 webcli 同模式）+ `term.rs` 或单独路由反向代理 WS + 鉴权对接（--credential 或面板门票转发）。前端要么直接 iframe ttyd 自带页（与面板统一 UI 割裂），要么复刻 ttyd 的 xterm.js 前端协议（二进制帧，与现前端几乎一致）。
- **"单二进制、无额外运行时、Tailscale 内网单用户"约束下**，ttyd 的 C/libwebsockets 二进制（约 1-2MB，需 MSVC 构建版）是可以接受的，但多了一个需要随发布管理的二进制与一个需持续跟踪的 ConPTY 兼容面。

---

## 四、推荐方案（对比 + 理由 + 改动量估算）

### 方案 A（推荐）：修复并微调现有 webcli —— 保留 portable-pty/ConPTY 架构

改动内容：

1. **修复 8788 独立页前端**（`webcli/static/index.html`，约 10 行 JS）：`ws.onmessage` 增加 `if (ev.data instanceof Blob) { ev.data.arrayBuffer().then(b=>term.write(new Uint8Array(b))); return; }`，对齐面板前端写法（panel/index.html 1203-1205 行现成代码可直接参考）。这是**确定性修复**，直连路径立即恢复。
2. **`chcp 65001` 注入改为可配置/后置**（`webcli/src/main.rs` 192-197 行）：把代码页切换从"PTY 创建后立即注入"改为首帧用户输入前、或做成 `TERM_CHCP` 环境变量开关，并在会话内以 `chcp 65001` 输出重定向消除潜在清屏（对齐 #9446 的规避做法）；同时前端 Blob 路径已能正确处理 GBK 字节时（UTF-8 解码会乱码，所以 chcp 保留但延迟执行更稳）。
3. **可选增强**：升级 `portable-pty` 0.8.1 → 0.9.0（Cargo.toml 一行，crate 兼容）；`TERM_SHELL` 默认值在 README/start.bat 注明可指向 `wsl -d Ubuntu-24.04` 或 git-bash，解决"htop 在 cmd 里根本没有"的问题。
4. **可选**：若想根治 Ctrl+C/进程组语义，可 fork 级改造（自写 ConPTY spawn 加 `CREATE_NEW_PROCESS_GROUP`）——不建议一期做。

- 改动量：约 **0.5~1 人天**（前端 10 行 + main.rs 微调 + 构建发布验证）。
- 理由：根因层面已确认 PTY 架构正确、面板链路字节级正确；wezterm 用同款 ConPTY 代码生产跑 TUI 多年。修复成本最低、风险最小，完全保留现有鉴权（门票/WEBTOKEN）、审计、单二进制、daemon 托管模型。

### 方案 B（兜底）：集成 ttyd

- 改动量：约 **2~3 人天**：下载/托管 MSVC 版 ttyd.exe（1.7.7，注意避开 MinGW 构建的 Win11 25H2 坑）、`daemon.rs`+`tray.rs` 增组件、`term.rs` 代理改指向 ttyd 端口、鉴权对接（--credential/--token 或门票）、前端嵌入（iframe 或协议对齐）、回归测试（面板解锁流、断线、审计）。
- 收益：获得久经实战的终端服务（ZMODEM/Sixel/CLI 参数丰富）；代价：破坏"单二进制"、新增外部依赖跟踪面、面板 UI 与 ttyd 页面的融合成本。

### 结论

> **先做方案 A（修独立页前端 + 调整 chcp + 升 portable-pty 0.9），验证面板页与直连页的 TUI 是否恢复；若面板页在调整后仍因 ConPTY 会话层行为不可用（概率低），再评估方案 B 集成 ttyd。** gotty（无 Windows）、Wetty（Node 运行时）、tty-share（无 Windows）均不适合本项目。

### 附：与"本机单用户、无认证/面板已认证、进程树随面板托管"约束的适配

- 方案 A 完全不变：webcli 继续由面板 daemon 托管（`CREATE_NO_WINDOW` + 任务管理器 taskkill /T 已满足进程树托管），鉴权维持门票+WEBTOKEN 双层。
- 方案 B：ttyd 需以 `-p` 端口 + `-c` 凭据或 `--token` 启动，绑定 127.0.0.1，由面板代理承接认证 —— 与现有安全模型一致，无额外暴露面。

---

## 附：关键证据索引

- webcli：`C:\FlyGoLegacy\webcli\src\main.rs`（Binary 帧 238 行、resize 281-287、chcp 192-197）
- 独立页缺陷：`C:\FlyGoLegacy\webcli\static\index.html` 232-237 行
- 面板代理：`C:\FlyGoLegacy\panel\src\api\term.rs` ax2tw/tw2ax 299-326 行
- 面板前端：`C:\FlyGoLegacy\panel\static\index.html` 1202-1211（Blob→write）、1112-1129（fit/resize）
- daemon 托管：`C:\FlyGoLegacy\panel\src\daemon.rs` 584-654（CREATE_NO_WINDOW，无 TERM_SHELL → cmd.exe）
- portable-pty 0.8.1 源码：`~/.cargo/registry/src/rsproxy.cn-*/portable-pty-0.8.1\src\win\{conpty,psuedocon,procthreadattr}.rs`
- ConPTY 官方实现：microsoft/terminal `src/winconpty/winconpty.cpp`、`src/host/VtInputThread.cpp`、`src/terminal/parser/InputStateMachineEngine.cpp`
- 微软 issue：#9446（chcp 清屏，open）、#12166（ConPTY 修改输入转义序列）、#16343（WIN32_INPUT_MODE 破坏 WSL interop / ReadConsoleInputW 应用）
- ttyd：#1501（Win11 25H2 MinGW 构建失败）、#1502（MSVC 构建支持）
