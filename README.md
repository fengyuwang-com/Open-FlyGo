# Open-FlyGo — 把旧 Windows 电脑变成完整的 NAS（Open 版 / 已脱敏）

> 面板一面总览：文件、影视、相册、同步、远程、容器。不用换硬件、不用折腾 Linux，一台 Windows 电脑就是完整的 NAS。

---

## 这是什么

**FlyGo（飞狗）** 是一个私有云数据平台：把闲置电脑/手机/硬盘变成私有云节点，核心是**任意文件夹的多向同步 + 多副本 + 随身取用**。

| 组件 | 说明 | 技术栈 |
|---|---|---|
| `panel/` | 节点面板（主产品）：Web 总览 + 服务卡片墙 + 同步视图 + Web 终端 + 托盘托管，单 exe | Rust (axum)，内嵌前端，`:8388` |
| `control-plane/` | 控制面：设备身份 / 激活码 / 订阅状态机 / 心跳（只收元数据，不碰数据平面） | Rust (axum + rusqlite + JWT)，`:9001` |
| `webcli/` | Web 终端（独立 crate，被面板托管拉起） | Rust + xterm.js，`:8788` |
| `site/` | 产品官网静态页 | 纯 HTML |
| `docs/` | 设计文档 / 调研报告 / 架构图 / 验收报告 | Markdown |
| `logo/` | 品牌资产源文件 | SVG/PNG |
| `scripts/` | 运维与验收脚本（含 `check-secrets.sh` 敏感文件门禁） | bash / python |

产品哲学（详见 `AGENTS.md` / `docs/产品哲学与工程原则.md`）：

1. **1 EXE + 1 APK**：对外交付永远两个文件，内部复杂度全隐藏。
2. **拼积木**：连接走 Tailscale（绝不重造），同步走 Syncthing（绝不自研引擎）；自研只发生在无现成积木且是差异化核心的地方。
3. **按需启动**：常驻几十 MB，Docker/WSL/容器用时才拉起。
4. **本地功能全免费**：开源软件免费、服务收费（类 Tailscale 模式）；到期只停服务，绝不停本地功能与已有数据。

## 关于本仓库（脱敏说明）

本仓库是 FlyGo 私有开发仓库的 **Open 公开镜像**，取自已提交的源码与文档。收录时做了脱敏处理，剔除内容包括（笼统说明，不展开细节）：

- 内部会话记忆与 AI 协作交接记录；
- 内部执行计划 / 通宵冲刺时间线 / 内部运营文档；
- 含真实部署标识与测试激活码的部署笔记（`.env.example` 等模板文件保留，均为空值占位符）；
- 与本产品无关的其他项目截图。

产品代码、架构与设计文档、产品规划、商业策略文档（`商业计划书.md` 等）完整保留。`docs/flygo-app-plan.md` 中一处私有组网地址已替换为通用占位符。

## 快速上手（从源码构建）

依赖：Windows 10/11 + Rust 工具链（stable）。

```bash
# 1) 节点面板
cd panel
cargo build --release
# 产物 target/release/flygo.exe，运行后浏览器打开 http://127.0.0.1:8388

# 2) 控制面（可选，自托管身份/订阅服务）
cd control-plane
cp .env.example .env        # 按模板填 FENGDOG_SECRET（openssl rand -hex 32）
cargo build --release
# 产物 flygo-control-plane.exe --port 9001

# 3) Web 终端
cd webcli
cargo build --release
```

细节见 `control-plane/README.md`、`control-plane/API-CONTRACT.md`、`docs/project-structure.md`。
当前版本：`2026.09.07-0130`（见根目录 `Version` 文件，格式 `YYYY.MM.DD-HHMM`）。可执行产物见 [Releases](../../releases/latest)。

## 文档导航

| 文件 | 内容 |
|---|---|
| [`AGENTS.md`](AGENTS.md) | 架构总纲：项目是什么、四层架构、产品哲学 |
| [`DESIGN.md`](DESIGN.md) | UI 设计宪法（配色 / 结构细节 / 文案清单） |
| [`商业计划书.md`](商业计划书.md) | 商业模式与产品矩阵（AGPL 开源 + 服务订阅） |
| [`docs/architecture-diagrams.md`](docs/architecture-diagrams.md) | Mermaid 架构图（7 张） |
| [`docs/project-structure.md`](docs/project-structure.md) | 每个文件写在哪里、为什么在那里 |
| [`docs/产品哲学与工程原则.md`](docs/产品哲学与工程原则.md) | 产品哲学全文 |
| [`docs/flygo-app-plan.md`](docs/flygo-app-plan.md) | 手机客户端（APK）产品规划 |
| [`docs/acceptance/`](docs/acceptance/) | 北极星验收报告（mock 模式） |

## 许可

本项目按项目规划文档中的声明以 **AGPL-3.0** 开源。`webcli/` 及内嵌的第三方前端资产（xterm.js 等）保留其原始许可证（Apache-2.0 / MIT），见各目录下的 LICENSE。
