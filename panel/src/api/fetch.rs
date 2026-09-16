// /api/fetch - component download system, v2 (CTO spec: winget first).
//
// Two layers: winget (Win10/11 built-in, official manifests with SHA)
// is the primary installer; our own manifest download is the fallback
// for machines without winget or without network to its sources.
// See docs/开源拼装地图.md section 7 and docs/syncthing-deep-dive.md.
//
// Endpoints:
//   GET  /api/fetch/components   -> manifest + live status per item
//   POST /api/fetch/install      -> body {id} one item (winget, else download)
//   POST /api/fetch/install-all  -> every missing REQUIRED item, sequential
//
// Item kinds: winget (system component via winget id), archive (zip ->
// member -> target), msi (quiet msiexec, elevation required), file
// (single file -> target, e.g. APK), image (docker pull in WSL).
//
// Syncthing rule (deep-dive section 3): installers replace ONLY the exe;
// -home (identity + pairings) is never touched.
//
// Security: install targets come from the manifest only (request carries
// just an id). sha256 enforced when pinned. Temp files always carry a
// real extension (Expand-Archive/msiexec refuse extensionless files;
// lesson from the v1 prototype).
//
// All code comments are pure ASCII; JSON payloads are UTF-8.
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use axum::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// CREATE_NO_WINDOW: every child stays invisible.
use crate::winproc::CREATE_NO_WINDOW;
const DOWNLOAD_TIMEOUT_SECS: u64 = 600;
const INSTALL_TIMEOUT_SECS: u64 = 900;

/// One manifest entry.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct FetchItem {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub version: String,
    pub required: bool,
    /// winget package id (primary path when winget exists).
    #[serde(default)]
    pub winget_id: String,
    /// Fallback download URLs (domestic mirror first).
    #[serde(default)]
    pub urls: Vec<String>,
    pub sha256: String,
    /// file/archive: destination; archive encodes "member|dest".
    #[serde(default)]
    pub target_rel: String,
    /// image: pull ref.
    #[serde(default, rename = "ref")]
    pub ref_: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct FetchStatus {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub version: String,
    pub required: bool,
    pub installed: bool,
    pub installed_version: String,
    pub install_via: String,
    pub sha_pinned: bool,
    pub note: String,
}

/// Built-in manifest. syncthing sha256 pinned to the upstream release
/// file (v2.1.3 windows-amd64 zip); the same hash appears independently
/// in the winget community manifest (cross-validated 2026-09-06).
fn builtin_manifest() -> Vec<FetchItem> {
    let apk_url = std::env::var("FENGPAGE_APK_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let mut items = vec![
        FetchItem {
            id: "syncthing".into(),
            name: "Syncthing (sync engine)".into(),
            kind: "archive".into(),
            version: "v2.1.3".into(),
            required: true,
            winget_id: "Syncthing.Syncthing".into(),
            urls: vec![
                "https://ghfast.top/https://github.com/syncthing/syncthing/releases/download/v2.1.3/syncthing-windows-amd64-v2.1.3.zip".into(),
                "https://github.com/syncthing/syncthing/releases/download/v2.1.3/syncthing-windows-amd64-v2.1.3.zip".into(),
            ],
            sha256: "c0b79cffa6ce5dad5ed41ede86454f3325d13ccac33447a528cb59d65fbc3a21".into(),
            target_rel: "syncthing-windows-amd64-v2.1.3/syncthing.exe|external/syncthing/syncthing.exe".into(),
            ref_: String::new(),
            note: "Sync engine. Only the exe is replaced; -home (identity) is never touched.".into(),
        },
        FetchItem {
            id: "tailscale".into(),
            name: "Tailscale (network)".into(),
            kind: "msi".into(),
            version: "stable".into(),
            required: true,
            winget_id: "Tailscale.Tailscale".into(),
            urls: vec![
                "https://ghfast.top/https://pkgs.tailscale.com/stable/tailscale-setup-latest.msi".into(),
                "https://pkgs.tailscale.com/stable/tailscale-setup-latest.msi".into(),
            ],
            sha256: String::new(),
            target_rel: String::new(),
            ref_: String::new(),
            note: "MSI is Authenticode-signed (trust anchor). Needs elevation when installed directly.".into(),
        },
        FetchItem {
            id: "docker-desktop".into(),
            name: "Docker Desktop (compute)".into(),
            kind: "winget".into(),
            version: "stable".into(),
            required: true,
            winget_id: "Docker.DockerDesktop".into(),
            urls: Vec::new(),
            sha256: String::new(),
            target_rel: String::new(),
            ref_: String::new(),
            note: "winget-only (installer is hundreds of MB; no manifest mirror). Failing that, manual guide.".into(),
        },
    ];
    if let Some(url) = apk_url {
        items.push(FetchItem {
            id: "flygo-mobile".into(),
            name: "FlyGo mobile APK".into(),
            kind: "file".into(),
            version: "configured".into(),
            required: false,
            winget_id: String::new(),
            urls: vec![url],
            sha256: String::new(),
            target_rel: "bin/flygo-mobile-latest.apk".into(),
            ref_: String::new(),
            note: "Phone client. URL from FENGPAGE_APK_URL; unpinned sha reports verified:false.".into(),
        });
    }
    items
}

