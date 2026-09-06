# 飞狗控制面 — VPS 部署手册（DEPLOY-VPS）

> 本文档随 M13（admin UI）交付，是控制面 **首次公网 VPS 部署** 的操作手册。
> API 契约见同目录 `API-CONTRACT.md`；本地开发/自测见 `README.md`。
> 全程只装一个二进制 + 一个 SQLite 文件，无其它依赖。

## 0. 部署物清单

| 物 | 说明 |
|---|---|
| `flygo-control-plane` | 发布二进制（Linux 命名；Windows 构建产物为 `flygo-control-plane.exe`，**不部署到 VPS**） |
| `control-plane.db` | SQLite 数据库（首次启动自动创建；后续随备份走） |
| 环境变量 | `FENGDOG_SECRET`（必填，≥16 字符）、`FENGDOG_ADMIN_TOKEN`（管理口令，建议 ≥16 字符）、`FENGDOG_DB`、`FENGDOG_BIND`、`FENGDOG_PORT` |

> 二进制名 = Cargo 包名 `flygo-control-plane`（Cargo.toml 未设 `[[bin]]`，三平台同名）。
> 不要改名为 `control-plane`——systemd/运维脚本全部按 `flygo-control-plane` 写。

## 1. 构建（在构建机，例如开发机 Windows）

```bash
cd control-plane
cargo build --release
# 产物：target/release/flygo-control-plane.exe
```

- 依赖走国内镜像（`C:/FlyGoLegacy/cargo/config.toml` 的 rsproxy 配置）。
- 产物是单文件、strip 过（`[profile.release]` 已开 `opt-level=z / lto / strip`），约几 MB。
- 把 `flygo-control-plane.exe` 拷到 VPS 后改名 `flygo-control-plane`（Linux 无后缀）。

## 2. VPS 目录布局（推荐）

```
/opt/control-plane/
  flygo-control-plane
  control-plane.db          # 首次启动自动生成
  control-plane.log         # stdout 日志（systemd journal 也会收一份）
/var/lib/control-plane/     # 可选：若想隔离数据目录，见下方 systemd 变体
```

## 3. systemd 单元

`/etc/systemd/system/control-plane.service`：

```ini
[Unit]
Description=FlyGo control plane (identity + subscription + heartbeat)
After=network.target

[Service]
# 二进制名是 flygo-control-plane（= Cargo 包名），不是 control-plane
ExecStart=/opt/control-plane/flygo-control-plane --port 9001
Environment=FENGDOG_SECRET=<openssl rand -hex 32 生成，至少 32 字符>
Environment=FENGDOG_ADMIN_TOKEN=<管理口令，至少 16 字符>
Environment=FENGDOG_DB=/opt/control-plane/control-plane.db
Environment=FENGDOG_BIND=127.0.0.1
WorkingDirectory=/opt/control-plane
Restart=always
RestartSec=5
User=controlplane
# 硬化建议
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/opt/control-plane

[Install]
WantedBy=multi-user.target
```

启用：

```bash
sudo useradd --system --home /opt/control-plane --shell /usr/sbin/nologin controlplane
sudo mkdir -p /opt/control-plane
sudo install -m 0755 -o controlplane flygo-control-plane /opt/control-plane/
sudo systemctl daemon-reload
sudo systemctl enable --now control-plane
systemctl status control-plane        # active (running)
```

- **不监听 0.0.0.0**：`FENGDOG_BIND=127.0.0.1`，公网只走 nginx 反代（下一节）。
- 换密钥/换库后 `sudo systemctl restart control-plane`。

## 4. 反代 + HTTPS（nginx + Let's Encrypt）

```nginx
server {
    listen 443 ssl http2;
    server_name cp.example.com;   # 换成控制面域名

    ssl_certificate     /etc/letsencrypt/live/cp.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/cp.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:9001;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # 管理端限速再兜一层（本服务内存限速只防单 IP 滥用）
    limit_req zone=adminlogin burst=5 nodelay;
}
```

证书（certbot）：

```bash
sudo apt install certbot python3-certbot-nginx
sudo certbot --nginx -d cp.example.com    # 自动改配置 + 续期
```

- **管理端必须走 HTTPS**：登录页会提交管理员口令，明文 HTTP 下绝不可用。
- Tailscale 内网走 9001 直连仍可用（见 `README.md` 防火墙规则一节，本服务不终结 TLS）。

## 5. 运维要点

- **备份 = 拷 SQLite 文件**：`sqlite3 control-plane.db ".backup backup-$(date +%F).db"`（或快照）。控制面无状态化设计，换机迁移 = 拷库 + 换密钥（换密钥会让旧 JWT 全失效，节点需重新激活）。
- **密钥生成**：`openssl rand -hex 32`。密钥轮换会使所有旧 JWT 失效——节点重新激活即可，数据平面不受影响。
- **节点自治**：控制面挂掉不影响节点本地同步/互备（心跳失败不惩罚），恢复后自动补报。所以 VPS 宕机不是事故，补上备份即可。
- **日志**：`journalctl -u control-plane -f`；stdout 同时落 `/opt/control-plane/control-plane.log`。
- **升级**：停服 → 覆盖二进制 → 起服（`systemctl restart`）。SQLite schema 是 `CREATE TABLE IF NOT EXISTS` 幂等迁移，旧库直接可用。

## 6. 部署后验收（管理台全流程）

```bash
# 1. 服务存活
curl -s -o /dev/null -w "%{http_code}\n" https://cp.example.com/          # 200，返回管理页 HTML

# 2. 登录换管理 JWT
curl -s -X POST https://cp.example.com/api/admin/login \
  -H 'Content-Type: application/json' -d '{"token":"<你的 FENGDOG_ADMIN_TOKEN>"}'
#    -> {"ok":true,"token":"...","expires_in_secs":43200}

# 3. 签发一个 pro 激活码
curl -s -X POST https://cp.example.com/api/admin/codes \
  -H 'Content-Type: application/json' -H 'Authorization: Bearer <管理 JWT>' \
  -d '{"kind":"pro"}'
#    -> {"ok":true,"code":{"code":"XXXXXXX8","kind":"pro","status":"issued",...}}

# 4. 浏览器打开 https://cp.example.com/ →
#    输入管理口令登录 → 设备表可见 → 激活码面板可签发/作废
# 5. 真机 APK：控制面地址填 https://cp.example.com（HTTPS 公网）走注册/激活/心跳
```

## 7. FAQ

- **管理页 503 / 登录不可用**：`FENGDOG_ADMIN_TOKEN` 未设置。登录接口会返回
  `admin token not configured (set FENGDOG_ADMIN_TOKEN)`——这是设计（默认关闭管理端）。
- **不带 token 也能看到登录页？** 是。登录页是公开静态页；所有管理数据接口（devices/codes）都要
  管理 JWT 或原始 token，页面本身不含任何敏感数据。
- **需要开放 9001 吗？** 公网不需要（走 443 反代）。Tailscale/局域网直连才用 9001，
  且要配防火墙白名单（`README.md` 有规则示例）。