# PlainApp 手机网页发射问题调查报告

> 日期：2026-08-16 · 性质：纯调研（未改动 FlyGo 任何代码）
> 研究目标：PlainApp「发射 / Web 服务器」功能在安卓 16 手机上无法使用的问题定位、修复方案、兜底方案、与 FlyGo 结合分析。
> 源码版本：plainhub/plain-app（原 ismartcoding/plain-app，仓库已改名）main 分支，版本号 v3.3.2（2026-08-15 发布）。
> 本地源码副本：`~/AppData/Local/Temp/plainapp-research/plain-app`

---

## 0. TL;DR（先看这个）

1. **PlainApp 的服务器本身没问题**：Ktor/Netty 内嵌服务器，绑定 `0.0.0.0:8080`（HTTP）+ `0.0.0.0:8443`（HTTPS 自签证书），权限齐全，targetSdk=36（就是安卓 16），最新版 v3.3.2 昨天刚发，**"安卓 16 太新"不是根因**。
2. **最大嫌疑是网络层**："发射出去了但外面看不到" 的典型原因是——手机连着 WiFi 但路由器开了 **AP 隔离/访客网络**、手机和电脑**不在同一网段**、手机在**纯流量/热点**下、或开了 **VPN**。服务器其实在跑，但其他设备到不了手机的 IP。
3. **第二嫌疑是国产 ROM 杀后台 + 权限**：通知权限没给、电池优化没设"无限制"、自启动被禁 → 前台服务起不来或刚起就被杀。代码里甚至有专门针对 ColorOS/OnePlus 的 workaround，说明这是已知高发问题。
4. **60 秒自诊断法**（电脑上执行）：
   - `curl http://<手机IP>:8080/health` 返回包名 `com.ismartcoding.plain` → 服务器在跑、网络通；问题在 Web UI 层（见 §6.6）
   - 超时/拒连 → 网络不通（§6.1）或服务没起来（§6.2/6.3/6.4）
5. **底线方案（今天 100% 可落地）**：装官方 Tailscale App + Termux 里 `python -m http.server 8000`，电脑访问 `http://100.x.y.z:8000`。全程 30 分钟，见 §8。

---

## 1. 研究材料与方法

| 项 | 内容 |
|---|---|
| 仓库 | `plainhub/plain-app`（原 `ismartcoding/plain-app`，GitHub 已改名，6.5k stars，2026-08-16 仍在更新） |
| 语言/架构 | Kotlin Multiplatform（Android + iOS），Compose UI；Android 端为 `app` + `shared` 模块 |
| 版本 | v3.3.2（2026-08-15 发布），main 分支与 release 同步 |
| 获取方式 | `gh api` 下载 tarball（43MB），未走 git clone（ghfast.top 镜像不稳定，API 直连快） |
| 构建配置 | Gradle 9.7.0，AGP 最新，targetSdk=36，compileSdk=37，minSdk=28 |

---

## 2. 「发射 / Web 服务器」功能的技术实现

### 2.1 服务器本体

- **库**：Ktor Server + **Netty** 引擎（`io.ktor.server.netty.Netty`），内嵌进 App 进程。
- **绑定地址**：`connector { port = httpPort }` **未指定 host**，Ktor 默认绑定 **`0.0.0.0`（所有网卡接口）**——包括 WiFi、热点、USB 共享、Tailscale 接口。
  - 关键文件：`shared/src/androidMain/kotlin/com/ismartcoding/plain/httpserver/HttpServerLifecycle.kt` 的 `createHttpServerAsync()`（第 109 行）。
- **端口**：HTTP **8080**、HTTPS **8443**（`TempData.kt` 第 14 行，可在 App 内改）。
- **HTTPS**：运行时用 BouncyCastle 生成自签 BKS 密钥库（P-256 ECDSA），`sslConnector` 起 8443；Web UI 里会显示证书指纹供核验。
- **Web UI**：Vue SPA，从 classpath `web/` 静态资源提供，`/` 即管理界面。
- **mDNS**：`NsdHelper` 注册 `plainapp.local`，方便局域网免 IP 访问；同时 App 界面列出手机全部 IPv4。

