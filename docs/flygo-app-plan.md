# FlyGo 手机客户端（APK）产品规划

> 状态：v1.0 规划稿 · 2026-08-16 · 性质：权威规划，指导后续所有版本开发；与本文档冲突的既有约定以本文档为准（冲突先更新本文档再动手）。
> 依据：`docs/产品哲学与工程原则.md`、`docs/plainapp-report.md`、`docs/plainapp-integration.md`、`docs/android-env-and-plainapp-fork-report.md`、`docs/ui-design-guide.md`、`C:\FengNAS\control-plane\README.md`、panel 现状（源码核对）。
> 范围：只定义"做什么、什么时候做、怎么验收"；实现细节见各调研报告。拿不准处标【待确认】。

## 0. 规划结论（TL;DR）

1. **定位**：手机客户端 = 随身移动网盘 + 面板遥控入口 + 节点身份展示，PWA 替代不了（手机当服务器需要前台服务/本地存储/原生加密配对）。
2. **三链路**：手机↔面板走 Syncthing（自动汇聚）+ 手机 HTTPS 8080/8443（即时取用）+ 面板 Web（遥控）；手机↔控制面走 HTTPS REST 元数据；面板↔控制面同 API。数据平面永不经过控制面。
3. **分期**：v0.1 纯品牌定制不碰功能；v0.2 面板侧打通（手机零改造，引导式）；v1.0 才进 fork 代码（控制面客户端 + HTTP 客户端模式 + 虚拟显示器遥控）；远期 iOS / W3 互备 / VIP 闭环。
4. **技术要点**：新增三块模块，全部复用 PlainApp 扩展点（HttpRouteRegistry / SEND 转发 / Compose 设置页 / DataStore / 前台服务编排），零新增第三方依赖。
5. **风险**：AGPL fork 义务；保活靠引导清单不靠代码硬抗；控制面只收元数据；离线自治。

## 1. 产品定位

**一句话**：手机客户端是 FlyGo 体系里"随身携带的节点"——自动向面板汇聚数据（Syncthing），随时充当 1TB 移动网盘（HTTP 文件服务），并作为面板远程遥控入口与节点/订阅身份的随身展示。

**为什么需要 APK 而不是浏览器 PWA**：

| 能力 | PWA | 原生 APK |
|---|---|---|
| 手机当服务器（8080/8443 端口常驻） | 做不到 | 前台服务 + 通知 + 系统权限 |
| 本地文件管理、端到端加密配对（ECDH+Ed25519、XChaCha20-Poly1305） | 做不到 | 原生能力 |
| 扫码配对、Syncthing 后台同步、服务状态通知栏 | 受限 | 系统级集成 |
| 交付形态 | 无 | "1 个 EXE + 1 个 APK"铁律的 APK 侧 |

PWA 仍作兜底（产品哲学 §7：iOS 用户浏览器访问面板，不阻塞）。

**角色边界**（沿用技术计划结论）：手机**不是节点**——不承担 Docker/转码/7×24 服务承诺；是"按需在线"的文件服务端点 + 汇聚客户端 + 遥控终端。关键数据以"汇聚到节点 + 节点互备"为准，PlainApp 通道是取用通道不是备份通道。

## 2. 体系架构图（三节点，文字版）

```
                  局域网 192.168.x  /  Tailscale 100.64.0.0/10
┌──────────────────┐                                       ┌──────────────────┐
│  手机 (FlyGo APK) │ ◄───────── 文件/状态/控制 ─────────►  │  面板 (EXE)       │
│  ① PlainApp 服务  │   Syncthing TLS · 手机 HTTPS 8080/8443 │  :8388 Web ·      │
│  ② HTTP 客户端(v1)│   8388 Web · TCP 探测                 │  Syncthing ·      │
│  ③ 控制面客户端(v1)│                                      │  AList · 虚拟显示器 │
└──────────────────┘                                       └──────────────────┘
      │    ▲  HTTPS REST（元数据）                                │    ▲
      │    │  注册/激活/JWT/心跳/订阅                              │    │  HTTPS REST
      ▼    │                                                   ▼    │  激活向导/心跳/订阅
┌───────────────────────────────── 公网 VPS ─────────────────────────────────┐
│  控制面（Rust+axum :9001）  JWT 身份（HS256）· 订阅状态机 · 心跳 · 管理端     │
└─────────────────────────────────────────────────────────────────────────────┘
```

