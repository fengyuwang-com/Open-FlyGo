# 飞狗控制面（control-plane）

飞狗计划 P0 · W1：云端小后端 MVP。**身份 + 订阅 + 心跳**，只管理元数据，**不碰数据平面**
（文件只在节点间经 Tailscale/Syncthing 流动）。

技术栈：Rust + axum 0.7 + SQLite（rusqlite bundled），单二进制，无外部服务依赖。

## 功能范围

- 设备注册：节点上报 `device_name / node_kind / hardware_id`，下发 8 位激活码（绑定硬件指纹）
- 激活：激活码 + 硬件指纹匹配 → 开始 30 天试用订阅 → 下发设备 JWT（HS256，有效期 7 天）
- 激活码运营：管理端可签发**独立激活码**（`trial` = 30 天试用 / `pro` = 1 年正式版），用户在已注册设备上凭码激活；可作废未使用码
- 心跳：节点每几分钟上报 `disk_free_bytes / online / version`；15 分钟无心跳判离线
- 订阅状态机：`pending → trial(30天) → active(1年) → expiring(到期前30天) → expired`；续费 = 管理端续期（新激活码再次激活）；`cancel` 为终态
- 管理端：`/api/admin/login` 换 12 小时会话 JWT → `/api/admin/devices` 设备列表（心跳 + 订阅状态 + 激活码）→ 手动续期 / 取消 → `/api/admin/codes` 激活码签发 / 列表 / 作废
- 管理页：`GET /` 内嵌单页 HTML（冷 slate + 单蓝设计语言，同面板 include_str! 风格，含设备搜索过滤与激活码管理）

**边界（设计上不做）**：不存用户文件、不中转数据、不介入 Syncthing/Tailscale 数据平面、不做支付（激活码由运营人工签发）。

## API 清单

| 方法 | 路径 | 鉴权 | 请求体 | 说明 |
|---|---|---|---|---|
| POST | `/api/register` | 限速 10/分/IP | `{device_name, node_kind: computer\|phone\|drive, hardware_id}` | 注册，返回激活码；同硬件未激活时幂等返回原码；已激活返回 409 |
| POST | `/api/activate` | 限速 10/分/IP | `{activation_code, hardware_id, public_key?}` | 激活：注册码走默认试用条款；管理端签发的独立码按 `kind` 生效（trial/pro）。返回 `token`（JWT） |
| GET | `/api/status` | 设备 JWT | — | 设备信息 + 订阅状态 + 到期时间 + 配额 |
| POST | `/api/heartbeat` | 设备 JWT | `{disk_free_bytes, online, version}` | 更新心跳；响应携带当前订阅状态 |
| POST | `/api/renew` | 管理端 | `{hardware_id, days?, plan?}` | 续期（默认 +365 天 / active / 1 TiB 配额） |
| POST | `/api/cancel` | 管理端 | `{hardware_id}` | 取消订阅（终态） |
| POST | `/api/pair/invite` | 设备 JWT · 限速 10/分/IP | `{syncthing_id}` | 互备配对：生成 6 位数字邀请码（10 分钟过期；新邀请覆盖旧邀请；需已激活且订阅未过期） |
| POST | `/api/pair/accept` | 设备 JWT · 限速 10/分/IP | `{code, accepter_syncthing_id}` | 对端凭码接受 → 返回邀请方 `inviter_syncthing_id`（码一次性；过期/自配对分别 400/409） |
| GET | `/api/pair/poll` | 设备 JWT | — | 邀请方轮询：`status = none/pending/matched/expired`；matched 时携带 `accepter_syncthing_id` |
| POST | `/api/admin/login` | 限速 5/分/IP | `{token}` | 用 `FENGDOG_ADMIN_TOKEN` 换 12h 管理 JWT |
| GET | `/api/admin/devices` | 管理 JWT 或原始管理 token | — | 设备列表（心跳/订阅/激活码全量） |
| GET | `/api/admin/codes` | 管理 JWT 或原始管理 token | — | 已签发的独立激活码列表（新→旧，最多 500 条） |
| POST | `/api/admin/codes` | 管理 JWT 或原始管理 token | `{kind: trial\|pro}` | 签发独立激活码（trial = 30 天试用 / pro = 1 年正式版） |
| POST | `/api/admin/codes/revoke` | 管理 JWT 或原始管理 token | `{code}` | 作废未使用的码（已用码 409 / 未知码 404） |
| GET | `/` | 无 | — | 内嵌管理页（登录 → 设备表 → 激活码管理） |

错误格式统一：`{"ok": false, "message": "..."}`（与 FengNAS 面板一致）。

## 环境变量

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `FENGDOG_SECRET` | **是** | 无 | JWT 签名密钥（≥16 字符）。**未设置则拒绝启动**——本二进制绝不硬编码密钥 |
| `FENGDOG_ADMIN_TOKEN` | 否 | 无 | 管理端引导 token；不设置则管理端点不可用（503） |
| `FENGDOG_DB` | 否 | `./control-plane.db` | SQLite 文件路径 |
| `FENGDOG_BIND` | 否 | `127.0.0.1` | 监听地址；`0.0.0.0` 只在有防火墙/反代兜底时用 |
| `FENGDOG_PORT` | 否 | `9001` | 监听端口（`--port` 参数优先） |