### 2.2 启动编排（Android 端）

```
用户点「发射」开关
  └─ MainViewModel.enableHttpServer(true)
      ├─ 检查 POST_NOTIFICATIONS 权限 → 未授予则弹窗引导授权，等 App 回前台才继续
      └─ StartHttpServerEvent
          └─ startHttpServerService() → ContextCompat.startForegroundService(HttpServerService)
              └─ HttpServerService (前台服务, specialUse|dataSync)
                  ├─ onCreate: 建通知渠道、mDNS 注册、加锁（HttpServerLockManager）
                  └─ ON_START: startHttpServerAsync() 编排器
                      ├─ 停旧实例 → 检查端口占用 → 最多重试 2 次
                      ├─ startHttpEngineAsync() → Ktor/Netty bind 0.0.0.0:8080 + 8443
                      ├─ checkHttpServerAsync()：curl http://127.0.0.1:8080/health 探活
                      ├─ 成功 → 注册 mDNS、起 PeerStatus、更新通知栏（显示访问地址）
                      └─ 失败 → 状态 ERROR，错误信息上屏（端口冲突/启动失败/健康检查失败）
```

关键文件：
- `shared/src/androidMain/kotlin/com/ismartcoding/plain/services/HttpServerService.kt` — 前台服务生命周期
- `shared/src/androidMain/kotlin/com/ismartcoding/plain/platform/HttpServer.android.kt` — 平台入口
- `shared/src/commonMain/kotlin/com/ismartcoding/plain/platform/HttpServer.kt` — 共享编排逻辑（第 140 行 `startHttpServerAsync`）

### 2.3 访问控制（重要，防呆点）

- 路由层有 `desktopAccessEnabled` 门禁：**默认 true**（`Preferences.kt` 第 213 行）。
- 若为 false，除 `peer`/DLNA/`/health` 等白名单路由外，**一切请求返回 404**（`HttpModule.kt` 第 84-99 行拦截器）——包括整个 Web UI。
- 主页的「发射」卡片下方有独立的「**Desktop access**」开关（`DesktopAccessSection.kt`），默认开，但若被关过，网站会表现为"打不开"。

### 2.4 通知栏与地址展示

- 服务运行通知内容：`http://plainapp.local:8080` + `https://plainapp.local:8443`（mDNS 主机名）。
- 主页 `WebAddressBar` 显示：`http://plainapp.local:8080` + 每个实际 IP（`http://192.168.x.x:8080`），每个带二维码。

---

## 3. AndroidManifest 权限清单（与发射功能相关部分）

`app/src/main/AndroidManifest.xml`（逐条核对过）：

| 项 | 状态 | 说明 |
|---|---|---|
| `INTERNET` | ✅ | 网络必需 |
| `FOREGROUND_SERVICE` | ✅ | 起前台服务必需 |
| `FOREGROUND_SERVICE_SPECIAL_USE` | ✅ | specialUse 类型运行时权限（API 34+ 必需） |
| `FOREGROUND_SERVICE_DATA_SYNC` | ✅ | dataSync 类型运行时权限（API 34+ 必需） |
| `POST_NOTIFICATIONS` | ✅ | API 33+ 通知运行时权限，前台服务通知依赖 |
| `usesCleartextTraffic="true"` | ✅ | 明文 HTTP 放行（无 networkSecurityConfig 拦截） |
| `NEARBY_WIFI_DEVICES`（neverForLocation） | ✅ | API 33+ 附近 WiFi 设备 |
| `ACCESS_WIFI_STATE` / `CHANGE_WIFI_STATE` | ✅ | WiFi 信息 |
| `WAKE_LOCK` / `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` | ✅ | 保活相关 |
| `HttpServerService` 声明 | ✅ | `foregroundServiceType="specialUse\|dataSync"` + `PROPERTY_SPECIAL_USE_FGS_SUBTYPE="web server"`（Android 15 强制要求，已满足） |
| ⚠️ `android:stopWithTask="true"` | ⚠️ | **从最近任务划掉 App 即停止服务**（onTaskRemoved 里 stopSelf + 关端口），国产 ROM 一键清理会连坐 |