/// Manifest file override (<root>/data/fetch-manifest.json or
/// FENGPAGE_FETCH_MANIFEST): JSON array replacing built-in.
fn load_manifest() -> Vec<FetchItem> {
    let path: PathBuf = std::env::var("FENGPAGE_FETCH_MANIFEST")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::get().root.join("data").join("fetch-manifest.json"));
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(items) = serde_json::from_str::<Vec<FetchItem>>(&text) {
            if !items.is_empty() {
                return items;
            }
        }
    }
    builtin_manifest()
}

fn root() -> PathBuf {
    crate::config::get().root.clone()
}

fn target_path(item: &FetchItem) -> Option<PathBuf> {
    match item.kind.as_str() {
        "file" => {
            if item.target_rel.is_empty() {
                None
            } else {
                Some(root().join(&item.target_rel))
            }
        }
        "archive" => item
            .target_rel
            .split_once('|')
            .map(|(_, dest)| root().join(dest.trim())),
        _ => None,
    }
}

fn archive_member(item: &FetchItem) -> Option<String> {
    item.target_rel
        .split_once('|')
        .map(|(m, _)| m.trim().to_string())
}

/// Temp file suffix per kind (v1 lesson: extensionless temp files break
/// Expand-Archive and msiexec).
fn temp_suffix(kind: &str) -> &'static str {
    match kind {
        "archive" => ".zip",
        "msi" => ".msi",
        _ => ".bin",
    }
}

/// Exact winget install argv (pure: unit-tested).
fn winget_install_args(id: &str) -> Vec<String> {
    vec![
        "install".into(),
        "--id".into(),
        id.into(),
        "--exact".into(),
        "--silent".into(),
        "--accept-package-agreements".into(),
        "--accept-source-agreements".into(),
    ]
}

/// winget presence probe.
fn winget_present() -> bool {
    run_capture("winget", &["--version"], 20)
        .trim()
        .strip_prefix('v')
        .map(|v| v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
        .unwrap_or(false)
}

fn is_installed(item: &FetchItem) -> bool {
    match item.kind.as_str() {
        // syncthing may live at the fetch target OR as a winget shim (T-09).
        "archive" if item.id == "syncthing" => crate::config::syncthing_exe_found().is_some(),
        "file" | "archive" => target_path(item).map(|p| p.is_file()).unwrap_or(false),
        "msi" | "winget" => match item.id.as_str() {
            "tailscale" => tailscale_present(),
            "docker-desktop" => docker_present(),
            _ => false,
        },
        "image" => docker_image_present(&item.ref_),
        _ => false,
    }
}

fn installed_version(item: &FetchItem) -> String {
    match item.id.as_str() {
        "syncthing" => crate::config::syncthing_exe_found()
            .map(|p| run_capture(&p.to_string_lossy(), &["--version"], 15))
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        "tailscale" => run_capture("tailscale", &["version"], 15)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        "docker-desktop" => run_capture("docker", &["--version"], 15).trim().to_string(),
        _ => String::new(),
    }
}

fn tailscale_present() -> bool {
    if run_capture("tailscale", &["version"], 15)
        .lines()
        .next()
        .map(|l| l.trim().starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(false)
    {
        return true;
    }
    // where.exe covers custom install dirs and PATH-visible copies; the
    // Program Files check covers the default MSI layout (often not on PATH).
    if run_capture("where", &["tailscale.exe"], 10)
        .lines()
        .any(|l| l.trim().to_lowercase().ends_with("tailscale.exe"))
    {
        return true;
    }
    PathBuf::from("C:/Program Files/Tailscale/tailscale.exe").is_file()
}

fn docker_present() -> bool {
    run_capture("docker", &["--version"], 20)
        .trim()
        .starts_with("Docker version")
}

fn docker_image_present(ref_: &str) -> bool {
    if ref_.trim().is_empty() {
        return false;
    }
    let d = crate::api::wsl::distro();
    match Command::new("wsl")
        .args(["-d", &d, "-u", "root", "--", "docker", "images", "-q", ref_])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// Hidden command with captured stdout (timeout via wait loop).
fn run_capture(program: &str, args: &[&str], timeout_secs: u64) -> String {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out);
        }
        let _ = tx.send(out);
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return String::new();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return String::new(),
        }
    }
    rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default()
}

