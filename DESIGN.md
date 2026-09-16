# FlyGo DESIGN — UI 设计宪法（所有界面唯一权威）

> 2026-09-07 用户两轮拍板定稿：**配色 = 原版简洁蓝白**（冷 slate + 单一强调蓝 #2563eb）；**结构细节 = 参照 `~/llm-text-processor/frontend/`**（阴影层次、按键布局、侧栏细节、动效——它的好不在配色在结构）。
> 适用范围：面板 `panel/static/index.html`、`login.html`、官网 `site/`、APK 内界面/WebView、控制面 admin 页——**一切 FlyGo 界面**。实现与本文冲突时以本文为准。
> 判定总纲：**FlyGo 是"给人用的工具"**。信息密度与操作效率优先。

## 1. 灵魂五条

1. **理性客观，不坑人**：文案不自夸不说教（禁"强大/极致/秒开"），给真实预期（时长、阈值、后果、失败原因+怎么办）。
2. **冷 slate + 单一强调蓝**：全系统只有灰蓝 + 一个 accent 蓝；暖色（绿/黄/红）只做状态指示，绝不进品牌/按钮主色/装饰。
3. **强调来自颜色与笔画，不来自字重**：字重固定 400/500/600 三档，无斜体，不堆 bold；系统 UI 字体栈，无品牌字体。
4. **深色模式 = 两套显式值**：CSS 变量在主题根一次性定义；任何新元素必须两主题都定义，禁止裸 `#fff`/`#000`。
5. **一视图一焦点**：每个视图一个主任务；辅助说明进 `.sub` 或折叠区。

## 2. 核心 Token（配色，原版蓝白）

| Token | Light | Dark | 职责 |
|---|---|---|---|
| `--bg` | `#f3f5f8` | `#0f1115` | 页面底（侧栏同底色，靠 border 分区） |
| `--panel` | `#ffffff` | `#15181e` | 一级卡片 |
| `--panel2` | `#f0f2f6` | `#1a1e26` | 二级卡片/内嵌 |
| `--border` | `#dbe0ea` | `#262c36` | 全部 1px 描边 |
| `--text` | `#1a2433` | `#e5e7eb` | 主文本 |
| `--muted` | `#5b6b80` | `#8b93a1` | 次文本 |
| `--accent` | `#2563eb` | `#4c9aff` | **唯一**强调蓝（主按钮/焦点/选中/链接） |
| `--accent-hover` | `#1d4ed8` | `#3b8ae6` | 主色 hover |
| `--accent-bg` | `#e3ecfb` | `#20314a` | 强调浅底（选中态/徽章/nav 激活） |
| `--focus-ring` | `0 0 0 3px rgba(37,99,235,.15)` | `rgba(76,154,255,.18)` | 输入框/焦点光晕 |
| `--ok/--warn/--err` | `#059669/#d97706/#dc2626` | `#34d399/#f59e0b/#ef4444` | 语义色（仅状态点/状态字/进度条/危险按钮/警告卡） |

## 3. 结构细节（从 llm-text-processor 移植，2026-09-07 v2 落地）

这些是"显得精心制作"的来源，**改 UI 时必须保持**：

- **侧栏**：与页面同底色 + `border-right` 分区（不用深色底）；品牌 logo 为**圆形 accent 底 + 同色柔投影**（`0 4px 10px rgba(accent,.3)`）；导航项 `border-radius:8px`，hover=浅灰底，激活=accent 浅底 + accent 字（不做色条 inset）。
- **卡片阴影两档**：常态 `--shadow-sm`（1px 环 + 2px 8px 软影）；可点卡片 hover 抬升（边框转 accent 半透明 + `0 6px 16px` 软影），过渡 `300ms`。卡片圆角 12px，按钮/输入框 8px。
- **输入框**：白(panel)底 + `--shadow-sm`，focus = accent 边 + `--focus-ring` 光晕（`transition:border-color .2s, box-shadow .2s`）。
- **全局细节**：6px 暖灰滚动条（thumb=border 圆角 10px）；`::selection` = accent 底白字；`-webkit-font-smoothing:antialiased`。
- **动效（微交互=优雅的来源，2026-09-07 二轮精读提炼）**：视图切换 `viewIn .45s cubic-bezier(0.2,0.8,0.2,1)`（淡入+10px 上滑）；**主按钮物理浮起**——常态带 accent 色投影 `0 4px 12px rgba(accent,.25)`，hover `translateY(-1px)`，按下回落 `translateY(0)`（不是变色是浮起）；可点卡片/tile hover `translateY(-2px)` + 抬升影；**进度条填充 `transition:width .7s ease-out`**（数字在流动）；标题 `letter-spacing:-.01em`、小节标签 `+.05em`（排版呼吸）；`prefers-reduced-motion` 全降级。
- 二级按钮（.btn.off）：白底描边，hover 边框转 accent 半透明 + 字转 accent。
- **顶栏工具条=一张白卡**：`.tiles` 整体白底描边圆角+环影，内部 tile 透明无边框、hover 浅灰底微抬升（状态灯 tile 无 hover）。
- **空态=英雄卡**：JS 注入的 `.dim` 空态文本由 CSS `:only-child` 挂钩升级为虚线大卡（12px 圆角 + 56px 圆角图标块 + 居中文案，图标 hover `scale(1.1) rotate(3deg)` .5s）；选择器清单见 index.html「优雅空态英雄卡」段，新增空态容器照抄一行选择器即可，零逻辑改动。

## 4. 排版与白空间

- 系统字体栈（Windows 中文显式声明 Microsoft YaHei）；字号/行高/圆角走刻度表，无魔法数字。
- 白空间是特性：卡片间隙统一刻度，层级靠留白+阴影，不靠加深底色堆砌；卡片实色优先，覆盖层才毛玻璃。

## 5. 文案检查表（写任何界面前过一遍）

不用第二人称（祈使句/陈述句）· 不自夸 · 时长给数字 · 危险操作写后果 · 错误态="原因+怎么办" · 空态="是什么+怎么开始" · 状态词全站统一 · emoji 每区块至多一个。

## 6. 排除清单

个人署名页脚（FlyGo 页脚=产品名+版本号）· hero 大图 · 名片墙 · 博客排版 · 艺术页例外（无例外页）· 品牌字体。
**采用**：下拉菜单、面包屑（≤3 级）、语言切换（中文主）、功能性过渡动效。
