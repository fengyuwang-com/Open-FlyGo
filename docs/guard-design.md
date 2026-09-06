# FlyGo 面板收编 WSL / Docker / 容器管理 — 技术方案（guard-design）

> 状态：**方案（待评审）** · 关联铁律：AGENTS.md 五-6（端口不乱改）、哲学 §四（按需启动）、§二（能拼就拼）。
> 本文档解决：WSL VM 空闲被终止导致容器反复重启；外部计划任务 WSLKeepAlive + 常驻 wsl.exe 会话保活，不符合"一切由 EXE 统一管理"。
> 决策冲突时以 `docs/产品哲学与工程原则.md` 为准。

---

## 0. 现状与根因（已实测）

| 项 | 现状 |
|---|---|
| WSL 版本 | 2.6.3.0（kernel 6.6.87.2-1），Store 版，足够新 |
| 业务分发版 | Ubuntu-24.04（dockerd 由 systemd 托管，compose 项目在 /mnt/c/FlyGoLegacy） |
| .wslconfig | `memory=6GB` `vmIdleTimeout=6000000`（100 分钟）`autoMemoryReclaim=gradual` |
| 保活现状 | 外部计划任务 **WSLKeepAlive**（Logon + Time 触发）→ `~/wsl_keepalive.ps1` 常驻 2 个 `wsl -d Ubuntu-24.04 -u root -- bash` 会话 |
| 面板现状 | `dockerctl.rs` 已有 `wsl_vm_running()`（只查 vmmemWSL 进程，**不唤醒原则**）、全局串行锁、超时杀树；`daemon.rs` 已有 USB 守卫线程模式可复用 |
| 引擎路径 | `scripts/docker-ctl.cmd`：engine on = `systemctl start docker`；engine off = `wsl --terminate Ubuntu-24.04` |

**vmIdleTimeout 为何实测不生效（根因，别再重复排查）：**

Microsoft 官方语义（.wslconfig reference）：`vmIdleTimeout` = "VM 空闲多久后关闭，**仅在没有任何 WSL 分发版运行时才适用**"。

即它管的是"**最后一个会话退出后** VM 多少毫秒关闭"，它**不会**让"有 dockerd 后台服务但无交互会话"的 VM 存活——恰恰相反，会话全退后 VM 会被按时判定 idle 并关闭。本机场景是"无交互会话 + systemd 托管 dockerd"，所以：

- `vmIdleTimeout` 对本场景**方向性不适用**（不是写错，是语义不匹配）；
- 正确的保活机制就是"**持有一个会话**"（会话存在 = VM 不 idle）——用户用 2 个常驻 wsl.exe bash 会话，机制是对的，错在放在外部计划任务里；
- 已知坑：`.wslconfig` 修改后必须 `wsl --shutdown` 全量重启 VM 才生效；旧版 WSL 会忽略该参数（本机版本无此问题）。

**结论：vmIdleTimeout 保留作"兜底"（保活会话意外死亡时，VM 100 分钟不关而非 60 秒），保活职责收编进面板 EXE。**

---

## 1. 面板启动自检流程（一打开就能用）

EXE 启动时在**后台线程**执行（不阻塞 8388 端口就绪），按序、带超时、每步可独立重试：

```
自检启动（后台，预算 ~15s）
├─ 1) Tailscale 服务
│     sc query Tailscale → 非 RUNNING 则 net start Tailscale（需管理员时走提权路径）
│     验证：tailscale status 出 100.x 地址（复用 /api/services 现有探测）
├─ 2) portproxy
│     netsh interface portproxy show v4tov4 → 与期望规则表比对（规则表 = 配置文件）
│     缺失/漂移 → 重建（提权脚本，规则命名 FlyGo-* 便于审计回滚）
├─ 3) WSL VM 状态（只查 vmmemWSL 进程，绝不 wsl 唤醒——沿用 dockerctl.rs 不唤醒原则）
│     目标态 = 开（引擎上次退出时为开 / 用户配置"容器常驻"）→ 拉保活会话（见 §2-C）
│     目标态 = 关 → 保持停机，跳过 4/5
├─ 4) dockerd
│     仅当 VM 在跑：wsl -d Ubuntu-24.04 -u root -- systemctl is-active docker
│     非 active 且目标态=开 → systemctl start docker（120s 超时，复用 ACTION_TIMEOUT 模式）
├─ 5) 容器
│     docker ps -a 对比期望清单（compose 项目 /mnt/c/FlyGoLegacy）→ 缺/退出 → docker compose up -d
└─ 6) 结果上报
      每步 ✓/✗ 写入面板「自检日志」+ 状态卡（一键修复按钮，见 §3）
```

原则：
- **不唤醒原则保留**：查询/轮询一律先查 vmmemWSL；只有"目标态=开"或用户显式点「引擎 开」才允许唤醒。
- 目标态持久化在面板本地状态文件（`%LOCALAPPDATA%\FlyGo\state.json`），不依赖注册表。
- 自检失败不阻塞启动；失败项在 UI 标红并提供「一键修复」。

## 2. 让 vmIdleTimeout 正确生效 / 主动保活

**A. 验证路径（写进文档而非代码）：**
1. `wsl --version` ≥ 2.0（本机 2.6.3 OK）；
2. `.wslconfig` 必须在 `%UserProfile%\.wslconfig`（本机已在）；
3. 每次改 `.wslconfig` 后必须 `wsl --shutdown` 再 `wsl -d Ubuntu-24.04 -u root -- true` 冷启，参数才生效；
4. 观察法：不保持会话，等 100 分钟看 vmmemWSL 是否退出（对照 60 秒默认值可临时改小验证）。

