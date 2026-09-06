# FlyGo Logo 部署产物

## 定稿源文件

- `flydog-logo.svg` — FlyGo 定稿 Logo v5（512×512，用户已确认）：
  - 深蓝渐变圆角方块背景（#0A2E6E → #1A73E8，rx=112）
  - 白色狗头剪影（头部中心 (310,235)，包围盒约 x151–469 × y68–397，狗头占画布 61.8% 黄金比例）
  - 深蓝眼睛（圆，中心 321,227，r=8.5）/ 鼻头（椭圆，中心 464,247，rx=7 ry=5）
  - 左下 3 条白色速度线

## 产物一：Android adaptive icon（适配图标）

### `flydog_adaptive_foreground.xml`（前景，必须使用）

- vector drawable：108dp 画布，viewport 保持 512×512 原始坐标
- 内容：白色狗头剪影（`d` 原样搬入，fillColor #FFFFFF）+ 眼睛/鼻头（转为 fillColor #0A2E6E 的独立 `<path>`；vector drawable 不支持 circle/ellipse 元素，已转 arc path）
- 缩放/平移参数（`<group>`）：
  - `scaleX=scaleY=0.6`
  - `translateX = 256 − 310×0.6 = 70`（头部中心 x=310 移到画布中心 x=256）
  - `translateY = 256 − 235×0.6 = 115`（头部中心 y=235 移到画布中心 y=256）
  - 变换后头部约 190.8×197.4 视口单位，完全落在 66dp 安全区（画布中央半径约 156.5 单位的圆）内

### `flydog_adaptive_background.xml`（背景，可选）

- 纯色 #1A73E8（v5 渐变终点色）矩形 vector
- 更简单的方案：不用这个文件，直接在 AdaptiveIconDrawable 上写 `android:background="#1A73E8"`

### 替换位置（本次只产出文件，替换动作由后续任务执行）

- 前景：`C:\FengProj\PlainApp\app\src\main\res\drawable\launcher_foreground.xml`（现有文件可替换为 flydog_adaptive_foreground.xml 的内容）
- 背景：`C:\FengProj\PlainApp\app\src\main\res\values\colors.xml` 中 `launcher_background` 色值。
  注意：检查发现该色值**当前已是 #1A73E8**，与定稿 Logo 渐变终点一致，大概率无需修改，执行时确认一下即可。

## 产物二：favicon data URI

### `favicon-data-uri.txt`

- **第一行**是可直接粘贴的 `<link>` 标签（`data:image/svg+xml,<全量 URL-encoded>`），`# → %23`、`< → %3C`、`" → %22`、空格 → `%20` 等，浏览器可直接解析（已做编码-解码往返校验）
- 编码内容长度：2762 字符
- 插入位置：`C:\FengProj\FlyGo\panel\static\index.html` 的 `<head>` **第一行**（现有 `<head>` 已确认存在）
- 行下方附有说明（勿粘贴进 HTML）与普通文件引用方案：
  - 若 panel 以后加静态路由，把 flydog-logo.svg 放到静态根为 `/favicon.svg`，改用 `<link rel="icon" type="image/svg+xml" href="/favicon.svg">`

## 生成方式

- 前景/背景 XML：由 flydog-logo.svg 手工转换（眼睛/鼻头 circle/ellipse → arc path；group 变换参数见上）
- data URI：Python `urllib.parse.quote(svg, safe="")` 全量编码生成，脚本用完即删
