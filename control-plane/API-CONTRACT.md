# FlyGo Control Plane — API Contract

> 供所有 FlyGo 开发 Agent 对齐的 API 契约。来源：`src/main.rs` / `src/api/device.rs` / `src/api/admin.rs` / `src/db.rs` / `src/auth.rs`（封存仓库 FengNAS/control-plane 同源）。
> 服务：Rust + axum 0.7 + SQLite（rusqlite bundled），单二进制，HTTP（非 TLS）。默认监听 `127.0.0.1:9001`（公网必须走反代终结 TLS）。

## 端点总览

| 方法 | 路径 | 鉴权 | 限速 | 说明 |
|---|---|---|---|---|
| POST | `/api/register` | 无 | 10/分/IP | 注册新设备，返回 8 位激活码；同硬件未激活时幂等返回原码；已激活返回 409 |
| POST | `/api/activate` | 无 | 10/分/IP | 激活码 + 硬件指纹匹配 → 下发设备 JWT。注册码走默认 30 天试用；管理端签发的独立码按 `kind` 生效（trial/pro） |
| GET | `/api/status` | 设备 JWT | — | 设备信息 + 订阅状态 |
| POST | `/api/heartbeat` | 设备 JWT | — | 上报 `disk_free_bytes / online / version`；响应携带当前订阅状态 |
| POST | `/api/renew` | 管理端 | — | 续期（默认 +365 天 / active / 1 TiB 配额） |
| POST | `/api/cancel` | 管理端 | — | 取消订阅（终态） |
| POST | `/api/admin/login` | 无（引导 token） | 5/分/IP | 用 `FENGDOG_ADMIN_TOKEN` 换 12h 管理 JWT |
| GET | `/api/admin/devices` | 管理 JWT 或原始管理 token | — | 设备全量列表（含激活码、心跳、订阅） |
| GET | `/api/admin/codes` | 管理 JWT 或原始管理 token | — | 已签发的独立激活码列表（新→旧，最多 500 条） |
| POST | `/api/admin/codes` | 管理 JWT 或原始管理 token | — | 签发独立激活码 `{kind: trial\|pro}` |
| POST | `/api/admin/codes/revoke` | 管理 JWT 或原始管理 token | — | 作废未使用的码（已用 409 / 未知 404 / 重复作废幂等） |
| POST | `/api/pair/invite` | 设备 JWT | 10/分/IP | 互备配对：生成 6 位数字邀请码（10 分钟过期；需已激活且订阅未过期） |
| POST | `/api/pair/accept` | 设备 JWT | 10/分/IP | 对端凭码接受 → 返回邀请方 `inviter_syncthing_id`（码一次性） |
| GET | `/api/pair/poll` | 设备 JWT | — | 邀请方轮询：`status = none/pending/matched/expired` |
| GET | `/` | 无 | — | 内嵌管理页（HTML：登录 → 设备表 → 激活码管理） |

## 通用约定

- **错误格式统一**：`{"ok": false, "message": "..."}`；成功为 `{"ok": true, ...}`。
- 鉴权头：`Authorization: Bearer <JWT>`（设备 JWT 或管理 JWT；`/api/admin/*` 端点也可直接用原始管理 token 做 Bearer）。
- 时间：全部 Unix epoch 秒（UTC），服务端统一用 `chrono::Utc::now().timestamp()`。
- JSON 请求体必需 `Content-Type: application/json`。
- 激活码字母表排除易混淆字符：`ABCDEFGHJKLMNPQRSTUVWXYZ23456789`（无 0/O/1/I），固定 8 位。

## 端点详情

### POST /api/register — 注册

请求体：
```json
{
  "device_name": "node-a",            // 必填，trim 后 1-64 字符
  "node_kind": "computer",            // 必填，枚举：computer | phone | drive
  "hardware_id": "HW-ABC-123"         // 必填，trim 后 4-128 字符（硬件指纹）
}
```

响应（200）：
```json
{
  "ok": true,
  "device": { /* 见下方 Device 对象，include_code=true */ }
}
```
- 同 `hardware_id` 未激活：幂等返回原激活码（不重新生成）。
- 已激活：409 `{"ok": false, "message": "hardware already activated, one activation per device"}`。

