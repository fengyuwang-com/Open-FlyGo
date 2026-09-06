// /api/docker — docker 服务清单
// 直接从 docker-compose.yml 动态解析（与 Portainer/compose 完全一致），
// 并按容器真实运行状态标记 running。刻意不硬编码清单，避免与实际容器脱节。
use crate::ApiResult;
use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct DockerService {
    pub key: String,
    pub name: String,
    pub desc: String,
    pub port: u16,
    pub icon: String,
    /// compose 里的 container_name（未写时默认等于 key）；真实容器名可能与 key 不同
    /// （如 key=immich-server、container_name=immich_server），供前端映射状态与启停按钮
    pub ctn: String,
    pub running: bool,
    /// User-overridable card URL (may contain ${FENG_IP} template); None =
    /// derive http://host:port from the port field on the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Hidden from the overview card wall (still managed on the docker page).
    pub hidden: bool,
}

/// Bundled starter compose (G-02): a fresh install has no compose file,
/// which used to leave the services wall permanently empty. Written to
/// <root>/data/docker-compose.yml on first use when nothing else is
/// configured; the user is free to edit or replace it afterwards.
const STARTER_COMPOSE: &str = include_str!("../../static/docker-compose.starter.yml");

/// compose 文件路径由 config 解析（FENGPAGE_DOCKER_COMPOSE 可覆盖）。
/// 无任何配置文件时落到内置 starter seed（首次自动落盘 data/）。
fn compose_path() -> Option<std::path::PathBuf> {
    let configured = crate::config::get().docker_compose.clone();
    if let Some(p) = &configured {
        if p.exists() {
            return configured;
        }
    }
    // No usable configured file -> starter pack.
    let seed = crate::config::get().root.join("data").join("docker-compose.yml");
    if !seed.exists() {
        match std::fs::write(&seed, STARTER_COMPOSE) {
            Ok(_) => {
                crate::log::log(
                    "INFO",
                    "docker.compose",
                    &format!("starter compose written to {}", seed.display()),
                );
            }
            Err(e) => {
                crate::log::log(
                    "WARN",
                    "docker.compose",
                    &format!("starter compose write failed: {e}"),
                );
                return configured; // keep old behavior when the disk refuses
            }
        }
    }
    Some(seed)
}

/// compose 文本 -> (键名, 容器名) 列表的纯解析（供 compose_services 与单测共用）。
fn parse_services(contents: &str) -> Vec<(String, String)> {
    let mut services: Vec<(String, String)> = Vec::new();
    let mut in_services = false;
    let mut cur: Option<(String, String)> = None;
    for line in contents.lines() {
        // 顶层键：无缩进、以字母开头、以 : 结尾
        if !line.starts_with(' ') && !line.starts_with('\t') && !line.trim().is_empty() {
            let t = line.trim();
            if t == "services:" { in_services = true; continue; }
            if t == "volumes:" || t == "networks:" { in_services = false; continue; }
            continue;
        }
        if !in_services { continue; }
        // 顶层 services 定义：恰好 2 空格缩进、以 name: 结尾、非注释
        if line.starts_with("  ") && !line.starts_with("    ") && !line.trim_start().starts_with('#') {
            let t = line.trim();
            if t.ends_with(':') && !t.contains(": ") {
                let name = t.trim_end_matches(':').trim();
                if !name.is_empty() && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
                    if let Some(prev) = cur.take() { services.push(prev); }
                    cur = Some((name.to_string(), name.to_string()));
                }
            }
            continue;
        }
        // 服务定义块内的 container_name: xxx（容器名可能与键名不同，如 immich_server）
        if let Some((_, ctn)) = cur.as_mut() {
            let t = line.trim_start();
            if let Some(v) = t.strip_prefix("container_name:") {
                let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                if !v.is_empty() { *ctn = v; }
            }
        }
    }
    if let Some(prev) = cur.take() { services.push(prev); }
    services
}

