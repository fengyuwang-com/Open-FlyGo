// config.rs - device-agnostic runtime configuration for the FlyGo panel.
//
// Every machine-specific path (drive letters, install locations, user names)
// is resolved at startup in this order:
//   1. FENGPAGE_* environment variable (explicit user override)
//   2. Install-root-relative default (the exe's own directory layout)
//   3. Legacy install root (probed with exists(); keeps existing
//      deployments working with zero configuration)
//
// The install root is auto-detected by walking up from the executable until
// a directory containing `webcli/` or `scripts/` is found. Set FENGPAGE_ROOT
// to pin it explicitly (services, portable installs, unusual layouts).
//
// Env var reference (all optional):
//   FENGPAGE_ROOT, FENGPAGE_WEB_CLI_EXE, FENGPAGE_WEB_CLI_DIR,
//   FENGPAGE_SYNCTHING_EXE, FENGPAGE_SYNCTHING_HOME, FENGPAGE_SYNCTHING_DIR,
//   FENGPAGE_NODE, FENGPAGE_HOMEPAGE_DIR, FENGPAGE_HOMEPAGE_CONFIG_DIR,
//   FENGPAGE_HOMEPAGE_ALLOWED, FENGPAGE_DOCKER_COMPOSE, FENGPAGE_DOCKER_CTL,
//   FENGPAGE_USB_CTL, FENGPAGE_SYNC_SCRIPT, FENGPAGE_PORTPROXY_SCRIPT,
//   FENGPAGE_TERM_ENV, FENGPAGE_AUTOSTART_BACKUP,
//   FENGPAGE_MIRROR_LOG, FENGPAGE_MIRROR_SRC, FENGPAGE_MIRROR_DST,
//   FENGPAGE_COMPONENTS, FENGPAGE_SYNC_LINKS (data/sync-links.json),
//   FENGPAGE_CONTROL_STATE (data/control-state.json), FENGPAGE_UPLOADS (data/uploads),
//   FENGPAGE_PHONES (data/phones.json), FENGPAGE_VDISP_PIN (data/vdisp-pin.json),
//   FENGPAGE_PAIR_STATE (data/pair-state.json),
//   FENGPAGE_CONFIG (explicit flygo.toml path; see settings.rs)
//
// All code comments are pure ASCII.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct Config {
    pub root: PathBuf,
    /// Global config file (flygo.toml) if one exists; see settings.rs for
    /// the resolution chain (FENGPAGE_CONFIG > root config/ > legacy).
    pub config_path: Option<PathBuf>,
    pub webcli_exe: Option<PathBuf>,
    pub webcli_dir: Option<PathBuf>,
    pub syncthing_exe: Option<PathBuf>,
    pub syncthing_home: Option<PathBuf>,
    pub syncthing_dir: Option<PathBuf>,
    pub homepage_node: String,
    pub homepage_dir: Option<PathBuf>,
    pub homepage_config: Option<PathBuf>,
    pub homepage_allowed: String,
    pub docker_compose: Option<PathBuf>,
    pub docker_ctl: Option<PathBuf>,
    pub usb_ctl: Option<PathBuf>,
    pub sync_script: Option<PathBuf>,
    pub portproxy_script: Option<PathBuf>,
    pub term_env: PathBuf,
    pub autostart_backup: PathBuf,
    pub mirror_log: String,
    pub mirror_src: String,
    pub mirror_dst: String,
    /// Editable components config (nav/tiles/cards/frames overlay), written
    /// by the panel editor; absent until the user saves for the first time.
    pub components_path: PathBuf,
    /// Sync link registry for the sync view wizard (mirror/syncthing/manual),
    /// written atomically by api/sync.rs (tmp + rename).
    pub sync_links: PathBuf,
    /// Control-plane client state (hardware id / activation code / JWT /
    /// subscription cache); written atomically by api/control.rs.
    pub control_state: PathBuf,
    /// Mutual-backup pair state (peer Syncthing id / invite / selected
    /// folders); written atomically by api/pair.rs.
    pub pair_state: PathBuf,
    /// Uploads landing directory (<root>/data/uploads, dated subdirectories).
    pub uploads_dir: PathBuf,
    /// Registered phone nodes (FlyGo app web service addresses), written
    /// atomically by api/plainapp.rs.
    pub phones_path: PathBuf,
    /// Virtual display resolution/refresh pin (data/vdisp-pin.json), written
    /// atomically by daemon.rs (VdispPinState); loaded at panel boot.
    pub vdisp_pin_path: PathBuf,
    /// User-defined native shortcuts (data/native-shortcuts.json: exe /
    /// script / url entries the user adds from the panel UI), written
    /// atomically by api/native.rs. Absent until the first shortcut is saved.
    pub native_path: PathBuf,
    /// Native-shortcut spawn policy (data/native-policy.json: user-controlled
    /// exe whitelist roots), written atomically by api/native.rs.
    pub native_policy_path: PathBuf,
    /// Operation log directory (<root>/data/logs); log.rs writes flygo.log
    /// here. Derived straight from the install root (CEO-pinned path).
    pub logs_dir: PathBuf,
}

