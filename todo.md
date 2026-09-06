# FlyGo 任务账本（todo.md · 唯一根本）

> 2026-09-07 精简：2026-08 历史复盘全卷已按用户指示删除，完整历史在 git（`git log --diff-filter` + 3974420 之前的 todo.md）与 FENGMEM.md（33 轮会话记忆）。只留活跃任务。

## 第 38 轮起：通用性清零 + 本地快捷方式（2026-09-06 用户拍板：所有不通用都不可接受）

> 两轮全仓扫描结论。已修不入账：`webcli/start.bat` 相对化、`fetch.rs` where.exe 探针、WSL 发行版自动探测（`wsl -l -q` UTF-16LE 无 BOM 解码 + 5min 缓存 + 单测 3，270 绿）。
> 已验证通用不修：tcpfwd ipconfig 解析（只认 ASCII "IPv4"/数字/点，GBK-lossy 下可证正确，中英单测向量齐）、fetch manifest 版本 pin（有意 pin + ghfast/github 双 URL + `FENGPAGE_FETCH_MANIFEST` 可整体覆盖）、LOCALAPPDATA 兜底（全 Windows 通用）、config legacy 路径（exists 门控只读）。

### G-02 空服务墙 starter pack
- 内容：安装器只装 panel exe，买家无 compose → 卡片墙空。内置默认 compose seed（alist+jellyfin+immich 最小集）或首次启动向导（二选一，先议方案再动手）。
- 前置：T-08。验收：空机器首次启动有可用服务墙；测试绿。
- **[x] 完成（2026-09-07）**：选 compose seed 方案。内置 `panel/static/docker-compose.starter.yml`（alist+jellyfin 可跑，immich 注释块待解锁）；`docker.rs compose_path()` 无可用配置文件时首次自动落盘 `<root>/data/docker-compose.yml`；纯解析函数抽出 + 2 单测。

### G-03 winget 缺失兜底 + 国内源速度
- 内容：LTSC/精简系统无 winget 时 manifest 兜底覆盖率逐项核对；docker-desktop 数 GB 国内下载速度实测；必要时补国内镜像。
- 前置：T-08。验收：无 winget 机器全组件可装；测试绿。
- **[x] 代码侧完成（2026-09-07）**：核对结果——syncthing（zip 兜底+sha pin）、tailscale（MSI 兜底，本轮补 ghfast 镜像 URL）无 winget 可装；docker-desktop 数 GB 无国内镜像（winget-only + 手动指南，已在 manifest note 声明）。真机速度实测留用户。

### G-04 非管理员降级验收
- 内容：无管理员权限时防火墙规则/schtasks/portproxy 逐项验收（降级提示 vs 报错），统一文案，无静默失败。
- 验收：标准用户全流程可走；测试绿。
- **[x] 代码侧完成（2026-09-07）**：逐项审查无静默失败（elevated 标志/need elevation 消息/ok:false+文案），台账 docs/g04-degradation-audit.md；真机标准账户实测留用户。

### G-05 Tailscale 自有账号登录闭环
- 内容：买家登自己号：下载→登录→`tailscale ip` 探测→serve 生效，onboarding 全程走通。
- 前置：T-10。验收：新号全程录屏。

### G-06 fetch manifest 版本升级流程
- 内容：syncthing pin v2.1.3 的升级操作手册（改 URL + 换 sha256 pin + winget 官方 manifest 交叉验证），下次升级照着做。
- 验收：`docs/fetch-manifest-upgrade.md` 落盘。
- **[x] 完成（2026-09-07）**：docs/fetch-manifest-upgrade.md（升级四步 + 交叉验证 + FENGPAGE_FETCH_MANIFEST 整体替换 + pin 铁律）。

### N-01 本地快捷方式（用户在面板自助添加本机任意程序）
- 内容：新页"本地服务"：用户自己加 exe/script/url（名/路径/参数/工作目录/端口探针/进程名探针/开机自启）。存 `data/native-shortcuts.json`（独立于 components.json，原子写 + .bak）。
- 后端：新模块 `api/native.rs`（增删改 + 启停，校验抄 components validate）；`daemon.rs` 子进程注册表化（`HashMap<id, Child>`，CREATE_NO_WINDOW 复用）；开机自启建 `FlyGo-<id>` 计划任务（抄 lecoofan 模板），autostart 页自动收录。
- 白名单：由用户在 UI 控制（2026-09-06 拍板），缺省=安装根。
- 安全：走面板 auth；白名单外拒绝 + 记日志；exe 直接 spawn（argv 数组不拼 shell），.bat/.cmd 走 `cmd /c`、.ps1 走 `powershell -File`，全程审计日志。
- **[x] 完成（2026-09-07）**：7 条 API + nav 13 前端全接线；280 测试绿；stop/start 全循环实测（fin-system-map 端口-only 停起、aiexport managed 精确杀 8064 且 ZCode 21336 存活、白名单拒绝 calc.exe）；UI 列表/启停/刷新/白名单点验通过。接手轮额外修 1 个 bug：probe 假阳性（配置端口探针且端口关闭时 needle 多匹配误判 running，如 AIExport 匹配 ZCode cwd）——`needle_alive()` 纯函数 + 单测。详见 docs/交接-2026-09-07-native.md。
- 种子验收：fengweb 23456 / fengmedia-webui 23457 / system-map 34567 / aiexport 8787 / FengMedia-Daily，面板一键全起状态灯全绿。
- 验收：UI 加→启→停→删→自启开关全流程 + 回归测试绿。


