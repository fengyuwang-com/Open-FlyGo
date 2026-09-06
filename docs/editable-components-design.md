# FlyGo 组件可编辑系统设计（Editable Components Design）

> 日期：2026-08-16 · 范围：`panel/`（Rust 后端 + `panel/static/index.html` 单页前端）
> 目标：**每个页面都可以编辑，所有组件都可以自由编辑**——服务卡片、总览图标（顶栏灯）、导航项、
> 端口/URL、健康检查、容器列表的展示，用户（在他自己的电脑上）都能增删改，无需重新编译 EXE。
> 前置：`docs/device-agnostic-report.md`（路径设备无关已做完）、`panel/src/config.rs`（三级路径解析）。
> 约束：1 个 EXE + 1 个 APK 的交付哲学不变；不引入重量级依赖；无配置文件时行为与现状完全一致。

---

## 一、现状调查：写死的组件与服务定义

### 1.1 前端 `panel/static/index.html`（1398 行单页）

| 区域 | 位置 | 写死内容 | 数据来源 |
|---|---|---|---|
| 左侧导航 | L256-267 | 12 个 `navi` 按钮：emoji + 中文名 + `data-v`（总览/文件/影视/相册/终端/容器/系统/自启动/访问控制/同步/同步网络/手机接入） | **硬编码** |
| 页面标题映射 | L643 `TITLES` | 每个 view 的中文标题 | **硬编码** |
| 顶栏 tiles | L276-283 | 复制入口 / 主题 / 重启 3 个动作钮 + 3 盏服务灯（`d-sync`/`d-ts`/`d-ssh`）+ 温度 | 动作钮硬编码；服务灯靠 L866 `loadServices` **按后端确切名字**（`SyncthingNAS`/`Tailscale`/`sshd`）点亮 |
| 文件页 | L316 | `<iframe data-svc="alist" data-src="http://${FENG_IP}:5244">` | **硬编码**（端口 5244、容器 key=alist、启动文案"10~30 秒"） |
| 影视页 | L340 | `<iframe data-svc="jellyfin" data-src="http://${FENG_IP}:8096">` | **硬编码**（端口 8096） |
| 相册页 | L361 | `<iframe data-svc="immich-server" data-src="http://${FENG_IP}:2283">` | **硬编码**（端口 2283，容器 key=immich-server） |
| iframe 冷启动逻辑 | L658-743 `embed()` | 超时 8s、轮询 2.5s、上限 90s、失败诊断查 `/api/docker` | 逻辑硬编码，URL/容器名来自 iframe 标签 |
| 服务卡片墙 | L836-862 `loadSvcWall` | 卡片本身**动态**（`/api/health/services`），但点击行为 = `http://host:port`、无端口=灰 | 后端驱动（详见 1.2） |
| Docker 容器页 | L407-409、L879 `renderCtl` | 引擎 / 全部容器 / **Portainer** 三个开关是硬编码特殊项；容器列表动态（`/api/docker`） | 混合 |
| 系统页 | L424、L892、L1379 | "USB 移动盘 **(E:)**" 标题、`/E:\\/` 正则判断挂载、confirm 文案 "E: (USB ADATA HV300)" | **硬编码**（E 盘专有） |
| 管家组件面板 | L289-299、L1234 `TRAY_META` | webcli / Syncthing / Homepage / 同步定时器 / 外置盘 的标签与顺序 | **硬编码**（状态来自 `/api/tray/status`） |
| 手机接入页 | L516-611 | 三步引导、APK 下载链接（GitHub/F-Droid）、各厂商保活清单 | **硬编码**（二维码/设备 ID 动态） |
| 总览/系统页说明文案 | L302、L330-332 等 | 连通性检测说明、"\\host\NAS → E:" 等 | **硬编码** |
| 页脚 | L613 | "FlyGo · Tailscale 内网" | **硬编码** |

### 1.2 后端 `panel/src/`

