# PlainApp fork 改造 FlyGo 客户端 — 环境研究与可行性分析报告

> 日期：2026-08-16 · 性质：纯调研（只读检查，未装任何东西、未改任何代码、未提交 git）
> 源码版本：PlainApp **v3.3.2**（仓库 `ismartcoding/plain-app`，现已更名 `plainhub/plain-app`；两个地址的 v3.3.2 tag 内容一致）
> 源码获取方式：**GitHub 网页/raw 直连分析**（`C:\FengProj\PlainApp` 的并行克隆尚未完成，为避免抢克隆操作未使用本地目录；另有本地副本 `C:\Users\<user>\AppData\Local\Temp\plainapp-research\plain-app` 可参考）

---

## 一、本机安卓构建环境侦查（实测）

| 项目 | 状态 | 详细 |
|---|---|---|
| Java | ✅ 有，但版本偏高 | `java -version` = **OpenJDK 25.0.1 LTS**（Oracle，位于 `C:\Program Files\Java\jdk-25`）；`javac 25.0.1` 可用。**JAVA_HOME 未设置**（空）。无 Adoptium / Android Studio jbr / FengNAS 下的 JDK。路径入口：`C:\Program Files\Common Files\Oracle\Java\javapath` |
| Gradle | ❌ 无 | `gradle` 命令不存在；`C:\Users\<user>\.gradle` **不存在**（无 wrapper 分发缓存、无依赖缓存，一切从零下载）。仓库自带 `gradlew`/`gradlew.bat`（wrapper 指向 **Gradle 9.7.0**），首次构建会自动下载 ~140MB 分发包 |
| Android SDK | ❌ 完全没有 | `ANDROID_HOME` / `ANDROID_SDK_ROOT` 均未设置；`C:\Users\<user>\AppData\Local\Android\Sdk`、`C:\Android\sdk` 均不存在；**D 盘不存在**。无 cmdline-tools、无 platform-tools/adb、无任何 platforms/build-tools |
| Kotlin / IDE | ❌ 无 | 无独立 `kotlinc`；Android Studio 未安装（`C:\Program Files\Android\` 存在但为空目录）。编译不依赖 AS，但缺 SDK 组件需用 cmdline-tools 的 sdkmanager 补 |
| 磁盘 | ✅ 充足 | C 盘 476G 总量，**剩余 109G**，78% 占用 |
| 内存/CPU | ⚠️ 需注意 | **内存仅 15.4G**（当前空闲 ~5.8G），CPU AMD Ryzen 7 5800H（8C/16T）。**PlainApp 的 `gradle.properties` 写死 `-Xmx12g -XX:MaxMetaspaceSize=2g` + kotlin daemon `-Xmx6g`，在本机必 OOM，需下调**（建议 `-Xmx4g` / kotlin daemon `-Xmx2g`） |
| 网络/代理 | ✅ 直连通畅 | 系统代理已禁用（`ProxyEnable=0`，残留 `ProxyServer=127.0.0.1:7897` 未生效）；无环境变量代理。实测 **GitHub raw、dl.google.com（0.7s）、services.gradle.org、repo1.maven.org（0.6s）、maven.google.com（5.4s 较慢）、plugins.gradle.org、api.foojay.io 全部可达** |

### 国内镜像源可用性（已探测，未下载）

| 镜像 | 端点 | 结果 |
|---|---|---|
| 阿里云 maven（google 仓库） | `https://maven.aliyun.com/repository/google/com/android/tools/build/gradle/maven-metadata.xml` | ✅ 200 |
| 阿里云 maven（central） | `https://maven.aliyun.com/repository/central/org/jetbrains/kotlin/kotlin-gradle-plugin/maven-metadata.xml` | ✅ 200 |
| 阿里云 maven（gradle-plugin 门户仓库） | `https://maven.aliyun.com/repository/gradle-plugin/...` | ✅ 200 |
| 腾讯 gradle 分发镜像 | `https://mirrors.cloud.tencent.com/gradle/gradle-9.7.0-bin.zip` | ✅ 200/206（支持断点续传，响应 0.14s） |
| 腾讯 maven 镜像 | `https://mirrors.cloud.tencent.com/maven/...` | ✅ 200 |
| 腾讯 AndroidSDK 镜像 | `https://mirrors.cloud.tencent.com/AndroidSDK/repository2-1.xml` | ✅ 200（platform-tools zip 也可取） |
| 清华 TUNA | 不镜像 android maven/gradle 分发 | —（用不到） |

