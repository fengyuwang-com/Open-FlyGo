# vdisp 虚拟显示器"固定分辨率/刷新率"方案设计

> 调研日期：2026-08-16 ｜ 性质：只读调研 + 方案文档（未改任何代码/配置/驱动）
> 相关代码：`panel/src/api/vdisp.rs`、`panel/src/daemon.rs`、`panel/static/index.html`
> 相关文档：`docs/flygo-app-plan.md`（v1.0 虚拟显示器遥控）、`docs/todo.md`（第 21/23 轮）

---

## 1. 背景与目标

用户问题：
1. 面板 Web UI 目前**没有**把虚拟显示器固定成某个分辨率/刷新率的入口；
2. 该设置"经常被改回去"：Windows 切换显示器、虚拟显示器进程/驱动一变，就恢复原值；
3. 问：后台守护（检测到变化重新应用）？配置源头锁死？还是两者结合？

目标：给出可落地的"固定分辨率/刷新率"方案（含 UI 入口、后台机制、工作量），并说明与手机端 v1.0"虚拟显示器遥控"的关系。

---

## 2. 现状调查

### 2.1 面板侧（A）：vdisp 只有"开/关 + 状态"，没有任何分辨率/刷新率能力

| 位置 | 内容 |
|------|------|
| `panel/src/main.rs:236-237` | 路由仅两条：`GET /api/vdisp/status`、`POST /api/vdisp/ctl` |
| `panel/src/api/vdisp.rs:21-34` | status 只返回 `parsec_detected / parsec_status / mode`（布尔标志），无分辨率字段 |
| `panel/src/api/vdisp.rs:42-55` | ctl 只接受 `action: "on"\|"off"`，无 resolution/refresh 参数 |
| `panel/src/daemon.rs:455-477` | `vdisp_on()` 仅校验 Parsec 适配器状态后 `set_vdisp_mode(Some(true))`（置内存标志）；`vdisp_off()` 置 None + 点亮物理屏。**全程没有调用任何显示模式 API**（无 SetDisplayConfig / ChangeDisplaySettingsExW / 驱动 IOCTL） |
| `panel/src/daemon.rs:486-501` | `monitor_power()` 只做 SC_MONITORPOWER 熄/亮物理屏（d326a8d 后开模式已不再熄屏，仅 off 时兜底点亮） |
| `panel/src/daemon.rs:508-556` | `parsec_detect_live()` 只探测"Parsec"适配器在线/禁用/缺失三态（第 509 行按 `FriendlyName -match 'Parsec'` 匹配），与分辨率无关 |
| `panel/static/index.html:555-567` | "虚拟显示器模式"卡片：检测状态 + 模式状态 + 开启/关闭两个按钮，**无分辨率/刷新率控件** |
| `panel/static/index.html:1179-1204` | `loadVdisp()/vdispCtl()`：只调 status 和 ctl(on/off) 两个接口 |
| `panel/static/index.html:810、848` | 中英文 i18n 键：`vdisp.detect/mode/act/on/off/...`，无任何 res/refresh 键 |
| `panel/src/settings.rs:107、935` | 唯一相关配置是 `[daemon] vdisp_cache_secs = 60`（探测缓存秒数），无分辨率/刷新率/固定开关配置 |

**结论（A）：网页端"改不了分辨率"不是被禁止，而是从 API 到 UI 到配置三层都根本没有这个入口。** 现有 vdisp 功能的语义是"远程兜底模式开关"，与分辨率设置是两条线。

补充：`d326a8d`（2026-08-16，panel: virtual display mode stops blanking the physical screen）把 vdisp_on 从"SC_MONITORPOWER 熄物理屏"改为"远程兜底只验适配器、不动物理屏"；UI 文案未同步更新（按钮仍写"开启（熄物理屏）"，`index.html:561、566`），`docs/todo.md:738` 已记录"待 UI 任务跟进"。本次加分辨率控件时应一并修正文案。

### 2.2 Windows 侧（B）：本机实况（2026-08-16 实测）

**本机装了两个虚拟显示器驱动（都是标准 UMDF IDD 驱动，IddCx0102）：**