**每对链路的数据流与协议**：

| 链路 | 数据流 | 协议/端点 |
|---|---|---|
| 手机→面板 自动汇聚 | 照片/下载目录 → 节点汇聚目录 | Syncthing TLS（手机侧 send-only） |
| 手机→面板 即时取用 | 浏览/下载/上传手机文件 | 手机 HTTPS 8080/8443（Ktor/Netty，自签证书，mDNS plainapp.local） |
| 手机↔面板 配对/身份 | Syncthing 设备 ID 扫码 | 面板 `GET /api/onboarding/syncthing-qr` |
| 面板→手机 在线探测 | 绿/黄/红状态 | svcwall TCP probe（15s 刷新） |
| 手机→面板 遥控/浏览 | 面板 Web 页面 | HTTPS 8388（v0.2 浏览器直达 / v1.0 内嵌 WebView；状态推送可走面板 /ws/status，待确认） |
| 手机↔面板 服务控制 | 启停手机 HTTP 服务 | `${applicationId}.action.START/STOP_HTTP_SERVER` 广播（跨设备可达性【待确认】） |
| 手机→控制面 身份 | 注册→激活码→JWT | POST /api/register、/api/activate、GET /api/status（限速 10/分/IP） |
| 手机→控制面 心跳 | disk_free / online / version | POST /api/heartbeat（几分钟一次；15 分钟无心跳判离线；响应携带订阅状态） |
| 面板→控制面 身份/订阅 | 激活向导绑定激活码、心跳、订阅展示 | 同上 API（面板侧功能；手机端展示同一订阅状态） |

## 3. 深度连接三条链路设计

### 3a. 手机 ↔ 面板（局域网 / Tailscale）

**扫码配对（v0.2，复用现有引导，不改流程）**：面板"手机接入"页三步引导已存在（① 装 Syncthing-Fork（必须）+ Tailscale（可选）→ ② 扫码配对 → ③ 等待同步；含分厂商保活清单折叠区）。v0.2 目标是把三步走通为闭环验收。

**面板远程控制（明确进/不进）**：
- v1.0 进：面板远程状态（总览"手机节点"卡片，绿/黄/红随探测实时变化；手机按需在线，离线是常态不误报故障）；虚拟显示器遥控（手机打开面板远程视图，vdisp 启停/状态可看；画面流走 Parsec 独立客户端，APK 只做入口与状态展示，不打包 Parsec）【细节待确认】。
- 明确不进：webcli 终端（手机上敲面板命令，安全面与 UX 均不值，面板侧已完整）；服务/Docker 启停（面板已覆盖，手机端只读状态为宜，写操作留面板）。

**文件互传（双通道分工 + 设计红线）**：
- Syncthing 双向同步：手机目录一律 send-only（防误删传播的根；"节点删除不会反向删手机"是验收项）；
- 手机访问面板文件：手机浏览器打开面板（v0.2）→ v1.0 WebView/AList 文件页（面板 AList 已有，全盘挂载已验证）；
- 面板访问手机文件：浏览器直连手机 HTTPS 服务（v0.1 即有）；
- **红线**：PlainApp 通道永不接入自动同步逻辑——它是手动直连文件服务，自动同步只走 Syncthing 一条路。

### 3b. 手机 ↔ 控制面（公网，v1.0）

- **身份注册**：POST /api/register `{device_name, node_kind: phone, hardware_id}` → 8 位激活码（绑定硬件指纹，幂等返回原码）。hardware_id 取稳定设备指纹【待确认：Android ID / Syncthing device ID】。
- **激活**：POST /api/activate（激活码 + 指纹匹配）→ 设备 JWT（HS256，7 天，过期重新激活）。
- **心跳**：POST /api/heartbeat，随订阅状态回传顺带刷新；控制面挂了不惩罚（节点自治，恢复自动补报）。
- **订阅展示**：GET /api/status；trial(30 天)/expiring(到期前 30 天) 客户端显著提醒；未激活 → 引导去官网/面板。
- **订阅归属**【待确认，影响 VIP 定价模型】：手机注册为独立设备（node_kind: phone，与硬件指纹绑激活码机制天然一致，推荐）还是共享面板订阅——需商业侧拍板。

### 3c. 面板 ↔ 控制面（面板侧功能，手机端同源展示）

