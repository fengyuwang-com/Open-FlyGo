// settings.rs - global configuration file (flygo.toml) schema and read-in.
//
// Layering (docs/global-config-design.md):
//   FENGPAGE_* env vars > config file > built-in defaults
// File resolution:
//   FENGPAGE_CONFIG (env, explicit) > <install root>/config/flygo.toml >
//   legacy C:/FlyGoLegacy/flygo.toml (exists() gated) > built-in defaults.
// Every default value equals the current hardcoded value, so a deployment
// without any config file behaves exactly like the previous builds.
//
// The file only carries non-secret tuning parameters. Secrets keep their own
// files (term_token.env etc.) and never enter flygo.toml.
//
// P1 scope: full schema parse + read-in + validation. Only a few high-value
// knobs are wired into runtime code ([server].addr, [webcli], [term],
// [sync], [services].ports); the rest are parsed and served via /api/config
// and take effect in later phases.
//
// This module is shared with the launcher binary (src/bin/launcher.rs)
// through `#[path = "../settings.rs"] mod settings;`, so it must stay free
// of panel-only dependencies (no tokio, no axum).
//
// All code comments are pure ASCII.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// defaults (single source of truth for the current hardcoded values)
// ---------------------------------------------------------------------------

/// Default panel listen address (main.rs bind; FENGPAGE_ADDR overrides).
pub const DEFAULT_SERVER_ADDR: &str = "0.0.0.0:8388";
/// Default webcli listener port (daemon.rs spawn env WEBPORT).
pub const DEFAULT_WEB_CLI_PORT: u16 = 8788;
/// Default syncthing GUI/REST port (daemon.rs --gui-address).
pub const DEFAULT_SYNCTHING_PORT: u16 = 8384;
/// Default homepage listener port (daemon.rs spawn env PORT).
pub const DEFAULT_HOMEPAGE_PORT: u16 = 9199;
/// Default Syncthing REST base URL (shared by sync/syncview/onboarding).
pub const DEFAULT_SYNCTHING_ADDR: &str = "http://127.0.0.1:8384";
/// Default webcli address used by the terminal WS reverse proxy.
const DEFAULT_TERM_WEB_CLI_ADDR: &str = "127.0.0.1:8788";
/// Default WSL distro (wsl.rs keepalive/selfcheck).
const DEFAULT_WSL_DISTRO: &str = "Ubuntu-24.04";
/// Default Tailscale CGNAT allow range (access.rs scope policies).
const DEFAULT_TAILSCALE_CIDR: &str = "100.64.0.0/10";
/// Config file name under the install root `config/` directory.
pub const CONFIG_FILE_NAME: &str = "flygo.toml";
/// Legacy install root probed when the new location is absent.
pub const LEGACY_CONFIG_PATH: &str = "C:/FlyGoLegacy/flygo.toml";
/// Default control-plane base URL (local MVP; VPS after deploy).
pub const DEFAULT_CONTROL_BASE_URL: &str = "http://127.0.0.1:9001";
/// Default heartbeat interval in seconds.
pub const DEFAULT_CONTROL_HEARTBEAT_SECS: u64 = 300;
/// Default upload size cap in MB (POST /api/upload, 413 above).
pub const DEFAULT_CONTROL_UPLOAD_MAX_MB: u64 = 500;
/// Control-plane offline window: 15 min without a heartbeat (API contract).
/// The heartbeat interval must stay below it so the node is never flagged
/// offline by its own cadence.
pub const CONTROL_OFFLINE_WINDOW_SECS: u64 = 900;
/// Remaining time below which an active subscription is shown as "expiring".
pub const CONTROL_EXPIRING_WINDOW_SECS: i64 = 30 * 86400;