| 模块 | 写死内容 | 数据来源 |
|---|---|---|
| `api/services.rs` | `wanted()`：ssh/Tailscale/syncthing 三个 `(key, 显示名, 进程名)` 硬编码，sysinfo 枚举进程判断 | 进程探测（动态）+ 名单（硬编码）；前端三盏灯与之耦合 |
| `api/docker.rs` | `default_icon()`（15 项 emoji map）、`default_desc()`（21 项中文 map）；端口解析只取服务定义里**第一个** host 端口；刻意不硬编码清单（compose 动态解析） | docker-compose.yml 解析（动态）+ icon/desc map（硬编码） |
| `api/health.rs` | 探测列表直接复用 `docker::compose_list()`；探测模式**只有 TCP**（`127.0.0.1:port`，3s 超时）；绿/黄/红/灰阈值常量 | compose 动态 + 探测动态 |
| `api/system.rs` | 定时任务过滤正则 `'Sync\|Vault\|NetReset\|Feng'`；SMART/温度/共享采集脚本 | PowerShell 采集（动态）+ 过滤名单（硬编码） |
| `api/autostart.rs` | `DEPRECATED_HINTS` 废弃候选名单（fengvault/openclaw/hermes/百度/夸克） | 硬编码（只标记不硬删） |
| `daemon.rs` | `DEFAULT_COMPONENTS = ["webcli"]`；托盘组件名 webcli/syncthing/homepage | 硬编码 |
| `api/stats.rs` | `/api/flags` 运行时 GET/POST 开关（`feature.*`），**不持久化**（重启丢失） | 先例：单 handler 收 GET+POST 的写法可借鉴 |
| `config.rs` | 三级路径解析（env > 安装根 > legacy C:\FlyGoLegacy），所有机器特定路径已收敛于此 | 本次设计直接复用此模式 |

### 1.3 结论：哪些动态、哪些硬编码

- **纯动态（API 驱动，无需改）**：docker 容器列表、健康探测、系统大盘（磁盘/任务/共享）、自启动、同步/同步网络、访问控制、终端、托盘、手机配对状态。
- **半动态（数据动态，展示骨架/映射硬编码）**：服务卡片墙（点击 URL 由后端 port 推导）、容器页（Portainer 特殊项）、顶栏灯（名字映射硬编码）。
- **纯硬编码（本次改造重点）**：导航项、页面标题、iframe 页（URL/端口/容器 key/文案）、顶栏动作钮、USB E 盘、TRAY_META、手机页静态引导、各处说明文案。
- **后端硬编码但数据源自探测**：services 名单、docker icon/desc map、system 任务过滤名单——这些是"默认模板"，应改为用户可覆盖。

---

## 二、成熟方案调研（只调研不安装）

| 项目 | 配置格式 | 配置位置 | UI 编辑器 | 校验 | 默认/模板 | 服务发现 |
|---|---|---|---|---|---|---|
| **Homepage** (gethomepage) | YAML（settings/services/widgets/bookmarks.yaml） | 独立 config 目录（Docker 挂载） | 无，纯手改文件 | YAML 解析，未知键容忍 | 无模板机制 | Docker/K8s label 声明式；widget 需 `key` |
| **Dashy** (Lissy93) | YAML `conf.yml` | `/app/user-data/conf.yml`（Docker 挂载） | **有**：编辑模式 + 右键编辑 + 原始 YAML 编辑器，改动预览后 "Save to Disk" | 编辑器内 schema 校验 + 导出/查看/备份/重置 | 首次运行给默认 conf | 无自动发现；图标可 auto favicon / 图标包 / emoji / URL |
| **Flame** | 无文件格式，SQLite（Sequelize ORM） | `/app/data`（Docker 挂载） | **有**：应用/书签 GUI 编辑器 | ORM 层 | 内置默认 | Docker label / K8s ingress 自动发现 |
| **Homarr** | 无 YAML，拖拽网格，后端 JSON 持久化 | Docker volume | **有**：完整拖拽网格编辑器 + 图标选择器（万级） | 后端校验 | 内置默认 | 40+ 集成 widget |
| **Glance** | YAML `glance.yml`（pages → columns → widgets） | 二进制同目录，`--config` 可覆盖 | 无，纯手改文件 | 解析时报错 | 无模板机制 | 无；widget 类型多（iframe/html/custom-api 等） |