static CFG: OnceLock<Config> = OnceLock::new();

/// Build and freeze the global config. Must be called once at startup,
/// before any component is spawned or any API handler runs. Also freezes
/// the flygo.toml settings (env > file > defaults) so both layers are ready
/// before the first request.
pub fn init() {
    crate::settings::init();
    let _ = CFG.set(Config::build());
}

/// Access the frozen global config.
/// Syncthing exe resolution shared by the daemon launcher (T-09) and the
/// component self-check (T-08): configured path, then the fetch-system
/// target (<root>/external/syncthing/syncthing.exe), then the winget
/// portable-package shim. None = genuinely not installed.
pub fn syncthing_exe_found() -> Option<PathBuf> {
    let cfg = get();
    if let Some(p) = &cfg.syncthing_exe {
        if p.is_file() {
            return Some(p.clone());
        }
    }
    let target = cfg.root.join("external/syncthing/syncthing.exe");
    if target.is_file() {
        return Some(target);
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        let shim = PathBuf::from(la).join("Microsoft").join("WinGet").join("Links").join("syncthing.exe");
        if shim.is_file() {
            return Some(shim);
        }
    }
    None
}

pub fn get() -> &'static Config {
    CFG.get().expect("config::init() must run before config::get()")
}

/// Install root: FENGPAGE_ROOT > walk up from the exe looking for a
/// `webcli/` or `scripts/` sibling > exe directory > current directory.
fn detect_root() -> PathBuf {
    if let Ok(r) = std::env::var("FENGPAGE_ROOT") {
        if !r.trim().is_empty() {
            return PathBuf::from(r.trim());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let mut cand = dir.to_path_buf();
            for _ in 0..=3 {
                if cand.join("webcli").is_dir() || cand.join("scripts").is_dir() {
                    return cand;
                }
                if !cand.pop() {
                    break;
                }
            }
            return dir.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Non-empty env var, or None.
fn env_ok(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// Optional path: env override > root-relative (if it exists) > legacy
/// Legacy install root (if it exists). None = component not installed /
/// not configured (callers must surface a clear "not found" message).
fn optional_path(env_key: &str, root_rel: &str, legacy: &str) -> Option<PathBuf> {
    if let Some(v) = env_ok(env_key) {
        return Some(PathBuf::from(v));
    }
    let rp = detect_root().join(root_rel);
    if rp.exists() {
        return Some(rp);
    }
    let lp = PathBuf::from(legacy);
    if lp.exists() {
        return Some(lp);
    }
    None
}

/// Read/write target: env override > legacy location (if it exists) >
/// root-relative default. Never returns None: the caller can create the
/// file at the resolved location.
fn target_path(env_key: &str, root_rel: &str, legacy: &str) -> PathBuf {
    if let Some(v) = env_ok(env_key) {
        return PathBuf::from(v);
    }
    let lp = PathBuf::from(legacy);
    if lp.exists() {
        return lp;
    }
    detect_root().join(root_rel)
}

/// Free-form string: env override > legacy location (if it exists) > default.
fn str_or(env_key: &str, legacy: &str, def: &str) -> String {
    if let Some(v) = env_ok(env_key) {
        return v;
    }
    if Path::new(legacy).exists() {
        return legacy.to_string();
    }
    def.to_string()
}

impl Config {
    fn build() -> Config {
        let root = detect_root();
        let webcli_exe =
            optional_path("FENGPAGE_WEB_CLI_EXE", "bin/flygo-webcli.exe", "C:/FlyGoLegacy/bin/flygo-webcli.exe");
        let webcli_dir = optional_path("FENGPAGE_WEB_CLI_DIR", "webcli", "C:/FlyGoLegacy/webcli");
        let syncthing_exe = optional_path(
            "FENGPAGE_SYNCTHING_EXE",
            "external/syncthing/syncthing.exe",
            "C:/FlyGoLegacy/syncthing/syncthing.exe",
        );
        let syncthing_dir =
            optional_path("FENGPAGE_SYNCTHING_DIR", "external/syncthing", "C:/FlyGoLegacy/syncthing");
        let syncthing_home = optional_path(
            "FENGPAGE_SYNCTHING_HOME",
            "external/syncthing/config",
            "C:/FlyGoLegacy/syncthing/config",
        )
        .or_else(|| {
            syncthing_exe
                .as_ref()
                .and_then(|e| e.parent())
                .map(|d| d.join("config"))
        });
        let homepage_dir = optional_path(
            "FENGPAGE_HOMEPAGE_DIR",
            "external/homepage-src",
            "C:/FlyGoLegacy/homepage-src",
        );
        let homepage_config = optional_path(
            "FENGPAGE_HOMEPAGE_CONFIG_DIR",
            "external/homepage-src/config",
            "C:/FlyGoLegacy/homepage/config",
        )
        .or_else(|| homepage_dir.as_ref().map(|d| d.join("config")));
        let homepage_node = env_ok("FENGPAGE_NODE").unwrap_or_else(|| "node".to_string());
        let homepage_allowed = env_ok("FENGPAGE_HOMEPAGE_ALLOWED")
            .unwrap_or_else(|| "localhost:9199,127.0.0.1:9199".to_string());
        // Resolve the operation-log directory before `root` moves into the
        // struct below (<root>/data/logs; log.rs writes flygo.log here).
        let logs_dir = root.join("data").join("logs");

        Config {
            root,
            config_path: {
                let s = crate::settings::get();
                if s.exists {
                    Some(s.path.clone())
                } else {
                    None
                }
            },
            webcli_exe,
            webcli_dir,
            syncthing_exe,
            syncthing_home,
            syncthing_dir,
            homepage_node,
            homepage_dir,
            homepage_config,
            homepage_allowed,
            docker_compose: optional_path(
                "FENGPAGE_DOCKER_COMPOSE",
                "docker-compose.yml",
                "C:/FlyGoLegacy/docker-compose.yml",
            ),
            docker_ctl: optional_path(
                "FENGPAGE_DOCKER_CTL",
                "scripts/docker-ctl.cmd",
                "C:/FlyGoLegacy/scripts/docker-ctl.cmd",
            ),
            usb_ctl: optional_path(
                "FENGPAGE_USB_CTL",
                "scripts/usb-ctl.ps1",
                "C:/FlyGoLegacy/scripts/usb-ctl.ps1",
            ),
            sync_script: optional_path(
                "FENGPAGE_SYNC_SCRIPT",
                "scripts/sync-important-files.ps1",
                "C:/FlyGoLegacy/backup/sync-important-files.ps1",
            ),
            portproxy_script: optional_path(
                "FENGPAGE_PORTPROXY_SCRIPT",
                "scripts/apply-portproxy.ps1",
                "C:/FlyGoLegacy/scripts/apply-portproxy.ps1",
            ),
            term_env: target_path(
                "FENGPAGE_TERM_ENV",
                "term_token.env",
                "C:/FlyGoLegacy/panel/term_token.env",
            ),
            autostart_backup: target_path(
                "FENGPAGE_AUTOSTART_BACKUP",
                "autostart_backup.json",
                "C:/FlyGoLegacy/panel/autostart_backup.json",
            ),
            mirror_log: str_or(
                "FENGPAGE_MIRROR_LOG",
                "C:/FlyGoLegacy/backup/sync-important-files.log",
                "",
            ),
            mirror_src: str_or("FENGPAGE_MIRROR_SRC", "", ""),
            mirror_dst: str_or(
                "FENGPAGE_MIRROR_DST",
                "C:/FlyGoLegacy/Mirror/Sync_Important_Files",
                "",
            ),
            components_path: target_path("FENGPAGE_COMPONENTS", "data/components.json", ""),
            sync_links: target_path("FENGPAGE_SYNC_LINKS", "data/sync-links.json", ""),
            control_state: target_path("FENGPAGE_CONTROL_STATE", "data/control-state.json", ""),
            pair_state: target_path("FENGPAGE_PAIR_STATE", "data/pair-state.json", ""),
            uploads_dir: target_path("FENGPAGE_UPLOADS", "data/uploads", ""),
            phones_path: target_path("FENGPAGE_PHONES", "data/phones.json", ""),
            vdisp_pin_path: target_path("FENGPAGE_VDISP_PIN", "data/vdisp-pin.json", ""),
            native_path: target_path("FENGPAGE_NATIVE", "data/native-shortcuts.json", ""),
            native_policy_path: target_path("FENGPAGE_NATIVE_POLICY", "data/native-policy.json", ""),
            logs_dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T-02 (D1) regression: legacy absolute fallbacks are existence-gated.
    // On a user machine without C:/FlyGoLegacy etc. they must never fire.
    const NOPE_LEGACY: &str = "C:/definitely-not-here-t02/legacy.json";

    #[test]
    fn optional_path_ignores_missing_legacy() {
        std::env::remove_var("FENGPAGE_TEST_T02_A");
        let r = optional_path("FENGPAGE_TEST_T02_A", "data/t02-nope.json", NOPE_LEGACY);
        assert!(r.is_none(), "missing legacy must yield None, got {r:?}");
    }

    #[test]
    fn target_path_falls_back_to_root_when_legacy_missing() {
        std::env::remove_var("FENGPAGE_TEST_T02_B");
        let r = target_path("FENGPAGE_TEST_T02_B", "data/t02-nope.json", NOPE_LEGACY);
        assert_eq!(r, detect_root().join("data/t02-nope.json"));
    }

    #[test]
    fn str_or_uses_default_when_legacy_missing() {
        std::env::remove_var("FENGPAGE_TEST_T02_C");
        let r = str_or("FENGPAGE_TEST_T02_C", NOPE_LEGACY, "DEF");
        assert_eq!(r, "DEF");
    }

    #[test]
    fn existing_legacy_still_honored() {
        std::env::remove_var("FENGPAGE_TEST_T02_D");
        let dir = std::env::temp_dir().join("flygo-t02-legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("legacy.json");
        std::fs::write(&f, "{}").unwrap();
        let r = optional_path("FENGPAGE_TEST_T02_D", "data/t02-nope.json", f.to_str().unwrap());
        assert_eq!(r, Some(f.clone()));
        let _ = std::fs::remove_file(&f);
    }
}
