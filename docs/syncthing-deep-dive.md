# Syncthing 深入研究（T-07）

> 2026-09-06 · 结论先行：引擎本身无需任何改造；T-09 只做三件事——必装（manifest pin v2.1.3）+ 按需托管（daemon）+ 复用现有编排（M06/M07 不动）。
> 证据等级：A=本机实测（C:/FengNAS/syncthing，v2.1.3 Hafnium Hornet）· B=官方文档（docs.syncthing.net，2026-09-06 实读）· C=仓内代码（panel 调用点）。

---

## 一、版本基线（A）

- 本机：`syncthing v2.1.3 "Hafnium Hornet" (go1.26.5 windows-amd64)`，27.7MB 单文件，`--version` 实测。
- 状态：未运行（8384 无监听）；历史配对 6 设备；2 文件夹。
- v2 迁移证据：`index-v2/` + `index-v0.14.0.db-migrated` 存在——本机已完成 v1→v2 数据库迁移。
- **铁律（由上得出）**：升级只升不降（v2 跑过后不再回 v1）；manifest pin 版本，升级流程=停→换二进制→起→`GET /rest/system/status` 核对版本。

## 二、进程模型与部署（A+C）

- 单二进制，无依赖（Go 静态编译）；数据目录由 `-home` 指定（默认 `%LOCALAPPDATA%/Syncthing`，我们显式指定）。
- GUI/API：8384 **仅本机绑定**（`127.0.0.1:8384`），外网永远不暴露；认证=请求头 `X-API-Key`（值在 `<home>/config.xml` 的 `<apikey>`，daemon 启动时读入内存，**永不打日志**）。
- 启动参数（T-09 用）：`syncthing -home <dir> -gui-address 127.0.0.1:8384 -no-browser`，隐藏窗口（CREATE_NO_WINDOW）。
- **身份即文件**：`<home>/key.pem + cert.pem` = 设备 ID。重装/迁移**必须保留 `-home`**，否则等于换了一台设备，全部配对作废。这是 T-09 最高优先级约束。

## 三、`-home` 归属决策（A，本节是 T-09 的直接输入）

- 现状： canonical home = `C:/FengNAS/syncthing/config`（6 配对 + vfengvault + staggered 参数全在里面）；`config.rs` 的 root-relative 默认是 `external/syncthing/config`（空，不存在）。
- 决策：daemon 的 `-home` 解析顺序 = `FENGPAGE_SYNCTHING_HOME` env > **legacy 现存目录**（保留配对）> root-relative 新目录（新用户）。
- 禁止：安装器/组件更新**永远不删 `-home`**（只换 exe）；卸载保留数据（只停不删，与容器卸载语义一致）。

## 四、REST 操作面（C：仓内实际调用的 7 个端点 + B 语义）

| 端点 | 用途 | 调用方 | 轮询/触发 |
|---|---|---|---|
| `GET /rest/system/status` | 版本/ID/在线时长（版本核对、配对页 device ID 来源） | pair/sync | 配对时 + 自检 |
| `GET /rest/system/connections` | 对端在线/延迟/流量（健康监控） | M07 健康 | 60s |
| `GET /rest/system/config` | 全量配置快照（编排前读基线） | pair | 配对/改配时 |
| `GET/POST /rest/config/devices` | 互加设备（邀请码换 device ID 后写入） | M07 配对 | 触发 |
| `GET/POST /rest/config/folders` | 目录下发（staggered 参数随 folder 写死） | M07 配对 | 触发 |
| `GET /rest/db/status?folder=` | need files/进度（恢复进度页） | M07 恢复页 | 恢复时 5s |
| （预留）`GET /rest/db/completion` | 按设备完成度（恢复页增强用，未接入） | — | T-09 可选 |

- 认证：全部请求带 `X-API-Key`；连接失败=引擎未起（daemon 按需拉起，不是报错给用户看）。
- 注意：`POST /rest/config/*` 需要重启生效？——现代版本 config API 即时生效（需实测确认，列入 T-09 联调清单，不写死结论）。

## 五、Folder types（B，手机 send-only 的法理依据）