结论：**本机直连外网完全可用**，镜像作为加速/兜底方案现成可用（阿里云 google 仓库可替代慢速的 maven.google.com）。

---

## 二、PlainApp v3.3.2 源码结构与改造点

### 1. 项目概况

| 项 | 值 |
|---|---|
| 类型 | **Kotlin Multiplatform**（androidMain + iosMain + commonMain），Compose Multiplatform UI |
| 模块 | `:app`（Android 壳）+ `:shared`（KMP 核心，几乎全部逻辑）+ `:shared-lib` + `:litert-stubs` + `:kgraphql-ksp` |
| Gradle | wrapper **9.7.0**（`gradle/wrapper/gradle-wrapper.properties`） |
| AGP | **9.3.1**（`com.android.tools.build:gradle`） |
| Kotlin | **2.4.10**（K2 + KSP2 `com.google.devtools.ksp:2.3.11`） |
| SDK 版本 | **compileSdk 37 / minSdk 28（Android 9+）/ targetSdk 36**；Java 17 源/目标（`jvmTarget=17`）；daemon 工具链 JETBRAINS 21（foojay 自动下载） |
| applicationId | `com.ismartcoding.plain`（namespace 同名），versionName 3.3.2，versionCode 637/636（arm64） |
| 主要依赖 | **Ktor 3.5.2**（server-netty 内嵌 HTTP/HTTPS + client-okhttp）、Room 2.8.4、Coroutines 1.11.0、**Google Tink 1.23.0**（Ed25519）、BouncyCastle 1.70（证书）、OkHttp 5.4.0、Coil 3.5.0、Media3、CameraX、DataStore、zxing、MediaPipe/LiteRT（仅 github/google 渠道 flavor） |
| 构建变体 | 3 个 flavor（`github`/`google`/`fdroid`）× debug/release；debug 自动加 `applicationIdSuffix=".debug"` |
| 服务器架构 | Ktor + Netty 内嵌服务器：HTTP（默认 **8080**，候选端口组 8080/8180/…/8980）+ HTTPS（默认 **8443**，候选 8043/8143/…/8943，端口占用自动轮换）；GraphQL API（mainschemas）+ REST 路由（Files/Upload/Zip/System/Nearby/Dlna）+ WebSocket；前端 UI 内置在 `app/src/main/resources/web/`（PWA） |

> 说明：任务描述中"默认 6789"不准确——**实际默认是 HTTP 8080 / HTTPS 8443**（`TempData.kt`），并在 10 端口组内自动避让占用（`HttpServerPorts.kt`）。健康检查端点 `/health` 返回包名。

### 2. 构建要求（README + CI）

- **CI**（`.github/workflows/release.yml`）：`actions/setup-java` **JDK 17 zulu** → `bash build-apk.sh` → `./gradlew assembleGithubRelease`（arm64 + armeabi-v7a 两遍）。
- **README Build 段**：只需准备 release 签名（`keytool` 生成 `app/release.jks` + 根目录 `keystore.properties`）；`build-apk.sh` 用环境变量 `ANDROID_STORE_PASSWORD`/`ANDROID_KEY_PASSWORD` 生成 `keystore.properties` 并写 `local.properties`（sdk.dir）。
- **assembleDebug 开箱情况**：✅ 基本开箱可用——debug 构建**不读 keystore.properties**（release signingConfig 才用）；debug 签名走 `debugPinned` 配置引用 `~/.android/debug.keystore`（**本机已存在**，2666 字节，21:44 生成）。仅需 `local.properties` 指向 SDK 目录。
- 注意点：a) `gradle.properties` 的 `-Xmx12g` 需下调（见环境表）；b) 首次构建需下载 Gradle 9.7.0 分发 + 全部依赖 + SDK 组件（platform 37 / build-tools），量大但网络通畅；c) 需要 `compileSdk 37`（Android 17）的 platforms 包——较新，阿里云/腾讯镜像与 dl.google.com 均可取。

### 3. 改造点评估（fork 定制 FlyGo 客户端）

#### a. 应用身份（一期必改，实现与官方版共存安装）