- **激活向导**：面板首页绑定 8 位激活码（register→activate；FENGDOG_SECRET/FENGDOG_ADMIN_TOKEN 环境变量已就绪；控制面 VPS 部署状态【待确认】）。
- **心跳与订阅展示**：面板侧实现（心跳上报 disk_free/online/version；订阅状态入面板"关于/设置"）。
- **手机端展示同一订阅状态**：数据来源二选一【待确认】——手机直连控制面（与 3b 共用一套控制面客户端代码，推荐）/ 面板 API 转发。

## 4. 功能范围分期

### v0.1 —— 今天的 APK（品牌定制 + PlainApp 原生能力）

**内容（已定，建设中）**：applicationId/namespace → `com.flygo.client`（与官方版共存安装）；app_name → FlyGo；adaptive 图标（蓝底白狗）；设置页官网入口 flygo.fengyuwang.com；gradle Xmx 下调至 4g；versionName 0.1.0-flygo；构建 `assembleGithubDebug` → `bin/flygo-mobile-20260816-debug.apk`。

**PlainApp 原生能力（天然拥有，不改动）**：手机当文件服务器（HTTPS 8080/8443，10 端口组自动避让，自签证书指纹核验）；本地文件管理/网盘（含 WebDAV/SMB/AList 等远端）；Syncthing 同步；分享转发（SEND/SEND_MULTIPLE）；端到端加密配对（ECDH+Ed25519，XChaCha20-Poly1305）。

**不做清单**：不改任何功能代码；不做 HTTP 客户端模式；不接控制面；不做面板配对引导；不打包 Syncthing 进 APK（沿用独立 App 拼装哲学）；不动保活行为（stopWithTask 维持官方值，是否改留 v0.2 评估）。

**验收标准**：与官方 PlainApp 共存安装；名称/图标为 FlyGo；设置页官网入口可达；发射/文件/同步/分享功能可用；versionName 0.1.0-flygo。

### v0.2 —— 手机 ↔ 面板打通（手机零改造，面板侧实施）

- 扫码配对闭环验收（复用 onboarding 三步 + 保活清单）；
- 总览"手机节点"卡片（svcwall TCP 探测，15s，按需在线语义）；
- Syncthing 文件互传（send-only 汇聚目录验证）；
- 手机访问面板 Web 版（引导页给地址/二维码）；
- 移动网盘引导区块（plainapp-integration 三步：下载 → 发射 → 地址登记；`/api/onboarding/plainapp` 从占位升级为真实数据；`/api/plainapp/*` 登记与探测——方案已有）。

**验收标准**：手机扫码 → 面板出现手机节点 → 双向同步一个文件 → 手机浏览器打开面板 → 手机节点卡片状态实时变化。

### v1.0 —— 深度连接（fork 代码进入）

- 控制面客户端：注册/激活/心跳/订阅展示（3b 全量）；
- HTTP 客户端模式（差异化空白）：设置页"面板地址" + 面板 WebView（或浏览器跳转）+ 分享到面板（SEND 转发目标 + OkHttp POST 上传）；可选 `/flygo/*` 反代路由（HttpRouteRegistry）；
- 移动网盘差异化深化：面板登记 + 引导体验；
- 虚拟显示器遥控（手机端入口 + 状态展示，见 3a）；
- 明确不进：webcli 终端、服务/Docker 启停。

**验收标准**：手机注册→激活→心跳上线；订阅状态展示与 trial/expiring 提醒；分享文件到面板成功；虚拟显示器遥控可用。

### 远期

- iOS：PWA 兜底，评估 React Native/现成开源 iOS 客户端拼装（产品哲学 §7，不承诺）；
- W3 互备：Syncthing 一键配对 + staggered 版本控制（技术计划阶段二）；
- VIP 付费闭环：激活码运营签发 → 支付回调 → /api/renew 续期（控制面"已知简化"项）。

## 5. 技术方案要点（PlainApp fork 新增模块）

| 模块 | 内容 | 复用点 |
|---|---|---|
| 控制面客户端（v1.0，commonMain） | register/activate/heartbeat/status + JWT 存储；心跳定时器挂前台服务编排 | ktor-client-okhttp / OkHttp 已有；DataStore preferences；HttpServerService 启动编排模式 |
| HTTP 客户端模式（v1.0） | 设置页"FlyGo 面板地址"项（PCard+PListItem+PSwitch）；面板页（Compose androidView WebView，明文 HTTP 已放行）；分享到面板（MainActivityIntentHandler 的 ACTION_SEND 转发对话框 + 上传） | SettingsCards/SettingsCardItems；MainActivityIntentHandler；usesCleartextTraffic 已开 |
| 面板配对（v0.2，面板侧为主） | 手机端零改造；fork 仅需下载二维码/官网入口 | onboarding 既有 QR 渲染（`?text=`） |