| 适配器 | 实例 ID | 驱动 | 版本 | 安装位置 | 实况模式（Win32_VideoController） |
|--------|---------|------|------|----------|----------------------------------|
| Parsec Virtual Display Adapter | ROOT\DISPLAY\0001 | oem58.inf（原 mm.inf） | 0.45.0.0（2024-01-25） | `C:\Program Files\Parsec Virtual Display Driver\` | **1920x1080 @ 100Hz** |
| GameViewer Virtual Display Adapter | ROOT\DISPLAY\0000 | oem114.inf（原 gamevieweridddriver.inf） | 15.6.5.199（2026-02-28） | `C:\Program Files\Netease\GameViewer\` | **3840x2160 @ 144Hz** |
| AMD Radeon（物理屏） | PCI\...\DEV_1638 | oem12.inf | 30.0.13044.3001 | 系统 | 2880x1800 @ 90Hz |

**关键事实：**

1. **两个驱动的 INF 都没有 `[DeviceMode]` 默认模式表**（`mm.inf`、`gamevieweridddriver.inf` 全文核实）。IDD 虚拟显示器的模式列表由驱动运行时动态给出，Windows 只在"该拓扑没有历史记录"时套用系统首选初始模式。→ **"配置源头锁死"没有可锁的静态源头（INF 层面）。**
2. **Parsec 官方工具 `nefconw.exe --help` 只有设备节点/驱动安装/过滤器/服务操作**（--create-device-node / --remove-device-node / --install-driver / --uninstall-driver / --add-class-filter / --create-driver-service 等），**没有任何设置分辨率/刷新率的命令**。
3. **Windows 按拓扑持久化显示模式的机制实测确认**：`HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration\PSCCDD08675309_11_07E4_C6^352B140C51977C2E6D51DAA34282E933\00\00` 里 Parsec 虚拟显示器（PSCCDD08675309 即 Parsec 显示器 EDID 前缀）持久化的模式是 **1366x768 @ 60Hz**，而当前实况是 1920x1080 @ 100Hz → 说明该拓扑历史记录与用户当前设置不一致，**Windows 拓扑重算时会优先回落到这套旧持久化模式**。
4. **进程/服务实况**：无 parsec.exe 客户端进程；GameViewerService（自动启动服务）+ GameViewerServer/Healthd/GameViewer 进程在跑。GameViewer 的 `bin/streamer-config.json` 只有编解码策略，`config/` 下无分辨率配置——即 GameViewer 虚拟显示器的模式同样由运行时决定（串流会话/用户手工设置），无静态配置。

### 2.3 结论："被改回去"的机制（驱动配置从哪来）

虚拟显示器模式**没有静态持久配置**，由三方动态决定，任何一方都能覆盖：

1. **Windows 拓扑重算**（物理屏插拔、投影模式 Ctrl+P、合盖/开盖）：Windows 重新计算路径后，优先套用 `GraphicsDrivers\Configuration` 里该拓扑的历史持久化模式——实测 Parsec 的历史记录是 1366x768@60，一旦触发重算就"改回去"；
2. **设备节点重建/驱动重枚举**（GameViewerService 重启、Parsec 设备节点 create/remove、驱动更新）：Configuration 键失效，回落到 Windows 系统首选初始模式；
3. **串流客户端每会话重设**（GameViewer / Parsec 客户端按自己的会话配置把虚拟显示器改成对应分辨率/刷新率，覆盖用户手工设置）。

> 结论：这是 IDD 虚拟显示器的固有行为，**"锁死不改"做不到（没有可锁的静态源头），只有"检测到偏离后重新应用"是可靠的**。用户问的"后台守护 vs 源头锁死 vs 两者结合"，答案是：源头锁死基本不可行，后台守护必须做；托管收编可作为辅助补位。

---

## 3. 方案对比

### 方案 1：后台守护（面板 daemon 巡检）— 推荐主力

- **原理**：daemon 里加一个巡检循环（面板已有同类先例：`daemon.rs:236-241` sync_ticker、`daemon.rs:322-329` USB 轮询），每 10-30 秒查一次 vdisp 在线状态 + 分辨率/刷新率，偏离目标就调用显示模式 API 重新应用。
- **实现成本**：中。查询用 `Win32_VideoController`（面板已有 PowerShell 惯例）或 `QueryDisplayConfig`；设置用 `ChangeDisplaySettingsExW`（**面板现有依赖 `windows 0.52` 已启用 `Win32_Graphics_Gdi` 特性，该函数在 `Gdi/mod.rs:163`，零新增依赖**；`SetDisplayConfig` 在 `Win32_Devices_Display` 模块，需在 Cargo.toml 加一行特性，非必需）。
- **可靠性**：高——轮询保证最终一致，任何来源（拓扑重算/进程重启/客户端会话）把模式改掉后，最迟一个巡检周期内纠正。
- **副作用**：极低，只改显示模式不碰驱动；但每次重设瞬间目标屏可能闪一下（可接受，低频）。
- **对远程/采集场景**：只读巡检无影响；重设动作只影响虚拟显示器本身，不影响物理屏；若与串流客户端会话"拉锯"（客户端每会话强制覆盖），需要会话感知（见 3.4 P2）或接受"会话中让位、会话后纠正"。
- **局限**：有检测窗口期（≤ 巡检间隔）；需要正确枚举驱动支持的模式（设了不支持的会被拒绝）。

### 方案 2：配置源头锁死 — 基本不可行

- **原理**：改驱动默认配置（INF/注册表/启动参数）+ 文件只读，让驱动每次启动都回到目标值。
- **实测否决**：两个驱动的 INF 均无 `[DeviceMode]`；IDD 模式由驱动运行时决定。改 INF 需重签（`PnpLockDown=1` + CatalogFile 签名校验），等于自签驱动，风险高；`nefconw.exe` 无模式参数；GameViewer 侧无静态分辨率配置。即便改 `GraphicsDrivers\Configuration` 持久化键，Windows 拓扑重算和串流客户端仍会绕过。
- **成本**：低-中，但**可靠性低，不可作为主力**。唯一可保留的价值：守护在应用目标模式后，顺手把该拓扑的 Configuration 持久化键一并刷新（降低下次拓扑重算回落的概率），作为方案 1 的加固动作（可选）。

### 方案 3：面板托管收编（vdisp 像 webcli/syncthing 一样由面板管）— 只做辅助

- **原理**：把 vdisp 设备节点/服务纳入 daemon 组件管理（面板已有先例：`daemon.rs:31` `COMPONENTS = ["webcli","syncthing","homepage"]`，`daemon.rs:592` 起 start_component）。Parsec 侧可用 `nefconw.exe --create-device-node/--remove-device-node` 管理设备节点存在性；GameViewer 侧可探测 GameViewerService 状态。
- **成本**：中-高。**关键局限：它管的是"设备在不在"，管不了"模式被拓扑重算改掉"**——设备在也会被改分辨率，所以不能替代方案 1。
- **副作用**：接管第三方服务/设备节点，故障面大（尤其 GameViewer 是自家远控服务，启停它可能影响其串流功能）。
- **定位**：作为方案 1 的补位——设备节点缺失时自动重建（Parsec nefconw；GameViewer 提示或重启服务），不做全面托管。

### 方案 4：组合（推荐）

**方案 1 巡检守护为主 + 方案 3 设备存在性补位为辅 + 方案 2 的轻量加固（刷新 Configuration 持久化键）为可选。**

为什么：IDD 虚拟显示器的"被改回去"是平台固有行为，唯一可靠手段是"最终一致地重新应用"（方案 1）；源头锁死（方案 2）在 INF 层面实测不存在可锁源头；托管收编（方案 3）解决不了模式漂移，只解决"设备没了"这类次生问题。

---

## 4. 推荐方案实施设计（分阶段）

### P0：巡检守护 + API + 配置（核心，约 4 人天）

1. **配置（flygo.toml 新增 `[vdisp]` 段）**，在 `settings.rs` 扩展 schema（照 `[daemon] vdisp_cache_secs` 的模式，`settings.rs:107、935`）：
   ```toml
   [vdisp]
   pin_enabled   = false        # 是否固定分辨率/刷新率
   adapter       = "parsec"     # parsec | gameviewer（本机两者并存，需可选）
   width         = 1920
   height        = 1080
   refresh_hz    = 100          # 0 = 不固定刷新率，只固定分辨率
   poll_secs     = 15           # 巡检间隔（10-30 合理）
   reapply_delay_secs = 3       # 会话感知：检测到串流会话活跃时推迟重设
   ```
2. **探测扩展**：`daemon.rs:508` 的 `parsec_detect_live()` 目前只匹配 'Parsec'，扩展为按配置的 adapter 匹配（Parsec / GameViewer），并新增模式读取（Win32_VideoController 或 `QueryDisplayConfig`），输出 分辨率+刷新率 实况。
3. **设置模块**：优先 `ChangeDisplaySettingsExW`（现有 `windows` crate `Win32_Graphics_Gdi` 已含，`Gdi/mod.rs:163`；用 `EnumDisplaySettingsExW` 枚举驱动支持的模式，只允许在支持列表内选择，避免 `DISP_CHANGE_BADMODE`）；需要更细拓扑控制时再启用 `Win32_Devices_Display` 的 `SetDisplayConfig`（Cargo.toml 加一行特性）。
4. **巡检循环**：仿 `sync_ticker`/USB 轮询（`daemon.rs:236-241、322-329`），`pin_enabled` 时每 `poll_secs` 检测：在线 + 模式偏离目标 → 重新应用；设备缺失 → 记日志（P1 才自动重建）。
5. **API 扩展**（`api/vdisp.rs`）：
   - `GET /api/vdisp/status` 增加：`resolution`（实况）、`refresh_hz`（实况）、`target`（目标配置）、`last_applied`（上次成功应用时间）、`drift_count`（被改回次数）；
   - `POST /api/vdisp/pin`：`{enabled, width, height, refresh_hz}` 写 flygo.toml + 立即应用一次（复用现有配置写入链路）。
6. **单元测试**：模式解析/偏离判定做成纯函数（仿 `parse_parsec_probe` 测试风格，`daemon.rs:1039` 起）。

### P1：设备存在性补位（约 1-1.5 人天）

- 巡检发现适配器缺失时：Parsec → 调 `nefconw.exe --create-device-node --hardware-id Root\Parsec\VDA ...`（读安装目录的 vddinstall.bat 确认参数）；GameViewer → 检测 GameViewerService 停止时面板提示/一键重启服务。
- 重建后等设备就绪再应用目标模式（探测缓存已有 60s 先例，`daemon.rs:433-448`）。

### P2：会话感知，避免与串流客户端拉锯（约 0.5-1 人天）

- 检测 GameViewer 串流会话/Parsec 会话活跃（进程存在或会话状态）时**暂停强制重设**（避免和客户端互相覆盖造成闪烁），会话结束后恢复 pin；或简化为"仅在巡检时发现偏离且会话空闲才重设"。

### 面板 UI（约 1-1.5 人天，并入 P0）

- **位置**：系统页"虚拟显示器模式"卡片（`index.html:555-567`）扩展，或拆为独立"虚拟显示器"卡片。控件：
  - 适配器下拉（本机 Parsec / GameViewer 并存，需可选）；
  - 分辨率下拉（从驱动支持列表枚举，默认含 1280x720 / 1920x1080 / 2560x1440 / 3840x2160）；
  - 刷新率下拉（60/100/120/144 等，按驱动支持列表过滤）；
  - "固定当前模式"一键按钮（把实况模式设为目标）+ "启用固定"开关；
  - 状态行：实况 vs 目标、最近一次被改回时间/次数。
- **i18n**：`index.html:810、848` 的 i18n 表新增 `vdisp.res / vdisp.refresh / vdisp.pin / vdisp.target / vdisp.drift` 等键（中英）。
- **顺手修正**：卡片文案"开启（熄物理屏）/开启后物理屏熄灭"已过时（`index.html:561、566`，`docs/todo.md:738` 待跟进项），一并改为"远程兜底"新语义。
- JS 沿用 `loadVdisp()` 轮询节奏（`index.html:1994` 15s 全局刷新已覆盖该卡片，加载新字段即可）。

### 工作量汇总

| 阶段 | 内容 | 人天 |
|------|------|------|
| P0 | 探测扩展 + 模式枚举/设置模块 + 巡检循环 + API + flygo.toml schema + 单测 | 2.5-3 |
| P0 | UI 控件 + i18n + 文案修正 | 1-1.5 |
| P1 | 设备节点自动重建/服务补位 | 1-1.5 |
| P2 | 会话感知（可选） | 0.5-1 |
| **合计** | | **5-7**（含实测联调） |

---

## 5. 与手机端 v1.0"虚拟显示器遥控"的关系

- `docs/flygo-app-plan.md:71` 定义：v1.0 虚拟显示器遥控 = **手机打开面板远程视图**，vdisp 启停/状态可看；画面流走 Parsec 独立客户端，APK 只做入口与状态展示。
- 本方案是**面板 Web UI 的能力增强**（分辨率/刷新率选择 + 固定开关 + 实况状态），手机端 WebView 打开面板远程视图后**自动获得**这些控件，**不需要 APK 额外开发**；只需在 v1.0 验收（`flygo-app-plan.md:181`）时确认新控件在窄屏可用（卡片响应式已支持，`index.html` 移动端导航先例 `37cad82`）。
- 交互细节待定项（`flygo-app-plan.md:193`"虚拟显示器遥控的交互细节"）可并入：手机端只做 pin 开关与状态展示，不做复杂的模式枚举；枚举与设置动作都在面板侧完成。

---

## 6. 附录：本机实测数据（2026-08-16）

- 适配器与驱动：见 2.2 表格（pnputil /enum-devices /class Display /drivers 输出）。
- Parsec 安装目录：`C:\Program Files\Parsec Virtual Display Driver\`（mm.inf/mm.dll/mm.cat/mm.man/nefconw.exe/vddinstall.bat/vdduninstall.bat/uninstall.exe）；设备硬件 ID `Root\Parsec\VDA`。
- GameViewer 安装目录：`C:\Program Files\Netease\GameViewer\`；服务 `GameViewerService`（Automatic，`GameViewerService.exe --service`）；无 parsec.exe 客户端进程。
- 持久化模式证据：`HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration\PSCCDD08675309_11_07E4_C6^352B140C51977C2E6D51DAA34282E933\00\00` → ActiveSize 1366x768 / VSyncFreq 60（历史） vs 实况 1920x1080@100。
- 依赖核实：`windows 0.52` 本地源码 `src/Windows/Win32/Graphics/Gdi/mod.rs:163` 含 `ChangeDisplaySettingsExW`（已启用特性）；`SetDisplayConfig/QueryDisplayConfig` 位于 `src/Windows/Win32/Devices/Display/mod.rs`（需新增 `Win32_Devices_Display` 特性）。