// ---------------------------------------------------------------------------
// schema (16 sections; every field is optional with a built-in default)
//
// Struct-level #[serde(default)] fills missing keys from the struct's
// Default impl (the current hardcoded values), so a partial file behaves
// exactly like the old builds; only a type error is reported.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ServerCfg {
    pub addr: String,
    pub bind_retries: u8,
    pub bind_retry_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct WebcliCfg {
    pub host: String,
    pub port: u16,
    pub shell: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TermCfg {
    pub session_ttl_hours: u64,
    pub password_min_len: u8,
    /// WS reverse proxy target; None = derive from [webcli] host:port.
    pub webcli_addr: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct SyncCfg {
    pub interval_secs: u64,
    pub syncthing_addr: String,
    /// REST timeout used by /api/syncview.
    pub timeout_secs: u64,
    /// REST timeout used by /api/sync.
    pub sync_timeout_secs: u64,
    /// REST timeout used by /api/onboarding (slower scanning allowance).
    pub onboarding_timeout_secs: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct DaemonCfg {
    pub usb_poll_secs: u64,
    pub probe_timeout_secs: u64,
    pub vdisp_cache_secs: u64,
    pub instance_retries: u8,
    pub instance_retry_ms: u64,
    pub eject_grace_secs: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct WslCfg {
    pub distro: String,
    pub keepalive_poll_secs: u64,
    pub cmd_timeout_secs: u64,
    pub ps_timeout_secs: u64,
    pub vm_wait_secs: u64,
    pub vm_poll_step_secs: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct DockerCfg {
    pub status_timeout_secs: u64,
    pub action_timeout_secs: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct HealthCfg {
    pub probe_timeout_secs: u64,
    pub color_green_max_ms: u64,
    pub color_yellow_max_ms: u64,
    pub error_rate_healthy_pct: u8,
    pub uptime_cache_secs: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct SystemCfg {
    pub cache_ttl_secs: u64,
    pub route_stats_max: usize,
    pub api_body_max_kb: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AccessCfg {
    pub uac_wait_secs: u64,
    pub cache_ttl_secs: u64,
    pub port_spec_max_len: u8,
    pub label_max_len: u8,
    pub name_max_len: u8,
    pub tailscale_cidr: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ServicesPorts {
    pub webcli: u16,
    pub syncthing: u16,
    pub homepage: u16,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ServicesCfg {
    pub default_start: Vec<String>,
    pub ports: ServicesPorts,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct UiCfg {
    pub poll_ms: u64,
    pub tray_poll_ms: u64,
    pub phone_poll_ms: u64,
    pub embed_timeout_ms: u64,
    pub boot_poll_ms: u64,
    pub boot_timeout_ms: u64,
    pub embed_probe_ms: u64,
    pub fetch_retries: u8,
    pub cpu_warn_pct: u8,
    pub mem_warn_pct: u8,
    pub temp_err_c: u8,
    pub temp_warn_c: u8,
    pub smart_temp_err_c: u8,
    pub smart_wear_err_pct: u8,
    /// webcli page WS reconnect backoff [min_ms, max_ms].
    pub ws_reconnect_ms: Vec<u64>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct LauncherCfg {
    pub wait_secs: u64,
    pub poll_interval_secs: u64,
    pub connect_timeout_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct FeatureFlags {
    pub auto_retry: bool,
    pub usb_eject: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct FlagsCfg {
    pub feature: FeatureFlags,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct TrayCfg {
    pub balloon_ms: u64,
}

/// [control] — FlyGo control-plane client (register / activate / heartbeat /
/// upload). The panel acts as a "computer" node of the control plane; this
/// section only carries non-secret knobs (the device JWT lives in
/// data/control-state.json, never here).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ControlCfg {
    pub enabled: bool,
    pub base_url: String,
    pub device_name: String,
    pub heartbeat_secs: u64,
    pub upload_max_mb: u64,
}

/// [auth] — panel login layer (M09). Default OFF: with enabled = false the
/// auth guard middleware is a pure pass-through and panel behavior is
/// byte-identical to a build without the layer. The password is the one
/// exception to the "no secrets in flygo.toml" rule: for the local EXE
/// scenario a plaintext toml entry is accepted, but it must never appear in
/// logs, /api/config responses or error messages (see api/auth.rs and the
/// masking in default_json/effective_json below; PUT /api/config refuses to
/// touch the section).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AuthCfg {
    pub enabled: bool,
    pub password: String,
}

/// One SMB share entry under [smb].shares: the share name IS the drive
/// letter (product decision: "whatever drive the software sees, share it
/// under its own letter" — C:\ becomes \\host\C, E:\ becomes \\host\E).
/// `grant` is the SHARE-level access granted at creation (NTFS ACLs remain
/// the real permission wall); a grant with an empty string falls back to
/// the built-in default.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct SmbShareItem {
    pub name: String,
    pub path: String,
    pub grant: String,
}

impl Default for SmbShareItem {
    fn default() -> Self {
        SmbShareItem { name: String::new(), path: String::new(), grant: String::new() }
    }
}

/// [smb] — SMB share lifecycle ("open the app, the drives appear on the
/// network"; stop the app, they disappear). The panel creates each
/// configured share at boot and removes it at shutdown, so a stopped panel
/// never exposes the disks. Requires an elevated panel (`net share` needs
/// admin rights; the autostart scheduled task runs with highest privileges).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct SmbCfg {
    pub enabled: bool,
    /// Share entries; empty list = manage nothing even when enabled.
    pub shares: Vec<SmbShareItem>,
    /// Built-in share-level grant used when an entry's grant is empty.
    pub default_grant: String,
    /// "无密码访问" support. When true (default), the panel applies the
    /// Windows SMB anonymous/guest access baseline (Guest account enabled,
    /// anonymous sessions allowed, Guest removed from the network-logon deny
    /// list) and grants Everyone:Read on each mounted drive root, so any
    /// device on the private network can browse the shares without
    /// credentials. Turning it off restores the stock restricting policies.
    pub guest_access: bool,
}

impl Default for SmbCfg {
    fn default() -> Self {
        // No machine-specific shares by default: a fresh install on any PC
        // starts with an empty set and the SMB hotplug watcher shares every
        // present fixed/removable drive within seconds of boot (see smb.rs).
        // Shipping C:/E: defaults would show phantom "unmounted" entries on
        // machines without those letters.
        SmbCfg {
            enabled: true,
            shares: Vec::new(),
            default_grant: "Everyone,READ".into(),
            guest_access: true,
        }
    }
}

/// Full config file schema. A missing section or field always falls back to
/// the built-in default; only a type error is reported.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct FileConfig {
    pub server: ServerCfg,
    pub webcli: WebcliCfg,
    pub term: TermCfg,
    pub sync: SyncCfg,
    pub daemon: DaemonCfg,
    pub wsl: WslCfg,
    pub docker: DockerCfg,
    pub health: HealthCfg,
    pub system: SystemCfg,
    pub access: AccessCfg,
    pub services: ServicesCfg,
    pub ui: UiCfg,
    pub launcher: LauncherCfg,
    pub flags: FlagsCfg,
    pub tray: TrayCfg,
    pub control: ControlCfg,
    pub auth: AuthCfg,
    pub openlist: OpenlistCfg,
    pub smb: SmbCfg,
}

impl Default for ServerCfg {
    fn default() -> Self {
        ServerCfg { addr: DEFAULT_SERVER_ADDR.into(), bind_retries: 10, bind_retry_ms: 350 }
    }
}
impl Default for WebcliCfg {
    fn default() -> Self {
        WebcliCfg { host: "127.0.0.1".into(), port: DEFAULT_WEB_CLI_PORT, shell: "cmd.exe".into() }
    }
}
impl Default for TermCfg {
    fn default() -> Self {
        TermCfg {
            session_ttl_hours: 12,
            password_min_len: 4,
            webcli_addr: None, // derived from [webcli] host:port at runtime
        }
    }
}
impl Default for SyncCfg {
    fn default() -> Self {
        SyncCfg {
            interval_secs: 600,
            syncthing_addr: DEFAULT_SYNCTHING_ADDR.into(),
            timeout_secs: 3,
            sync_timeout_secs: 2,
            onboarding_timeout_secs: 5,
        }
    }
}
impl Default for DaemonCfg {
    fn default() -> Self {
        DaemonCfg {
            usb_poll_secs: 15,
            probe_timeout_secs: 10,
            vdisp_cache_secs: 60,
            instance_retries: 15,
            instance_retry_ms: 200,
            eject_grace_secs: 2,
        }
    }
}
impl Default for WslCfg {
    fn default() -> Self {
        WslCfg {
            distro: DEFAULT_WSL_DISTRO.into(),
            keepalive_poll_secs: 30,
            cmd_timeout_secs: 30,
            ps_timeout_secs: 10,
            vm_wait_secs: 15,
            vm_poll_step_secs: 2,
        }
    }
}
impl Default for DockerCfg {
    fn default() -> Self {
        DockerCfg { status_timeout_secs: 15, action_timeout_secs: 120 }
    }
}
impl Default for HealthCfg {
    fn default() -> Self {
        HealthCfg {
            probe_timeout_secs: 3,
            color_green_max_ms: 200,
            color_yellow_max_ms: 1000,
            error_rate_healthy_pct: 5,
            uptime_cache_secs: 60,
        }
    }
}
impl Default for SystemCfg {
    fn default() -> Self {
        SystemCfg { cache_ttl_secs: 12, route_stats_max: 512, api_body_max_kb: 8 }
    }
}
impl Default for AccessCfg {
    fn default() -> Self {
        AccessCfg {
            uac_wait_secs: 12,
            cache_ttl_secs: 12,
            port_spec_max_len: 64,
            label_max_len: 24,
            name_max_len: 64,
            tailscale_cidr: DEFAULT_TAILSCALE_CIDR.into(),
        }
    }
}
impl Default for ServicesPorts {
    fn default() -> Self {
        ServicesPorts {
            webcli: DEFAULT_WEB_CLI_PORT,
            syncthing: DEFAULT_SYNCTHING_PORT,
            homepage: DEFAULT_HOMEPAGE_PORT,
        }
    }
}
impl Default for ServicesCfg {
    fn default() -> Self {
        ServicesCfg { default_start: vec!["webcli".into()], ports: ServicesPorts::default() }
    }
}
impl Default for UiCfg {
    fn default() -> Self {
        UiCfg {
            poll_ms: 15000,
            tray_poll_ms: 5000,
            phone_poll_ms: 5000,
            embed_timeout_ms: 8000,
            boot_poll_ms: 2500,
            boot_timeout_ms: 90000,
            embed_probe_ms: 5000,
            fetch_retries: 3,
            cpu_warn_pct: 85,
            mem_warn_pct: 90,
            temp_err_c: 70,
            temp_warn_c: 60,
            smart_temp_err_c: 55,
            smart_wear_err_pct: 60,
            ws_reconnect_ms: vec![1000, 10000],
        }
    }
}
impl Default for LauncherCfg {
    fn default() -> Self {
        LauncherCfg { wait_secs: 120, poll_interval_secs: 1, connect_timeout_ms: 500 }
    }
}
impl Default for FeatureFlags {
    fn default() -> Self {
        FeatureFlags { auto_retry: true, usb_eject: true }
    }
}
impl Default for FlagsCfg {
    fn default() -> Self {
        FlagsCfg { feature: FeatureFlags::default() }
    }
}
impl Default for TrayCfg {
    fn default() -> Self {
        TrayCfg { balloon_ms: 6000 }
    }
}
impl Default for ControlCfg {
    fn default() -> Self {
        ControlCfg {
            enabled: false,
            base_url: DEFAULT_CONTROL_BASE_URL.into(),
            device_name: "flygo-panel".into(),
            heartbeat_secs: DEFAULT_CONTROL_HEARTBEAT_SECS,
            upload_max_mb: DEFAULT_CONTROL_UPLOAD_MAX_MB,
        }
    }
}
impl Default for AuthCfg {
    fn default() -> Self {
        // Default OFF: the panel stays open exactly like the pre-auth builds.
        AuthCfg { enabled: false, password: String::new() }
    }
}
/// [openlist] — 自动把本机盘符同步为 OpenList 本地存储（"插盘即在列表里"）。
/// Default OFF。password 遵循 [auth] 的例外规则：允许写在本地 flygo.toml，
/// 但绝不进日志/API 响应/报错（见 masking）；环境变量 FENGPAGE_OPENLIST_*
/// 优先（换机/迁移时不用改文件）。
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct OpenlistCfg {
    pub enabled: bool,
    pub base_url: String,
    pub username: String,
    pub password: String,
}
impl Default for OpenlistCfg {
    fn default() -> Self {
        // Default OFF: no OpenList traffic until the user opts in with
        // credentials (fail-closed: enabled without creds syncs nothing).
        OpenlistCfg {
            enabled: false,
            base_url: "http://127.0.0.1:5244".into(),
            username: String::new(),
            password: String::new(),
        }
    }
}
impl Default for FileConfig {
    fn default() -> Self {
        FileConfig {
            server: ServerCfg::default(),
            webcli: WebcliCfg::default(),
            term: TermCfg::default(),
            sync: SyncCfg::default(),
            daemon: DaemonCfg::default(),
            wsl: WslCfg::default(),
            docker: DockerCfg::default(),
            health: HealthCfg::default(),
            system: SystemCfg::default(),
            access: AccessCfg::default(),
            services: ServicesCfg::default(),
            ui: UiCfg::default(),
            launcher: LauncherCfg::default(),
            flags: FlagsCfg::default(),
            tray: TrayCfg::default(),
            control: ControlCfg::default(),
            auth: AuthCfg::default(),
            openlist: OpenlistCfg::default(),
            smb: SmbCfg::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// resolution + loading
// ---------------------------------------------------------------------------

/// Value source for the /api/config source annotations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Env,
    File,
    Default,
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::File => "file",
            Source::Default => "default",
        }
    }
}

/// One validation/parse problem found while loading or validating a config.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

/// Loaded configuration: resolved file path, raw file values and any
/// non-fatal problems. `raw` equals the built-in defaults when no file is
/// present (behavior identical to a build without a config file).
#[derive(Clone, Debug)]
pub struct Settings {
    /// Resolved config file path (FENGPAGE_CONFIG > root config/flygo.toml >
    /// legacy C:/FlyGoLegacy/flygo.toml; always a concrete path, also the PUT
    /// target when the file does not exist yet).
    pub path: PathBuf,
    /// Whether `path` exists on disk.
    pub exists: bool,
    /// Values read from the file; built-in defaults where absent.
    pub raw: FileConfig,
    /// Non-fatal parse/validation problems (never blocks startup).
    /// Reserved for the self-check page; currently write-only (kept, not removed).
    #[allow(dead_code)]
    pub errors: Vec<FieldError>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

// ---------------------------------------------------------------------------
// Mutable SMB overlay
//
// The frozen Settings above is written once at boot and cannot change. But the
// SMB section is user-editable at runtime (mount/unmount/guest via /api/smb),
// and persist_smb only rewrote the on-disk flygo.toml - leaving the in-memory
// copy stale so the panel/UI kept showing the old drive set until a restart.
// This overlay holds the live [smb] state, primed from the file at boot and
// refreshed by persist_smb after every write, so the frozen accessors report
// what the disk actually contains right now.
// ---------------------------------------------------------------------------
static SMB_LIVE: OnceLock<Mutex<SmbCfg>> = OnceLock::new();

/// Snapshot of the live mutable SMB overlay (updated by persist_smb so the
/// runtime reflects disk immediately after a write). Falls back to the frozen
/// value when the overlay has not been primed (defensive; init() primes it).
fn smb_live() -> Option<SmbCfg> {
    SMB_LIVE.get().and_then(|m| m.lock().ok().map(|g| g.clone()))
}

/// Parse the config file once at startup and freeze the result. Must run
/// before settings::get() (called from config::init()).
pub fn init() {
    let _ = SETTINGS.set(Settings::load());
    // Prime the mutable SMB overlay from the frozen file state so the first
    // runtime reads match disk at boot.
    let _ = SMB_LIVE.set(Mutex::new(get().raw.smb.clone()));
}

/// Access the frozen startup configuration.
pub fn get() -> &'static Settings {
    SETTINGS.get().expect("settings::init() must run before settings::get()")
}

/// Non-panicking access for contexts that may run before init (unit tests,
/// early probes). None = fall back to built-in defaults.
pub fn try_get() -> Option<&'static Settings> {
    SETTINGS.get()
}

/// Non-empty env var, or None.
fn env_ok(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// Install root: FENGPAGE_ROOT > walk up from the exe looking for a
/// `webcli/` or `scripts/` sibling. Mirrors panel/src/config.rs.
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

/// Resolve the config file location: (path, exists_on_disk).
fn resolve_path() -> (PathBuf, bool) {
    if let Some(v) = env_ok("FENGPAGE_CONFIG") {
        let p = PathBuf::from(v);
        let exists = p.exists();
        return (p, exists);
    }
    let root_path = detect_root().join("config").join(CONFIG_FILE_NAME);
    if root_path.exists() {
        return (root_path, true);
    }
    let legacy = PathBuf::from(LEGACY_CONFIG_PATH);
    if legacy.exists() {
        return (legacy, true);
    }
    (root_path, false)
}

/// The path PUT /api/config writes to (same resolution as the read path).
pub fn config_target() -> PathBuf {
    resolve_path().0
}

/// Persist only the [smb] section of the on-disk flygo.toml (drive-letter
/// set + guest access flag) without touching any other section. The file is
/// parsed as a generic TOML value, the [smb] table replaced in place and the
/// document re-serialized atomically (tmp + rename). Best-effort; a missing
/// or unparseable file means we cannot safely merge, so we report it and the
/// caller keeps working in-memory. Returns the bytes actually written.
pub fn persist_smb(shares: &[SmbShareItem], guest_access: bool) -> Result<usize, String> {
    use std::io::Write;
    let target = config_target();
    let text = std::fs::read_to_string(&target)
        .map_err(|e| format!("read {} failed: {e}", target.display()))?;
    let mut doc: toml::Value = toml::from_str(&text)
        .map_err(|e| format!("parse {} failed: {e}", target.display()))?;
    let table = match doc.as_table_mut() {
        Some(t) => t,
        None => return Err("flygo.toml is not a table".into()),
    };
    // Rebuild the shares array preserving each entry's grant (empty string
    // falls back to the built-in grant at create time).
    let shares_val: Vec<toml::Value> = shares
        .iter()
        .map(|s| {
            let mut m = toml::map::Map::new();
            m.insert("name".into(), toml::Value::String(s.name.clone()));
            m.insert("path".into(), toml::Value::String(s.path.clone()));
            m.insert("grant".into(), toml::Value::String(s.grant.clone()));
            toml::Value::Table(m)
        })
        .collect();
    table.insert(
        "smb".into(),
        toml::Value::Table({
            let mut m = toml::map::Map::new();
            m.insert("enabled".into(), toml::Value::Boolean(true));
            m.insert("guest_access".into(), toml::Value::Boolean(guest_access));
            m.insert("shares".into(), toml::Value::Array(shares_val));
            m
        }),
    );
    let out = toml::to_string(&doc).map_err(|e| format!("serialize failed: {e}"))?;
    let tmp = target.with_extension("toml.tmp");
    {
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("create {} failed: {e}", tmp.display()))?;
        f.write_all(out.as_bytes())
            .map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    }
    let bytes = out.len();
    std::fs::rename(&tmp, &target)
        .map_err(|e| format!("replace {} failed: {e}", target.display()))?;
    // Refresh the in-memory overlay so the panel/UI reflects the change
    // immediately instead of only after a restart (persist_smb always writes
    // enabled = true for the [smb] table it manages).
    if let Some(m) = SMB_LIVE.get() {
        if let Ok(mut g) = m.lock() {
            g.shares = shares.to_vec();
            g.guest_access = guest_access;
            g.enabled = true;
        }
    }
    Ok(bytes)
}

impl Settings {
    /// Read the file (if present), apply defaults, run validation.
    pub fn load() -> Settings {
        let (path, exists) = resolve_path();
        let mut raw = FileConfig::default();
        let mut errors: Vec<FieldError> = Vec::new();
        if exists {
            match std::fs::read_to_string(&path) {
                Ok(text) => match toml::from_str::<FileConfig>(&text) {
                    Ok(cfg) => raw = cfg,
                    Err(e) => {
                        let (line, col) = span_line_col(&text, e.span());
                        let field = key_from_span(&text, e.span())
                            .or_else(|| key_from_message(e.message()))
                            .unwrap_or_else(|| CONFIG_FILE_NAME.into());
                        errors.push(FieldError {
                            field,
                            message: format!("line {line}, col {col}: {}", e.message()),
                        });
                    }
                },
                Err(e) => errors.push(FieldError {
                    field: CONFIG_FILE_NAME.into(),
                    message: format!("read {} failed: {e}", path.display()),
                }),
            }
        }
        errors.extend(validate(&raw));
        Settings { path, exists, raw, errors }
    }

    // ---- effective accessors (env > file > default) ----

    /// Effective panel listen address with its source.
    pub fn server_addr(&self) -> (String, Source) {
        server_addr_pick(env_ok("FENGPAGE_ADDR").as_deref(), self.exists, &self.raw.server.addr)
    }

    pub fn server_bind_retries(&self) -> u8 {
        file_or(&self.raw.server.bind_retries, &ServerCfg::default().bind_retries)
    }

    pub fn server_bind_retry_ms(&self) -> u64 {
        file_or(&self.raw.server.bind_retry_ms, &ServerCfg::default().bind_retry_ms)
    }

    pub fn webcli_host(&self) -> String {
        file_or(&self.raw.webcli.host, &WebcliCfg::default().host)
    }

    pub fn webcli_port(&self) -> u16 {
        file_or(&self.raw.webcli.port, &WebcliCfg::default().port)
    }

    pub fn webcli_shell(&self) -> String {
        file_or(&self.raw.webcli.shell, &WebcliCfg::default().shell)
    }

    /// Terminal WS reverse proxy target: [term].webcli_addr when set, else
    /// derived from the effective [webcli] host:port (keeps a port change in
    /// one place consistent with the term proxy).
    pub fn term_webcli_addr(&self) -> String {
        match &self.raw.term.webcli_addr {
            Some(v) if !v.trim().is_empty() => v.clone(),
            _ => format!("{}:{}", self.webcli_host(), self.webcli_port()),
        }
    }

    pub fn sync_interval_secs(&self) -> u64 {
        file_or(&self.raw.sync.interval_secs, &SyncCfg::default().interval_secs)
    }

    pub fn syncthing_addr(&self) -> String {
        file_or(&self.raw.sync.syncthing_addr, &SyncCfg::default().syncthing_addr)
    }

    /// /api/syncview REST timeout.
    pub fn syncview_timeout_secs(&self) -> u64 {
        file_or(&self.raw.sync.timeout_secs, &SyncCfg::default().timeout_secs)
    }

    /// /api/sync REST timeout.
    pub fn sync_timeout_secs(&self) -> u64 {
        file_or(&self.raw.sync.sync_timeout_secs, &SyncCfg::default().sync_timeout_secs)
    }

    /// /api/onboarding REST timeout (slower scanning allowance).
    pub fn onboarding_timeout_secs(&self) -> u64 {
        file_or(&self.raw.sync.onboarding_timeout_secs, &SyncCfg::default().onboarding_timeout_secs)
    }

    /// Managed component ports (webcli, syncthing, homepage).
    pub fn services_ports(&self) -> (u16, u16, u16) {
        let p = &self.raw.services.ports;
        let d = ServicesPorts::default();
        (file_or(&p.webcli, &d.webcli), file_or(&p.syncthing, &d.syncthing), file_or(&p.homepage, &d.homepage))
    }

    // Used by the launcher binary (src/bin/launcher.rs, which shares this
    // module via #[path]); dead in the panel binary itself.
    #[allow(dead_code)]
    pub fn launcher_wait_secs(&self) -> u64 {
        file_or(&self.raw.launcher.wait_secs, &LauncherCfg::default().wait_secs)
    }

    #[allow(dead_code)]
    pub fn launcher_poll_interval_secs(&self) -> u64 {
        file_or(&self.raw.launcher.poll_interval_secs, &LauncherCfg::default().poll_interval_secs)
    }

    #[allow(dead_code)]
    pub fn launcher_connect_timeout_ms(&self) -> u64 {
        file_or(&self.raw.launcher.connect_timeout_ms, &LauncherCfg::default().connect_timeout_ms)
    }

    /// Browser/probe URL for the panel page (unspecified host -> 127.0.0.1).
    pub fn panel_browser_url(&self) -> String {
        let (addr, _) = self.server_addr();
        let sa: SocketAddr = addr.parse().unwrap_or_else(|_| DEFAULT_SERVER_ADDR.parse().unwrap());
        let host = if sa.ip().is_unspecified() { "127.0.0.1".to_string() } else { sa.ip().to_string() };
        format!("http://{host}:{}", sa.port())
    }

    // ---- [control] effective accessors (FENGPAGE_CONTROL_* > file > default) ----

    /// Whether the heartbeat task reports to the control plane. Env override:
    /// FENGPAGE_CONTROL_ENABLED accepts 1/true/yes/on.
    pub fn control_enabled(&self) -> bool {
        match env_ok("FENGPAGE_CONTROL_ENABLED") {
            Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            None => self.raw.control.enabled,
        }
    }

    /// Control-plane base URL (env FENGPAGE_CONTROL_BASE_URL wins).
    pub fn control_base_url(&self) -> String {
        env_ok("FENGPAGE_CONTROL_BASE_URL").unwrap_or_else(|| self.raw.control.base_url.clone())
    }

    /// Device name reported at registration (env FENGPAGE_CONTROL_DEVICE_NAME).
    pub fn control_device_name(&self) -> String {
        env_ok("FENGPAGE_CONTROL_DEVICE_NAME").unwrap_or_else(|| self.raw.control.device_name.clone())
    }

    /// Heartbeat interval in seconds (env FENGPAGE_CONTROL_HEARTBEAT_SECS).
    pub fn control_heartbeat_secs(&self) -> u64 {
        env_ok("FENGPAGE_CONTROL_HEARTBEAT_SECS")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| self.raw.control.heartbeat_secs)
    }

    /// POST /api/upload size cap in MB (env FENGPAGE_CONTROL_UPLOAD_MAX_MB).
    pub fn control_upload_max_mb(&self) -> u64 {
        env_ok("FENGPAGE_CONTROL_UPLOAD_MAX_MB")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| self.raw.control.upload_max_mb)
    }

    // ---- [auth] effective accessors (file-only, no env override) ----

    /// Whether the login-layer middleware protects the panel (M09).
    /// Restart-effective: frozen at boot together with the whole Settings.
    pub fn auth_enabled(&self) -> bool {
        self.raw.auth.enabled
    }

    /// Panel login password ([auth].password). Never log, never serialize
    /// into API responses (see default_json/effective_json masking).
    pub fn auth_password(&self) -> String {
        self.raw.auth.password.clone()
    }

    // ---- [openlist] effective accessors (FENGPAGE_OPENLIST_* > file > default) ----

    /// Whether newly-present drives are auto-synced as OpenList storages.
    /// Env override: FENGPAGE_OPENLIST_ENABLED accepts 1/true/yes/on.
    pub fn openlist_enabled(&self) -> bool {
        match env_ok("FENGPAGE_OPENLIST_ENABLED") {
            Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            None => self.raw.openlist.enabled,
        }
    }

    /// OpenList base URL (env FENGPAGE_OPENLIST_BASE_URL wins).
    pub fn openlist_base_url(&self) -> String {
        env_ok("FENGPAGE_OPENLIST_BASE_URL").unwrap_or_else(|| self.raw.openlist.base_url.clone())
    }

    /// OpenList admin username (env FENGPAGE_OPENLIST_USERNAME wins).
    pub fn openlist_username(&self) -> String {
        env_ok("FENGPAGE_OPENLIST_USERNAME").unwrap_or_else(|| self.raw.openlist.username.clone())
    }

    /// OpenList admin password (env FENGPAGE_OPENLIST_PASSWORD wins).
    /// Never log, never serialize into API responses (masked like [auth]).
    pub fn openlist_password(&self) -> String {
        env_ok("FENGPAGE_OPENLIST_PASSWORD").unwrap_or_else(|| self.raw.openlist.password.clone())
    }

    // ---- [smb] effective accessors ----

    /// Whether SMB share lifecycle management is on (boot-create / exit-remove).
    /// Reads the live overlay so a runtime edit is reflected immediately.
    pub fn smb_enabled(&self) -> bool {
        smb_live().map(|c| c.enabled).unwrap_or_else(|| self.raw.smb.enabled)
    }

    /// The configured SMB share entries (name = drive letter, path). Reads the
    /// live overlay (updated by persist_smb) so mount/unmount changes show on
    /// the very next read instead of waiting for a restart.
    pub fn smb_shares(&self) -> Vec<SmbShareItem> {
        smb_live().map(|c| c.shares).unwrap_or_else(|| self.raw.smb.shares.clone())
    }

    /// Share-level grant used when an entry carries an empty grant.
    pub fn smb_default_grant(&self) -> &str {
        let g = self.raw.smb.default_grant.trim();
        if g.is_empty() { "Everyone,READ" } else { g }
    }

    /// Whether the panel applies the SMB anonymous/guest access baseline.
    /// Reads the live overlay so a runtime guest toggle is reflected immediately.
    pub fn smb_guest_access(&self) -> bool {
        smb_live().map(|c| c.guest_access).unwrap_or_else(|| self.raw.smb.guest_access)
    }

    // ---- [wsl] effective accessors ----

    /// Managed WSL distro name (env FENGPAGE_WSL_DISTRO wins, then the
    /// [wsl].distro file value, then the built-in default). Never hardcode a
    /// distro name at call sites; always go through here.
    pub fn wsl_distro(&self) -> String {
        env_ok("FENGPAGE_WSL_DISTRO").unwrap_or_else(|| {
            let f = self.raw.wsl.distro.trim();
            if f.is_empty() { DEFAULT_WSL_DISTRO.into() } else { f.into() }
        })
    }
}

/// File value when present, else the built-in default.
fn file_or<T: PartialEq + Clone>(file: &T, def: &T) -> T {
    if file != def { file.clone() } else { def.clone() }
}

/// server.addr layering: FENGPAGE_ADDR (when it parses) > [server].addr
/// (when a file exists and the value parses) > built-in default. A
/// set-but-invalid env value falls through to the file, keeping the panel
/// bootable in every case; with no file the source is always "default".
fn server_addr_pick(env: Option<&str>, exists: bool, file: &str) -> (String, Source) {
    if let Some(v) = env {
        if v.parse::<SocketAddr>().is_ok() {
            return (v.to_string(), Source::Env);
        }
    }
    if exists && file.parse::<SocketAddr>().is_ok() {
        return (file.to_string(), Source::File);
    }
    (DEFAULT_SERVER_ADDR.to_string(), Source::Default)
}

// ---------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------

/// Validate a parsed config. Errors are keyed per field; used both at load
/// (non-fatal, surfaced by GET /api/config) and by PUT /api/config (reject).
pub fn validate(cfg: &FileConfig) -> Vec<FieldError> {
    let mut errs: Vec<FieldError> = Vec::new();
    if cfg.server.addr.parse::<SocketAddr>().is_err() {
        errs.push(FieldError {
            field: "server.addr".into(),
            message: format!("invalid socket address `{}` (expected host:port)", cfg.server.addr),
        });
    }
    if cfg.server.bind_retries == 0 {
        errs.push(FieldError {
            field: "server.bind_retries".into(),
            message: "must be >= 1".into(),
        });
    }
    if cfg.server.bind_retry_ms == 0 {
        errs.push(FieldError {
            field: "server.bind_retry_ms".into(),
            message: "must be >= 1".into(),
        });
    }
    // Linkage rule (global-config-design.md, risk 1): the slow-collect cache
    // TTL must stay strictly below the frontend poll interval, otherwise the
    // cache never refreshes between polls.
    let poll_s = cfg.ui.poll_ms / 1000;
    if cfg.system.cache_ttl_secs >= poll_s {
        errs.push(FieldError {
            field: "system.cache_ttl_secs".into(),
            message: format!(
                "must be strictly less than ui.poll_ms/1000 ({poll_s}) so the slow-collect cache refreshes between frontend polls"
            ),
        });
    }
    if cfg.ui.poll_ms == 0 {
        errs.push(FieldError { field: "ui.poll_ms".into(), message: "must be >= 1".into() });
    }
    if cfg.ui.ws_reconnect_ms.len() != 2 {
        errs.push(FieldError {
            field: "ui.ws_reconnect_ms".into(),
            message: "expected [min_ms, max_ms] with exactly 2 entries".into(),
        });
    } else if cfg.ui.ws_reconnect_ms[0] > cfg.ui.ws_reconnect_ms[1] {
        errs.push(FieldError {
            field: "ui.ws_reconnect_ms".into(),
            message: "first entry must be <= second entry".into(),
        });
    }
    // [control]: base URL must be http(s); the heartbeat cadence must stay
    // below the control-plane offline window (15 min) so the node is never
    // flagged offline by its own interval.
    if !(cfg.control.base_url.starts_with("http://") || cfg.control.base_url.starts_with("https://")) {
        errs.push(FieldError {
            field: "control.base_url".into(),
            message: format!("must start with http:// or https:// (got `{}`)", cfg.control.base_url),
        });
    }
    if cfg.control.device_name.trim().is_empty() || cfg.control.device_name.trim().chars().count() > 64 {
        errs.push(FieldError {
            field: "control.device_name".into(),
            message: "must be 1-64 chars after trimming".into(),
        });
    }
    if !(10..CONTROL_OFFLINE_WINDOW_SECS).contains(&cfg.control.heartbeat_secs) {
        errs.push(FieldError {
            field: "control.heartbeat_secs".into(),
            message: format!(
                "must be 10..{} (below the 15-min offline window so the node stays online)",
                CONTROL_OFFLINE_WINDOW_SECS
            ),
        });
    }
    if !(1..=4096).contains(&cfg.control.upload_max_mb) {
        errs.push(FieldError {
            field: "control.upload_max_mb".into(),
            message: "must be 1..=4096".into(),
        });
    }
    // [auth]: enabling the login layer requires a password. Fail-closed at
    // runtime (an empty password can never log in, see api/auth.rs); this
    // check surfaces the misconfiguration early via GET /api/config errors.
    // The message must never quote the stored value.
    if cfg.auth.enabled && cfg.auth.password.trim().is_empty() {
        errs.push(FieldError {
            field: "auth.password".into(),
            message: "must be set (non-empty) when auth.enabled is true; edit flygo.toml directly and restart".into(),
        });
    }
    // [smb]: validate each configured share (name must look like a drive
    // letter, path must be a plain drive root with forward-consistent form).
    // Only the enabled flag governs lifecycle; a stray entry is reported but
    // never crashes the panel.
    if cfg.smb.enabled {
        for (i, s) in cfg.smb.shares.iter().enumerate() {
            let name = s.name.trim();
            if name.len() != 1 || !name.chars().next().unwrap().is_ascii_alphabetic() {
                errs.push(FieldError {
                    field: format!("smb.shares[{i}].name"),
                    message: format!(
                        "must be a single drive letter (e.g. `C`); got `{}`",
                        s.name
                    ),
                });
            }
            let p = s.path.trim();
            if !(p.len() == 3 && p.as_bytes()[1] == b':' && (p.ends_with('\\') || p.ends_with('/'))) {
                errs.push(FieldError {
                    field: format!("smb.shares[{i}].path"),
                    message: format!(
                        "must be a drive root like `C:\\` or `E:\\`; got `{}`",
                        s.path
                    ),
                });
            }
        }
    }
    errs
}

/// 1-based line/col of a toml byte span (used for parse error reporting).
pub fn span_line_col(text: &str, span: Option<std::ops::Range<usize>>) -> (usize, usize) {
    let start = span.map(|r| r.start).unwrap_or(0).min(text.len());
    let prefix = &text[..start];
    let line = prefix.matches('\n').count() + 1;
    let col = start - prefix.rfind('\n').map(|i| i + 1).unwrap_or(0) + 1;
    (line, col)
}

/// Best-effort dotted key (e.g. "server.addr") for a toml error span:
/// the assignment key on the errored line prefixed by the enclosing
/// [section] header. toml 0.8 errors carry a byte span but no key path.
pub fn key_from_span(text: &str, span: Option<std::ops::Range<usize>>) -> Option<String> {
    let start = span?.start.min(text.len());
    let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[start..].find('\n').map(|i| start + i).unwrap_or(text.len());
    let key_part = text[line_start..line_end].split('=').next()?.trim();
    if key_part.is_empty() {
        return None;
    }
    let mut section: Option<String> = None;
    for l in text[..line_start].lines().rev() {
        let t = l.trim();
        if t.starts_with('[') && t.ends_with(']') && !t.starts_with("[[") {
            section = Some(t[1..t.len() - 1].trim().to_string());
            break;
        }
    }
    Some(match section {
        Some(s) if !s.is_empty() => format!("{s}.{key_part}"),
        _ => key_part.to_string(),
    })
}

/// Extract the dotted key from a toml error message (`... for key `x.y``),
/// kept as a fallback for toml versions that embed the key in the message.
pub fn key_from_message(msg: &str) -> Option<String> {
    msg.split("for key `").nth(1).and_then(|s| s.split('`').next()).map(|s| s.trim().to_string())
}

// ---------------------------------------------------------------------------
// /api/config JSON payloads
// ---------------------------------------------------------------------------

/// Built-in defaults as a plain JSON document (no source annotations).
pub fn default_json() -> Value {
    let mut v = serde_json::to_value(FileConfig::default()).expect("defaults serialize");
    // term.webcli_addr default is derived from [webcli], not null.
    v["term"]["webcli_addr"] = json!(DEFAULT_TERM_WEB_CLI_ADDR);
    // Secrets discipline: the [auth] password must never leave the process
    // through any API payload, not even as a default (defensive mask; the
    // built-in default is the empty string anyway).
    v["auth"]["password"] = json!("");
    // Same discipline for [openlist]: admin password never leaves the process.
    v["openlist"]["password"] = json!("");
    // [smb] serializes fully (no secrets inside). Entries already serialize
    // their grant; leave as-is.
    v
}

/// One effective value annotated with its source.
fn eff<T: Serialize>(value: T, source: &'static str) -> Value {
    json!({ "value": value, "source": source })
}

/// Effective JSON: merged env > file > default with per-item source tags.
pub fn effective_json(self_s: &Settings) -> Value {
    let def = FileConfig::default();
    // Live mutable SMB state (updated by persist_smb) so the payload reflects
    // the in-memory share set the panel manages, not the frozen boot value.
    let live_smb = smb_live().unwrap_or_else(|| self_s.raw.smb.clone());
    let (addr, addr_src) = self_s.server_addr();
    let webcli_addr = match &self_s.raw.term.webcli_addr {
        Some(v) if !v.trim().is_empty() => eff(v, "file"),
        _ => eff(self_s.term_webcli_addr(), "default"),
    };
    json!({
        "server": {
            "addr": eff(addr, addr_src.label()),
            "bind_retries": eff(&self_s.raw.server.bind_retries, src_of(&self_s.raw.server.bind_retries, &def.server.bind_retries)),
            "bind_retry_ms": eff(&self_s.raw.server.bind_retry_ms, src_of(&self_s.raw.server.bind_retry_ms, &def.server.bind_retry_ms)),
        },
        "webcli": {
            "host": eff(&self_s.raw.webcli.host, src_of(&self_s.raw.webcli.host, &def.webcli.host)),
            "port": eff(&self_s.raw.webcli.port, src_of(&self_s.raw.webcli.port, &def.webcli.port)),
            "shell": eff(&self_s.raw.webcli.shell, src_of(&self_s.raw.webcli.shell, &def.webcli.shell)),
        },
        "term": {
            "session_ttl_hours": eff(&self_s.raw.term.session_ttl_hours, src_of(&self_s.raw.term.session_ttl_hours, &def.term.session_ttl_hours)),
            "password_min_len": eff(&self_s.raw.term.password_min_len, src_of(&self_s.raw.term.password_min_len, &def.term.password_min_len)),
            "webcli_addr": webcli_addr,
        },
        "sync": {
            "interval_secs": eff(&self_s.raw.sync.interval_secs, src_of(&self_s.raw.sync.interval_secs, &def.sync.interval_secs)),
            "syncthing_addr": eff(&self_s.raw.sync.syncthing_addr, src_of(&self_s.raw.sync.syncthing_addr, &def.sync.syncthing_addr)),
            "timeout_secs": eff(&self_s.raw.sync.timeout_secs, src_of(&self_s.raw.sync.timeout_secs, &def.sync.timeout_secs)),
            "sync_timeout_secs": eff(&self_s.raw.sync.sync_timeout_secs, src_of(&self_s.raw.sync.sync_timeout_secs, &def.sync.sync_timeout_secs)),
            "onboarding_timeout_secs": eff(&self_s.raw.sync.onboarding_timeout_secs, src_of(&self_s.raw.sync.onboarding_timeout_secs, &def.sync.onboarding_timeout_secs)),
        },
        "daemon": {
            "usb_poll_secs": eff(&self_s.raw.daemon.usb_poll_secs, src_of(&self_s.raw.daemon.usb_poll_secs, &def.daemon.usb_poll_secs)),
            "probe_timeout_secs": eff(&self_s.raw.daemon.probe_timeout_secs, src_of(&self_s.raw.daemon.probe_timeout_secs, &def.daemon.probe_timeout_secs)),
            "vdisp_cache_secs": eff(&self_s.raw.daemon.vdisp_cache_secs, src_of(&self_s.raw.daemon.vdisp_cache_secs, &def.daemon.vdisp_cache_secs)),
            "instance_retries": eff(&self_s.raw.daemon.instance_retries, src_of(&self_s.raw.daemon.instance_retries, &def.daemon.instance_retries)),
            "instance_retry_ms": eff(&self_s.raw.daemon.instance_retry_ms, src_of(&self_s.raw.daemon.instance_retry_ms, &def.daemon.instance_retry_ms)),
            "eject_grace_secs": eff(&self_s.raw.daemon.eject_grace_secs, src_of(&self_s.raw.daemon.eject_grace_secs, &def.daemon.eject_grace_secs)),
        },
        "wsl": {
            "distro": eff(&self_s.raw.wsl.distro, src_of(&self_s.raw.wsl.distro, &def.wsl.distro)),
            "keepalive_poll_secs": eff(&self_s.raw.wsl.keepalive_poll_secs, src_of(&self_s.raw.wsl.keepalive_poll_secs, &def.wsl.keepalive_poll_secs)),
            "cmd_timeout_secs": eff(&self_s.raw.wsl.cmd_timeout_secs, src_of(&self_s.raw.wsl.cmd_timeout_secs, &def.wsl.cmd_timeout_secs)),
            "ps_timeout_secs": eff(&self_s.raw.wsl.ps_timeout_secs, src_of(&self_s.raw.wsl.ps_timeout_secs, &def.wsl.ps_timeout_secs)),
            "vm_wait_secs": eff(&self_s.raw.wsl.vm_wait_secs, src_of(&self_s.raw.wsl.vm_wait_secs, &def.wsl.vm_wait_secs)),
            "vm_poll_step_secs": eff(&self_s.raw.wsl.vm_poll_step_secs, src_of(&self_s.raw.wsl.vm_poll_step_secs, &def.wsl.vm_poll_step_secs)),
        },
        "docker": {
            "status_timeout_secs": eff(&self_s.raw.docker.status_timeout_secs, src_of(&self_s.raw.docker.status_timeout_secs, &def.docker.status_timeout_secs)),
            "action_timeout_secs": eff(&self_s.raw.docker.action_timeout_secs, src_of(&self_s.raw.docker.action_timeout_secs, &def.docker.action_timeout_secs)),
        },
        "health": {
            "probe_timeout_secs": eff(&self_s.raw.health.probe_timeout_secs, src_of(&self_s.raw.health.probe_timeout_secs, &def.health.probe_timeout_secs)),
            "color_green_max_ms": eff(&self_s.raw.health.color_green_max_ms, src_of(&self_s.raw.health.color_green_max_ms, &def.health.color_green_max_ms)),
            "color_yellow_max_ms": eff(&self_s.raw.health.color_yellow_max_ms, src_of(&self_s.raw.health.color_yellow_max_ms, &def.health.color_yellow_max_ms)),
            "error_rate_healthy_pct": eff(&self_s.raw.health.error_rate_healthy_pct, src_of(&self_s.raw.health.error_rate_healthy_pct, &def.health.error_rate_healthy_pct)),
            "uptime_cache_secs": eff(&self_s.raw.health.uptime_cache_secs, src_of(&self_s.raw.health.uptime_cache_secs, &def.health.uptime_cache_secs)),
        },
        "system": {
            "cache_ttl_secs": eff(&self_s.raw.system.cache_ttl_secs, src_of(&self_s.raw.system.cache_ttl_secs, &def.system.cache_ttl_secs)),
            "route_stats_max": eff(&self_s.raw.system.route_stats_max, src_of(&self_s.raw.system.route_stats_max, &def.system.route_stats_max)),
            "api_body_max_kb": eff(&self_s.raw.system.api_body_max_kb, src_of(&self_s.raw.system.api_body_max_kb, &def.system.api_body_max_kb)),
        },
        "access": {
            "uac_wait_secs": eff(&self_s.raw.access.uac_wait_secs, src_of(&self_s.raw.access.uac_wait_secs, &def.access.uac_wait_secs)),
            "cache_ttl_secs": eff(&self_s.raw.access.cache_ttl_secs, src_of(&self_s.raw.access.cache_ttl_secs, &def.access.cache_ttl_secs)),
            "port_spec_max_len": eff(&self_s.raw.access.port_spec_max_len, src_of(&self_s.raw.access.port_spec_max_len, &def.access.port_spec_max_len)),
            "label_max_len": eff(&self_s.raw.access.label_max_len, src_of(&self_s.raw.access.label_max_len, &def.access.label_max_len)),
            "name_max_len": eff(&self_s.raw.access.name_max_len, src_of(&self_s.raw.access.name_max_len, &def.access.name_max_len)),
            "tailscale_cidr": eff(&self_s.raw.access.tailscale_cidr, src_of(&self_s.raw.access.tailscale_cidr, &def.access.tailscale_cidr)),
        },
        "services": {
            "default_start": eff(&self_s.raw.services.default_start, src_of(&self_s.raw.services.default_start, &def.services.default_start)),
            "ports": {
                "webcli": eff(&self_s.raw.services.ports.webcli, src_of(&self_s.raw.services.ports.webcli, &def.services.ports.webcli)),
                "syncthing": eff(&self_s.raw.services.ports.syncthing, src_of(&self_s.raw.services.ports.syncthing, &def.services.ports.syncthing)),
                "homepage": eff(&self_s.raw.services.ports.homepage, src_of(&self_s.raw.services.ports.homepage, &def.services.ports.homepage)),
            },
        },
        "ui": {
            "poll_ms": eff(&self_s.raw.ui.poll_ms, src_of(&self_s.raw.ui.poll_ms, &def.ui.poll_ms)),
            "tray_poll_ms": eff(&self_s.raw.ui.tray_poll_ms, src_of(&self_s.raw.ui.tray_poll_ms, &def.ui.tray_poll_ms)),
            "phone_poll_ms": eff(&self_s.raw.ui.phone_poll_ms, src_of(&self_s.raw.ui.phone_poll_ms, &def.ui.phone_poll_ms)),
            "embed_timeout_ms": eff(&self_s.raw.ui.embed_timeout_ms, src_of(&self_s.raw.ui.embed_timeout_ms, &def.ui.embed_timeout_ms)),
            "boot_poll_ms": eff(&self_s.raw.ui.boot_poll_ms, src_of(&self_s.raw.ui.boot_poll_ms, &def.ui.boot_poll_ms)),
            "boot_timeout_ms": eff(&self_s.raw.ui.boot_timeout_ms, src_of(&self_s.raw.ui.boot_timeout_ms, &def.ui.boot_timeout_ms)),
            "embed_probe_ms": eff(&self_s.raw.ui.embed_probe_ms, src_of(&self_s.raw.ui.embed_probe_ms, &def.ui.embed_probe_ms)),
            "fetch_retries": eff(&self_s.raw.ui.fetch_retries, src_of(&self_s.raw.ui.fetch_retries, &def.ui.fetch_retries)),
            "cpu_warn_pct": eff(&self_s.raw.ui.cpu_warn_pct, src_of(&self_s.raw.ui.cpu_warn_pct, &def.ui.cpu_warn_pct)),
            "mem_warn_pct": eff(&self_s.raw.ui.mem_warn_pct, src_of(&self_s.raw.ui.mem_warn_pct, &def.ui.mem_warn_pct)),
            "temp_err_c": eff(&self_s.raw.ui.temp_err_c, src_of(&self_s.raw.ui.temp_err_c, &def.ui.temp_err_c)),
            "temp_warn_c": eff(&self_s.raw.ui.temp_warn_c, src_of(&self_s.raw.ui.temp_warn_c, &def.ui.temp_warn_c)),
            "smart_temp_err_c": eff(&self_s.raw.ui.smart_temp_err_c, src_of(&self_s.raw.ui.smart_temp_err_c, &def.ui.smart_temp_err_c)),
            "smart_wear_err_pct": eff(&self_s.raw.ui.smart_wear_err_pct, src_of(&self_s.raw.ui.smart_wear_err_pct, &def.ui.smart_wear_err_pct)),
            "ws_reconnect_ms": eff(&self_s.raw.ui.ws_reconnect_ms, src_of(&self_s.raw.ui.ws_reconnect_ms, &def.ui.ws_reconnect_ms)),
        },
        "launcher": {
            "wait_secs": eff(&self_s.raw.launcher.wait_secs, src_of(&self_s.raw.launcher.wait_secs, &def.launcher.wait_secs)),
            "poll_interval_secs": eff(&self_s.raw.launcher.poll_interval_secs, src_of(&self_s.raw.launcher.poll_interval_secs, &def.launcher.poll_interval_secs)),
            "connect_timeout_ms": eff(&self_s.raw.launcher.connect_timeout_ms, src_of(&self_s.raw.launcher.connect_timeout_ms, &def.launcher.connect_timeout_ms)),
        },
        "flags": {
            "feature": {
                "auto_retry": eff(&self_s.raw.flags.feature.auto_retry, src_of(&self_s.raw.flags.feature.auto_retry, &def.flags.feature.auto_retry)),
                "usb_eject": eff(&self_s.raw.flags.feature.usb_eject, src_of(&self_s.raw.flags.feature.usb_eject, &def.flags.feature.usb_eject)),
            }
        },
        "tray": {
            "balloon_ms": eff(&self_s.raw.tray.balloon_ms, src_of(&self_s.raw.tray.balloon_ms, &def.tray.balloon_ms)),
        },
        "control": {
            "enabled": eff(self_s.control_enabled(), ctl_src_bool("FENGPAGE_CONTROL_ENABLED", self_s.raw.control.enabled, def.control.enabled)),
            "base_url": eff(self_s.control_base_url(), ctl_src_str("FENGPAGE_CONTROL_BASE_URL", &self_s.raw.control.base_url, &def.control.base_url)),
            "device_name": eff(self_s.control_device_name(), ctl_src_str("FENGPAGE_CONTROL_DEVICE_NAME", &self_s.raw.control.device_name, &def.control.device_name)),
            "heartbeat_secs": eff(self_s.control_heartbeat_secs(), ctl_src_num("FENGPAGE_CONTROL_HEARTBEAT_SECS", self_s.raw.control.heartbeat_secs, def.control.heartbeat_secs)),
            "upload_max_mb": eff(self_s.control_upload_max_mb(), ctl_src_num("FENGPAGE_CONTROL_UPLOAD_MAX_MB", self_s.raw.control.upload_max_mb, def.control.upload_max_mb)),
        },
        // [auth] surfaces only the enabled flag; auth.password is a secret
        // and never enters any API response (GET /api/auth/status carries
        // the live value instead). PUT /api/config rejects the section.
        "auth": {
            "enabled": eff(self_s.auth_enabled(), src_of(&self_s.raw.auth.enabled, &def.auth.enabled)),
        },
        // [openlist] surfaces everything except the admin password (masked
        // exactly like [auth]: presence of a password is not even disclosed).
        "openlist": {
            "enabled": eff(self_s.openlist_enabled(), ol_src_bool("FENGPAGE_OPENLIST_ENABLED", self_s.raw.openlist.enabled, def.openlist.enabled)),
            "base_url": eff(self_s.openlist_base_url(), ol_src_str("FENGPAGE_OPENLIST_BASE_URL", &self_s.raw.openlist.base_url, &def.openlist.base_url)),
            "username": eff(self_s.openlist_username(), ol_src_str("FENGPAGE_OPENLIST_USERNAME", &self_s.raw.openlist.username, &def.openlist.username)),
            "password": eff("", ol_password_src(self_s)),
        },
        // [smb] carries no secrets and serializes in full (entries + grant).
        // Reads the live overlay so a runtime edit matches what the panel
        // actually manages right now.
        "smb": {
            "enabled": eff(self_s.smb_enabled(), src_of(&self_s.raw.smb.enabled, &def.smb.enabled)),
            "guest_access": eff(&live_smb.guest_access, src_of(&live_smb.guest_access, &def.smb.guest_access)),
            "default_grant": eff(&live_smb.default_grant, src_of(&live_smb.default_grant, &def.smb.default_grant)),
            "shares": live_smb.shares.iter().map(|s| {
                json!({
                    "name": s.name,
                    "path": s.path,
                    "grant": s.grant,
                })
            }).collect::<Vec<_>>(),
        },
    })
}

/// Source tag for a [control] bool: "env" when the env var is set, else
/// file/default (mirrors src_of, but env presence beats the file value).
fn ctl_src_bool(env_key: &str, file: bool, def: bool) -> &'static str {
    if env_ok(env_key).is_some() { "env" } else { src_of(&file, &def) }
}

fn ctl_src_str(env_key: &str, file: &str, def: &str) -> &'static str {
    if env_ok(env_key).is_some() { "env" } else { src_of(file, def) }
}

/// Source tags for [openlist] (same env-beats-file rule as [control]).
fn ol_src_bool(env_key: &str, file: bool, def: bool) -> &'static str {
    if env_ok(env_key).is_some() { "env" } else { src_of(&file, &def) }
}

fn ol_src_str(env_key: &str, file: &str, def: &str) -> &'static str {
    if env_ok(env_key).is_some() { "env" } else { src_of(file, def) }
}

/// Password presence source without disclosing anything: env when the env
/// var is set, else file when a file password exists, else default.
fn ol_password_src(self_s: &Settings) -> &'static str {
    if env_ok("FENGPAGE_OPENLIST_PASSWORD").is_some() {
        "env"
    } else if self_s.raw.openlist.password.is_empty() {
        "default"
    } else {
        "file"
    }
}

fn ctl_src_num<T: PartialEq>(env_key: &str, file: T, def: T) -> &'static str {
    if env_ok(env_key).is_some() { "env" } else { src_of(&file, &def) }
}

/// "file" when the file carries a value different from the default, else
/// "default" (value equal either way).
fn src_of<T: PartialEq + ?Sized>(file: &T, def: &T) -> &'static str {
    if file != def { "file" } else { "default" }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests below mutate process-global FENGPAGE_CONTROL_* env vars;
    /// cargo runs tests on parallel threads, so they must be serialized.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const FULL_SAMPLE: &str = r#"
# Full-schema example used by the unit tests; values must equal the
# built-in defaults so the sample stays behavior-neutral.
[server]
addr = "0.0.0.0:8388"
bind_retries = 10
bind_retry_ms = 350

[webcli]
host = "127.0.0.1"
port = 8788
shell = "cmd.exe"

[term]
session_ttl_hours = 12
password_min_len = 4

[sync]
interval_secs = 600
syncthing_addr = "http://127.0.0.1:8384"
timeout_secs = 3
sync_timeout_secs = 2
onboarding_timeout_secs = 5

[daemon]
usb_poll_secs = 15
probe_timeout_secs = 10
vdisp_cache_secs = 60
instance_retries = 15
instance_retry_ms = 200
eject_grace_secs = 2

[wsl]
distro = "Ubuntu-24.04"
keepalive_poll_secs = 30
cmd_timeout_secs = 30
ps_timeout_secs = 10
vm_wait_secs = 15
vm_poll_step_secs = 2

[docker]
status_timeout_secs = 15
action_timeout_secs = 120

[health]
probe_timeout_secs = 3
color_green_max_ms = 200
color_yellow_max_ms = 1000
error_rate_healthy_pct = 5
uptime_cache_secs = 60

[system]
cache_ttl_secs = 12
route_stats_max = 512
api_body_max_kb = 8

[access]
uac_wait_secs = 12
cache_ttl_secs = 12
port_spec_max_len = 64
label_max_len = 24
name_max_len = 64
tailscale_cidr = "100.64.0.0/10"

[services]
default_start = ["webcli"]
[services.ports]
webcli = 8788
syncthing = 8384
homepage = 9199

[ui]
poll_ms = 15000
tray_poll_ms = 5000
phone_poll_ms = 5000
embed_timeout_ms = 8000
boot_poll_ms = 2500
boot_timeout_ms = 90000
embed_probe_ms = 5000
fetch_retries = 3
cpu_warn_pct = 85
mem_warn_pct = 90
temp_err_c = 70
temp_warn_c = 60
smart_temp_err_c = 55
smart_wear_err_pct = 60
ws_reconnect_ms = [1000, 10000]

[launcher]
wait_secs = 120
poll_interval_secs = 1
connect_timeout_ms = 500

[flags]
feature.auto_retry = true
feature.usb_eject = true

[tray]
balloon_ms = 6000

[control]
enabled = false
base_url = "http://127.0.0.1:9001"
device_name = "flygo-panel"
heartbeat_secs = 300
upload_max_mb = 500

[auth]
enabled = false
password = ""

[openlist]
enabled = false
base_url = "http://127.0.0.1:5244"
username = ""
password = ""

[smb]
enabled = true
default_grant = "Everyone,READ"
guest_access = true
"#;

    #[test]
    fn defaults_match_current_hardcoded_values() {
        let d = FileConfig::default();
        assert_eq!(d.server.addr, DEFAULT_SERVER_ADDR);
        assert_eq!(d.server.bind_retries, 10);
        assert_eq!(d.server.bind_retry_ms, 350);
        assert_eq!(d.webcli.host, "127.0.0.1");
        assert_eq!(d.webcli.port, 8788);
        assert_eq!(d.webcli.shell, "cmd.exe");
        assert_eq!(d.term.session_ttl_hours, 12);
        assert_eq!(d.term.password_min_len, 4);
        assert!(d.term.webcli_addr.is_none());
        assert_eq!(d.sync.interval_secs, 600);
        assert_eq!(d.sync.syncthing_addr, "http://127.0.0.1:8384");
        assert_eq!(d.sync.timeout_secs, 3);
        assert_eq!(d.sync.sync_timeout_secs, 2);
        assert_eq!(d.sync.onboarding_timeout_secs, 5);
        assert_eq!(d.daemon.usb_poll_secs, 15);
        assert_eq!(d.wsl.distro, "Ubuntu-24.04");
        assert_eq!(d.docker.action_timeout_secs, 120);
        assert_eq!(d.health.color_green_max_ms, 200);
        assert_eq!(d.system.cache_ttl_secs, 12);
        assert_eq!(d.system.route_stats_max, 512);
        assert_eq!(d.access.tailscale_cidr, "100.64.0.0/10");
        assert_eq!(d.services.default_start, vec!["webcli"]);
        assert_eq!(d.services.ports.webcli, 8788);
        assert_eq!(d.services.ports.syncthing, 8384);
        assert_eq!(d.services.ports.homepage, 9199);
        assert_eq!(d.ui.poll_ms, 15000);
        assert_eq!(d.ui.tray_poll_ms, 5000);
        assert_eq!(d.ui.ws_reconnect_ms, vec![1000, 10000]);
        assert_eq!(d.launcher.wait_secs, 120);
        assert_eq!(d.launcher.connect_timeout_ms, 500);
        assert!(d.flags.feature.auto_retry);
        assert!(d.flags.feature.usb_eject);
        assert_eq!(d.tray.balloon_ms, 6000);
        assert!(!d.control.enabled);
        assert_eq!(d.control.base_url, "http://127.0.0.1:9001");
        assert_eq!(d.control.device_name, "flygo-panel");
        assert_eq!(d.control.heartbeat_secs, 300);
        assert_eq!(d.control.upload_max_mb, 500);
        assert!(!d.auth.enabled, "login layer must default to OFF");
        assert_eq!(d.auth.password, "");
        assert!(d.smb.enabled, "SMB lifecycle must default to ON");
        assert!(d.smb.shares.is_empty(), "default SMB shares must be empty (hotplug shares present drives at boot)");
        assert_eq!(d.smb.default_grant, "Everyone,READ");
        assert!(d.smb.guest_access, "anonymous SMB access must default ON");
    }

    #[test]
    fn full_schema_parses() {
        let cfg: FileConfig = toml::from_str(FULL_SAMPLE).expect("full sample parses");
        assert_eq!(cfg, FileConfig::default());
        assert!(validate(&cfg).is_empty(), "sample must validate clean: {:?}", validate(&cfg));
    }

    #[test]
    fn partial_file_overrides_only_present_keys() {
        let cfg: FileConfig =
            toml::from_str("[server]\naddr = \"127.0.0.1:9000\"\n[sync]\ninterval_secs = 120\n")
                .expect("partial file parses");
        assert_eq!(cfg.server.addr, "127.0.0.1:9000");
        assert_eq!(cfg.server.bind_retries, 10, "absent key falls back to default");
        assert_eq!(cfg.server.bind_retry_ms, 350);
        assert_eq!(cfg.sync.interval_secs, 120);
        assert_eq!(cfg.sync.syncthing_addr, "http://127.0.0.1:8384");
        assert_eq!(cfg.webcli.port, 8788, "absent section falls back to default");
        assert_eq!(cfg.ui.poll_ms, 15000);
    }

    #[test]
    fn type_error_is_reported_not_fatal() {
        let text = "[server]\naddr = 123\n[system]\ncache_ttl_secs = \"x\"\n";
        let res = toml::from_str::<FileConfig>(text);
        assert!(res.is_err());
        let err = res.unwrap_err();
        let key = key_from_span(text, err.span()).or_else(|| key_from_message(err.message()));
        assert_eq!(key.as_deref(), Some("server.addr"));
        let (line, col) = span_line_col(text, err.span());
        assert_eq!((line, col), (2, 8));
    }

    #[test]
    fn env_beats_file_beats_default_for_addr() {
        assert_eq!(
            server_addr_pick(Some("127.0.0.1:9999"), true, "0.0.0.0:8388"),
            ("127.0.0.1:9999".to_string(), Source::Env)
        );
        assert_eq!(
            server_addr_pick(None, true, "127.0.0.1:9000"),
            ("127.0.0.1:9000".to_string(), Source::File)
        );
        // File present but value equals the default: still a "file" source.
        assert_eq!(
            server_addr_pick(None, true, "0.0.0.0:8388"),
            ("0.0.0.0:8388".to_string(), Source::File)
        );
        // No file at all: always "default", even when the raw value is the
        // default string (raw is synthesized from defaults).
        assert_eq!(
            server_addr_pick(None, false, "0.0.0.0:8388"),
            (DEFAULT_SERVER_ADDR.to_string(), Source::Default)
        );
        // Invalid env falls through to the file; invalid file falls to default.
        assert_eq!(
            server_addr_pick(Some("not-an-addr"), true, "127.0.0.1:9000"),
            ("127.0.0.1:9000".to_string(), Source::File)
        );
        assert_eq!(
            server_addr_pick(Some("not-an-addr"), true, "also-broken"),
            (DEFAULT_SERVER_ADDR.to_string(), Source::Default)
        );
    }

    #[test]
    fn ttl_must_stay_below_poll_interval() {
        let mut cfg = FileConfig::default();
        cfg.system.cache_ttl_secs = 15;
        cfg.ui.poll_ms = 15000;
        let errs = validate(&cfg);
        assert!(errs.iter().any(|e| e.field == "system.cache_ttl_secs"));
        cfg.system.cache_ttl_secs = 12;
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn effective_json_annotates_sources() {
        let cfg: FileConfig = toml::from_str(
            "[server]\naddr = \"127.0.0.1:9000\"\n[system]\ncache_ttl_secs = 5\n[sync]\ninterval_secs = 300\n",
        )
        .expect("parses");
        let s = Settings { path: PathBuf::from("x.toml"), exists: true, raw: cfg, errors: vec![] };
        let j = effective_json(&s);
        assert_eq!(j["server"]["addr"]["source"], "file");
        assert_eq!(j["server"]["addr"]["value"], "127.0.0.1:9000");
        assert_eq!(j["server"]["bind_retry_ms"]["source"], "default");
        assert_eq!(j["system"]["cache_ttl_secs"]["value"], 5);
        assert_eq!(j["system"]["cache_ttl_secs"]["source"], "file");
        assert_eq!(j["sync"]["interval_secs"]["value"], 300);
        // term.webcli_addr derives from [webcli] host:port when not set.
        assert_eq!(j["term"]["webcli_addr"]["value"], "127.0.0.1:8788");
        assert_eq!(j["term"]["webcli_addr"]["source"], "default");
        assert_eq!(j["flags"]["feature"]["auto_retry"]["value"], true);
    }

    #[test]
    fn effective_json_env_wins_for_addr() {
        let cfg = FileConfig::default();
        let s = Settings { path: PathBuf::from("x.toml"), exists: false, raw: cfg, errors: vec![] };
        let (addr, src) = s.server_addr();
        assert!(addr.parse::<SocketAddr>().is_ok());
        match src {
            Source::Env => assert_eq!(addr, std::env::var("FENGPAGE_ADDR").unwrap()),
            _ => assert_eq!(addr, DEFAULT_SERVER_ADDR),
        }
    }

    #[test]
    fn control_section_parses_and_validates() {
        // Bad base_url / device_name / heartbeat / upload cap are rejected
        // with per-field errors.
        let bad: FileConfig = toml::from_str(
            r#"[control]
base_url = "ftp://x"
device_name = "  "
heartbeat_secs = 5
upload_max_mb = 0
"#,
        )
        .expect("parses");
        let errs = validate(&bad);
        assert!(errs.iter().any(|e| e.field == "control.base_url"));
        assert!(errs.iter().any(|e| e.field == "control.device_name"));
        assert!(errs.iter().any(|e| e.field == "control.heartbeat_secs"));
        assert!(errs.iter().any(|e| e.field == "control.upload_max_mb"));

        // Heartbeat must stay below the 15-min offline window.
        let far: FileConfig = toml::from_str("[control]\nheartbeat_secs = 901\n").expect("parses");
        assert!(validate(&far).iter().any(|e| e.field == "control.heartbeat_secs"));
        let ok: FileConfig = toml::from_str("[control]\nheartbeat_secs = 300\n").expect("parses");
        assert!(validate(&ok).is_empty());
    }

    #[test]
    fn control_env_overrides_file_and_default() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FENGPAGE_CONTROL_ENABLED", "true");
        std::env::set_var("FENGPAGE_CONTROL_BASE_URL", "http://192.168.1.52:9001");
        let cfg: FileConfig = toml::from_str("[control]\nenabled = false\n").expect("parses");
        let s = Settings { path: PathBuf::from("x.toml"), exists: true, raw: cfg, errors: vec![] };
        assert!(s.control_enabled(), "env beats file");
        assert_eq!(s.control_base_url(), "http://192.168.1.52:9001");
        assert_eq!(s.control_device_name(), "flygo-panel", "no env -> file/default");
        let j = effective_json(&s);
        assert_eq!(j["control"]["enabled"]["source"], "env");
        assert_eq!(j["control"]["base_url"]["source"], "env");
        assert_eq!(j["control"]["device_name"]["source"], "default");
        std::env::remove_var("FENGPAGE_CONTROL_ENABLED");
        std::env::remove_var("FENGPAGE_CONTROL_BASE_URL");

        // Invalid numeric env falls back to the file value.
        std::env::set_var("FENGPAGE_CONTROL_HEARTBEAT_SECS", "not-a-number");
        let s2 = Settings { path: PathBuf::from("x.toml"), exists: false, raw: FileConfig::default(), errors: vec![] };
        assert_eq!(s2.control_heartbeat_secs(), 300);
        std::env::remove_var("FENGPAGE_CONTROL_HEARTBEAT_SECS");
    }

    #[test]
    fn control_base_url_file_override_flows_to_accessor() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // T-06 (D5): no env -> file value must reach the accessor (VPS cutover path).
        std::env::remove_var("FENGPAGE_CONTROL_BASE_URL");
        let cfg: FileConfig = toml::from_str(
            "[control]\nenabled = true\nbase_url = \"https://cp.example.com:9001\"\n",
        )
        .expect("parses");
        let s = Settings { path: PathBuf::from("x.toml"), exists: true, raw: cfg, errors: vec![] };
        assert_eq!(s.control_base_url(), "https://cp.example.com:9001");
        let j = effective_json(&s);
        assert_eq!(j["control"]["base_url"]["source"], "file");
        // Section absent -> built-in default (local MVP).
        let d = Settings { path: PathBuf::from("x.toml"), exists: true, raw: FileConfig::default(), errors: vec![] };
        assert_eq!(d.control_base_url(), DEFAULT_CONTROL_BASE_URL);
    }
}
