# 更新日志

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## 0.3.1（2026-08-16）
- **修复黑屏 bug**：0.3.0 只路由了 `/`，`/vendor/xterm/*`、`/vendor/addon-fit/*`
  全部 404 → 浏览器 `Terminal is not defined` 黑屏。新增 fallback 静态文件路由
  （`rust-embed` 内嵌文件按扩展名配 Content-Type），终端页面恢复正常
- 验证：无头浏览器实测 `/vendor/xterm/xterm.js` 等 200，ConPTY 会话出现
  cmd 提示符（`C:\Users\<user>>`）

## 0.3.0（2026-08-16）
- **从 Python 重写为 Rust 单二进制**：axum + tokio + portable-pty（ConPTY）后端，
  前端改为 xterm.js 真终端（含 addon-fit），静态资源由 rust-embed 编译期内嵌，
  发布只需一个 `..\bin\flygo-webcli.exe`
- **移除 Python 版**：单命令模式（`cmd /c` 逐条执行）、高危命令二次确认、
  Python 包与 pyproject.toml、pytest 套件全部移除
- **新协议**：端点 `/ws` 改为 `/term`；消息类型改为 `input` / `resize`（入）与
  `output` / `exit`（出）；每连接一个全新 ConPTY 会话，支持全屏 TUI 程序
- **新增环境变量**：`TERM_SHELL`（默认 `cmd.exe`）、`TERM_CWD`（默认用户主目录）；
  保留 `WEBHOST` / `WEBPORT` / `WEBTOKEN`（鉴权逻辑不变，不匹配仍 close 1008）
- **审计日志改为按会话**：`audit.log` 记录来源 IP / shell / 时长 / 退出码，
  替代原按命令逐条的 `access.log`
- **静态页内嵌**：`/` 由 rust-embed 直接内嵌提供，无外部静态资源目录，断线自动重连
  与全屏适配保留并升级（xterm.js + addon-fit）

## 0.2.0（2026-08-15）
- 重构为 src 布局（src/feng_webcli/），新增 pyproject.toml 与 flygo-webcli 命令行入口
- 测试改为 pytest（tests/，7 个用例），新增 .github/workflows/ci.yml（ruff + pytest 双平台）
- 新增可选 WEBTOKEN 鉴权（/ws 连接须带 ?token=，默认关闭）
- 文档体系补齐：README / CONTRIBUTING / SECURITY / PULL_REQUEST_TEMPLATE 同步更新
- 移除扁平布局遗留的 smoke_test.py / sec_check.py / TEST_PLAN.md（功能已被 pytest 套件覆盖）

## [0.1.0] - 2026-08-15

### 初始发布

webcli 首个可用版本：浏览器远程操作 Windows `cmd` 的网页命令行应用。

#### 新增

- **后端**（`server.py`）
  - FastAPI + WebSocket 服务，`/ws` 端点执行命令并逐行流式回显
  - 高危命令二次确认：命中规则先回 `confirm_required`，确认前不执行
  - 确认超时（60s）与命令超时（300s，终止进程树）保护
  - 审计日志：每条命令追加 JSON 到 `access.log`（IP/命令/耗时/退出码/是否被拒/摘要）
  - 安全默认：绑定 `127.0.0.1:8788`，支持 `WEBHOST` / `WEBPORT` 环境变量
  - Windows 兼容：`cmd /c` 执行、`CREATE_NO_WINDOW`、GBK→UTF-8 解码防乱码
  - 每连接独立 Handler，并发连接互不共享状态
- **前端**（`static/index.html`）
  - 零构建单页终端 UI：流式输出、连接状态栏、断线自动重连
  - 高危确认弹窗（继续执行 / 取消）、`clear` 清屏、`↑`/`↓` 命令历史
- **测试**
  - `smoke_test.py`：4 用例冒烟测试（正常执行 / 高危拦截 / 批准执行 / 拒绝码 130）
  - `sec_check.py`：安全专项验收（字符串 `"false"` 不得被误判为批准）
  - `TEST_PLAN.md`：验收测试计划

#### 安全说明

- 本版本**无内置认证**，访问控制完全依赖网络层绑定（默认 `127.0.0.1`），
  生产部署请绑定 Tailscale 私网 IP，勿暴露公网（详见 README「安全模型」）。

#### 已知限制

- 无 token/密码认证机制
- `requirements.txt` 未锁定依赖版本
- 命令执行面向 Windows `cmd`，非 Windows 平台适配不完整

[0.1.0]: https://github.com/example/feng-orchestrator/releases/tag/webcli-v0.1.0