**结论：权限声明完整，符合安卓 14/15/16 全部前台服务规范，无需补权限。**

---

## 4. 安卓 16（API 36）兼容性分析

| 检查项 | 结论 |
|---|---|
| targetSdk | **36**（= 安卓 16），compileSdk 37，minSdk 28。官方已全面适配最新系统 |
| 前台服务类型 | specialUse 优先、dataSync 兜底，双权限都在；Android 15 的 `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` 要求已满足 |
| dataSync 6 小时/24 小时限制（API 35+） | App 用 **specialUse 优先**（无时长限制），只有 fallback 才用 dataSync → 已规避 |
| 16 KB 内存页（安卓 15/16 设备要求） | App 无内置 .so 原生库 → 不受影响 |
| 安卓 16 行为变更 | 无针对内嵌 HTTP 服务器/前台服务的新致命限制（16 新增的 FGS 类型仅 Wear OS 心率类） |
| 已知 issue | GitHub issues 中无"安卓 16 无法发射"的报告；v3.3.2（2026-08-15）即昨天发布，社区活跃，若有系统级问题早已爆出 |

**结论：若用户安装的是 v3.3.2 最新版，"安卓 16 太新"不是根因。若装的是旧版 APK（几周/几个月前的），才需要考虑旧版 bug（例如早期版本缺 FGS 类型权限声明，在安卓 14+ 上会直接起不来）。**

---

## 5. 根因清单（按可能性排序）

> 每项给出：判断方法（如何确认是不是它）+ 对应验证动作。**先做 §0 的 60 秒自诊断，能砍掉一半候选。**

### 5.1 网络层不可达（最大嫌疑，症状完全吻合）

服务器发射成功（手机端显示 ON、通知栏有地址），但电脑访问不到 → 用户感知"没有网站能看到"。

- **AP 隔离 / 访客网络**：路由器开了"AP 隔离/无线隔离/访客网络"（小米、TP-Link、华为路由器都有此功能），同 WiFi 下设备互相隔离。国内家庭路由器很常见。
- **不在同一网段**：电脑连 192.168.1.x，手机连 192.168.0.x（双频分开的两个 SSID 或二级路由）。
- **手机在纯流量/热点下**：手机只连蜂窝数据时 `getDeviceIP4()` 拿到的是运营商 NAT 内网地址（10.x），外部设备不可达；手机开热点给电脑时同理（手机热点有客户端隔离）。
- **VPN 开启**：App 检测到 VPN 会在主页显示警告横幅（`vpn_web_conflict_warning`），且 IP 选择逻辑会过滤 tun 接口。

**判断**：电脑 `ping <手机IP>` 不通 / `curl` 超时；手机浏览器访问 `http://127.0.0.1:8080/` 却正常。
**验证**：手机 WiFi 详情看 IP；进路由器管理页看"无线设置→AP 隔离"；电脑手机都连主路由同一个 2.4G/5G SSID。

### 5.2 国产 ROM 杀后台 / 权限未给（第二嫌疑）

手机厂商（小米 HyperOS、OPPO/ColorOS、vivo OriginOS、荣耀 MagicOS 等）默认激进杀后台：

- 前台服务没被授予"电池无限制" → 锁屏/后台几分钟后被杀，服务停了，端口释放，外部自然访问不到。
- **代码实锤**：`HttpServerService.kt` 第 62-70 行有专门注释——ColorOS/OnePlus 会 START_STICKY 反复杀/重启服务导致端口占用死循环，为此加了"重启等 5 秒再绑端口"。说明 OEM 杀服务是官方已知的普遍问题。
- **一键清理/省电模式**会把服务连同通知一起清掉。