### 2.1 共通的成熟模式（提取）

1. **配置文件驱动 + 文件即真相**：五个项目全部以"一个用户可编辑的配置文件"为最终真相，启动时加载、写盘持久。UI 编辑器只是写文件的入口（Dashy 明确 "Save to Disk"）。
2. **配置放数据目录，跟程序走**：都是"程序目录/挂载卷下的独立配置子目录"，不做系统注册表/全局设置——**便携式部署的惯例**。
3. **组件模型趋同**：`group/section → item` 两级；item 字段 = `name / icon / url(href) / desc / type / hidden / sort / widget(健康监测)`。图标支持 emoji + 图标包 + URL 三档。
4. **无配置文件 = 内置默认**：首次运行生成默认配置，或直接用内置默认渲染（FlyGo 现状的硬编码 UI 天然就是"默认模板"）。
5. **手改文件 vs UI 编辑的谱系**：Glance/Homepage 纯文件（面向程序员）；Flame/Homarr 纯 UI（面向家庭用户）；Dashy 两者都要（推荐形态，门槛最低）。
6. **配置只描述"差异/覆盖"或"全量"两种哲学**：Homepage 全量描述；FlyGo 已有的 compose 动态解析天然适合"动态默认 + 用户覆盖"。

---

## 三、设计方案

### 3.1 配置文件位置与格式

**位置**：`<安装根>/data/components.json`，路径解析完全复用 `config.rs` 的三级模式：

```
1. FENGPAGE_COMPONENTS 环境变量（显式覆盖，可指向任意路径/用户目录）
2. 安装根相对路径 <root>/data/components.json（默认，跟随 EXE 目录迁移）
3. legacy：无（新功能没有历史位置，字段预留即可）
```

- 选**安装根 data/ 而非 %APPDATA%**：产品是便携式 NAS 面板（"1 个 EXE + 1 个目录"），整目录拷走即整体迁移，与 Glance 的 glance.yml 同目录惯例一致；用户目录在重装/换机时反而容易丢。
- 解析函数建议：`target_path("FENGPAGE_COMPONENTS", "data/components.json", "")`（`target_path` 已有，"若不存在则用 root-relative 默认"语义正好吻合，见 `config.rs` L118）。
- 写盘失败兜底：写临时文件 `components.json.tmp` → rename 原子替换 → 保留上一份 `components.json.bak`（Dashy 的 backup 语义，防止改坏）。

**格式：JSON**（一期）。理由：
- `serde_json` 已在依赖（`Cargo.toml` L27），**零新依赖**；项目 release 是 `opt-level=z + panic=abort` 的体积哲学。
- UI 编辑器写 JSON 无歧义；手改 JSON 虽不如 YAML 友好，但一期以 UI 编辑为主、手改为辅，且提供"导入/导出/重置"弥补。
- YAML 需要新依赖（serde_yaml 已停维护），列为三期可选（serde_yml），不阻塞一期。

### 3.2 组件模型

顶层结构（JSON，字段名 ASCII）：