/// 从 compose 文件读取并解析；文件缺失/未配置返回空清单。
fn compose_services() -> Vec<(String, String)> {
    let Some(path) = compose_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(path) {
        Ok(c) => parse_services(&c),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests (pure parsing; no filesystem, no docker)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_compose_parses_with_expected_services() {
        let svcs = parse_services(STARTER_COMPOSE);
        let keys: Vec<&str> = svcs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["alist", "jellyfin"], "active services only; immich stays commented out");
        let alist = svcs.iter().find(|(k, _)| k == "alist").unwrap();
        assert_eq!(alist.1, "alist");
    }

    #[test]
    fn parse_handles_container_name_and_top_level_keys() {
        let yml = "services:\n  immich-server:\n    container_name: immich_server\nvolumes:\n  x:\n";
        let svcs = parse_services(yml);
        assert_eq!(svcs.len(), 1);
        assert_eq!(svcs[0], ("immich-server".to_string(), "immich_server".to_string()));
    }
}

/// 尽力从 compose 里该服务的定义提取端口（无则 0）
fn service_port(name: &str) -> u16 {
    let Some(path) = compose_path() else {
        return 0;
    };
    let contents = std::fs::read_to_string(path).unwrap_or_default();
    let mut in_svc = false;
    for line in contents.lines() {
        let t = line.trim();
        if !in_svc {
            if line.starts_with("  ") && t == format!("{name}:") { in_svc = true; }
            continue;
        }
        if line.starts_with("  ") && !line.starts_with("    ") && t.ends_with(':') {
            break; // 下一个顶层服务
        }
        // 端口行：- "xxxx:xxxx" 或 - "xxxx:80"
        if t.starts_with('-') && t.contains(':') && t.contains('"') {
            if let Some(start) = t.find('"') {
                if let Some(end) = t[start + 1..].find('"') {
                    let seg = &t[start + 1..start + 1 + end];
                    let host_part = seg.split(':').next().unwrap_or("0");
                    if let Ok(p) = host_part.trim().parse::<u16>() {
                        if p > 0 { return p; }
                    }
                }
            }
        }
    }
    0
}

/// 为无图标/中文描述的服务生成一个默认图标（避免显示空）
fn default_icon(name: &str) -> &'static str {
    use std::collections::HashMap;
    let mut m: HashMap<&str, &str> = HashMap::new();
    m.insert("portainer", "\u{1F433}");      // 🐳
    m.insert("alist", "\u{1F4C1}");           // 📁
    m.insert("vaultwarden", "\u{1F512}");     // 🔒
    m.insert("adguard", "\u{1F6E1}\u{FE0F}"); // 🛡
    m.insert("kuma", "\u{1F4CA}");            // 📊
    m.insert("qinglong", "\u{23F0}");         // ⏰
    m.insert("code-server", "</>");
    m.insert("filebrowser", "\u{1F4C2}");     // 📂
    m.insert("metube", "\u{1F3AC}");          // 🎬
    m.insert("immich", "\u{1F4F8}");          // 📷
    m.insert("n8n", "\u{26A1}");              // ⚡
    m.insert("reactive-resume", "\u{1F4DD}"); // 📝
    m.insert("homepage", "\u{1F3E0}");        // 🏠
    m.insert("stirling", "\u{1F4C4}");        // 📄
    m.insert("syncthing", "\u{1F4C3}");       // 📃
    m.insert("web", "\u{1F310}");             // 🌐
    m.get(name).copied().unwrap_or("\u{1F9F9}") // 🧹 默认
}

fn default_desc(name: &str) -> &'static str {
    use std::collections::HashMap;
    let mut m: HashMap<&str, &str> = HashMap::new();
    m.insert("portainer", "Docker 管理");
    m.insert("alist", "网盘聚合");
    m.insert("vaultwarden", "密码管理");
    m.insert("adguard", "DNS 去广告");
    m.insert("kuma", "服务监控");
    m.insert("qinglong", "定时脚本");
    m.insert("code-server", "在线 VS Code");
    m.insert("filebrowser", "文件管理");
    m.insert("metube", "视频下载");
    m.insert("immich", "照片管理");
    m.insert("n8n", "工作流自动化");
    m.insert("reactive-resume", "简历生成");
    m.insert("homepage", "仪表盘");
    m.insert("stirling", "PDF 工具箱");
    m.insert("web-app-template", "全栈模板");
    m.insert("ts-nav", "导航页");
    m.insert("email-agent-claude", "邮件助手");
    m.insert("cloudflared", "CDN 隧道");
    m.insert("one-api", "LLM 网关");
    m.insert("template-api", "模板后端");
    m.insert("openclaw", "AI 助手");
    m.insert("filebrowser-quantum", "文件管理(Q)");
    m.get(name).copied().unwrap_or("Docker 服务")
}