**判断**：手机锁屏几分钟后，通知栏的服务常驻通知消失。
**验证**：按 §7.2 清单逐项授权（电池无限制 + 自启动 + 后台活动 + 通知）。

### 5.3 通知权限未授予导致启动流程卡住

启动编排里：`enableHttpServer(true)` 检查 `POST_NOTIFICATIONS`，**未授予时弹确认框 → 请求授权 → 等 App 回前台才真正发启动事件**（`MainViewModel.kt` 第 50-66 行）。若用户一直没回 App 或反复跳过，服务永远不启动。

**判断**：点"发射"后手机没有出现服务常驻通知，主页状态停在 STARTING 很久。
**验证**：设置 → 应用 → PlainApp → 通知 → 允许。

### 5.4 端口 8080 被占用

部分定制 ROM / 运营商预装应用会占用 8080。App 有端口冲突检测：状态显示 ERROR，错误信息提示端口被占，并提供"一键换端口重启"按钮（`HomePage.kt` 第 112-128 行：随机换端口 + 重启 App）。

**判断**：主页服务卡片显示 ERROR + "port 8080 in use" 类文案。
**验证**：点"重启修复"按钮换端口；或在手机浏览器访问 `http://127.0.0.1:8080/health` 看是否返回别的应用内容。

### 5.5 安装的是旧版本 APK

Play 商店渠道 / 几周前的 APK 可能存在安卓 14+ FGS 权限缺失等旧 bug；当前 v3.3.2（2026-08-15）已修复。

**判断**：设置 → 关于，版本号 < 3.3.2。
**验证**：从 GitHub Releases 下载 `PlainApp-3.3.2-64bit-Universal-Recommended.apk`（国内加速：`https://ghfast.top/https://github.com/plainhub/plain-app/releases/download/v3.3.2/PlainApp-3.3.2-64bit-Universal-Recommended.apk`）。

### 5.6 Desktop access 开关被关闭（全站 404）

默认 true，但若被关过：服务器正常跑、`/health` 正常，但 `/` 及所有 Web UI 请求 404 → "打开网址但什么也没有"。

**判断**：`curl http://<手机IP>:8080/health` 返回 `com.ismartcoding.plain`，但 `curl http://<手机IP>:8080/` 返回 404。
**验证**：主页「发射」卡片下方打开「Desktop access」开关。

### 5.7 mDNS 解析失败（plainapp.local 打不开）

通知栏地址是 `http://plainapp.local:8080`。Windows 10/11 的 mDNS 解析（用于网络发现的 Function Discovery 服务）时好时坏，Chrome 也不保证解析 `.local` 域名。

**判断**：`ping plainapp.local` 不通，但用 IP 直连正常。
**验证**：改用主页显示的 IP 直链 `http://192.168.x.x:8080`（有二维码，扫码即达）。

### 5.8 纯系统级安卓 16 问题（可能性低，已排除）

见 §4。官方 targetSdk 36 + 昨天刚发版，系统级障碍不存在。

---

## 6. 修复方案

### 6.1 问题在代码里怎么办（改动点 + 自建 APK 可行性）

> 仅当按 §7.2 清单全部授权后仍复现，且确认是代码行为时才需要。多数情况下不需要走到这一步。

| 需求 | 改哪个文件 | 怎么改 | 影响 |
|---|---|---|---|
| 划掉最近任务不杀服务 | `app/src/main/AndroidManifest.xml` 第 183 行 | `android:stopWithTask="true"` → `"false"` | 服务不被划掉连坐；代价是服务更常驻 |
| 一键清理连坐保护 | `shared/src/androidMain/.../services/HttpServerService.kt` `onTaskRemoved()`（第 139 行） | 不 `stopSelf()`，仅重置状态 | 同上 |
| 通知栏直接显示 IP 直链（防 mDNS 失效） | `shared/src/commonMain/.../httpserver/HttpServerManager.kt` `getNotificationContent()`（第 153 行） | 拼接 `getDeviceIP4()` 的 IP 地址进通知文案 | 用户一眼拿到可点地址 |
| 启动时联动提示 Desktop access | `shared/src/commonMain/.../ui/models/MainViewModel.kt` `enableHttpServer()` | 若 `desktopAccessEnabled=false` 先弹提示 | 避免"全站 404"困惑 |
| 错误信息更显眼（失败时给 IP+原因） | `shared/src/commonMain/.../platform/HttpServer.kt` 第 193-203 行 | 错误文案附上 `httpServerError` 与建议动作 | 可自诊断 |

