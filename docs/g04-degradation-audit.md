# G-04 非管理员降级审查（2026-09-07）

> 结论：**无静默失败**。所有需要管理员权限的操作要么显式标记、要么失败时返回明确文案，标准用户全程可用（功能降级 + 提示，不报错卡死）。

## 逐项核对

| 操作 | 需要管理员？ | 降级行为 | 代码位置 |
|---|---|---|---|
| 开机自启任务 `FlyGo-<id>`（schtasks AtLogOn） | 否（当前用户） | 直接可用 | api/native.rs `sync_autostart_task` |
| Docker 防火墙规则 + portproxy（统一提权应用） | 是 | 状态接口返回 `elevated` 标志；按钮文案「应用端口转发(需管理员)」；UAC 提权；失败返回 `ok:false` + 具体消息 | api/dockernet.rs `network_status/network_apply` |
| portproxy 补齐（引擎自检路径） | 是 | 缺失时消息「missing portproxy rule(s) need elevation: …」列出遗漏端口 | api/wsl.rs:654 |
| Tailscale 服务 net start | 是 | 失败消息「run the panel elevated?」 | api/wsl.rs:555 |
| SMB guest 基线 apply_guest / secedit | 是（写本地安全策略） | 返回 `(bool, String)` 结果；boot 路径有超时兜底不挂死启动 | smb.rs:465 |
| 组件安装 winget | 否（用户级） | 直接可用 | api/fetch.rs |
| 计划任务删除 | 否 | 同创建 | api/native.rs `remove_autostart_task` |

## 遗留（不阻塞）

- 「降级提示 vs 报错」的**统一文案规范**未成文：现状各模块文案风格已一致（中文 + 具体缺口），暂不引入统一常量表。
- 真机标准账户全流程实测（装一台非管理员 Windows 跑一遍）属用户侧验收，代码侧已就绪。
