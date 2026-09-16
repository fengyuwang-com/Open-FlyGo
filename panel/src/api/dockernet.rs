// /api/docker/network + /api/docker/autostart — 系统级统一 Docker 网络
//
// 用户明确要求：Docker 自启动 / 放行网段 / 防火墙规则必须是**系统级统一**的，
// 而不是"每个容器一套配置"（否则总有容器打不开）。
//
// 本模块做三件事，全部以"compose 端口的全集"为输入（同一来源 wsl::compose_host_ports）：
//   1) 一个系统级放行网段 allowed_cidr（默认 Tailscale 100.64.0.0/10），持久化在
//      state.json，应用于所有 compose 宿主端口；
//   2) 一个统一的入站规则组 FlyGo-DockD-<n>（≤8 端口/条），覆盖全部 compose 端口 + 同一个 remoteip；
//   3) engine 开机自启动开关 docker_autostart：面板下次启动时自动把引擎目标置 ON
//      （WSL/dockerd/容器随面板恢复），由 wsl::init_state 消费。
//
// 应用动作（firewall + portproxy）走 access.rs 的 UAC 提权通道，一次确认全做完，
// 不做重复 UAC 弹窗。
use crate::ApiResult;
use axum::Json;
use serde::Serialize;

/// 统一规则名前缀（与 access.rs 的 FlyGo-TS-/LAN-/ALL- 区分开，是一套系统级全集）。
const RULE_PREFIX: &str = "FlyGo-DockD-";
/// 每条防火墙规则容纳的最大端口数（netsh 每条规则 localport 上限 10，取 8 留余量）。
const MAX_PORTS_PER_RULE: usize = 8;

/// 单条规则的内容（name, additional args）。
fn firewall_entries(ports: &[u16], cidr: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, chunk) in ports.chunks(MAX_PORTS_PER_RULE).enumerate() {
        let name = format!("{RULE_PREFIX}{i}");
        let portlist = chunk
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        out.push((
            name,
            format!("dir=in action=allow protocol=TCP localport={portlist} remoteip={cidr}"),
        ));
    }
    out
}

/// PowerShell 单引号转义（与 access.rs 相同，避免注入）。
fn sq(s: &str) -> String {
    s.replace('\'', "''")
}

/// 生成一次提权应用脚本：清掉旧 FlyGo-DockD-*，重建统一规则组，再补齐 portproxy
/// 缺失项（delete+add，幂等），最后写 RESULT=OK/ERROR 到 run_op 的 result 文件。
/// all 值（ip、port、cidr）都经过严格字符集校验，不转义。
fn build_apply_script(
    ip: &str,
    ports: &[u16],
    entries: &[(String, String)],
    result_path: &std::path::Path,
) -> String {
    let result = sq(&result_path.to_string_lossy());
    let port_csv = ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut script = format!(
        "$ErrorActionPreference = 'Continue'\n$resultFile = '{result}'\n$failed = @()\n\
         Get-NetFirewallRule -DisplayName '{RULE_PREFIX}*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue\n"
    );
    for (name, extra) in entries {
        script.push_str(&format!(
            "$name = '{name}'\n\
             $null = netsh advfirewall firewall delete rule name={name} 2>&1\n\
             $null = netsh advfirewall firewall add rule name={name} {extra} 2>&1\n\
             $exists = [bool](Get-NetFirewallRule -DisplayName {name} -ErrorAction SilentlyContinue)\n\
             if (-not $exists) {{ $failed += '{name}' }}\n",
            name = sq(name),
            extra = extra
        ));
    }
    for p in ports {
        script.push_str(&format!(
            "$null = netsh interface portproxy delete v4tov4 listenaddress={ip} listenport={p} 2>&1\n\
             $null = netsh interface portproxy add v4tov4 listenaddress={ip} listenport={p} connectaddress=127.0.0.1 connectport={p} 2>&1\n\
             $last = $LASTEXITCODE\n\
             if ($last -ne 0) {{ $failed += 'portproxy-{p}' }}\n"
        ));
    }
    script.push_str(&format!(
        "if ($failed.Count -eq 0) {{ $res = 'RESULT=OK docknet-applied; ports={port_csv}' }} else {{ $res = 'RESULT=ERROR failed=' + ($failed -join ',') }}\n\
         [IO.File]::WriteAllText($resultFile, $res, [Text.Encoding]::UTF8)\n"
    ));
    script
}

