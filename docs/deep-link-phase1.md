# 深度连接 阶段1：手机 ↔ 面板互相看见（注册契约）

> 2026-09-11。阶段1目标：手机 App 主动把自身登记到电脑面板，面板把已登记手机渲染成设备卡片（名称/地址/在线状态/直达链接）。数据平面不经过面板——浏览器直连手机文件服务，面板只存地址 + TCP 探测。

## 1. 角色与数据流

```
手机 App (PlainApp fork, com.flygo.client)          电脑面板 (Rust axum, :8388)
├─ ktor 内嵌 HTTP 文件服务（TempData.httpPort，默认 8080）   ├─ POST /api/plainapp/register  ← 手机自报
└─ 设置 → FlyGo 面板 → 连接到电脑面板（本契约客户端）        ├─ GET  /api/plainapp/status    ← 面板 TCP 探测
                                                          └─ data/phones.json             ← 持久化
浏览器/面板前端 → http://<手机IP>:<port> 直达手机文件服务（数据平面）
```

- 实现位置：panel/src/api/plainapp.rs（服务端）；PlainApp `shared/src/commonMain/kotlin/com/flygo/panel/PanelLinkClient.kt` + `ui/page/settings/FlyGoPanelCard.kt`（客户端）。

## 2. 注册契约

### POST /api/plainapp/register

请求体 JSON：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| name | string | 是 | 设备显示名，1-64 字符，唯一键（同名=原地更新） |
| host | string | 是 | 手机自身 HTTP 服务地址，裸 IP 或主机名（不带 scheme/path/空格，≤253 字符） |
| port | int | 否 | 手机 HTTP 服务端口，1-65535，缺省 8080 |
| note | string | 否 | 备注，≤256 字符 |

成功 200：

```json
{"ok": true, "count": 1, "phones": [{"name": "Feng-Phone", "host": "192.168.1.51", "port": 8080, "note": ""}]}
```

失败（400 校验不通过 / 500 落盘失败），HTTP 非 200，body：

```json
{"error": "host must be a bare IP or hostname (no scheme, no path)"}
```

错误消息为英文 ASCII。典型错误：`name must be 1-64 chars` · `host must be a bare IP or hostname...` · `port must be 1-65535` · `note too long (max 256 chars)`。

### GET /api/plainapp/status

并发 TCP 探测全部已登记手机（超时 1.5s），返回 200：

```json
{"ok": true, "count": 1, "phones": [
  {"name": "Feng-Phone", "host": "192.168.1.51", "port": 8080, "note": "",
   "reachable": true, "latency_ms": 42, "reason": "ok", "last_check": 1760000000}
]}
```

- `reason`: ok / refused / timeout（refused 含 DNS 失败）。
- **离线是正常状态**：手机按需在线，探测失败不是错误，HTTP 仍 200。
- 三色阈值：reachable 且 <200ms 绿 / <1000ms 黄 / 其余红。

### DELETE /api/plainapp/register?name=N

从面板移除登记（不影响手机本身）。未知 name 返回 400 `unknown phone: N`。

### GET /api/plainapp/register

只读列出登记表（不探测），返回同 `{ok, count, phones}`。

## 3. 手机端行为（阶段1 已实现）

- 设置 → FlyGo 面板 → **连接到电脑面板**：弹窗填面板地址（`host:port`，支持省略 `http://` 前缀与结尾路径，端口缺省 8388）+ 设备显示名。
- host/port 自动取手机现状：`getDeviceIP4()` + `TempData.httpPort`（8080）。
- **服务未开启时（`isHttpServerRunning()` == false）禁用登记按钮并如实提示**「Web 文件服务未开启，请先打开发射再登记」。
- 成功/失败 Toast；面板地址与设备名持久化到 DataStore（`flygo_panel_link_address` / `flygo_panel_link_name`）。

## 4. 面板端行为（阶段1 已有）

- 前端「手机节点」卡片墙（static/index.html `#phonewall`）：名称 / 地址:端口 / 三色在线灯 / 延迟 / 直达链接 `http://host:port`（+ `https://host:8443` 备用）/ ✕ 移除，中英 i18n 齐全，15 秒自动刷新。

## 5. 路线图

- **阶段2（手机 ↔ 面板文件浏览/上传）**：面板远程浏览手机文件服务目录、手机→面板已有 send-to-panel 通道对齐；探测升级为 HTTP 健康检查（/init 或等价端点）。
- **阶段3（控制面参与）**：登记状态上报控制面心跳（node_kind=phone），订阅到期只停服务不停本地登记；换新/迁移时手机登记关系随身份走。