```json
{
  "version": 1,
  "nav": [
    { "id": "overview", "label": "总览", "icon": "🏠", "target": "page:overview", "sort": 1 },
    { "id": "files",    "label": "文件", "icon": "📂", "target": "page:files",    "sort": 2 },
    { "id": "dl",       "label": "下载站", "icon": "⬇️", "target": "iframe:lf-download", "sort": 9 }
  ],
  "tiles": [
    { "id": "t-copy", "type": "action",       "label": "复制入口", "icon": "📋", "action": "copy-url" },
    { "id": "t-theme","type": "action",       "label": "白天",     "icon": "🌤", "action": "theme" },
    { "id": "t-sync", "type": "service-light","label": "Syncthing","service": "syncthing" },
    { "id": "t-ts",   "type": "service-light","label": "Tailscale", "service": "tailscale" },
    { "id": "t-ssh",  "type": "service-light","label": "SSH",       "service": "ssh" }
  ],
  "cards": [
    { "id": "alist",    "name": "alist",    "icon": "📁", "desc": "网盘聚合", "url": "http://${FENG_IP}:5244", "port": 5244, "group": "services", "sort": 1 },
    { "id": "jellyfin", "name": "Jellyfin", "icon": "🎬", "desc": "影视",     "url": "http://${FENG_IP}:8096", "port": 8096, "group": "services", "sort": 2 },
    { "id": "router",   "name": "路由器",   "icon": "📶", "desc": "家里路由", "url": "http://192.168.1.1",       "port": 80,   "group": "network", "sort": 1 },
    { "id": "immich",   "name": "immich",   "icon": "📷", "desc": "照片",     "url": "http://${FENG_IP}:2283", "port": 2283, "hidden": true }
  ],
  "frames": [
    { "id": "lf-files",  "title": "文件", "url": "http://${FENG_IP}:5244", "container": "alist",         "boot_hint": "首次约需 10~30 秒" },
    { "id": "lf-media",  "title": "影视", "url": "http://${FENG_IP}:8096", "container": "jellyfin" },
    { "id": "lf-photos", "title": "相册", "url": "http://${FENG_IP}:2283", "container": "immich-server" },
    { "id": "lf-download","title": "下载站","url": "http://${FENG_IP}:8080","container": "metube" }
  ]
}
```

各集合的字段语义（Serde 结构体，全部字段可缺省）：

| 集合 | type / 形态 | 关键字段 | 说明 |
|---|---|---|---|
| `nav` | 导航项 | `id, label, icon, target(page:xx / iframe:xx / url:https://...), sort, hidden, removed` | `target=url:` 即"自定义外链页"入口（跳转）；`iframe:` 引用 `frames` 里的定义 |
| `tiles` | 顶栏图标 | `id, type(action/service-light), label, icon, action, service, hidden` | `service-light` 绑定 `services.rs` 的 key（解决现按名字硬匹配的耦合）；`action` 白名单：`copy-url/theme/restart` |
| `cards` | 服务卡片 | `id, name, icon, desc, url, port, group, sort, hidden, removed, health{mode,port,timeout_ms}` | `url` 支持 `${FENG_IP}` 模板（现有先例，L666）；`port=0` = 不探测（灰卡）；`removed` 仅用于删除内置项 |
| `frames` | iframe 页 | `id, title, url, container, boot_hint, fullscreen(bool), hidden` | `container` = compose 容器 key，用于一键启动/失败诊断（复用 L680-739 逻辑）；`url` 支持 `${FENG_IP}` |
| 顶层 | 站点 | `version, title(页脚/标题), theme(可选覆盖)` | `version` 用于未来 schema 演进 |

**设计要点——覆盖式（overlay）而非全量式**：

- 后端**默认模板** = 当前硬编码清单（导航 12 项、3 盏灯、3 个 iframe、compose 解析出的卡片、services.rs 名单），代码里作为 `default_config()` 内置，不落盘。
- 用户配置按 `id` **合并**：同 id 覆盖字段；新 id 追加；`hidden:true` 隐藏；`removed:true` 删除内置项。
- 效果：compose 里新增服务仍会自动出现在卡片墙（保持 docker.rs"刻意不硬编码清单"的动态哲学），用户只需写差异。
- 无配置文件 → 返回默认模板 → **行为与现状逐像素一致**（迁移兼容的根基）。

### 3.3 编辑入口与权限

**两者都要**（Dashy 形态）：

1. **面板 UI 编辑**（主入口，目标用户是"他自己的电脑"）：
   - 顶栏新增"✏️ 编辑"按钮进入编辑模式：卡片右上角出现 ⚙/✕，导航项可改 icon/label/隐藏，iframe 页可改 URL/端口/容器 key，全部表单化。
   - 编辑结果前端收集为完整 JSON → `PUT /api/config/components` → "保存到磁盘"语义（Dashy 同款）。
   - 拖拽排序放二期（一期先做 sort 数值字段 + 上下移按钮）。