**B. .wslconfig 正确写法（现状已对，仅作基准）：**
```
[wsl2]
memory=6GB
vmIdleTimeout=6000000   # ms；仅"无任何分发版会话"时生效，单位毫秒
autoMemoryReclaim=gradual
```

**C. EXE 内主动保活（主方案，替代外部计划任务）：**
- 面板内新增 **WSL 保活线程**（参考 daemon.rs 的 USB 守卫线程模式：`std::thread::spawn` + 循环 + 30s 间隔）：
  - 每 30s：查 vmmemWSL 进程 + 查保活会话进程（wsl.exe 命令行含 `Ubuntu-24.04`）；
  - 目标态=开 且 会话缺失 → 拉起 **1 个**隐藏会话 `wsl -d Ubuntu-24.04 -u root -- bash`（CREATE_NO_WINDOW；1 个即可——面板自身常驻 + 30s 探测已构成双重保活，比现 2 个省一半会话内存）；
  - 目标态=关 → 不碰 WSL（不唤醒）；
- 引擎联动：`docker-ctl engine on` 成功 → 目标态=开，保活线程接管；`engine off` → 目标态=关，杀掉保活会话 + `wsl --terminate Ubuntu-24.04` 释放内存（现状行为保留）；
- 面板退出：优雅降级——保活会话随之退出，VM 由 vmIdleTimeout 兜底（100 分钟），面板重启后自检恢复；退出前按目标态=关则主动 terminate。

## 3. 面板内 WSL / Docker / 容器管理 UI 功能点

扩展「容器」页（`v-docker`），新增「WSL / Docker 自检」面板；移动端沿用汉堡菜单，三步可达：

| 功能 | 说明 |
|---|---|
| WSL 状态卡 | VM 运行/停机、保活会话数、vmmemWSL 内存占用、.wslconfig 关键项摘要、vmIdleTimeout 语义提示（"仅无会话时生效"） |
| 引擎开关（已有） | 「引擎 开」= 唤醒 + 拉保活 + 起 dockerd；「引擎 关」= 杀保活 + terminate VM |
| 一键修复 | 按钮 → 完整跑 §1 六步 → 逐步 ✓/✗ 展示（绿/红，一眼可读） |
| 容器清单（已有 svc 卡） | 增加异常容器红标 + 「一键拉起全部」（compose up -d） |
| 计划任务治理 | 检测到 `WSLKeepAlive` 计划任务 → 提示"已被面板接管" + 一键删除（见 §4） |
| 常驻策略（设置） | 三选一：**关机省内存**（默认，用时拉起）/ **会话保活**（容器常驻）/ **纯按需**（点击才启动）；持久化到 state.json |
| 自检日志 | 最近一次自检各步骤输出（复用 .log 样式，pre-wrap + break-all 已修好） |

## 4. 替代并删除 WSLKeepAlive 计划任务（迁移步骤）

1. 部署新版 EXE，确认「引擎保活」工作：面板「引擎 开」→ WSL 状态卡显示"保活中"；**重启面板 EXE 后** VM 仍在或自检自动恢复（关键验收点）。
2. 面板「计划任务治理」识别到 `WSLKeepAlive`（Logon/Time 触发，执行 `wsl_keepalive.ps1`）。
3. 一键删除：`schtasks /delete /tn WSLKeepAlive /f`；脚本 `~/wsl_keepalive.ps1` 先 `cp` 备份到 `C:\FlyGoLegacy\scripts\backup\` 再删除。
4. 验收：重启电脑 → 面板自启 → 自检拉保活 → VM 存活、容器正常、无外部任务依赖。
5. 回滚：备份脚本还在；面板异常时重新注册任务即可（迁移全程可逆）。

## 5. 分阶段实施清单

**本轮（P0，方案确认后实施）：**
- 阶段 1 — 保活收编：保活线程 + 引擎态联动 + state.json（目标态）+ 退出降级；
- 阶段 2 — 自检骨架：§1 六步（Tailscale/portproxy/WSL/dockerd/容器/上报）+ 一键修复按钮 + WSLKeepAlive 收编迁移（§4）。
  - 2026-09-05 更新：自检已扩到**七步**（新增第 3 步 `tcpforward`，见 `docs/tcpforward-design.md`）；
    容器端口唯一 owner 从 portproxy/serve 改为面板内建 TCP 转发层，serve 只留面板 443/8443。

**下轮：**
- 阶段 3 — 容器期望态编排：compose 项目级 up/down 差异修复、异常容器红标告警；评估收编/停用 Docker Desktop 自启动（避免双引擎打架，本机 autostart 里有 `Docker Desktop.exe` Run 值）；
- 阶段 4 — UI 完善：常驻策略三选项、自检历史日志、提权路径（admin 自检时 UAC 提示与结果回传）；
- 远期 — 跨平台：服务管理抽象层（Win=sc/netsh/wsl，macOS=launchd，Linux=systemd），对齐"三平台同码各编译"。

## 6. 实现落点（给子 Agent）

- 复用：`dockerctl.rs::wsl_vm_running()` / `LOCK` / `run_ctl_capture_to`（超时杀树）；`daemon.rs` USB 守卫线程模式；`scripts/docker-ctl.cmd` 不改。
- 新增：`src/api/wsl.rs`（/api/wsl/status、/api/wsl/selfcheck、/api/wsl/keepalive）或并入 dockerctl.rs；`daemon.rs` 加保活线程注册。
- 代码注释/日志：英文 ASCII；文档：UTF-8（本文件）。
- 端口不变：8388 面板 · 8788 webcli · 8384 Syncthing · 9199 Homepage · 9001 控制面。
