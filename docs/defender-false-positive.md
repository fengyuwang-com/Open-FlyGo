# Defender 误报处理（Trojan:Win32/Bearfoos.A!ml）

> 状态：ML 启发式误报（`!ml` 后缀），非真实病毒。自编译小众 EXE 常见：
> 无流行度 + 新 hash + 行为画像（写计划任务自启、监听一堆端口、拉起
> powershell/wsl、调定时器）全踩中。

## 一、已发生（2026-09-05）

Defender 隔离了 `bin/flygo.exe` + 自启计划任务 `FlyGo`。恢复流程（已验证）：

```powershell
# 1. 加白（目录+进程，二选一即可，建议都要）
Add-MpPreference -ExclusionPath "C:\FengProj\FlyGo"
Add-MpPreference -ExclusionProcess "flygo.exe"
# 2. 从隔离区全恢复
& "$env:ProgramFiles\Windows Defender\MpCmdRun.exe" -Restore -All
# 3. 校验：bin 与新构建 hash 一致再启动
(Get-FileHash .\bin\flygo.exe).Hash -eq (Get-FileHash .\panel\target\release\flygo.exe).Hash
```

上报当版 hash（2026-09-06，commit 2ce0372）：

- SHA256: `43389E0FEC9A2E576385FDDBCDAD8906B9E199A0B1F62F9FF204D2701916367D`
- 大小: 4531200 bytes

## 二、治本三档（按推荐排序）

1. **微软误报申诉（免费，2 分钟/版）**：https://www.microsoft.com/wdsi/filesubmission
   以"Software Developer"身份提交当版 exe，选 `Incorrectly detected as
   malware`，附 SHA256，通常 24-48h 洗白该 hash。每发一版交一次。
2. **OV/EV 代码签名证书（花钱，治本）**：SmartScreen/Defender 信誉核心
   就是签名。有签名后误报率断崖式下降；EV 还能直接过 SmartScreen。
   一年几百美元（OV）起。
3. **代码侧减特征（不推荐单做）**：如自启动从计划任务改 Run 键/快捷方式、
   少监听端口——但会损失提权自启与功能，治标。当前功能优先，不动。

## 三、千家万户分发时的必备动作

没签名就分发 = 每个用户都可能弹红。顺序：先拿 OV 签名 → 安装器签名 →
再谈下载站/官网分发。裸 exe 群发是最差形态。