2. **直接改文件**（进阶用户）：`data/components.json` 手改，面板"↻ 重载"按钮或改文件后前端自动重拉。

**权限**：面板现状无登录（单用户私有面板 + Tailscale 内网），编辑接口与 `/api/flags`、`/api/access/*` 同级信任模型，一期不加锁。预留：
- 所有写接口做**值白名单校验**（拒绝路径穿越、URL scheme 只允许 http/https、port 范围 1-65535、id 字符集），避免"能写配置"升级为"能写任意文件"。
- 二期可加"编辑口令"（复用 term_token.env 的 token 机制，独立文件 `edit_token.env`），由 `FENGPAGE_EDIT_TOKEN`/config 解析，PUT 时校验 header。

### 3.4 后端 API（新模块 `api/components.rs`）

```
GET  /api/config/components   -> { ok, config: {nav,tiles,cards,frames,title,theme}, merged: true|false }
PUT  /api/config/components   -> body 全量 JSON（仅用户配置部分）；原子写盘 + .bak；返回 { ok, errors: [...] }
POST /api/config/components/reset -> 删除文件，恢复内置默认
GET  /api/config/components/default -> 返回内置默认模板（编辑页"恢复默认"预览用）
```

- **校验**：serde 严格反序列化（未知字段拒绝或警告）+ 业务校验（id 唯一性、type 枚举、port 范围、`target`/`action`/`service` 引用白名单）；错误返回**逐字段** `errors: [{path, message}]`，前端表单直接定位。
- **热加载**：读路径加 `(mtime, content)` 内存缓存（仿 system.rs 12s 缓存惯例，但 PUT 后主动失效）；前端编辑保存后立即重拉，无需重启 EXE。
- **合并点**：`docker::compose_list()` 产出卡片后套 `cards` 覆盖（icon/desc/port/url/hidden）；`health.rs` 探测列表同样套用；`services.rs` 名单改为从 `tiles` 的 `service-light` 引用生成（或保留默认三件套 + 配置可增）；`index.html` 的 nav/tiles/frames 全部改由 `/api/config/components` 渲染。

### 3.5 前端改造（index.html）

- 新增 `loadComponents()` 在 init 时拉取一次，`CONFIG` 全局对象替代硬编码常量（nav 按钮、TITLES、iframe 标签、TRAY_META、顶栏灯映射）。
- nav 改为 JS 动态生成（桌面侧栏 + 手机菜单克隆逻辑保留）；`TITLES` 由 nav 的 label 推导。
- iframe 页改为一个通用模板容器，按 `frames` 定义生成（替换 L316/340/361 三处写死标签）；`embed()` 已按 `data-svc/data-src` 驱动，改造为从配置对象取。
- 顶栏灯：`loadServices()` 改为按 `tiles` 的 `service` 字段匹配 `st[service]`，不再硬编码 `sshd/Tailscale/SyncthingNAS`。
- 卡片墙：`loadSvcWall()` 渲染逻辑复用，URL 改用后端合并后的 `url` 字段（而非前端拼 `http://host:port`）。
- 编辑 UI：新增编辑模式（CSS 类 + 浮层表单），一期提供：改 nav（label/icon/hidden/sort）、改 tiles、改 cards（name/icon/url/port/desc/hidden）、改 frames（url/container/title）、新增/删除卡片、保存/重置/导出/导入。
- 说明文案类纯静态内容（手机页、保活清单）一期不动（量大收益低），列为三期"页面级可编辑"。

### 3.6 迁移兼容（无配置文件 = 现状）