**自建 APK 可行性：高。**

- 构建链路现成：`build-apk.sh` → `./gradlew :app:assembleGithubRelease`；GitHub Actions（`.github/workflows/release.yml`）已在跑。
- 本机条件：JDK 25 已装（建议另装 JDK 17/21 最稳，AGP 官方支持范围）；**缺 Android SDK**（无 ANDROID_HOME），需装 cmdline-tools + platform-37 + build-tools（约 1-2GB，走国内镜像：`https://mirrors.cloud.tencent.com/AndroidSDK/` 或阿里镜像）。
- 签名：`assembleGithubRelease` 需要 `keystore.properties`（release 密钥）；**本地自用可编 debug 变体（`./gradlew :app:assembleFdroidDebug`），免签名直接 adb 安装**；要发给用户装，用 debug 默认密钥签名的 APK 即可（`~/.android/debug.keystore`）。
- 首次构建时间：下载 Gradle 9.7 + 依赖约 30-60 分钟（建议配国内 Maven 镜像：阿里云 `maven.aliyun.com`），之后增量构建几分钟。
- 风险点：AGP 与 JDK 25 兼容性、Compose 依赖下载体积大。整体**今天可完成**，但作为兜底中的兜底。

### 6.2 权限 / 设置问题 → 手机逐项检查清单（安卓 16 路径）

按顺序过一遍，每项 10 秒：

1. **升级到最新版**：设置 → 关于 → 版本号 ≥ 3.3.2。不是就装 v3.3.2（下载见 §5.5）。
2. **通知权限**：设置 → 应用 → PlainApp → 通知 → 允许所有通知。（不授权 = 服务可能起不来，§5.3）
3. **电池无限制**：设置 → 应用 → PlainApp → 电池 → **无限制**（小米/OPPO 叫"无限制"，华为叫"不允许电池优化"）。这是国产 ROM 上最关键的一步。
4. **自启动/后台活动**：设置 → 应用 → PlainApp → 自启动（允许）；部分 ROM 另有"后台运行"开关一并打开。
5. **WiFi 环境**：确认手机**连着家庭主路由的 WiFi**（不是访客网络、不是热点、不是纯流量）；WiFi 详情里记下 IP（如 192.168.1.100）。
6. **路由器 AP 隔离**：进路由器后台（192.168.1.1 之类）→ 无线设置 → 关闭"AP 隔离/无线隔离/访客网络隔离"。
7. **电脑同网段**：电脑 IP 与手机 IP 前三位一致（192.168.1.x vs 192.168.1.x）。不一致 → 让两台设备连同一个路由器的同一个 SSID。
8. **VPN 检查**：手机关闭 VPN/加速器（App 主页有黄色 VPN 警告横幅时尤其注意）。
9. **服务调试页**：PlainApp → 设置 → 服务调试（Service Debug），看 HTTP Server 卡片：`service running`、端口、错误信息（有错误就是 §5.4/启动失败，截图留档）。
10. **手机本地自测**：手机浏览器开 `http://127.0.0.1:8080/`。
    - 手机本地能开 + 电脑开不了 → **网络问题**（回到 5/6/7）
    - 手机本地都开不了 → 服务没起来，看第 9 步的错误信息，或换端口（主页错误卡片上的"重启修复"按钮）
11. **电脑验证**：`curl http://<手机IP>:8080/health` → 应返回 `com.ismartcoding.plain`；再开 `http://<手机IP>:8080/`。
12. **锁屏保活测试**：开服务 → 锁屏 10 分钟 → 再 curl。若断了 → 第 3/4 步没生效或 ROM 仍杀，上 Termux 兜底或考虑改代码（§6.1 第一二行）。