/// 当前存在的统一规则名（非提权只读探测）。
fn dockd_rule_names() -> Vec<String> {
    let out = crate::api::access::ps(
        "Get-NetFirewallRule -DisplayName 'FlyGo-DockD-*' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty DisplayName",
    );
    out.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l.starts_with(RULE_PREFIX))
        .collect()
}

/// status 结构（GET 返回）。pub: 被 main.rs 的路由占用，axum handler 的返回类型必须可见。
#[derive(Serialize)]
pub struct NetStatus {
    ok: bool,
    autostart: bool,
    allowed_cidr: String,
    tailnet_ip: Option<String>,
    ports: Vec<u16>,
    portproxy_missing: Vec<u16>,
    firewall_rule_names: Vec<String>,
    firewall_complete: bool,
    elevated: bool,
    message: String,
}

fn current_status() -> NetStatus {
    let ports = crate::api::wsl::compose_host_ports();
    let allowed_cidr = crate::api::wsl::allowed_cidr();
    let autostart = crate::api::wsl::autostart_enabled();
    let ip = crate::api::wsl::tailnet_ip();
    let missing = ip
        .as_ref()
        .map(|ip| crate::api::wsl::missing_portproxy(&ports, ip))
        .unwrap_or_default();
    let present = dockd_rule_names();
    // 期望的规则数：按 8 端口/条 分块
    let expected = ports.chunks(MAX_PORTS_PER_RULE).count();
    let complete = !ports.is_empty() && present.len() >= expected;
    let message = if ports.is_empty() {
        "docker-compose.yml 未解析到宿主端口".to_string()
    } else {
        format!(
            "统一规则组 {} 条 / {expected} 条，portproxy 缺失 {} 个，放行网段 {allowed_cidr}",
            present.len(),
            missing.len()
        )
    };
    NetStatus {
        ok: true,
        autostart,
        allowed_cidr,
        tailnet_ip: ip,
        ports,
        portproxy_missing: missing,
        firewall_rule_names: present,
        firewall_complete: complete,
        elevated: crate::api::access::is_elevated(),
        message,
    }
}

/// GET /api/docker/network — 系统级 Docker 网络与开机自启动状态。
pub async fn network_status() -> ApiResult<Json<NetStatus>> {
    let status = tokio::task::spawn_blocking(current_status)
        .await
        .unwrap_or_else(|_| NetStatus {
            ok: false,
            autostart: false,
            allowed_cidr: String::new(),
            tailnet_ip: None,
            ports: Vec::new(),
            portproxy_missing: Vec::new(),
            firewall_rule_names: Vec::new(),
            firewall_complete: false,
            elevated: false,
            message: "spawn_blocking 执行失败".to_string(),
        });
    Ok(Json(status))
}

/// POST /api/docker/network  body: {"action":"apply"}
/// 一次提权把所有 compose 端口接入统一规则组 + 补齐 portproxy（遗漏项加固 / 网段变更后重灌）。
pub async fn network_apply() -> ApiResult<Json<serde_json::Value>> {
    let ports = crate::api::wsl::compose_host_ports();
    if ports.is_empty() {
        return Ok(Json(serde_json::json!({
            "ok": false, "message": "docker-compose.yml 未解析到宿主端口，无可应用规则"
        })));
    }
    let cidr = crate::api::wsl::allowed_cidr();
    let Some(ip) = crate::api::wsl::tailnet_ip() else {
        return Ok(Json(serde_json::json!({
            "ok": false, "message": "tailscale ip -4 不可用，无法配置 portproxy（先用面板开引擎/过自检）"
        })));
    };
    let entries = firewall_entries(&ports, &cidr);
    let result_path = std::env::temp_dir().join(crate::api::access::OP_RESULT);
    let script = build_apply_script(&ip, &ports, &entries, &result_path);
    let (ok, message) = tokio::task::spawn_blocking(move || crate::api::access::run_op(&script))
        .await
        .unwrap_or((false, "spawn_blocking 执行失败".to_string()));
    if ok {
        // 应用后再核对一次状态，让前端立刻拿到真实缺口
        let status = current_status();
        return Ok(Json(serde_json::json!({
            "ok": true,
            "message": format!("已应用：{}", message),
            "status": status,
        })));
    }
    Ok(Json(serde_json::json!({ "ok": false, "message": message })))
}