/// Run a long installer child to completion (msiexec/winget), hidden.
fn run_wait(program: &str, args: &[String], timeout_secs: u64) -> Result<i32, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("{program} spawn failed ({e})"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(st.code().unwrap_or(-1)),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(format!("{program} timed out"));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => return Err(format!("{program} wait failed ({e})")),
        }
    }
}

/// Download first reachable URL to a suffixed temp file.
fn download_first(urls: &[String], suffix: &str) -> Result<(PathBuf, u64), String> {
    if urls.is_empty() {
        return Err("no download URLs".to_string());
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build();
    let mut last_err = String::new();
    for url in urls {
        match agent.get(url).call() {
            Ok(resp) => {
                let tmp = std::env::temp_dir().join(format!(
                    "flygo-fetch-{}{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0),
                    suffix,
                ));
                let mut file = match std::fs::File::create(&tmp) {
                    Ok(f) => f,
                    Err(e) => {
                        last_err = format!("temp create failed ({e})");
                        continue;
                    }
                };
                match std::io::copy(&mut resp.into_reader(), &mut file) {
                    Ok(0) => {
                        drop(file);
                        let _ = std::fs::remove_file(&tmp);
                        last_err = "empty download".to_string();
                    }
                    Ok(n) => {
                        drop(file);
                        return Ok((tmp, n));
                    }
                    Err(e) => {
                        drop(file);
                        let _ = std::fs::remove_file(&tmp);
                        last_err = format!("download body failed ({e})");
                    }
                }
            }
            Err(e) => last_err = format!("GET failed ({e})"),
        }
    }
    Err(format!("all {} URL(s) failed; last: {last_err}", urls.len()))
}

fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("open for hash failed ({e})"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("hash read failed ({e})"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Install one item (blocking; callers use spawn_blocking).
/// Returns (ok, message, bytes_downloaded, verified, via).
fn install_item(item: &FetchItem) -> (bool, String, u64, bool, String) {
    // Layer 1: winget (official manifests, own SHA checks).
    if !item.winget_id.is_empty() && winget_present() {
        if item.kind == "winget" && item.urls.is_empty() {
            // winget-only item: no fallback exists.
            return match run_wait("winget", &winget_install_args(&item.winget_id), INSTALL_TIMEOUT_SECS) {
                Ok(0) => (true, "installed via winget".to_string(), 0, true, "winget".to_string()),
                Ok(c) => (false, format!("winget exit {c}"), 0, false, "winget".to_string()),
                Err(e) => (false, e, 0, false, "winget".to_string()),
            };
        }
        let winget_ok = || matches!(run_wait("winget", &winget_install_args(&item.winget_id), INSTALL_TIMEOUT_SECS), Ok(0));
        if (item.kind == "msi" || item.kind == "archive") && winget_ok() {
            return (true, "installed via winget".to_string(), 0, true, "winget".to_string());
            // winget failed: fall through to manifest download below.
        }
    }
    // Layer 2: manifest download (fallback / winget-only-needs-manual).
    if item.urls.is_empty() {
        if item.kind == "winget" {
            return (
                false,
                "winget unavailable or failed; install Docker Desktop manually (docker.com), then re-check".to_string(),
                0,
                false,
                "manual".to_string(),
            );
        }
        return (false, "no install source".to_string(), 0, false, "none".to_string());
    }
    if item.kind == "msi" && !crate::api::access::is_elevated() {
        return (
            false,
            "direct MSI needs elevation (run panel as admin) or install via winget".to_string(),
            0,
            false,
            "manifest".to_string(),
        );
    }
    if item.kind == "image" {
        let (ok, msg) = install_image(item);
        return (ok, msg, 0, false, "manifest".to_string());
    }
    let (tmp, bytes) = match download_first(&item.urls, temp_suffix(&item.kind)) {
        Ok(t) => t,
        Err(e) => return (false, format!("download failed: {e}"), 0, false, "manifest".to_string()),
    };
    let verified = if item.sha256.trim().is_empty() {
        false
    } else {
        match sha256_file(&tmp) {
            Ok(got) if got.eq_ignore_ascii_case(item.sha256.trim()) => true,
            _ => {
                let _ = std::fs::remove_file(&tmp);
                return (false, "sha256 MISMATCH (deleted; refusing to install)".to_string(), bytes, false, "manifest".to_string());
            }
        }
    };
    let (ok, msg) = match item.kind.as_str() {
        "file" => install_file(item, &tmp),
        "archive" => install_archive(item, &tmp),
        "msi" => match run_wait("msiexec", &["/i".into(), tmp.display().to_string(), "/quiet".into(), "/norestart".into()], INSTALL_TIMEOUT_SECS) {
            Ok(0) => (true, "installed (quiet)".to_string()),
            Ok(c) => (false, format!("msiexec exit {c}")),
            Err(e) => (false, e),
        },
        other => (false, format!("unknown kind {other}")),
    };
    let _ = std::fs::remove_file(&tmp);
    (ok, msg, bytes, verified, "manifest".to_string())
}

fn install_file(item: &FetchItem, tmp: &std::path::Path) -> (bool, String) {
    let Some(dest) = target_path(item) else {
        return (false, "no target configured".to_string());
    };
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return (false, "cannot create target dir".to_string());
        }
    }
    match std::fs::rename(tmp, &dest) {
        Ok(()) => (true, format!("installed {}", dest.display())),
        Err(_) => match std::fs::copy(tmp, &dest) {
            Ok(_) => (true, format!("installed {}", dest.display())),
            Err(e) => (false, format!("install copy failed ({e})")),
        },
    }
}

fn install_archive(item: &FetchItem, tmp: &std::path::Path) -> (bool, String) {
    let (Some(member), Some(dest)) = (archive_member(item), target_path(item)) else {
        return (false, "bad archive target (want member|dest)".to_string());
    };
    let dir = std::env::temp_dir().join(format!(
        "flygo-unzip-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let script = format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        tmp.display().to_string().replace('\'', "''"),
        dir.display().to_string().replace('\'', "''")
    );
    let ok = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_dir_all(&dir);
        return (false, "unzip failed".to_string());
    }
    let src = dir.join(&member);
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let r = std::fs::rename(&src, &dest)
        .map(|_| (true, format!("installed {}", dest.display())))
        .unwrap_or_else(|_| match std::fs::copy(&src, &dest) {
            Ok(_) => (true, format!("installed {}", dest.display())),
            Err(e) => (false, format!("member copy failed ({e})")),
        });
    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn install_image(item: &FetchItem) -> (bool, String) {
    if item.ref_.trim().is_empty() {
        return (false, "no image ref".to_string());
    }
    let d = crate::api::wsl::distro();
    match Command::new("wsl")
        .args(["-d", &d, "-u", "root", "--", "docker", "pull", item.ref_.trim()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) if o.status.success() => (true, format!("pulled {}", item.ref_)),
        Ok(o) => (
            false,
            format!("pull failed ({})", String::from_utf8_lossy(&o.stderr).trim().lines().next().unwrap_or("unknown")),
        ),
        Err(e) => (false, format!("wsl spawn failed ({e})")),
    }
}

/// GET /api/fetch/components
pub async fn components() -> crate::ApiResult<Json<serde_json::Value>> {
    // The whole status pass (winget probe + per-item installed checks)
    // spawns subprocesses with multi-second timeouts; it must run on a
    // blocking thread, never inside the async worker.
    let (winget, list) = tokio::task::spawn_blocking(|| {
        let winget = winget_present();
        let list = load_manifest()
            .iter()
            .map(|it| {
                let installed = is_installed(it);
                FetchStatus {
                    id: it.id.clone(),
                    name: it.name.clone(),
                    kind: it.kind.clone(),
                    version: it.version.clone(),
                    required: it.required,
                    installed,
                    installed_version: if installed {
                        installed_version(it)
                    } else {
                        String::new()
                    },
                    install_via: if !it.winget_id.is_empty() && winget { "winget".into() } else { "manifest".into() },
                    sha_pinned: !it.sha256.trim().is_empty(),
                    note: it.note.clone(),
                }
            })
            .collect::<Vec<FetchStatus>>();
        (winget, list)
    })
    .await
    .unwrap_or((false, Vec::new()));
    let missing_required = list.iter().filter(|s| s.required && !s.installed).count();
    Ok(Json(serde_json::json!({
        "ok": true, "winget": winget,
        "components": list, "missing_required": missing_required,
    })))
}

/// POST /api/fetch/install body {id}
pub async fn install(body: axum::Json<serde_json::Value>) -> crate::ApiResult<Json<serde_json::Value>> {
    let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Ok(Json(serde_json::json!({ "ok": false, "message": "missing id" })));
    }
    let item = load_manifest().into_iter().find(|it| it.id == id);
    let Some(item) = item else {
        return Ok(Json(serde_json::json!({ "ok": false, "message": "unknown component id" })));
    };
    let (ok, message, bytes, verified, via) = tokio::task::spawn_blocking(move || install_item(&item))
        .await
        .unwrap_or((false, "worker failed".to_string(), 0, false, "none".to_string()));
    Ok(Json(serde_json::json!({
        "ok": ok, "message": message, "bytes": bytes,
        "verified": verified, "via": via,
    })))
}

/// POST /api/fetch/install-all
pub async fn install_all() -> crate::ApiResult<Json<serde_json::Value>> {
    // is_installed spawns version probes with multi-second timeouts; the
    // missing-filter belongs on a blocking thread like the installs below.
    let items: Vec<FetchItem> = tokio::task::spawn_blocking(|| {
        load_manifest()
            .into_iter()
            .filter(|it| it.required && !is_installed(it))
            .collect()
    })
    .await
    .unwrap_or_default();
    if items.is_empty() {
        return Ok(Json(serde_json::json!({ "ok": true, "report": [], "message": "nothing missing" })));
    }
    let mut report = Vec::new();
    for item in items {
        let id = item.id.clone();
        let (ok, message, bytes, verified, via) =
            tokio::task::spawn_blocking(move || install_item(&item))
                .await
                .unwrap_or((false, "worker failed".to_string(), 0, false, "none".to_string()));
        report.push(serde_json::json!({
            "id": id, "ok": ok, "message": message,
            "bytes": bytes, "verified": verified, "via": via,
        }));
        if !ok {
            break;
        }
    }
    let all_ok = report.iter().all(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
    Ok(Json(serde_json::json!({ "ok": all_ok, "report": report })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_manifest_pins_syncthing() {
        std::env::remove_var("FENGPAGE_APK_URL");
        let m = builtin_manifest();
        let st = m.iter().find(|i| i.id == "syncthing").expect("syncthing entry");
        assert_eq!(st.kind, "archive");
        assert!(st.required);
        assert_eq!(st.winget_id, "Syncthing.Syncthing");
        assert_eq!(st.urls.len(), 2);
        assert_eq!(st.sha256.len(), 64);
        assert!(st.target_rel.contains('|'));
        assert!(archive_member(st).unwrap().ends_with("syncthing.exe"));
    }

    #[test]
    fn winget_install_args_use_exact_silent_agreements() {
        let a = winget_install_args("Syncthing.Syncthing");
        assert_eq!(
            a,
            vec![
                "install", "--id", "Syncthing.Syncthing", "--exact",
                "--silent", "--accept-package-agreements", "--accept-source-agreements",
            ]
        );
    }

    #[test]
    fn temp_suffix_covers_all_kinds() {
        assert_eq!(temp_suffix("archive"), ".zip");
        assert_eq!(temp_suffix("msi"), ".msi");
        assert_eq!(temp_suffix("file"), ".bin");
        assert_eq!(temp_suffix("winget"), ".bin");
        assert_eq!(temp_suffix("image"), ".bin");
        assert_eq!(temp_suffix("nope"), ".bin");
    }

    #[test]
    fn apk_entry_appears_only_with_configured_url() {
        std::env::remove_var("FENGPAGE_APK_URL");
        assert!(builtin_manifest().iter().all(|i| i.id != "flygo-mobile"));
    }

    #[test]
    fn sha256_file_matches_known_vector() {
        let p = std::env::temp_dir().join("flygo-fetch-shatest.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_kind_never_counts_as_installed() {
        let it = FetchItem { id: "x".into(), kind: "nope".into(), ..Default::default() };
        assert!(!is_installed(&it));
    }

    #[test]
    fn docker_desktop_is_winget_only_with_manual_fallback() {
        std::env::remove_var("FENGPAGE_APK_URL");
        let m = builtin_manifest();
        let d = m.iter().find(|i| i.id == "docker-desktop").expect("docker entry");
        assert_eq!(d.kind, "winget");
        assert!(d.required);
        assert!(d.urls.is_empty(), "no manifest mirror for the giant installer");
    }
}