---

## 7. 兜底方案：Termux 手机网页发射（今天 30 分钟可落地，100% 成功）

> 原理：Termux 是安卓上的 Linux 终端，`python -m http.server` 绑 0.0.0.0，配合官方 Tailscale App 打通网络。不依赖任何 App 的前台服务保活策略（Termux 保活方式见 7.4）。

### 7.1 安装

```bash
# 1. 安装 Termux（任选）
#    - F-Droid 官方: https://f-droid.org/packages/com.termux/ （推荐，更新最快）
#    - 或酷安搜 Termux
# 2. 打开 Termux，初始化
pkg update -y && pkg upgrade -y

# 3. 装 Web 服务器（任选其一，都装也行）
pkg install -y python          # Python http.server（零配置）
pkg install -y busybox         # busybox httpd（极轻量）
pkg install -y caddy           # Caddy（支持 HTTP/2、反代、自动 TLS，Termux 官方仓库有）

# 4. 保活工具
pkg install -y termux-api      # termux-wake-lock 等
```

### 7.2 启动命令（三选一）

```bash
# 方案 A: Python 单行发射（绑 0.0.0.0，端口 8000，根目录为手机内部存储）
python -m http.server 8000 --bind 0.0.0.0 --directory /sdcard

# 方案 B: busybox httpd（更省电，支持 CGI；目录同上）
busybox httpd -f -p 8000 -h /sdcard

# 方案 C: Caddy（静态站点 + 文件服务 + 反代一体，适合长期用）
mkdir -p ~/web && echo '<h1>FlyGo phone node</h1>' > ~/web/index.html
cat > ~/Caddyfile <<'EOF'
:8000 {
    root * /sdcard
    file_server browse
    encode gzip
}
EOF
caddy run --config ~/Caddyfile
```

### 7.3 访问地址

| 场景 | 地址 |
|---|---|
| 同一 WiFi（电脑/另一台手机） | `http://<手机WiFiIP>:8000`（如 `http://192.168.1.100:8000`） |
| 跨网（Tailscale，推荐） | `http://<手机TailscaleIP>:8000`（如 `http://100.101.102.103:8000`） |
| 手机自测 | `http://127.0.0.1:8000` |

**Tailscale 打通（关键）**：手机装**官方 Tailscale App**（Play 商店或 GitHub releases，登录同一账号/同一 tailnet），电脑已有 Tailscale。两边都上线后，电脑直接访问手机 `100.x` 地址——**无视 WiFi 隔离、无视网段、跨运营商都可达**。Termux 仓库里没有 tailscale 包（已核实 404），所以用官方 App，Tailscale 在安卓上通过 VPN 接口提供 100.x 网卡，Termux 里绑 0.0.0.0 的服务器天然可达。

### 7.4 保活（锁屏/后台不中断）

```bash
# 1. 锁屏保持 CPU 唤醒（Termux 内）
termux-wake-lock

# 2. 关掉 Termux 的系统省电优化
#    设置 → 应用 → Termux → 电池 → 无限制；自启动 → 允许

# 3. 开机自启（可选，需 F-Droid 装 Termux:Boot 插件）
mkdir -p ~/.termux/boot
cat > ~/.termux/boot/start-web.sh <<'EOF'
#!/data/data/com.termux/files/usr/bin/bash
termux-wake-lock
python -m http.server 8000 --bind 0.0.0.0 --directory /sdcard
EOF
chmod +x ~/.termux/boot/start-web.sh
```

> 注意：`python -m http.server` 是单进程阻塞式，Termux:Boot 脚本里用 `nohup ... &` 或把整行放前台（Termux:Boot 会保持该会话）皆可。长期使用建议换 Caddy（自带日志、断线自动恢复行为更稳）。

### 7.5 与 FlyGo 面板如何链接