## 第 39 轮起：活跃任务（2026-09-07 重建）

### T-13 售后诊断包（进行中，代码已完工未提交）
- [x] `api/diagbundle.rs`：GET /api/diagnostics/bundle——版本/自检/健康/服务/服务墙/配置摘要/日志尾 300 行；脱敏关键词与 check-secrets.sh 同源（单测守同步）；落盘 data/flygo-diag-*.json + 附件下载
- [x] 前端：组件自检页「🩺 导出诊断包」按钮（中英文）+ Defender 误报白名单提示入自检页（复用 docs/defender-false-positive.md）
- [x] docs/after-sales-sop.md：客服 SOP，第一句="请发诊断包"
- [x] 全量测试(284绿) → 装机 → 活体验收（bundle 导出 200/44KB）→ 提交双推（2ce67e9，2026-09-07）

### T-14 P0 Done 总验收
- 前置：T-13 收尾 + T-11 VPS / T-12 keystore / 真机线（均用户侧）

### 平台 WebUI 优雅截图（2026-09-07 新任务，进行中）
- 内容：FengInvest（23456）、FengMedia（23457）、FlyGo 面板（8388）的 WebUI 截图，电脑版 + 手机版各一套，呈现要优雅（体现精心制作）
- 产出目录：docs/screenshots-20260907/

## 📋 排队（下一批）

- [ ] 深度连接实施（手机客户端 ↔ 面板 ↔ 控制面；PlainApp 无 HTTP 客户端模式 = 差异化空白）
- [ ] PlainApp 移动网盘功能实施（docs/plainapp-integration.md）
- [ ] 面板接入控制面收尾（激活向导 + 心跳；客户端 UI 已有雏形）+ 控制面 VPS 部署（T-11）
- [ ] 手机端实测验收（Syncthing-Fork 扫码 → 同步 → 面板看到手机；T-12 release keystore）
- [ ] 官网 flygo.fengyuwang.com 部署上线
- [ ] G-05 Tailscale 自有账号登录闭环（新号全程录屏，用户侧）
- [ ] 真机关机 SMB 清理验证 + 裸机装 Setup ≤30min 验收（用户侧）

## 🗓 以后

- [ ] W3 互备编排（Syncthing 一键配对 + staggered 版本控制）
- [ ] VIP 订阅收费——**不做现在**
- [ ] iOS 评估（PWA 不阻塞）
- [ ] 电脑三平台 + 安卓 APK 打包管线
- [ ] UI 整改 feat/ui-fix worktree + Logo 全局部署（随品牌实施）

### UI 大整改（2026-09-07 完成，v2 定稿）
- [x] 第一版暖 stone+橙全量换皮（token 层+深侧栏+字体）——用户看后拍板：设计系统好在结构（阴影/按键/侧栏细节）不在配色，配色改回**原版简洁蓝白**
- [x] v2：读 llm-text-processor 源码提炼结构 DNA 并移植——侧栏=同底色+border 分区+圆形 accent logo+激活态浅底 accent 字；卡片 12px+阴影两档+hover 抬升；按钮 8px+次级 hover；输入框 focus ring 光晕；6px 暖滚动条；::selection accent；viewIn .45s slide-up；手机顶栏品牌字 bug 修复
- [x] DESIGN.md 换版（§3 结构细节清单）+ AGENTS.md 指针更新；login.html 同步
- [x] 昼夜双主题+手机宽度浏览器验收通过；截图 docs/screenshots/flygo-overview-{desktop,mobile,desktop-dark}.png
- [ ] 官网 shots/ 投放（fengyuwang_com dev 分支，待网络轮）
- [x] 2026-09-07 产品截图流程固化为项目 skill：`.zcode/skills/flygo-product-shots/SKILL.md`（平台清单/browser-use 铁律/命名双落点/董事长监工模式）
