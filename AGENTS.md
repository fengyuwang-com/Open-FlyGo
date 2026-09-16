# FlyGo（飞狗）— 项目技术总纲（AGENTS.md）

> FlyGo 的**架构性认知总纲**：所有参与开发的主 Agent 与子 Agent 开工前先读本文。只写架构与原则，不堆细节（细节在 docs/）。
> **Meta 规则（用户指令）**：任何架构级新决定、新原则、方向修正，必须**及时**更新进本文档；本文档保持精简——它是架构，不是流水账。

---

## 一、项目是什么

FlyGo（飞狗）= **私有云数据平台**：把闲置电脑/手机/硬盘变成私有云节点，卖"你的数据永远多副本、永远可用、永远在你手里"。

- **技术本质**：任意文件夹的**多向同步**（手机↔电脑、手机↔手机、电脑↔电脑、电脑↔手机）+ 多副本 + 随身取用。同步 UX 越简单越好（选文件夹→选设备→完成），同步方式（镜像/双向增量/版本化）由引擎承载，用户不感知复杂度。
- **连接走 Tailscale**（绝不重造），**同步走 Syncthing**（绝不自研引擎）。
- **Freemium 商业模型**：**本地功能全免费**（AGPL 软件免费——面板/APK/文件服务/局域网同步/镜像备份/终端/监控），**只有"服务"收费**（控制面订阅激活、跨设备互备编排、换新服务、Pro 增值）。expired 只停服务，**绝不停本地功能与已有数据**。

## 二、四层架构

| 层 | 角色 | 技术栈 | 状态 |
|---|---|---|---|
| **节点面板**（电脑） | 存储+连接+计算；托盘托管全部后台 | Rust(axum) 单 exe（自研 panel） | ✅ 运行中 8388 |
| **手机 APK**（一等公民） | 能力向电脑看齐：自身发 HTTP(S) 文件服务、多向同步、面板遥控、订阅展示；远期插件容器（Android 沙盒内尽力） | **Kotlin（Kotlin Multiplatform + Compose）**，fork 自 PlainApp（com.flygo.client） | 🔶 1.0 冲刺中 |
| **控制面** | 身份/激活码/订阅状态机/心跳（只收元数据，不碰数据平面） | Rust(axum+rusqlite+jwt) 单二进制 | ✅ 本机 9001 运行；VPS 待部署 |
| **官网** | 品牌/下载/转化 | 静态站 site/（flygo.fengyuwang.com） | 🔶 今晚建 |

**手机端定栈（已拍板）**：Kotlin——Kotlin Multiplatform + Compose，延续 PlainApp fork；共享 commonMain 业务逻辑，Android 为主体，iOS 走 KMP 远期。不换 Flutter/RN。

## 三、产品哲学（用户强要求，最高优先级）

1. **1 EXE + 1 APK**：对外交付永远两个文件；内部复杂度全隐藏（扫码配对，不暴露 Syncthing 细节）。
2. **Unix 哲学 + 拼积木**：能拼现成开源绝不自研（AGPL v3 无拼装障碍）；自研只发生在"无积木且是差异化核心"（面板/托盘/daemon/控制面）。
3. **按需启动**：常驻几十 MB；Docker/WSL/容器/存储用时才拉起；启动自检自愈。
4. **UI 一律按 `DESIGN.md`**（项目根，所有界面唯一权威）：配色=原版简洁蓝白（冷 slate+蓝 #2563eb）；结构细节（阴影两档/侧栏细节/按键布局/动效）2026-09-07 起对标 llm-text-processor，见 DESIGN.md §3。新界面开工前必读。
5. **版本号 `YYYY.MM.DD-HHMM`**，唯一事实来源=根 `Version` 文件。
6. **设备无关**：电脑三平台同码各编译 + Android APK；iOS 远期（PWA 不阻塞）。
7. **手机是汇聚客户端不是服务器**（按需在线是常态），但 APK 功能尽量向电脑端看齐。

## 四、当前状态（2026-09-06 全量审读轮刷新）