**依赖取舍**：保持零新增第三方依赖（Ktor/OkHttp/Tink/BouncyCastle 已内置；图标用 adaptive XML 不引图库）；不做推送（心跳轮询 + 状态通知栏足够）；不打包 Parsec/Termux/Syncthing 本体。

**构建与版本**：`assembleGithubDebug`（debug 签名即可安装分发）；SDK platform 37 走腾讯镜像；gradle 内存 4g 已定。v0.1 用 0.1.0-flygo；后续版本与根目录 Version 文件（YYYY.MM.DD-HHMM）的映射规则【待确认】。

## 6. UI / 品牌规范

- **Logo**：定稿 `logo/flydog-logo.svg`（蓝渐变 #0A2E6E→#1A73E8 圆角底 + 白色狗头剪影，金色比例构图）。Android 适配：adaptive icon——背景 = 蓝色渐变，前景 = 白狗头（安全区 66dp 内居中），圆角由系统 mask；splash 用同款蓝底（替换 PlainApp ic_splash）。通知栏小图标需导出白色狗头单色版【待确认】。
- **配色**：沿用面板 UI 宪法 token——冷 slate + 单一强调蓝（accent #2563eb / 深色 #4c9aff）；语义色（绿/黄/红）只做状态指示，绝不进品牌元素；深色模式两套显式值。
- **排版**：字号/字重/圆角沿用刻度（正文 S1 档、字重 400/500/600、圆角 6/8/10）；移动端触摸目标 ≥ 40px。
- **文案**：沿用面板调性——祈使句、真实预期（"约 5~10 秒"不说"秒开"）、三态完整（加载/空/错误给"原因+怎么办"）、emoji 每区块至多一个、不喊"强大"。
- **官网入口**：设置页放置（v0.1 已定）；官网 flygo.fengyuwang.com（静态页 + 下载入口 + 使用说明，部署排队中）。

## 7. 风险与合规

| 项 | 结论 |
|---|---|
| AGPL 义务 | fork 保留 LICENSE 与版权声明、修改文件头声明改动；公开分发 APK/源码须整体保持 AGPL-3.0 并公开修改版源码（fork 报告 §e）。与面板同协议，一致 |
| 保活 | 分厂商白名单清单已进引导页折叠区（小米/OPPO/vivo/荣耀/华为）；预期管理"按需在线，手机不是服务器"；stopWithTask=true 划掉连坐问题留 v0.2 评估（改 false 需权衡常驻代价） |
| 隐私 | 控制面只收元数据（device_name/node_kind/hardware_id/disk_free/online/version），不碰数据平面；文件只在节点间经 Syncthing/Tailscale/局域网直连流动；面板不代理手机流量 |
| 离线可用 | 控制面挂了手机本地功能不受影响（心跳失败不惩罚，恢复自动补报）；PlainApp 全部本地能力离线可用 |
| 安全 | 自签证书指纹核验；默认网络信任边界（面板默认仅 Tailscale+本机）；引导页提示"只在需要时打开发射，用完关闭" |

## 8. 验收清单（每期验收动作）

**v0.1**：
- [ ] 同一手机共存安装 FlyGo APK 与官方 PlainApp，互不冲突
- [ ] 应用名/图标为 FlyGo（蓝底白狗）；设置页可打开官网 flygo.fengyuwang.com
- [ ] 打开发射 → 电脑 `curl http://<手机IP>:8080/health` 返回包名 com.flygo.client；Web UI 可浏览/上传/下载
- [ ] SEND 分享转发可用；Syncthing 同步功能可用【待实测项】
- [ ] versionName 显示 0.1.0-flygo

**v0.2**：
- [ ] 手机扫码 → 面板出现手机节点；双向同步一个文件；验证"节点删除文件不反向删手机"（send-only）
- [ ] 手机浏览器打开面板 Web 版；总览手机节点卡片绿/黄/红随手机在线状态实时变化（15s）
- [ ] 手机不开服务时卡片灰/红且不误报"故障"（按需在线语义）
- [ ] 引导页 PlainApp 区块三步完整（下载 QR / 发射指引 / 登记按钮）；保活清单折叠区可用

