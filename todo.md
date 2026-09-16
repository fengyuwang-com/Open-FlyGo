# FlyGo 任务账本（todo.md · 唯一根本）

> **账本规则（2026-09-10 董事长立）**：本文件**只写还没做的事**。做完的事必须删除（历史去 git log 与 FENGMEM.md 找，本文件不留 `[x]` 存档）。每轮结束主 Agent 核对：用户说了什么 → 未入账的入账 → 已完成的删除。

## 🔴 用户侧待办（AI 无法代做）

- [ ] 真机验证通宵三项：托盘退出后 flygo.exe 进程真消失 / 全程无黑色弹窗 / 下次开机从 data/logs/flygo.log 读 `smb.boot step * took Xms` 回填开机基线（基线 100-154s）
- [ ] T-11 控制面 VPS 部署（DEPLOY-VPS.md 就绪）
- [ ] T-12 release keystore 签名 + 真机装 1.0 APK 实测
- [ ] G-05 Tailscale 自有账号登录闭环（新号全程录屏）
- [ ] DNS 解析 + 官网 flygo.fengyuwang.com 部署上线

## 📋 进行中

- [ ] 平台 WebUI 优雅截图（部分完成 c957fe4）：FengMedia 首页+Prompt 工坊、FengInvest 首页桌面版已截（docs/screenshots/2026-09-11_*，官网仓库 assets/img/shots/ 已放文件未提交）；**BLOCKED**：FlyGo 面板 8388 未运行 → 面板截图 + 电脑版其余页 + 手机版两套等董事长开机 panel 后补；官网推送需用户批准

## 📋 排队（下一批）

- [ ] phone-sync 审查 P3 尾巴（合并后跟进，非阻塞）：①GET→改→PUT 无并发防护/重试（面板单人可接受，多标签页并发最后写入者覆盖）②accept 已存在文件夹强制 unpaused 可能复活用户手动暂停的文件夹③pending 卡片不自动轮询（配对后需手点刷新）④JS 层无自动化测试（P1 笔误因此漏网，可加静态断言守卫）
- [ ] feat/host-metrics 增强版网速采样（delta_rates 防尖峰+陈旧窗 re-baseline、parse_temp_output 单测、metrics_api 集成测试）未进 main——main 现为 d1079dc 简化版；待董事长裁决 cherry-pick 或放弃（裁决前该分支不删）

- [ ] 深度连接实施（手机客户端 ↔ 面板 ↔ 控制面；PlainApp 无 HTTP 客户端模式 = 差异化空白）
- [ ] PlainApp 移动网盘功能实施（docs/plainapp-integration.md）
- [ ] 手机端实测验收（Syncthing-Fork 扫码 → 同步 → 面板看到手机）
- [ ] 真机裸机装 Setup ≤30min 验收 + 真机标准账户（非管理员）降级实测（G-04 真机侧）

## 🗓 以后

- [ ] W3 互备编排（Syncthing 一键配对 + staggered 版本控制）
- [ ] iOS 评估（PWA 不阻塞）
- [ ] 电脑三平台 + 安卓 APK 打包管线
- [ ] VIP 订阅收费——**不做现在**
