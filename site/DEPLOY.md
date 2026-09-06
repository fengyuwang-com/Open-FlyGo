# FlyGo 官网部署手册（flygo.fengyuwang.com）

> 部署物只有一个文件：`site/index.html`（单文件静态页，内嵌全部 CSS/JS，无任何外部依赖）。
> 三个方案任选其一；按国内访问速度排序：**方案二（Cloudflare Pages）> 方案三（自有服务器）> 方案一（GitHub Pages）**。

## 0. 部署前检查

- [ ] APK 构建产物已放到站点 `bin/` 目录：下载按钮直链为 `bin/flygo-mobile-1.0.0-20260817-debug.apk`，文件不存在时点击会 404。
- [ ] `index.html` 里的版本号（页脚）与仓库根 `Version` 文件一致。
- [ ] 域名 `fengyuwang.com` 的 DNS 管理权限在手（在哪家注册就在哪家改，或在 Cloudflare 托管统一管理）。

---

## 方案一：GitHub Pages（零成本，配置最少）

适合：先跑通、暂不追求国内速度。注意 GitHub Pages 在国内部分网络访问慢或不稳定。

### 步骤（点击级）

1. 打开仓库 `https://github.com/fengyuwang-com/FlyGo`。
2. 点顶部 **Settings** → 左侧菜单 **Pages**（直达：`https://github.com/fengyuwang-com/FlyGo/settings/pages`）。
3. **Build and deployment → Source** 选 **Deploy from a branch**。
4. **Branch** 下拉选 `main`，右侧文件夹下拉只能选 `/ (root)` 或 `/ (docs)`——都不含 `site/`，所以推荐改用下面的 **Actions 方式**；若不想加 workflow，就手动把 `site/index.html` 复制到 `gh-pages` 分支根目录（本地执行 `git checkout -b gh-pages && cp site/index.html . && git add index.html && git commit -m "site: deploy" && git push origin gh-pages`，然后回到第 3 步把 Branch 选成 `gh-pages` / `(root)`，点 **Save**）。
5. 等待 1~5 分钟，页面顶部出现绿色提示后，访问 `https://fengyuwang-com.github.io/FlyGo`（注意是 `fengyuwang-com`）验证。

### Actions 方式（推荐，main 的 site/ 自动发布）

在仓库 `.github/workflows/pages.yml` 放入：

```yaml
name: deploy-site
on:
  push:
    branches: [main]
    paths: ["site/**"]
permissions:
  contents: read
  pages: write
  id-token: write
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: github-pages
    steps:
      - uses: actions/checkout@v4
      - uses: actions/configure-pages@v5
      - uses: actions/upload-pages-artifact@v3
        with:
          path: site
      - uses: actions/deploy-pages@v4
```

然后 Settings → Pages → Source 选 **GitHub Actions**，push 一次即自动部署。

### 绑定 flygo.fengyuwang.com

1. Settings → Pages → **Custom domain** 输入 `flygo.fengyuwang.com` → 点 **Save**。
2. 到域名 DNS 控制台加记录：

   | 记录类型 | 主机记录 | 记录值 |
   |---|---|---|
   | CNAME | flygo | fengyuwang-com.github.io |

3. 回到 Pages 页等 DNS 校验通过（通常几分钟到 1 小时），勾选 **Enforce HTTPS**（等 Let's Encrypt 证书签发完成再勾，否则会报错）。

---

## 方案二：Cloudflare Pages（推荐，国内可达性较好，免服务器）

适合：正式对外。自动 HTTPS、免 CDN 配置、push 即部署。

### 步骤（点击级）

1. 打开 `https://dash.cloudflare.com`，注册 / 登录。
2. 左侧 **Workers & Pages** → **Create** → 顶部切到 **Pages** 标签 → **Connect to Git**。
3. 授权 GitHub，选择仓库 `fengyuwang-com/FlyGo` → **Begin setup**。
4. 填写：
   - **Project name**：`flygo`（决定默认域名 `flygo.pages.dev`）；
   - **Production branch**：`main`；
   - **Framework preset**：`None`；
   - **Build command**：留空；
   - **Build output directory**：`site`。
