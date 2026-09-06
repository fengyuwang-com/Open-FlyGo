// /api/services — 关键原生服务状态
// 通过 sysinfo 枚举进程名判断是否在跑，不 spawn 外部进程（省资源，内存管理第一）。
use crate::ApiResult;
use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct ServiceItem {
    pub key: String,
    pub name: String,
    pub kind: String,
    pub running: bool,
    pub pids: Vec<u32>,
}

#[derive(Serialize)]
pub struct Services {
    pub services: Vec<ServiceItem>,
}

/// Key services to probe.
/// Default three (ssh/tailscale/syncthing); the user config's service-light
/// tiles may add or remove entries (removing/hiding a light drops its probe,
/// adding a tile with a new service key probes it). No user config -> exactly
/// the built-in three, behavior unchanged.
struct Wanted {
    key: String,
    name: String,
    needle: String,
}

fn wanted() -> Vec<Wanted> {
    const DEFAULT: [(&str, &str, &str); 3] = [
        ("ssh", "sshd", "sshd"),
        ("tailscale", "Tailscale", "tailscaled"),
        ("syncthing", "Syncthing", "syncthing"),
    ];
    match crate::api::components::user_service_lights() {
        None => DEFAULT
            .iter()
            .map(|(k, n, nd)| Wanted { key: (*k).into(), name: (*n).into(), needle: (*nd).into() })
            .collect(),
        Some(ls) => ls
            .iter()
            .map(|(k, label)| {
                if let Some(d) = DEFAULT.iter().find(|d| d.0 == k) {
                    Wanted { key: k.clone(), name: d.1.into(), needle: d.2.into() }
                } else {
                    Wanted { key: k.clone(), name: label.clone(), needle: k.clone() }
                }
            })
            .collect(),
    }
}

pub async fn services() -> ApiResult<Json<Services>> {
    // 快照当前进程（sysinfo 0.30：refresh_processes() 无参刷新全部进程）
    let mut sys = sysinfo::System::new();
    sys.refresh_processes();

    let procs: Vec<_> = sys
        .processes()
        .iter()
        .map(|(pid, p)| {
            (
                *pid,
                p.name().to_lowercase(),
                p.exe().map(|e| e.to_string_lossy().into_owned().to_lowercase()),
            )
        })
        .collect();

    let mut items = Vec::new();
    for w in wanted() {
        let needle_l = w.needle.to_lowercase();
        let mut pids: Vec<u32> = Vec::new();
        for (pid, pname, pexe) in &procs {
            // 进程名或可执行路径包含关键词即命中（忽略大小写）
            if pname.contains(&needle_l) || pexe.as_deref().map_or(false, |e| e.contains(&needle_l)) {
                pids.push(pid.as_u32());
            }
        }
        let running = !pids.is_empty();
        items.push(ServiceItem {
            key: w.key.clone(),
            name: w.name.clone(),
            kind: "native".to_string(),
            running,
            pids,
        });
    }

    Ok(Json(Services { services: items }))
}