/// 服务清单（compose 解析 + 运行容器集合），/api/docker 与 /api/health/services
/// 共用同一份解析结果，避免两处漂移。
pub async fn service_list() -> Vec<DockerService> {
    let mut out = merged_service_list();
    // 用 docker-ctl status 拿真实运行容器名集合（WSL 子进程阻塞调用，放 spawn_blocking）
    let running = tokio::task::spawn_blocking(running_containers)
        .await
        .unwrap_or_default();
    for s in &mut out {
        // 真实容器名（docker ps 输出）与 compose 键可能不同，两者任一命中即视为运行
        s.running = running.contains(&s.key) || running.contains(&s.ctn);
    }
    out
}

/// 纯 compose 解析清单（不含运行态）：毫秒级、不碰 WSL。
/// /api/health/services 用它做探测列表，再把 docker-ctl 查询与 TCP 探测并行，
/// 保证单轮探测 <3s。
pub fn compose_list() -> Vec<DockerService> {
    compose_services()
        .into_iter()
        .map(|(name, ctn)| DockerService {
            key: name.clone(),
            name: name.clone(),
            desc: default_desc(&name).to_string(),
            port: service_port(&name),
            icon: default_icon(&name).to_string(),
            ctn: ctn.clone(),
            running: false,
            url: None,
            hidden: false,
        })
        .collect()
}

/// Compose list merged with the user's card config (icon/desc/port/url/hidden
/// overrides + user-only cards appended). Compose stays the dynamic source of
/// discovery: new services still appear unless explicitly removed/hidden.
/// The merged url field carries the ${FENG_IP} template for the client.
pub fn merged_service_list() -> Vec<DockerService> {
    crate::api::components::merged_cards()
        .into_iter()
        .map(|c| {
            let id = c.id.clone();
            let port = c.port.unwrap_or(0);
            let url = c
                .url
                .clone()
                .or_else(|| if port > 0 { Some(format!("http://${{FENG_IP}}:{port}")) } else { None });
            DockerService {
                key: id.clone(),
                name: c.name.clone().unwrap_or_else(|| id.clone()),
                desc: c.desc.clone().unwrap_or_default(),
                port,
                icon: c.icon.clone().unwrap_or_else(|| default_icon(&id).to_string()),
                ctn: c.ctn.clone().unwrap_or_else(|| id.clone()),
                running: false,
                url,
                hidden: c.hidden == Some(true),
            }
        })
        .collect()
}

pub async fn docker() -> ApiResult<Json<Vec<DockerService>>> {
    Ok(Json(service_list().await))
}

/// 返回当前运行中的容器名集合（解析 docker-ctl status 输出）
pub fn running_containers() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    // 引擎（WSL VM）未启动时绝不调用 wsl（wsl -d 会冷启动 VM、让容器常驻吃内存），
    // 直接按"全停机"处理；面板每 15s 轮询到这里就止步，不再拉起 WSL。
    if !crate::api::dockerctl::wsl_vm_running() {
        return set;
    }
    let status = crate::api::dockerctl::run_ctl_capture(&["status"]);
    for line in status.lines() {
        let t = line.trim();
        if t.is_empty() { continue; }
        // 格式：name <tab/space> Up|Exited|...  只看 "Up"
        let mut it = t.splitn(2, char::is_whitespace);
        if let Some(name) = it.next() {
            let rest = it.next().unwrap_or("");
            if rest.starts_with("Up") {
                set.insert(name.to_string());
            }
        }
    }
    set
}