## 本地启动

```bash
export FENGDOG_SECRET='change-me-32-chars-minimum'
export FENGDOG_ADMIN_TOKEN='my-admin-token'
cargo run -- --port 9001
# 或指定库文件
FENGDOG_DB=/path/to/control-plane.db cargo run
```

全链路自测：

```bash
# 1. 注册拿激活码
curl -s -X POST http://127.0.0.1:9001/api/register \
  -H 'Content-Type: application/json' \
  -d '{"device_name":"node-a","node_kind":"computer","hardware_id":"HW-ABC-123"}'

# 2. 激活拿 token（token 由服务端签发，以下为示意）
curl -s -X POST http://127.0.0.1:9001/api/activate \
  -H 'Content-Type: application/json' \
  -d '{"activation_code":"<上一步的码>","hardware_id":"HW-ABC-123"}'

# 3. 心跳（带 token）
curl -s -X POST http://127.0.0.1:9001/api/heartbeat \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer <token>' \
  -d '{"disk_free_bytes":1234567890,"online":true,"version":"0.1.0"}'

# 4. 管理端：登录换 JWT
curl -s -X POST http://127.0.0.1:9001/api/admin/login \
  -H 'Content-Type: application/json' -d '{"token":"my-admin-token"}'

# 5. 管理端设备列表
curl -s http://127.0.0.1:9001/api/admin/devices -H 'Authorization: Bearer <admin-jwt>'
```

## 测试

```bash
cargo test        # 全链路（注册→激活→心跳→管理列表）+ 错误路径 + 限速，临时 SQLite 文件
```

## 部署到 VPS（提示）

1. **构建**：在构建机上 `cargo build --release`（单二进制，产物 `target/release/flygo-control-plane.exe`，Linux 上为 `flygo-control-plane`），拷到 VPS。完整步骤与 systemd/nginx 配置见同目录 [DEPLOY-VPS.md](DEPLOY-VPS.md)。
2. **systemd 单元**（Linux，`/etc/systemd/system/control-plane.service`）：

```ini
[Unit]
Description=FengDog control plane
After=network.target

[Service]
ExecStart=/opt/control-plane/flygo-control-plane --port 9001
Environment=FENGDOG_SECRET=<强随机密钥，至少 32 字符>
Environment=FENGDOG_ADMIN_TOKEN=<管理口令>
Environment=FENGDOG_DB=/var/lib/control-plane/control-plane.db
Environment=FENGDOG_BIND=127.0.0.1
Restart=always
RestartSec=5
User=controlplane
# 硬化建议
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/control-plane

[Install]
WantedBy=multi-user.target
```

3. **反代 + HTTPS**（nginx 示例片段，Let's Encrypt 证书）：

```nginx
server {
    listen 443 ssl;
    server_name cp.example.com;
    ssl_certificate     /etc/letsencrypt/live/cp.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/cp.example.com/privkey.pem;
    location / {
        proxy_pass http://127.0.0.1:9001;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

4. **运维要点**：
   - 控制面无状态化：SQLite 文件定期备份（`sqlite3 .backup` 或快照），换机迁移 = 拷库 + 换密钥要重新发 token（节点重新激活即可）。
   - 限速在反代层再兜一层（nginx `limit_req`），本服务的限速只防单 IP 滥用。
   - 节点自治设计：控制面挂了，节点本地同步/互备不受影响（心跳失败不惩罚）；恢复后自动补报。
   - `FENGDOG_SECRET` 生成：`openssl rand -hex 32`。密钥轮换会让旧 JWT 全部失效，节点需要重新激活。

## 项目结构

```
src/
  main.rs         入口：环境变量/参数解析、AppState、优雅退出
  db.rs           SQLite 打开 + schema + 全部查询（rusqlite）
  auth.rs         JWT HS256 签发/校验、AuthDevice/AuthAdmin 提取器
  limit.rs        手写固定窗口内存限速器
  api/
    mod.rs        JSON 错误封装 {"ok":false,"message":...} + 路由组装
    device.rs     register / activate / status / heartbeat / renew / cancel
    admin.rs      login / devices / 管理页
  static/
    admin.html    内嵌管理页（include_str!）
  tests.rs        端到端集成测试（临时 SQLite 文件）
```

## 已知简化（MVP 范围外，后续迭代）

- 支付回调接口（激活码由运营后台人工签发）
- ~~互备信令中转（`/v1/pair-invite`，阶段二 W7 引入）~~ → 已落地为 `/api/pair/invite|accept|poll`（只转邀请码与 Syncthing device ID，不碰数据平面）
- 容量计量趋势、心跳时间线图形化
- 换新工单、设备迁移流程（阶段三）