- 面板引导页（W4 规划）放两样东西：
  1. 手机服务二维码/链接：`http://<手机IP>:8000` 或 Tailscale 域名 `http://<手机主机名>:8000`（Tailscale 支持 MagicDNS 主机名）。
  2. 一键检测按钮：面板 `/api/health/services` 卡片墙加"手机 Web"条目（HTTP 探活 `http://100.x:8000/`，三色状态与现有服务墙一致）。
- 验证口令：面板显示手机服务绿色 = 发射成功，闭环。

---

## 8. 与 FlyGo 结合分析（手机↔电脑互联统一入口）

### 8.1 双向服务拓扑

```
电脑 (Windows, 面板 8388)                    手机 (安卓 16)
┌──────────────────────────┐   Tailscale 100.x  ┌──────────────────────────┐
│ FlyGo 面板 :8388  ←──────┼────────────────────┼──→ PlainApp :8080/:8443  │
│  (电脑发射,手机访问)       │        /           │    或 Termux :8000        │
│  引导页 + 服务卡片墙       │   局域网 192.168.x  │  (手机发射,电脑访问)      │
│  Tailscale 100.x         │                    │  Tailscale 100.x         │
└──────────────────────────┘                    └──────────────────────────┘
```

- **电脑发服务器**：面板 8388（既有）。手机访问：同 WiFi 直连 `http://<电脑IP>:8388`，或 Tailscale `http://<电脑100.x>:8388`。防火墙规则 `FlyGo-LAN-*`/`FlyGo-TS-*` 已覆盖。
- **手机发服务器**：PlainApp（8080/8443）或 Termux（8000）。电脑访问：`http://<手机IP>:8080` 或 `http://<手机100.x>:8080`。
- **统一入口 = Tailscale + MagicDNS**：两边都登录同一 tailnet 后，用主机名互访（`http://phone-name:8080`、`http://pc-name:8388`），无 IP 记忆负担，跨网段跨运营商通吃。PlainApp 绑 0.0.0.0，Tailscale 接口同样可达，无需任何端口映射。

### 8.2 面板侧落地建议（W4 引导页，不在本次改动范围）

1. **设备卡片墙扩展**：`/api/health/services` 增加"手机 Web"探活条目（URL 可配置，默认 `http://<手机100.x>:8080`），三色状态一眼可读——沿用 HomeDash 精华的既有模式。
2. **引导页（手机汇聚页）**：放 PlainApp/Termux 安装指引 + 各自二维码（PlainApp 主页自带二维码；Termux 需面板生成 `http://<手机IP>:8000` 的二维码）。
3. **配对体验**：手机端 Tailscale 登录后，MagicDNS 主机名可作为手机服务在面板上的注册名，手机重装/换 IP 不失效。
4. **安全**：手机 Web 服务如对外开放，建议 PlainApp 走 HTTPS（8443 自签）或 Termux 端加密码（Caddy `basic_auth`）；面板访问控制维持"仅 Tailscale + 本机"默认。

### 8.3 长期取舍

- **PlainApp 路径**：功能全（文件/媒体/短信/通话），但保活依赖 ROM 配合（§5.2），且它的定位是"管理手机"而非"给 FlyGo 当节点"。
- **Termux 路径**：极简可控，与 FlyGo"Unix 哲学/拼积木"契合——Termux 是手机上的通用积木（可跑 syncthing、sshd、caddy 反代），保活方式明确（Termux:Boot + wake-lock），**建议 FlyGo 引导页主推 Termux**，PlainApp 作为"要完整手机管理功能"用户的可选项。
- 手机端做 FlyGo 真节点的方向仍按 AGENTS.md 结论：手机降级为汇聚客户端，不做完整节点。

---

## 9. 底线目标：今天可落地的执行顺序