**v1.0**：
- [ ] 手机端注册 → 输入 8 位激活码 → 激活 → 心跳上线（控制面管理端可见该设备）
- [ ] 订阅状态展示：trial 期/expiring 提醒文案正确
- [ ] 设置页配置面板地址 → 手机 WebView 打开面板；分享文件到面板上传成功
- [ ] 手机端打开面板远程视图，虚拟显示器遥控可用
- [ ] 控制面 /api/admin/devices 中 node_kind=phone 设备心跳正常

## 9. 实施进度（2026-08-17 冲刺更新）

| v1.0 工作项 | 状态 | 位置/证据 |
|---|---|---|
| 控制面本机部署（9001） | ✅ 运行中，API 闭环验证 | 入口 `https://<panel-node>.ts.net/ctrl`（Tailscale Serve） |
| 面板接入控制面（W2） | ✅ 已合 main（63eeaaa 控制面接入 + aa872e5 M15 加固） | api/control.rs、api/upload.rs、tests/control_api.rs + 心跳/上传/幂等/不可达降级 |
| fork 控制面客户端（shared） | ✅ 已提交 5e5b043 | C:\FengProj\PlainApp |
| fork app 模块（HTTP 客户端模式+SEND 转发+设置卡） | ✅ M01 已提交 392c560（16 文件：3 个缺失 i18n import 修复、默认 URL 置空） | compileGithubDebugKotlin 绿；1.0 APK 已构建（M02） |
| vdisp 遥控/固定分辨率 | ✅ M08 已实施 10bb603（模式 API + 60s 守护 + 卡片控制） | docs/vdisp-pinning-design.md；按设计文档方案落地，实机识别 ParsecVDA ✅ |
| 上游 PlainApp bug | ✅ issue #341 已报（OPEN） | plainhub/plain-app |
| Tailscale Serve 暴露 | ✅ 三入口全验证 | `/`→8388 面板、`/ctrl`→9001、`:8443`→8788 webcli（WS 101 通过） |
| 面板 WS 状态推送（M10） | ✅ c1c3ccf | /ws/status 聚合推送（1s ticker + 轮询降级），手机端 Web 遥控态可复用 |
| 宿主监控 网速+温度（M11） | ✅ d1079dc | NetworkThroughput 差值法 + 温度诚实状态 |
| control-plane admin UI（M13） | ✅ 4243e84+4f430d1+79bef25 | admin.html 519L + DEPLOY-VPS.md + API-CONTRACT.md；VPS 部署待用户 |
| UI 审计整改（M14） | ✅ 8b95dac+8f1c592+9d0bdf9+5c8a789 | Cache-Control/PRAGMA 响应头、tiles 仅总览、verify_ui.js 对齐 |
| 稳健加固（M15） | ✅ aa872e5 | 不可达降级、幂等上传、check-secrets.sh+pre-commit |
| 北极星验收脚本（M16） | 🔶 工作中（.wt-m16，未提交） | scripts/northstar_acceptance.py（mock/live/auto 三模式）；live 环节待真机 |
| 1.0 APK | ✅ 已构建（M02） | bin/flygo-mobile-1.0.0-20260817-debug.apk（150,655,780B，md5 805bf00b…eab86）；**release keystore 签名待用户** |

收尾顺序（更新）：M01–M15 已全数合 main 本地（186 测试绿）→ 双推 11 commits（待主 Agent/用户）→ VPS 部署（DEPLOY-VPS.md 就绪）→ release 签名 → 真机装 1.0 APK 三链路联调 → 发布。本文件其余章节（分期/验收）不变。

## 附：待确认清单

1. 手机硬件指纹 hardware_id 的选取（Android ID vs Syncthing device ID）
2. 手机订阅归属：独立设备 vs 共享面板订阅（影响 VIP 定价模型）
3. 手机端订阅状态数据来源：直连控制面 vs 面板转发
4. 广播 START/STOP_HTTP_SERVER 跨设备可达性
5. PlainApp 内 Syncthing 同步能力的具体形态（待实测）
6. 控制面 VPS 部署状态
7. 后续版本 APK versionName 与 Version 文件映射规则
8. 虚拟显示器遥控的交互细节（Parsec 独立客户端配合方式）