- **send-receive**（默认）：节点间互备用。双向传播=误删也传播，所以互备目录**强制 staggered**（M07 已有）。
- **send-only**：手机端唯一允许的类型。官方语义：远端变更全部忽略（只收不应用，显示 out of sync）；`Override Changes` 会把本机状态强推全网——**手机端永远不点，引导页不教**。
- **receive-only**：镜像/备份目的端备选（本地改动不外发 + `Revert Local Changes`）。链路①的 robocopy 镜像目录若改走 Syncthing，用此类型；P0 仍用 robocopy（不动）。
- 验收红线（已在 flygo-app-plan §8）："节点删文件不反向删手机"——由 send-only 语义保证，T-09 复验。

## 六、Staggered 版本控制（A+B）

- 机制：远端删/改到达时，旧版移入 `.stversions`（默认在共享目录内；自定义路径**必须同分区**，跨分区 move 会失败——B 明文）。
- 关键语义：**版本只收远端变更**（B 原文：本地自己改的文件不留版）。误删回滚演示必须从对端删起才看得到效果。
- 本机实参（vfengvault）：`type=staggered, cleanoutDays=28, maxAge=365000, cleanupIntervalS=3600`。
- M07 默认（10 版/7 天）与本机实参不一致——**统一为 M07 口径为唯一标准**，本机历史目录 T-09 时顺手对齐（改 folder 即 POST config，不动数据）。

## 七、config.xml 要点（A）

- `<folder id label path type rescanIntervalS fsWatcherEnabled>`：本机 vfinger `sendreceive + fsWatcher(10s延迟) + rescan 3600s`——watcher 为主、定时为兜底，T-09 新目录照抄此组合。
- `<device>` ×6：配对只增不改；邀请码流程换回的 ID 经 `POST /rest/config/devices` 写入（C）。
- `<gui>`：enabled + apikey；`gui-password.txt` 存在——daemon 读 apikey，不碰密码文件。
- 备份文件（`config.xml.bak-*`、`config.xml.v0/v37`）是 syncthing 自带轮转，不要删。

## 八、配对坑清单（B+C，M07 已踩过的别再踩）

1. 设备 ID 靠二维码/文本交换，**禁止手抄**（base32 分段，抄错一位=另一台设备）。
2. 单边添加=pending：A 加 B 而 B 没加 A，连接永远 `pending`——配对页必须双向确认态（M07 invite/accept/poll 已覆盖）。
3. 直连优先：Tailscale 下走 TCP 22000 直连；连不上才 fallback 中继——慢≠坏，健康页文案写清（"中继中" vs "离线"是两种状态）。
4. introducer/autoAccept 默认全关（防陌生设备自动进网）。
5. 手机 send-only 目录在节点侧显示 out of sync 是**正常**，不是故障（ equitable：面板文案已/待改为"汇聚中"而非红字）。

## 九、`.stignore`（A：本机 FengVaultBackup 无此文件）

- 现状无 ignore 文件=全量同步。T-09 实施项：给汇聚目录加默认模板（`(?d).tmp / ~$* / .~lock.* / Thumbs.db / .DS_Store`），每目录可追加。`(?d)` 前缀=删了也删远端可删标记，模板用 `(?d)` 保平安。

## 十、给 T-09 的实施清单（不动编排层）

1. manifest：syncthing archive 项 pin v2.1.3 + sha（winget 主通道 `Syncthing.Syncthing` 同版本，可互为校验）。
2. daemon：`-home` 按 §三顺序解析；`8384` 本机；apikey 运行时读；CREATE_NO_WINDOW；按需拉起（pair/sync/health 任一调用前 ensure）。
3. 对齐：vfengvault staggered 改 M07 口径；汇聚目录加 `.stignore` 模板。
4. 联调：POST config 即时生效确认；2 副本闭环；send-only 红线；误删回滚录屏。
5. 升级：T-10 安装器只换 exe；`-home` 碰都不碰。

---

*T-07 完成 2026-09-06 · 输入：T-09/W-C4 实施 · M06/M07 编排逻辑零改动。*