| 位置 | 现状 | 改成 |
|---|---|---|
| `app/build.gradle.kts` → `defaultConfig.applicationId` + 顶层 `namespace` | `com.ismartcoding.plain` | `com.flygo.client`（或类似）——**改包名是共存安装的关键**；注意 manifest 中有 `${applicationId}` 模板（FileProvider authority、广播 action），自动跟随 |
| `app/src/main/res/values/strings.xml` → `app_name` | `PlainApp`（translatable=false） | `FlyGo` |
| 图标 | `@mipmap/ic_launcher` / `ic_launcher_round` / `ic_splash`（adaptive 图标 XML，位于 `app/src/main/res/mipmap/`，另有一套在 `shared/src/androidMain/res/mipmap/`） | 换前景/背景 drawable；另有 `@drawable/app_icon`（QSTileService 用） |
| 版本号 | versionCode 637 / versionName 3.3.2 | 可保留或重置，`-PabiFilters` 参数控制 ABI |

#### b. 服务器功能（核心，几乎零改动即可用）

- 入口：UI 开关（`MainActivity`）→ 前台服务 **`HttpServerService`**（`foregroundServiceType="specialUse|dataSync"`，已声明在 `app/src/main/AndroidManifest.xml`）→ Ktor/Netty 内嵌服务器，绑定 `0.0.0.0`（WiFi/热点/Tailscale 全可达）。
- 端口：默认 8080/8443，10 端口组自动避让（`HttpServerPorts.kt`），可在设置页切换 HTTP/HTTPS。
- 证书：首次启动自动生成**自签 JKS**（BouncyCastle，`HttpServerLifecycle.kt` 的 `generateSslKeyStoreFile`），存 `filesDir`，密码存 DataStore 偏好；证书指纹展示给用户做配对校验。
- 加密：登录配对走 **ECDH(secp256r1) + Ed25519 签名**（Tink），业务载荷 **XChaCha20-Poly1305**（`platform/Crypto.kt`），会话 token + 防重放（ReplayGuard）。**安全模型成熟，fork 无需动**。
- 外部控制点已存在：广播接收器 `ServiceStopBroadcastReceiver` 监听 `${applicationId}.action.START_HTTP_SERVER` / `.STOP_HTTP_SERVER` / `.STOP_SCREEN_MIRROR`（可被别的应用/脚本远程启停服务器）。

#### c. 客户端功能（关键结论：**没有 HTTP 客户端模式**）

- PlainApp 是**纯服务器 + 本地工具**：手机上开服务器，电脑浏览器访问手机。**没有"连接另一台 PlainApp 服务器"的客户端模式**。
- 唯一的"节点互连"是近场 P2P：BLE / Wi-Fi Aware / Wi-Fi Direct + mDNS（`plainapp.local`）聊天与传文件（`PeerGraphQLService`），不走 HTTPS。
- 对 FlyGo 的意义：**"让手机访问面板/其他节点"是差异化空白**，需要二期新增（见 d）。

#### d. 深度连接潜力（与 FlyGo 面板 http://192.168.1.151:8388 互通）

现有可复用扩展点：

1. **内嵌面板 WebView**：App 是 Compose Multiplatform，可新增一个 Compose 页面用 `androidView { WebView }` 加载面板 URL。`android:usesCleartextTraffic="true"` 已允许明文 HTTP，**无需改 manifest**。最简替代：用 Intent `ACTION_VIEW` 跳浏览器。
2. **文件"分享到面板"**：`MainActivityIntentHandler.kt` 已处理 `ACTION_SEND`/`SEND_MULTIPLE`（分享 → 转发目标对话框 `showForwardTargetDialog`），可在此加"发送到 FlyGo 面板"目标（面板上传接口 POST，用已有依赖 OkHttp / ktor-client-okhttp）。
3. **面板 URL 预置设置页**：设置页由 `SettingsCards.kt` / `SettingsCardItems.kt` 的 `PCard`+`PListItem`+`PSwitch` 组合而成，照葫芦画瓢加一个"FlyGo 面板地址"项即可；持久化用项目已有的 DataStore preferences（`com.ismartcoding.plain.preferences` 包）。
4. **服务器端转发路由**：`HttpRouteRegistry` / `KtorRouteRegistry` 可注册新路由，把 `/flygo/*` 反向代理到面板（Ktor client 依赖已在）。
5. 入口 intent 已支持 `navigate_to_web_settings`、`CHAT_TARGET_ID`、`ACTION_PLAY_MEDIA` 等 extra，扩展 deep link 有先例。

#### e. 许可证合规（AGPL-3.0）