5. 点 **Save and Deploy**，约 1 分钟后访问 `https://flygo.pages.dev` 验证。

### 绑定 flygo.fengyuwang.com

1. 进入 Pages 项目 `flygo` → **Custom domains** → **Set up a custom domain**。
2. 输入 `flygo.fengyuwang.com` → **Continue** → **Activate domain**。
3. DNS 两种情况：
   - 域名 DNS 已托管在 Cloudflare：自动添加 CNAME 记录，无需手动操作；
   - 域名在别处（阿里云/腾讯云等）：手动加一条

     | 记录类型 | 主机记录 | 记录值 |
     |---|---|---|
     | CNAME | flygo | flygo.pages.dev |

     （若服务商不允许裸域加 CNAME，保持根域 `@` 用 A 记录指向源站，仅子域走 CNAME 即可。）
4. 等状态变为 **Active**（证书自动签发，通常几分钟），HTTPS 自动开启。

---

## 方案三：任意静态托管 + 手动 DNS（自有 VPS / NAS）

适合：已有服务器，数据与线路完全自控。

### 步骤（以 Nginx + Ubuntu 为例）

1. 上传文件到服务器：
   ```bash
   ssh user@your-server
   sudo mkdir -p /var/www/flygo
   # 本地执行（Windows Git Bash）：
   scp site/index.html user@your-server:/var/www/flygo/
   scp bin/flygo-mobile-1.0.0-20260817-debug.apk user@your-server:/var/www/flygo/bin/
   ```
2. 新建站点配置 `/etc/nginx/sites-available/flygo.conf`：
   ```nginx
   server {
       listen 80;
       server_name flygo.fengyuwang.com;
       root /var/www/flygo;
       index index.html;
   }
   ```
3. 启用并重载：`sudo ln -s /etc/nginx/sites-available/flygo.conf /etc/nginx/sites-enabled/ && sudo nginx -t && sudo systemctl reload nginx`。
4. DNS 控制台加记录（二选一）：

   | 场景 | 记录类型 | 主机记录 | 记录值 |
   |---|---|---|---|
   | 直连源站 | A | flygo | 服务器公网 IPv4（如 `123.45.67.89`） |
   | 走 CDN / 对象存储 | CNAME | flygo | CDN 或托管商给的域名 |

   只用子域 `flygo` 时根域 `@` 不用动；**A 记录必须是公网 IP，内网 / CGNAT 地址无效**（家宽无公网 IP 就选方案二）。
5. HTTPS：`sudo certbot --nginx -d flygo.fengyuwang.com`，按提示自动续期。
6. 家宽 / 内网穿透场景：frp 或 Cloudflare Tunnel 皆可，DNS 仍按上表 CNAME 指向隧道出口域名。

---

## 三方案对比

| | GitHub Pages | Cloudflare Pages | 自有服务器 |
|---|---|---|---|
| 国内速度 | 慢 / 不稳定 | 较好 | 取决于线路 |
| 成本 | 免费 | 免费 | 服务器 + 流量费 |
| HTTPS | 自动（需勾选） | 自动 | certbot 手动 |
| 部署方式 | push 自动 | push 自动 | 手动 scp |
| 推荐场景 | 临时预览 | 正式上线 | 已有基建 |

## 上线验收清单

- [ ] `curl -I https://flygo.fengyuwang.com` 返回 `200`；
- [ ] 手机浏览器打开官网，点「下载 APK」能开始下载（`bin/` 文件已就位）；
- [ ] 系统深色模式下页面自动切换为深色 token，右上角按钮可手动切换并记忆；
- [ ] 页脚版本号与根目录 `Version` 文件一致。
