# AList 全屏横屏问题调查报告

> 调查日期：2026-08-16 · 优先级：中 · 状态：已修复（面板侧）

## 一、现象

用户报告：通过 FlyGo 面板的「文件」页进入 AList（OpenList）服务，点击「⛶ 全屏」后，
页面被强制横屏显示，竖屏不可用。

## 二、调查过程

### 1. 面板侧（panel/static/index.html）

面板全屏实现位于 `initFs()`（index.html 约 564~620 行），三页（文件/影视/相册）共用：

- 优先原生 Fullscreen API：`wrap.requestFullscreen()`（wrap = `.frame-w` 容器）；
- 旧 iOS Safari 兜底：CSS 伪全屏（`.fs-pseudo`：fixed inset:0 盖住页面，隐藏导航）。

面板代码**没有任何显式强制横屏逻辑**：
- 无 `screen.orientation.lock(...)` 调用；
- 无横屏媒体查询 / 方向相关 CSS；
- iframe 尺寸逻辑为 `width:100%; height:70vh`（全屏时 `height:100%`），不强制方向。

### 2. AList 侧（http://127.0.0.1:5244，容器内 OpenList 前端）

抓取 AList 前端全部 28 个 Vite chunk（约 2.1MB）逐一检索：

- `orientation` / `landscape`：仅命中 CSS 属性表（text-orientation 等）、
  GameMaker/Maxima 语言定义等**误报**，无任何方向检测逻辑；
- `matchMedia`：仅用于 `prefers-color-scheme`（主题切换），与方向无关；
- `fullscreen`：仅有播放器自身的标准全屏切换
  （`document.fullscreenElement ? exitFullscreen() : requestFullscreen()`），无方向锁定；
- 中文「横屏」提示：**全文 0 命中**——AList 没有"请横屏使用"类提示。

### 3. 中间结论

横屏既不是面板显式强制，也不是 AList 自身逻辑。

## 三、根因

**移动端浏览器 Fullscreen API 的默认行为**：在 Android Chrome/Edge（及部分移动端
Firefox）上，`element.requestFullscreen()` 使元素进入原生全屏时，浏览器**自动将屏幕
旋转为横屏**——这是规范允许的 UA 默认行为（全屏内容通常视为视频场景），并非页面代码
所致。竖屏下进入全屏 → 浏览器强制横屏 → 表现为"全屏即横屏、竖屏不可用"。

iOS Safari 无此问题（不支持任意元素原生全屏，走 CSS 伪全屏路径，不触发旋转）。

## 四、修复方案（已实施）

**改动范围**：仅 `panel/static/index.html` 的 `initFs()` 全屏函数，未触碰其他部分
（并发中的顶栏 tiles / 溢出 CSS 任务改动零重叠）。

原理：Fullscreen API 规范允许在全屏状态下调用 `screen.orientation.lock()`（锁定的方向
在全屏退出时自动释放）。进入原生全屏后立即**锁定为进入时的当前方向**：

- 竖屏进入 → 锁 portrait → 竖屏可用（解决本问题）；
- 横屏进入（如看 Jellyfin 视频）→ 锁 landscape → 保持横屏；
- 桌面浏览器 `lock()` 会 reject（不支持）→ `.catch()` 吞掉，行为不变；
- 退出全屏自动释放锁；另在 `exitFullscreen().then()`、`fullscreenchange` 两处显式
  `screen.orientation.unlock()` 兜底，防止个别 Android 版本锁残留。

关键代码（index.html `initFs()` 内）：

```js
function fsUnlock(){const so=screen.orientation;if(so&&so.unlock)so.unlock()}
function fsLockCurrentOrientation(){
  const so=screen.orientation;
  if(!so||!so.lock)return;
  const cur=so.type.startsWith('portrait')?'portrait':'landscape';
  so.lock(cur).catch(()=>{});
}
// btn.onclick 内：
wrap.requestFullscreen().then(()=>fsLockCurrentOrientation()).catch(()=>{});
// 退出路径 + fullscreenchange 监听均调用 fsUnlock()
```

## 五、验证

- 提取 index.html 内联脚本 `new Function(...)` 语法检查通过；
- 面板 8388 端口正常返回页面（HTTP 200），页面含新函数 `fsLockCurrentOrientation`；
- AList 5244 端口正常（容器 Up，首页与静态资源 Content-Length 正常）；
- 真机验证建议：Android Chrome 竖屏进「文件」页 → 全屏 → 应保持竖屏。

## 七、第二轮（2026-09-05）：OpenList v4.2.5 无罪，按内容锁方向

用户反馈"每次全屏必横屏，没得选"，复查：

- 抓取现行前端全部 14 个 JS chunk 全文检索 `orientation`：命中全是
  CSS/SVG/ARIA 误报（`text-orientation`、`aria-orientation`、GameMaker
  音频 API 等），**无任何方向锁定/全屏旋转逻辑**——应用侧再次无罪。
- 真因收敛到两处：①浏览器原生全屏默认转横屏；②面板"锁进入时方向"
  在歪拿手机时瞬间误判 landscape 反锁横屏；③用户若**直接打开
  `http://100.x:5244/`**（tcpfwd 直连），根本不经过面板页，锁代码零执行。
- 修复（`panel/static/index.html`，随本次 release 生效）：按内容锁方向
  `FS_ORIENTATION={文件:portrait, 相册:portrait, 影视:landscape}`，
  不再跟随进入瞬间；锁定失败弹 toast 报原因（`fengToast`，3.5s 自消失），
  下次复现直接看报错定因（重点怀疑项：小米/Harmony 系浏览器缺
  `orientation.lock` 实现）。
- 验证待用户真机：面板页 → 文件页 → ⛶ → 应保持竖屏；若弹"方向锁定失败"
  把原因原文发回；若无 toast 仍横屏，改走 CSS 伪全屏兜底（永不触发旋转）。

## 八、第三轮（2026-09-05）：用户要横竖跟手，锁全部拿掉

用户明确：不要锁竖屏，要横竖都能用。`initFs` 现只剩光秃
`wrap.requestFullscreen()`，锁/toast 相关代码零残留（已全文件 grep 验证）。
预期行为：进全屏瞬间浏览器可能先闪横屏一次，之后转手机就跟（竖拿即竖屏）。
若仍"钉死横屏转不回来"，再切 CSS 伪全屏兜底。

## 六、补充说明（原文保留）

- 服务卡片墙点击（`window.open` 新标签打开）不走全屏，不受影响；
- AList 播放器内部自身的全屏按钮行为不变（浏览器默认横屏，属 AList 内体验）；
- 若后续希望"全屏内允许自由旋转"，可改为 `so.lock('any')`，但 'any' 在部分
  Chrome 版本仍会回落到横屏，当前"按内容锁方向"方案最稳（2026-09-05 起）。