### POST /api/activate — 激活

请求体：
```json
{
  "activation_code": "XXXX-XXXX",     // 必填（服务端 trim + 大写后比较，须 8 位）
  "hardware_id": "HW-ABC-123",        // 必填，必须与注册时一致
  "public_key": null                  // 可选，预留字段，MVP 接受并忽略
}
```

响应（200）：
```json
{
  "ok": true,
  "token": "<设备 JWT, HS256, 有效期 7 天>",
  "device": { /* Device, include_code=false */ },
  "subscription_state": "trial",
  "expires_at": 1893456000
}
```

两族激活码（同一套 8 位字母表）：

| 来源 | 生效条款 | 说明 |
|---|---|---|
| 注册时下发的设备绑定码（`/api/register` 返回） | 默认 30 天试用（trial 条款） | 绑定硬件指纹：换硬件报 400 `activation code does not match this hardware` |
| 管理端签发独立码（`/api/admin/codes`） | 按 kind：trial = 30 天试用 / pro = 1 年 active | 设备须先注册；单次兑换后即焚毁 |

- 激活码不存在/位数不对：400 `invalid activation code`。
- 激活码与硬件不匹配：400 `activation code does not match this hardware`。
- 激活码已用：409 `activation code already used`。
- 独立码已作废：400 `activation code has been revoked`。
- 硬件未注册就持码激活：400 `unknown hardware id, register the device first`。
- 已激活设备再烧第二个码：409 `device already activated`。

### GET /api/status — 设备状态

请求头：`Authorization: Bearer <设备 JWT>`

响应（200）：
```json
{
  "ok": true,
  "server_time": 1780000000,
  "device": { /* Device, include_code=false */ }
}
```

### POST /api/heartbeat — 心跳

请求体：
```json
{
  "disk_free_bytes": 1234567890,     // 可选，>= 0
  "online": true,                     // 可选，仅兼容保留；服务端按心跳新鲜度判定在线
  "version": "0.1.0"                  // 可选
}
```

响应（200）：
```json
{
  "ok": true,
  "server_time": 1780000000,
  "subscription_state": "active",
  "expires_at": 1893456000
}
```
- 15 分钟无心跳判离线（`online` 字段服务端推导）。
- 设备不存在：404。

### POST /api/renew — 管理端续期

请求体：
```json
{
  "hardware_id": "HW-ABC-123",
  "days": 365,            // 可选，默认 365，范围 1-3650
  "plan": "active"        // 可选，枚举：active | trial（默认 active）
}
```
- 配额随 plan 切换：active = 1 TiB（`1024^4`），trial = 100 GiB（`100*1024^3`）。

响应（200）：`{"ok": true, "device": { /* include_code=true */ }}`

### POST /api/cancel — 管理端取消（终态）

请求体：`{"hardware_id": "HW-ABC-123"}`

响应（200）：`{"ok": true, "device": { /* include_code=true, plan=cancelled */ }}`

### POST /api/admin/login — 管理登录

请求体：`{"token": "<FENGDOG_ADMIN_TOKEN 原始值>"}`

响应（200）：
```json
{
  "ok": true,
  "token": "<管理 JWT, HS256, 有效期 12 小时>",
  "expires_in_secs": 43200
}
```
- 未配置 `FENGDOG_ADMIN_TOKEN`：503 `admin token not configured`。
- token 错误：401 `wrong admin token`。

### GET /api/admin/devices — 设备列表

请求头：`Authorization: Bearer <管理 JWT 或原始管理 token>`

响应（200）：
```json
{
  "ok": true,
  "server_time": 1780000000,
  "count": 1,
  "devices": [ /* Device, include_code=true */ ]
}
```

### GET /api/admin/codes — 独立激活码列表

请求头：`Authorization: Bearer <管理 JWT 或原始管理 token>`

响应（200）：
```json
{
  "ok": true,
  "count": 1,
  "codes": [
    {
      "code": "XXXXXXX8",
      "kind": "trial",              // trial | pro
      "status": "issued",           // issued | used | revoked
      "created_at": 1780000000,
      "used_at": null,
      "used_by_device_id": null
    }
  ]
}
```
- 按签发顺序倒序（新→旧），上限 500 条；管理页数据源。