- main=`bb8550e` → 本轮修复后新提交：**全量代码审读完成，修 4 个逻辑 bug**（①syncview 测试 config::init ②Velopack 卸载钩子 T-10 漏入库已补回 ③fetch.rs async 阻塞挪 spawn_blocking ④probe_share "NOK".contains("OK") 恒可达）——116 panel 测试全绿。架构图画好：`docs/architecture-diagrams.md`（7 张 mermaid：四层架构/启动序列/退出契约/SMB 状态机/组件拉取/同步拓扑/bug 台账）。审读台账见 todo.md §八。
- T 系列进度：T-06/07/08/09/10 ✅ 代码侧完成（T-08 组件拉取系统 · T-09 Syncthing 落地 winget 三级解析+GUI 仅绑 localhost · T-10 Velopack 安装器 8.6MB Setup）；T-11 卡 VPS 部署、T-12 卡 release keystore（均用户白天亲手）；T-13 售后诊断包 ✅（2ce67e9：GET /api/diagnostics/bundle + 导出按钮 + docs/after-sales-sop.md）。
- 红色警报根治（已装机）：**Windows SMB 系统级共享关不掉/延迟大**——①SMB 共享不随进程消失、须显式删；②托盘「退出」原 `process::exit(0)` 竞态抢先优雅关停（`fed0fc1` 修）；③SMB 每次 spawn PowerShell 冷启动慢拖死退出（`5743e45` 批量 remove_many/revoke_many + ps()25s/drive_root_online 3s 超时 + close_app 8s 上限 + 开机孤儿自愈 reconcile_stale_shares）。配套：`22bdfbb` settings 内存态随 persist_smb 刷新（「不挂载」界面即时生效）· `e873be3` 全量结构化日志（log.rs → data/logs/flygo.log）· `b57fb97` 未挂载盘不再渲染可达 UNC · `9ca7390` panel-quality pack。进程退出挂起已由 main() 尾部 `exit(0)` 兜底根治。
- 实测铁证：新版托盘优雅退出后 **C/E 共享约 1.1s 内消失**（1056ms/1107ms），net share 仅剩系统默认管理共享。
- 「不挂载」语义（董事长拍板）= **仅停止网络共享**，不是弹出 USB 盘（与托盘"安全弹出外置盘"是两回事，勿混淆）。
- 遗留三件套 2026-09-10 夜全清（代码侧已并入 main，真机验证留白天）：①**进程退出挂起**=真凶实锤托盘 cmd-runner 阻塞 spawn_blocking（static CMD_TX 永不 drop）——修=手动 runtime+shutdown_timeout(3s)+SHUTTING_DOWN 位+六步关停记账（跑完自然退出，exit(0) 仅兜底并打 WARN）；②**黑色弹窗**=全仓 spawn 点统一收编 panel/src/winproc.rs（CREATE_NO_WINDOW 单一来源+hidden_command()，含魔数守卫单测防回归）；③**开机 SMB 慢**=关键瓶颈 PS_LOCK 串行（secedit 持锁 25s 堵 create_all）——修=create_all 调序+批量探测 shares_existing+smb.boot 计时日志全埋点（下次开机 flygo.log 回填实测）。另：check-secrets.sh 已修源码误报（.rs 等源码扩展只查 PEM，.env/PEM 防护不变）。**合并后全量审查**（0 P0/1 P1）已修：shares_existing 探测失败回退 Option、Ctrl+C 即时置位、panic hook 防挂死、内存序 Acquire/Release。质量基线：panel 316+control-plane 8 测试绿，clippy 双 crate 0 警告，`scripts/regression.sh` 十门全绿（合并前必跑）。
- 里程碑（history）：M10 /ws/status（c1c3ccf）· M11 网速+温度诚实化（d1079dc）· M13 control-plane admin UI + DEPLOY-VPS/API-CONTRACT（4243e84+4f430d1+79bef25）· M14 UI 审计整改+Cache-Control（8b95dac 等）· M15 稳健加固+check-secrets.sh（aa872e5）· M16 北极星验收脚本 · M17 文档 reconciliation（docs/m17-reconcile）。
- 手机：v0.1 品牌版已装机；1.0 APK 已构建 bin/flygo-mobile-1.0.0-20260817-debug.apk（M02，debug 签名；release 签名待用户）。**手机↔手机经电脑中转已实现**（feat/phone-sync-closure：pending 配对闭环 + POST /api/sync/folders/:id/share，详见 docs/phone-to-phone-sync.md；D6 真机验证留白天）。
- 基建：Tailscale Serve 三入口（`/`面板 `/ctrl`控制面 `:8443`webcli，tailnet-only）；skill-tailscale-serve 已建仓。
- 监控事实：**温度与网速均已有（M11）**——别再写反。
- 外部阻塞（商业上线硬前置，白天用户亲手，不阻塞本地开发）：DNS 解析 · VPS 部署（DEPLOY-VPS.md 就绪）· release keystore 签名 · 真机装 1.0 APK 实测 · 香港远程实测。详见 docs/master-plan-2026-08-17.md §五。
- 注意：pre-commit 敏感文件检查 hook 文件已在（.githooks/pre-commit），本 clone 需执行 `git config core.hooksPath .githooks` 激活。

