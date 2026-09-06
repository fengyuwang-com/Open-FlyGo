# fetch manifest 版本升级操作手册（G-06）

> 适用：`panel/src/api/fetch.rs` 内置 manifest（`builtin_manifest()`）。
> 原则：winget 为主通道（官方 manifest 自带校验），自备下载只是兜底；**凡兜底，必 pin sha256**。

## 一、升级步骤（以 Syncthing v2.1.3 → vN 为例）

1. **查上游发布**
   - GitHub Releases：`https://github.com/syncthing/syncthing/releases`，找 `syncthing-windows-amd64-vN.zip`。
2. **取新 sha256 并交叉验证（两处独立来源必须一致）**
   - 上游发布页/校验文件（sha256sum）；
   - winget 社区 manifest：`winget show Syncthing.Syncthing --versions` 确认版本存在，再在
     `microsoft/winget-pkgs` 仓库对应版本 yaml 里核对 `InstallerSha256`。
3. **改 `builtin_manifest()`**（fetch.rs）四处：
   - `version: "vN"`；
   - `urls:` 两条（ghfast 镜像在前，官方在后），URL 里的版本号同步替换；
   - `sha256: "<新哈希>"`；
   - `target_rel: "syncthing-windows-amd64-vN/syncthing.exe|external/syncthing/syncthing.exe"`（zip 内路径带版本号）。
4. **单测**：`cargo test`（fetch 相关断言含 pin 存在性与 URL 形态，改版后必须全绿）。
5. **真机验证一次**：删 `external/syncthing/syncthing.exe` → 自检页重新安装 → 确认 sha 校验通过、服务起来。
   注意：只换 exe，**绝不动 `-home` 身份目录**。

## 二、其他条目规则速查

| 条目 | 主通道 | 兜底 | pin 要求 |
|---|---|---|---|
| syncthing | winget `Syncthing.Syncthing` | GitHub zip（ghfast 镜像） | **sha256 必 pin** |
| tailscale | winget `Tailscale.Tailscale` | 官方 MSI | 免 pin（Authenticode 信任锚） |
| docker-desktop | winget `Docker.DockerDesktop` | 无（数 GB，走手动指南） | — |
| flygo-mobile APK | `FENGPAGE_APK_URL` 环境变量 | 同左 | 建议发布时 pin |

## 三、整体替换（不走改码）

设环境变量 `FENGPAGE_FETCH_MANIFEST=<绝对路径>` 指向自备 JSON（结构与
`GET /api/fetch/components` 返回条目一致），可整体覆盖内置 manifest。
适合紧急热修：发新 JSON → 重启面板生效，不用重编 exe。

## 四、铁律

- 国内镜像优先（ghfast 等），官方 URL 永远放第二条兜底；
- sha256 未 pin 的兜底条目视为**未完成**（评审不通过）；
- 版本 pin 是有意设计（可复现安装），升级时才改，不许删 pin。