### POST /api/admin/codes — 签发独立激活码

请求体：`{"kind": "trial"}`（枚举：trial = 30 天试用 / pro = 1 年正式版）

响应（200）：
```json
{
  "ok": true,
  "code": { "code": "XXXXXXX8", "kind": "trial", "status": "issued", "created_at": 1780000000, "used_at": null, "used_by_device_id": null }
}
```
- `kind` 非 trial/pro：400 `kind must be 'trial' or 'pro'`。
- 需管理鉴权（401）。

### POST /api/admin/codes/revoke — 作废独立激活码

请求体：`{"code": "XXXXXXX8"}`（服务端 trim + 大写）

响应（200）：
```json
{ "ok": true, "code": { "code": "XXXXXXX8", "kind": "trial", "status": "revoked", ... } }
```
- 未知码：404 `code not found`。
- 已使用的码：409 `code already used, cannot revoke`。
- 对已作废的码重复作废：幂等 200（不再变化）。

## Device 对象字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `device_id` | int | 自增 ID（JWT 的 sub 即此值） |
| `hardware_id` | string | 硬件指纹 |
| `name` | string | 设备名 |
| `node_kind` | string | computer / phone / drive |
| `plan` | string | trial / active / cancelled |
| `subscription_state` | string | pending / trial / active / expiring / expired / cancelled |
| `activated_at` | int \| null | 激活时间 |
| `expires_at` | int \| null | 订阅到期时间 |
| `quota_bytes` | int | 配额字节数 |
| `last_heartbeat_at` | int \| null | 最近心跳时间 |
| `disk_free_bytes` | int \| null | 最近上报的剩余磁盘 |
| `online` | bool | 心跳 15 分钟内是否在线 |
| `version` | string \| null | 节点版本 |
| `created_at` | int | 注册时间 |
| `activation_code` | string | 仅 include_code=true（注册/管理端）时返回 |

## 订阅状态机

```
pending(未激活) → trial(激活后30天) → active(续费后1年) → expiring(到期前30天内) → expired
cancel(任意时刻，除 cancelled 外) → cancelled（终态）
```
- 激活即进入 trial，`expires_at = 激活时间 + 30*86400`（独立 pro 码激活即 active，`expires_at = 激活时间 + 365*86400`）。
- `expiring` 判定：`plan==active && 到期剩余 <= EXPIRING_WINDOW_SECS`（30 天）。
- 已过期且 plan 为 trial：按 expired 呈现；cancelled 恒为终态。

## JWT

- 算法：HS256，密钥 = `FENGDOG_SECRET`。
- 设备 token：`sub=device_id`，有效期 **7 天**（`DEVICE_TOKEN_TTL_SECS`）。
- 管理 token：有效期 **12 小时**（`ADMIN_TOKEN_TTL_SECS`）。
- 密钥轮换会使所有旧 JWT 失效 → 节点需重新激活。

## 环境变量

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `FENGDOG_SECRET` | **是** | 无 | JWT 签名密钥，≥16 字符；未设置拒绝启动 |
| `FENGDOG_ADMIN_TOKEN` | 否 | 无 | 管理端引导 token；不设置则管理端点 503 |
| `FENGDOG_DB` | 否 | `./control-plane.db` | SQLite 文件路径 |
| `FENGDOG_BIND` | 否 | `127.0.0.1` | 监听地址；`0.0.0.0` 仅在防火墙兜底时使用 |
| `FENGDOG_PORT` | 否 | `9001` | 监听端口；CLI `--port` 参数优先 |

## 限速（固定窗口，内存态，按 IP）

- `register` / `activate`：10 次/60 秒/IP
- `admin/login`：5 次/60 秒/IP
- `pair/invite` / `pair/accept`：10 次/60 秒/IP

## 运维要点

- 单二进制产物：`target/release/flygo-control-plane.exe`（Linux 为 `flygo-control-plane`）——二进制名 = Cargo 包名，三平台一致。
- SQLite 单文件，迁移 = 拷库；换密钥需节点重新激活。
- 控制面挂掉不影响节点本地同步（心跳失败不惩罚），恢复后自动补报。
- 本服务不终结 TLS，公网必须走反代（见 DEPLOY-VPS.md）。