## 五、拼装清单（结论，别反复调研）

| 积木 | 用途 | 状态 |
|---|---|---|
| Syncthing | 全部同步链路（staggered 版本控制内置） | ✅ |
| Tailscale | 节点互联 | ✅ |
| **PlainApp（fork）** | **手机客户端主体**（2026-08-16 转向：fork 定案，不再是"只借鉴模式"）；手机↔手机同步两条路径：经电脑中转（Syncthing 同文件夹共享两台手机，已实现，见 docs/phone-to-phone-sync.md）+ 近场直连传文件（PeerFileDownloader，一次性非同步） | ✅ fork 中 |
| Jellyfin/Immich/Vaultwarden/qBittorrent 等 | 媒体/相册/密码/下载（容器按需） | 📋 |
| HomeDash 精华 / 服务卡片墙 | 三色检测+WS 推送 | ✅ 已拼入 |

明确不拼：Headscale（用官方 Tailscale）；自研同步引擎；PocketBase（控制面已自研完成）。
| Velopack | 安装器+EXE 自更新（D-01 裁决） | ✅ CTO 拍板 USE |
| winget | 系统组件安装主通道（T-08） | ✅ CTO 拍板 USE |
| Obtainium | APK 更新渠道零代码（T-12） | ✅ CTO 拍板 USE |
| Syncthing-Fork | 安卓汇聚端指定版 | ✅ CTO 拍板 USE |
| rclone | 云盘远端补充（bisync P0 禁止） | ✅ 条件 USE |
| Watchtower | 非关键容器自更新试点 | ✅ 条件 USE |
| restic | 加密备份 P2 | P2 候选 |
| CasaOS/Runtipi | 只借商店模型+模板通道 | 借鉴 |
| copyparty/Nextcloud/Cosmos | P0 不拼 | 不拼 |

## 六、铁律（违反会出事）

1. **只读区**：容器数据目录与 WSL 引用不要直接改；需要迁移时只做整目录复制。
2. **敏感文件绝不入库**：term_token.env / alist_pass.txt / docker-compose.yml.bak / *.env——提交前必跑 `scripts/check-secrets.sh`（已执行 `git config core.hooksPath .githooks` 时由 pre-commit 自动拦截）。
3. **编码**：代码注释/日志/错误消息英文 ASCII；中文文档显式 UTF-8。
4. **国内镜像**下载（rsproxy/TUNA/npmmirror/ghfast），不硬连境外源。
5. **子 Agent 后台调用**（串行优先防 captcha）；任务必须入账根 `todo.md`（唯一根本，**只写未完成项，做完即删**——历史去 git log/FENGMEM.md，不留 `[x]` 存档，2026-09-10 董事长立）；主 Agent 不闷头写大量代码。
6. **双推**：`git push origin main && git push gitee main`。
7. **端口**：8388 面板 · 8788 webcli · 8384 Syncthing · 9001 控制面 · 8443 webcli(TS)。
8. **UI 按 DESIGN.md**；新原则/架构决定**及时回写本 AGENTS.md**。

## 七、参考文档

`DESIGN.md`（UI 宪法）· `docs/project-structure.md`（项目结构：文件去哪/容器边界/写作路由）· `docs/产品哲学与工程原则.md` · `docs/飞狗软件技术计划.md` · `docs/工程与商业总计划-20260817.md` · `docs/overnight-2026-08-17.md`（通宵队列）· `todo.md`（根，任务账本，唯一根本）