1. 无 `components.json`：`GET /api/config/components` 返回内置默认模板，前端渲染结果与当前版本一致（导航、灯、iframe、卡片墙全部同款）。
2. 首次通过 UI 保存时生成文件（带 `"_comment"` 字段说明文件位置与用途，弥补 JSON 无注释）。
3. 文件损坏/版本不匹配：返回 `{ok:false, error}` + 前端提示"配置损坏，可重置"，重置后回默认；旧版 `version` 字段低于当前时按默认字段缺省补齐（`#[serde(default)]`）。
4. 删除配置 = 恢复出厂（符合"用户可自由编辑"的最终裁决权在用户）。

---

## 四、落地分期

| 期 | 内容 | 验收标准 | 规模 |
|---|---|---|---|
| **一期（核心闭环）** | ① config.rs 加 `components_path`；② 新模块 `api/components.rs`（模型 + `default_config()` 默认模板 + 读写/校验/合并 + 3 个路由）；③ docker.rs/health.rs/services.rs 接入覆盖点；④ index.html 改为配置驱动渲染（nav/tiles/cards/frames 动态化）；⑤ 表单编辑 UI（改/增/删/隐藏/排序数值 + 保存/重置） | 无配置时界面与现状一致；改端口/图标/导航后保存即生效；compose 新服务仍自动出现；重启后配置保留 | 后端 ~400 行新代码 + 前端 ~400 行 |
| **二期（编辑体验）** | 拖拽排序（nav/cards）；新增组件向导（选类型 → 填表单 → 生成 id）；`health.mode: http` 探测（用现有 ureq 依赖或 tokio 手写 GET）；导入/导出/备份（.bak 列表可回滚）；编辑口令（edit_token.env） | 编辑全程不碰文件；http 健康检查可用；误改可一键回滚 | 后端 ~150 行 + 前端 ~300 行 |
| **三期（页面级可编辑）** | 用户自建页（markdown/链接组/纯 iframe 页模板，nav 增 `page:custom`）；系统数据页（docker/自启动/系统）列可见性/标题重命名；静态文案（手机页等）改为可编辑；可选 serde_yml 支持手改 YAML | 用户能不加代码做出一个全新页面 | 后端 ~200 行 + 前端 ~400 行 |

**一期刻意不做**：拖拽、http 探测、自定义页、静态文案编辑——先跑通"配置驱动 + 可保存 + 迁移兼容"的最小闭环，每期独立可发版。

---

## 五、改动量评估

| 文件 | 改动性质 | 量级 |
|---|---|---|
| `panel/src/config.rs` | 新增 1 个字段 + 1 行解析（复用 `target_path`） | 小（~5 行） |
| `panel/src/api/components.rs`（新） | 组件模型结构体、`default_config()` 默认模板、JSON 读写（tmp+rename+bak）、合并逻辑、校验、3 个 handler | **新文件 ~350-450 行** |
| `panel/src/api/docker.rs` | `compose_list()` 产出后套 cards 覆盖（icon/desc/port/url）；`default_icon/default_desc` 保留为默认值来源 | 中（~40 行） |
| `panel/src/api/health.rs` | 探测列表套覆盖 + 可选 port 覆盖 | 小（~15 行） |
| `panel/src/api/services.rs` | `wanted()` 名单允许配置增删（默认值不变） | 小（~20 行） |
| `panel/src/main.rs` | 注册 3 个路由 + `mod components` | 小（~5 行） |
| `panel/static/index.html` | nav/TITLES/tiles/iframe 标签改动态渲染；`loadServices` 改配置映射；新增编辑模式 CSS+JS+表单 | **大（~400-600 行）** |
| `docs/editable-components-design.md` | 本文档 | — |
| 不动 | autostart.rs / access.rs / term.rs / sync*.rs / system.rs / daemon.rs / tray.rs（数据页保持后端驱动，仅展示层字段可配） | — |

**结论**：一期总量约 1 个新后端模块 + 1 个大前端改动，其余为小改；不新增任何 Cargo 依赖；风险集中在 index.html 的渲染改造（现有 1398 行单页无构建工具，需要保持 `embed()`/全屏/轮询等既有机制不回归——这也是把"配置驱动化"与"编辑 UI"拆开、先上驱动的理由）。
