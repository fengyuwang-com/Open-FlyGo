# FlyGo 架构图（Mermaid）

> 本文档用 Mermaid 画清 FlyGo 的架构与关键流程，配合 AGENTS.md 阅读。
> 代码事实来源：panel/src/（main.rs / daemon.rs / smb.rs / tray.rs / api/*）。
> 生成于 2026-09-06 全量代码审读轮（四个逻辑 bug 修复同轮）。

## 1. 系统总架构（四层）

```mermaid
flowchart TB
    subgraph phone["手机 APK（一等公民，Kotlin）"]
        APK["flygo-mobile APK<br/>发 HTTP 文件服务 · 多向同步 · 面板遥控"]
    end
    subgraph pc["Windows 节点（panel，单 exe，Rust/axum，:8388）"]
        UI["面板 SPA static/index.html"]
        API["axum API 层 api/*（50+ 路由）"]
        TRAY["托盘 tray.rs<br/>Win32 消息循环 + WM_ENDSESSION"]
        DAEMON["子进程托管 daemon.rs"]
        SMBM["SMB 生命周期 smb.rs"]
        AUTH["登录层 auth.rs（默认关）"]
    end
    subgraph children["托管子进程"]
        ST["Syncthing :8384<br/>(同步引擎)"]
        WC["webcli :8788<br/>(终端)"]
        HP["Homepage<br/>(服务卡片墙)"]
    end
    subgraph infra["基础设施"]
        TS["Tailscale<br/>(节点互联 + Serve 三入口)"]
        CP["控制面 :9001<br/>(身份/激活/订阅/心跳)"]
        DOCKER["Docker/WSL<br/>(Jellyfin/Immich 按需)"]
    end

    APK -- "Syncthing 私有中继/直连" --> ST
    APK -- "HTTP(S) + Tailscale" --> API
    UI --> API
    API --> DAEMON
    API --> SMBM
    API --> AUTH
    TRAY -- TrayCmd(mpsc) --> DAEMON
    DAEMON --> ST & WC & HP
    DAEMON --> DOCKER
    API -- "心跳(元数据 only)" --> CP
    TS --- APK & API & CP
```

## 2. 启动序列（bind-early，先应答后初始化）

```mermaid
sequenceDiagram
    participant U as 用户/开机
    participant M as main()
    participant V as Velopack
    participant S as axum :8388
    participant B as 后台启动任务

    U->>M: 双击 flygo.exe
    M->>V: VelopackApp.run()（最先，解析 --velopack-* 开关）
    M->>M: 单实例互斥锁（重试机制；占用则开浏览器即退出）
    M->>S: 绑定端口（失败重试 N 次，再失败 exit 1）
    M->>S: axum::serve + graceful shutdown watch
    Note over S: 此刻面板已可应答 /api/boot/status = "binding"
    M--)B: spawn 后台启动任务
    B->>B: stale_cleanup：清残留子进程端口
    B->>B: smb::open_app()：reconcile 残留共享 + 建配置共享
    B->>B: 启动 DEFAULT_COMPONENTS（webcli）
    B->>B: WSL keepalive + boot::set_ready()
    M--)M: 常驻后台任务：sync ticker / USB 守护 / SMB 热插拔 / TCP 转发 / vdisp pin / 控制面心跳 / ws 推送
```

## 3. 退出与 SMB 共享契约（"关软件 = 共享消失"）

```mermaid
flowchart TB
    subgraph triggers["三种退出触发"]
        T1["托盘菜单退出"]
        T2["POST /api/tray/exit"]
        T3["WM_ENDSESSION（关机/注销广播）"]
    end
    T1 & T2 --> K["kill_all() 子进程"]
    K --> RS["request_shutdown() → watch 通道"]
    RS --> GS["axum graceful shutdown 返回 main 尾部"]
    GS --> CA["smb::close_app()（8s 硬上限）<br/>remove_many 批量删共享"]
    GS --> WS["shutdown_wsl() 释放 keepalive"]
    GS --> AB["abort 后台 tokio 任务"]
    GS --> EX["release_single_instance + exit(0)"]
    T3 --> SE["wnd_proc WM_ENDSESSION：<br/>同步调 smb::close_app()（唯一清理窗口）"]
```

## 4. SMB 挂载/热插拔状态机

```mermaid
stateDiagram-v2
    [*] --> Boot: open_app()
    Boot --> Reconcile: smb_enabled
    Reconcile --> CreateAll: 删掉上次硬杀残留的配置共享
    CreateAll --> Managing: create_one（离线盘 skip-offline）
    Managing --> Managing: 热插拔任务 15s/轮
    Managing --> Denied: 用户手动卸载/全部不挂 → UNMOUNT_DENY
    Denied --> Managing: 手动挂载（clear_denied）
    Denied --> Managing: 重启（deny 集清零）
    Managing --> Offline: 盘拔出（共享随盘消失）
    Offline --> Managing: 盘插回 → pending_auto_share 自动重挂
    Managing --> [*]: close_app()（remove_managed，仅删本会话管理的）
```

## 5. 组件拉取系统（T-08，winget 优先双层）

```mermaid
flowchart TB
    A["GET /api/fetch/components"] --> B["spawn_blocking：winget 探测 + 逐项 is_installed"]
    C["POST /api/fetch/install {id}"] --> D["install_item（spawn_blocking）"]
    D --> E{"winget 存在且有 winget_id?"}
    E -- 是 --> F["winget install --id --exact --silent"]
    F -->|Ok 0| OK1["成功 via winget"]
    F -->|失败 且 kind=msi/archive| G["降级 manifest 下载"]
    E -- 否 --> G
    G --> H["download_first：国内镜像 URL 依次尝试"]
    H --> I{"sha256 已钉?"}
    I -- 是 --> J["校验，不符即删拒装"]
    I -- 否 --> K["verified=false 照装（报告注记）"]
    J & K --> L["按 kind 落地：archive→解压取成员 / file→rename / msi→msiexec /quiet"]
    L --> OK2["成功 via manifest"]
    M["install-all"] --> N["spawn_blocking 过滤 required && !installed → 逐个 install_item，失败即停"]
```

## 6. 同步拓扑（用户视角）

```mermaid
flowchart LR
    subgraph devices["用户设备（各自跑 Syncthing）"]
        P["手机 APK"]
        D1["电脑 A（panel）"]
        D2["电脑 B"]
    end
    P <-- "双向增量（选文件夹→选设备）" --> D1
    D1 <-- 多副本 --> D2
    P <-- 版本化(staggered) --> D2
    D1 -. "配对信令走控制面 pair.rs" .-> CP(("控制面 :9001"))
```

## 7. 本轮审读修掉的四个逻辑 bug（2026-09-06）

| # | 位置 | 症状 | 根因 | 修复 |
|---|---|---|---|---|
| 1 | `api/syncview.rs` 测试 | `live_overview_collect`/`routes` 间歇 panic "config::init() must run before config::get()" | 测试只调了 `settings::init()`，而 `read_apikey` 走 `config::get()`；跑序不同结果不同 | 测试改调 `config::init()`（其内部先冻结 settings） |
| 2 | `main.rs` | T-10 提交信息声称加了 Velopack 钩子，实际从未入库（release exe 也没打进去） | 提交时 main.rs 漏 add | 补回 `VelopackApp::build().on_before_uninstall_fast_callback(smb::close_app).run()` 于 main() 最前 |
| 3 | `api/fetch.rs` | `components`/`install-all` 在 async 上下文里直接跑 `is_installed()`（内部 spawn 子进程、15-20s 超时），会卡死 tokio worker | 阻塞调用没进 spawn_blocking | 状态枚举与缺失过滤整体挪进 spawn_blocking |
| 4 | `smb.rs probe_share` | UI 匿名可达探测失败时也显示"可达" | 脚本输出 `NOK`，代码用 `contains("OK")` 判定，"NOK" 包含 "OK" 子串 | 改为精确匹配 `eq_ignore_ascii_case("OK")` |
