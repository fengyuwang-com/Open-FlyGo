# FlyGo 项目结构总览（2026-09-07 整理）

> 回答一个问题：**每个文件写在哪里、为什么在那里**。配合根 `AGENTS.md`（架构总纲）与 `docs/architecture-diagrams.md`（Mermaid 架构图）阅读。
> 结构变动（新增顶层目录/新增大模块）必须回写本文。

## 1. 顶层目录一览

```
FlyGo/
├── AGENTS.md            # 架构总纲（所有 agent 开工前必读）
├── DESIGN.md            # UI 设计宪法（配色=蓝白，结构细节对标 llm-text-processor）
├── Version              # 唯一版本号事实源（YYYY.MM.DD-HHMM），发版时 bump
├── todo.md              # 任务总账本（唯一根本，每轮任务必须入账）
├── FENGMEM.md           # 会话记忆（每轮追加，只增不覆盖）
├── panel/               # ★ 节点面板（主产品，Rust axum 单 exe，:8388）
├── control-plane/       # 控制面（身份/激活码/订阅，Rust axum+rusqlite，:9001）
├── webcli/              # Web 终端（独立 crate，:8788，被 panel 托管拉起）
├── site/                # 官网静态站（flygo.fengyuwang.com）
├── bin/                 # 部署产物（flygo.exe/flygo-launcher.exe/flygo-webcli.exe；gitignore，本地运行用）
├── release/             # 发布产物（flygo.exe 入库；装机拷贝源）
├── dist/                # 安装器产物（Velopack Setup 包等，构建输出）
├── scripts/             # 运维/验证脚本（check-secrets.sh / make-installer.ps1 / tailscale-serve.sh …）
├── config/              # 用户配置 flygo.toml（面板读取的控制项）
├── data/                # 运行时数据（运行态 JSON、日志；native-*.json 不入库）
├── docs/                # 全部文档（设计/计划/交接/审计/截图）
└── logo/                # 品牌 logo 源文件
```

## 2. panel/（节点面板——四层架构的第 1 层）

```
panel/
├── Cargo.toml
├── static/              # 内嵌前端（include_str! 编进 exe，改完必须 cargo build 才生效！）
│   ├── index.html       # 面板 SPA 单体（全部 view + CSS + JS + i18n）
│   ├── login.html       # 登录页（auth.rs 内嵌）
│   ├── gather.html      # 聚合页
│   └── vendor/          # xterm.js / addon-fit 第三方前端资产
└── src/
    ├── main.rs          # 入口：路由装配 + 托盘启动 + 优雅退出契约（尾部 exit(0) 兜底）
    ├── tray.rs          # Win32 托盘（WM_ENDSESSION 删共享；「不挂载」=仅停共享不弹盘）
    ├── daemon.rs        # 子进程托管（Syncthing/webcli/Homepage 拉起与回收）
    ├── smb.rs           # SMB 共享生命周期（批量 remove/revoke + 超时兜底 + 开机孤儿自愈）
    ├── settings.rs      # 内存态设置（persist_smb 刷新，控制 env 覆盖文件）
    ├── config.rs        # flygo.toml 解析
    ├── log.rs           # 结构化日志 → data/logs/flygo.log（滚动）
    └── api/             # axum API 层（50+ 路由，一域一文件）
        ├── health.rs / stats.rs / system.rs / wsstatus.rs   # 总览/资源/WS 聚合
        ├── sync.rs / syncview.rs / pair.rs / onboarding.rs  # 同步/配对/手机接入
        ├── smb.rs / vdisp.rs / upload.rs                    # 共享/虚拟盘/上传
        ├── docker.rs / dockerctl.rs / dockernet.rs / wsl.rs # 容器与 WSL（按需拉起）
        ├── services.rs / components.rs / native.rs / autostart.rs  # 种子服务/组件卡/本地服务/自启动
        ├── term.rs          # 终端（内嵌 xterm 资产，ws -> webcli）
        ├── access.rs / auth.rs / cfg.rs / control.rs        # 防火墙/登录/配置/控制面桥
        ├── fetch.rs         # 组件下载系统（T-08，winget 三级解析）
        ├── diagnostics.rs / diagbundle.rs / gather.rs       # 诊断页/售后诊断包(T-13)/聚合
        └── plainapp.rs / openlist.rs / tcpfwd.rs / boot.rs / tray.rs / mod.rs
```

**铁律**：`panel/static/` 是 `include_str!` 内嵌——改 HTML 后必须 `cargo build --release` + 重启面板才能在浏览器看到。

## 3. 容器与运行时边界（什么东西在哪跑）

| 运行体 | 位置 | 端口 | 谁拉起 |
|---|---|---|---|
| panel 主进程 | `bin/flygo.exe`（托盘常驻） | 8388 | flygo-launcher / 开机自启 |
| Syncthing | panel daemon 子进程（winget 安装） | 8384（仅 127.0.0.1） | panel 管家组件页 |
| webcli | panel daemon 子进程 | 8788 | panel（默认自启） |
| Homepage | panel daemon 子进程 | 本地 | panel（默认不启） |
| Docker 容器 | WSL2 后端（FengNAS 引用只读） | 各自映射 | 容器页按需（portainer/alist/immich…） |
| 控制面 | `control-plane/` 单二进制（本机 9001，待 VPS） | 9001 | 手动/systemd（VPS） |
| Tailscale Serve | 系统服务 | 8443→webcli 等 | `scripts/tailscale-serve.sh` |

数据只落两处：`config/flygo.toml`（人写配置）与 `data/`（运行时状态，不入库敏感项）。**FengNAS（C:\FengNAS）只读**。

## 4. 文档去哪写（写作路由）

| 要写什么 | 写到哪 |
|---|---|
| 架构级新决定/原则 | 根 `AGENTS.md`（及时回写） |
| UI 规范 | 根 `DESIGN.md`；细刻度 `docs/ui-design-guide.md` |
| 任务/待办/验收 | 根 `todo.md`（唯一账本） |
| 会话记录 | `FENGMEM.md` 追加 |
| 专项设计/审计/事故报告 | `docs/<主题>-<性质>.md`（如 tcpfwd-design、g04-degradation-audit） |
| 轮次交接 | `docs/交接-YYYY-MM-DD[-主题].md` |
| 架构图 | `docs/architecture-diagrams.md`（Mermaid，随结构变动更新） |
| 截图 | `docs/screenshots/`（命名 `<product>-<page>-desktop|mobile.png`），官网用图另投 `fengyuwang_com/assets/img/shots/` |
| 售后/客服 SOP | `docs/after-sales-sop.md` |

## 5. 构建/发版流水线（panel 为例）

1. `cargo test`（panel/，全绿才继续）
2. `cargo build --release`
3. 停面板（taskkill）→ 拷 `target/release/flygo.exe` 到 `bin/` 与 `release/`
4. 重启 `bin/flygo-launcher.exe`，curl 8388 验证
5. bump 根 `Version`（YYYY.MM.DD-HHMM）
6. `bash scripts/check-secrets.sh` → 提交 → 双推 `origin main` + `gitee main`