/// POST /api/docker/autostart  body: {"enabled":bool, "allowed_cidr"?:cidr}
/// 系统级开机自启动开关 + 可选更新统一放行网段（一起持久化，重启生效）。
pub async fn autostart(body: axum::Json<serde_json::Value>) -> ApiResult<Json<serde_json::Value>> {
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(raw) = body.get("allowed_cidr").and_then(|v| v.as_str()) {
        let cidr = raw.trim();
        if !crate::api::access::valid_cidr(cidr) {
            return Ok(Json(serde_json::json!({
                "ok": false, "message": "网段格式不对（如 192.168.1.0/24 或 100.64.0.0/10）"
            })));
        }
        crate::api::wsl::set_allowed_cidr(cidr);
    }
    crate::api::wsl::set_autostart(enabled);
    let message = if enabled {
        "已开启开机自启动：下次面板启动会自动拉起 Docker 引擎与容器" 
    } else {
        "已关闭开机自启动（当前引擎状态不受影响）"
    };
    Ok(Json(serde_json::json!({
        "ok": true,
        "enabled": enabled,
        "allowed_cidr": crate::api::wsl::allowed_cidr(),
        "message": message,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 规则分块：≤8 端口/条，名连续，remoteip 统一为放行网段，localport 用端口全集。
    #[test]
    fn firewall_entries_chunks_ports_and_single_cidr() {
        let ports: Vec<u16> = (5244..5260).collect(); // 16 个端口
        let entries = firewall_entries(&ports, "100.64.0.0/10");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "FlyGo-DockD-0");
        assert_eq!(entries[1].0, "FlyGo-DockD-1");
        assert!(entries[0].1.contains("remoteip=100.64.0.0/10"));
        let p0: Vec<&str> = entries[0]
            .1
            .split("localport=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .split(',')
            .collect();
        assert_eq!(p0.len(), 8, "first rule must hold <=8 ports");
        assert_eq!(p0[0], "5244");
        assert_eq!(p0[7], "5251");
        // 空端口集 -> 无规则
        assert!(firewall_entries(&[], "100.64.0.0/10").is_empty());
    }

    /// 生成的脚本必须：先清旧组、逐条建规则、逐个补 portproxy、写 RESULT。
    #[test]
    fn apply_script_has_expected_shape() {
        let entries = firewall_entries(&[5244, 5005], "100.64.0.0/10");
        let script = build_apply_script(
            "100.101.102.103",
            &[5244, 5005],
            &entries,
            std::path::Path::new(r"C:\T\flygo_access_op.txt"),
        );
        assert!(script.contains("Remove-NetFirewallRule"));
        assert!(script.contains("localport=5244,5005"));
        assert!(script.contains("listenaddress=100.101.102.103"));
        assert!(script.contains("connectport=5005"));
        assert!(script.contains("[IO.File]::WriteAllText"));
        assert!(script.contains("RESULT=OK docknet-applied; ports=5244,5005"));
    }

    /// run_op 的从属文件名与 access.rs 常量保持一致（dockernet 脚本写同一 result 文件）。
    #[test]
    fn op_result_constant_alignment() {
        // 无法直接引用 access 私有常量；这里只保证 dockernet 里没有重复定义冲突。
        assert_eq!(crate::api::access::OP_RESULT, "flygo_access_op.txt");
    }
}