- `LICENSE` 为 **GNU AGPL v3.0** 全文。fork 必须：**保留 LICENSE 与版权声明**（不得删除）；修改文件建议在文件头声明改动；**对外分发 APK/源码必须整体保持 AGPL-3.0 并公开修改版源码**。
- 若 FlyGo 客户端仅供自用/内部，AGPL 网络服务条款风险很低；一旦公开分发（如发到群里/应用市场），即需附源码与许可证。商业化需谨慎评估（与"飞狗计划"是否冲突需另行决策）。

---

## 三、结论

### 1. 环境差距表（缺什么 → 怎么补 → 预估耗时）

| 缺失项 | 补齐方案（均已验证可达） | 预估耗时 |
|---|---|---|
| Android SDK（platform 37 / build-tools / platform-tools / cmdline-tools） | 下载 cmdline-tools → sdkmanager 装组件；源用 dl.google.com（0.7s）或腾讯镜像 `mirrors.cloud.tencent.com/AndroidSDK/` | 15–30 分钟（下载为主，约 1–1.5GB） |
| Gradle 9.7.0 分发（~140MB） | `./gradlew` 自动下载；如需加速改 `gradle-wrapper.properties` 的 distributionUrl 为腾讯镜像 `https://mirrors.cloud.tencent.com/gradle/gradle-9.7.0-bin.zip` | 3–8 分钟 |
| 依赖仓库（google/mavenCentral/pluginPortal） | 直连可用；maven.google.com 慢（5.4s）可换阿里云 `repository/google`；首次全量依赖下载量大 | 10–30 分钟 |
| 内存配置 | 改 `gradle.properties`：`-Xmx12g`→`-Xmx4g`、`-XX:MaxMetaspaceSize=2g`→`1g`、kotlin daemon `-Xmx6g`→`2g`（16G 内存必调） | 1 分钟 |
| JAVA_HOME | 建议设 `JAVA_HOME=C:\Program Files\Java\jdk-25`（Gradle 9.7 支持 JDK 25 运行；编译 target 17 由 JDK25 向下兼容）；daemon 工具链 21 由 foojay 插件自动拉取（api.foojay.io 可达） | 1 分钟 |
| debug 签名 | `~/.android/debug.keystore` **已存在**，无需处理 | 0 |
| 源码 | `C:\FengProj\PlainApp` 克隆进行中（已 75MB/未完成）；或直接用 `C:\Users\<user>\AppData\Local\Temp\plainapp-research\plain-app` 副本；或 `gh api` tarball 直连 | — |

合计：**环境就绪约 0.5–1 小时**（纯下载等待，可后台）。

### 2. 今天能否出 APK 的明确判断

**有条件能（偏乐观）。** 依据：

1. 网络全部直连可达（GitHub/dl.google/mavenCentral 均 200），镜像兜底现成，下载不是瓶颈；
2. 唯一"缺大件"是 Android SDK，属一次性下载安装（半小时级），且不需要 Android Studio；
3. 源码克隆并行进行中，改包名/应用名/图标的改动集很小（3 个文件以内），debug 构建无需签名配置；
4. **风险点**：a) 16G 内存必须调低 jvmargs（否则 OOM）；b) KMP 首次编译（KSP2 + Compose compiler）在 8C/16T 上预计 30–60 分钟；c) compileSdk 37 平台包需确认镜像有货（腾讯镜像已确认同步）。

### 3. 基于 PlainApp 改造的最可行路径（分期）

**一期（今天可完成的目标：可安装的"FlyGo 定制 debug APK"）最小改动集：**
1. 装 SDK（cmdline-tools + platform 37 + build-tools + platform-tools），写 `local.properties`；
2. 下调 `gradle.properties` 内存参数；
3. 改 `applicationId`/`namespace` → `com.flygo.client`（与官方版共存安装）；
4. 改 `app_name` → FlyGo + 替换图标资源（可选，图标不影响安装）；
5. `./gradlew :app:assembleGithubDebug`（或 `assembleDebug`）出 APK → adb 安装验证。
   - 产出：一个可安装、与官方 PlainApp 共存、品牌为 FlyGo 的客户端。

**二期（深度连接，独立排期）：**
- 设置页加"FlyGo 面板地址"（DataStore 持久化）；
- 新增面板 WebView 页面（Compose `androidView`，明文 HTTP 已放行）；
- "分享到面板"入口（复用 SEND intent 转发对话框 + OkHttp POST 上传）；
- （可选）服务器端 `/flygo/*` 转发路由；包名/品牌换好后官方版共存无冲突，可逐步灰度。

> 报告人：ZCode 调研子任务（2026-08-16）。数据均来自本机实测命令与 GitHub v3.3.2 tag 源文件直读。