| 步骤 | 动作 | 耗时 | 成败判定 |
|---|---|---|---|
| 1 | 手机浏览器开 `http://127.0.0.1:8080/`（PlainApp 已发射状态下） | 1 分钟 | 开不了 → 服务没起来，走 §7.2 清单；能开 → 网络问题，走步骤 2 |
| 2 | 电脑 `curl http://<手机IP>:8080/health` | 1 分钟 | 返回 `com.ismartcoding.plain` → 服务器与网络 OK，检查 Desktop access 开关（§5.6）与 mDNS（§5.7）；超时 → 走步骤 3 |
| 3 | 手机设置：通知权限 + 电池无限制 + 自启动 + 关 VPN；确认同 WiFi、关路由器 AP 隔离 | 10 分钟 | 电脑再 curl，通了 → 收工 |
| 4 | 若仍不通：手机装官方 Tailscale App 登录；电脑 Tailscale 访问 `http://<手机100.x>:8080` | 10 分钟 | 通了 → 收工（跨网方案成立） |
| 5 | 若 Tailscale 也访问不到：上 Termux 兜底（§7 全流程） | 30 分钟 | `http://<手机100.x>:8000` 出页面 → **底线达成** |

**验收标准**：电脑浏览器（局域网 `192.168.1.x` 或 Tailscale `100.x`）打开手机发出的网页，看到内容。步骤 1-4 走完大概率已达成；步骤 5 保证 100% 达成。

---

## 10. 附录：关键代码位置索引（供后续改动定位）

| 文件（相对仓库根） | 内容 |
|---|---|
| `app/src/main/AndroidManifest.xml` :179-187 | HttpServerService 声明（specialUse\|dataSync、stopWithTask） |
| `app/build.gradle.kts` :22-26, 37-38 | compileSdk 37 / targetSdk 36 / versionName 3.3.2 |
| `shared/src/androidMain/kotlin/com/ismartcoding/plain/httpserver/HttpServerLifecycle.kt` :109-140 | Ktor/Netty 建服（0.0.0.0、8080/8443、BKS 自签） |
| `shared/src/androidMain/kotlin/com/ismartcoding/plain/httpserver/HttpModule.kt` :84-99 | desktopAccess 门禁拦截器（404） |
| `shared/src/androidMain/kotlin/com/ismartcoding/plain/platform/HttpServer.android.kt` :37-52, 78-96 | 引擎启停 / startForegroundService 入口 |
| `shared/src/androidMain/kotlin/com/ismartcoding/plain/services/HttpServerService.kt` :90-133, 139-152 | FGS 启动（specialUse→dataSync→兜底）/ onTaskRemoved 自杀 |
| `shared/src/commonMain/kotlin/com/ismartcoding/plain/platform/HttpServer.kt` :140-227 | 启动编排（重试、端口冲突、健康检查、错误文案） |
| `shared/src/commonMain/kotlin/com/ismartcoding/plain/TempData.kt` :14-15, 59-62 | httpPort 8080 / canDesktopAccess |
| `shared/src/commonMain/kotlin/com/ismartcoding/plain/preferences/Preferences.kt` :203-219 | Service/DesktopAccess 偏好（默认 true） |
| `shared/src/androidMain/kotlin/com/ismartcoding/plain/lib/helpers/NetworkHelper.kt` :21-48 | getDeviceIP4（wlan0 优先、过滤 VPN 接口） |
| `shared/src/commonMain/kotlin/com/ismartcoding/plain/httpserver/HttpServerManager.kt` :153-156 | 通知内容（mDNS 地址） |
| `shared/src/commonMain/kotlin/com/ismartcoding/plain/ui/page/home/HomePage.kt` :99-128 | 发射开关 + 端口冲突一键换端口 |
| `shared/src/commonMain/kotlin/com/ismartcoding/plain/ui/page/home/DesktopAccessSection.kt` :75-107 | Desktop access 开关 + 地址栏 |
| `shared/src/commonMain/kotlin/com/ismartcoding/plain/ui/page/settings/ServiceDebugPage.kt` | 服务调试页（诊断入口） |

---

*报告完。研究过程未修改 FlyGo 代码；PlainApp 源码仅作只读分析（副本在临时目录）。*
