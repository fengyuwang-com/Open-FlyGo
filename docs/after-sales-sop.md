# FlyGo 售后客服 SOP（T-13）

## 第〇步（永远的第一句）

> **"请打开面板 → 组件页 → 点「🩺 导出诊断包」，把下载的 JSON 文件发给我。"**

面板地址：`http://<机器IP>:8388`；诊断包也可以在安装目录 `data/` 下找到
（文件名 `flygo-diag-*.json`）。**没有诊断包，不开始排查。**

## 一、诊断包里有什么

| 区块 | 内容 | 用途 |
|---|---|---|
| panel | 版本号 / exe 路径与构建时间 / 内嵌 UI md5 / 各后端通道延迟 | 判断版本陈旧、UI 漂移、哪个子系统慢 |
| health | CPU / 内存 / 温度 / 网速 / Tailscale-Syncthing-SSH 状态灯 | 判断资源与基础服务 |
| services | 全部服务卡片探针结果 | 哪个服务掉了 |
| docker_wall | 服务墙 compose 清单 | 容器配置是否存在 |
| config_summary | 端口 / 登录层开关 / 安装根 | 环境差异（**不含任何密码**） |
| log_tail_redacted | flygo.log 最后 300 行（脱敏后） | 直接定位报错 |

## 二、脱敏保证

日志区块经过与 `scripts/check-secrets.sh` **同一套关键词规则**脱敏
（token / secret / password / api_key / access_key / private_key …），
命中即替换为 `***REDACTED***`，PEM 私钥块整行丢弃。用户可以放心发送，
客服收到的包理论上不含明文密钥（仍建议用户发包前自查一眼）。

## 三、常见故障速查（凭诊断包字段）

1. `panel.panel_version == "unknown"` → 安装目录不完整，让用户重装。
2. `services` 里 tailscale 不通 → 让用户在组件页看 Tailscale 卡片，重登账号。
3. `docker_wall` 为空 → compose 未配置（新版会自动生成 starter 配置，老版本让用户升级）。
4. `health.syncthing` 红灯 → 看 `log_tail_redacted` 里 syncthing 段，多为端口占用/防火墙。
5. Defender 误报（exe 被隔离）→ 发 `docs/defender-false-positive.md` 给用户照做。

## 四、升级路径

问题定位不了 → 让用户再发一次诊断包（现象复现后立刻导出，日志尾部才是现场）。
