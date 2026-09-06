// /api/config/components - editable component config (phase 1).
//
// The panel UI is driven by an overlay config file `<root>/data/components.json`
// (path resolved in config.rs: FENGPAGE_COMPONENTS env > install root). Without
// a file the built-in default template is served and the UI is pixel-identical
// to the previous hardcoded version. The user config only describes diffs:
// same-id entries override fields, new ids append, hidden:true hides,
// removed:true deletes a built-in item. Compose-discovered services still
// appear on the card wall automatically (docker.rs stays dynamic).
//
// API:
//   GET  /api/config/components        -> {ok, config(merged), merged}
//   GET  /api/config/components/default-> {ok, config(built-in default template)}
//   PUT  /api/config/components        -> body = user config JSON; atomic write
//                                         + .bak backup; returns {ok, errors}
//   POST /api/config/components/reset  -> delete the file, back to defaults
//
// Write safety: write to components.json.tmp then rename over the target;
// the previous file is kept as components.json.bak. A (mtime, content) cache
// is used for reads and is invalidated after every write/reset.
//
// All code comments are pure ASCII.

use crate::{ApiError, ApiResult};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Model: every field is optional so the user file can describe only diffs.
// Missing fields on output are omitted (skip_serializing_if) and the frontend
// falls back to sensible defaults. `type` is a Rust keyword, so the tiles
// field is named type_ and renamed in JSON.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct NavItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// page:<view> | iframe:<frameId> | url:<http(s)://...>
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TileItem {
    pub id: String,
    /// action | service-light
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// copy-url | theme | restart
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// service-light only: services.rs key (ssh/tailscale/syncthing, ...)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct CardItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// May contain the ${FENG_IP} template; http(s) only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// 0 = no TCP probe (gray card).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// compose container_name (internal bookkeeping; user may override).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct FrameItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// May contain the ${FENG_IP} template; http(s) only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// compose container key used for one-click start / failure diagnosis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct UserConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Footer / site title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional theme override (reserved; not wired in phase 1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    pub nav: Vec<NavItem>,
    pub tiles: Vec<TileItem>,
    pub cards: Vec<CardItem>,
    pub frames: Vec<FrameItem>,
}

// ---------------------------------------------------------------------------
// Built-in default template == the previously hardcoded UI (nav 13, tiles 6,
// compose-driven card wall, iframe 3). Never written to disk.
// ---------------------------------------------------------------------------

pub fn default_config() -> UserConfig {
    UserConfig {
        version: Some(1),
        title: Some("FlyGo · Tailscale 内网".to_string()),
        theme: None,
        nav: vec![
            NavItem { id: "overview".into(), label: Some("总览".into()), icon: Some("🏠".into()), target: Some("page:overview".into()), sort: Some(1), hidden: None, removed: None },
            NavItem { id: "files".into(), label: Some("文件".into()), icon: Some("📂".into()), target: Some("page:files".into()), sort: Some(2), hidden: None, removed: None },
            NavItem { id: "media".into(), label: Some("影视".into()), icon: Some("📺".into()), target: Some("iframe:lf-media".into()), sort: Some(3), hidden: None, removed: None },
            NavItem { id: "photos".into(), label: Some("相册".into()), icon: Some("📷".into()), target: Some("iframe:lf-photos".into()), sort: Some(4), hidden: None, removed: None },
            NavItem { id: "terminal".into(), label: Some("终端".into()), icon: Some("💻".into()), target: Some("page:terminal".into()), sort: Some(5), hidden: None, removed: None },
            NavItem { id: "docker".into(), label: Some("容器".into()), icon: Some("🐳".into()), target: Some("page:docker".into()), sort: Some(6), hidden: None, removed: None },
            NavItem { id: "system".into(), label: Some("系统".into()), icon: Some("🗄".into()), target: Some("page:system".into()), sort: Some(7), hidden: None, removed: None },
            NavItem { id: "autostart".into(), label: Some("自启动".into()), icon: Some("🔛".into()), target: Some("page:autostart".into()), sort: Some(8), hidden: None, removed: None },
            NavItem { id: "access".into(), label: Some("访问控制".into()), icon: Some("🛡".into()), target: Some("page:access".into()), sort: Some(9), hidden: None, removed: None },
            NavItem { id: "sync".into(), label: Some("同步".into()), icon: Some("🔄".into()), target: Some("page:sync".into()), sort: Some(10), hidden: None, removed: None },
            NavItem { id: "syncnet".into(), label: Some("同步网络".into()), icon: Some("🌐".into()), target: Some("page:syncnet".into()), sort: Some(11), hidden: None, removed: None },
            NavItem { id: "phone".into(), label: Some("手机接入".into()), icon: Some("📱".into()), target: Some("page:phone".into()), sort: Some(12), hidden: None, removed: None },
            NavItem { id: "native".into(), label: Some("本地服务".into()), icon: Some("🖥️".into()), target: Some("page:native".into()), sort: Some(13), hidden: None, removed: None },
        ],
        tiles: vec![
            TileItem { id: "t-copy".into(), type_: Some("action".into()), label: Some("复制入口".into()), icon: Some("📋".into()), action: Some("copy-url".into()), service: None, hidden: None, removed: None },
            TileItem { id: "t-theme".into(), type_: Some("action".into()), label: Some("白天".into()), icon: Some("🌤".into()), action: Some("theme".into()), service: None, hidden: None, removed: None },
            TileItem { id: "t-restart".into(), type_: Some("action".into()), label: Some("重启".into()), icon: Some("🔁".into()), action: Some("restart".into()), service: None, hidden: None, removed: None },
            TileItem { id: "t-sync".into(), type_: Some("service-light".into()), label: Some("Syncthing".into()), icon: None, action: None, service: Some("syncthing".into()), hidden: None, removed: None },
            TileItem { id: "t-ts".into(), type_: Some("service-light".into()), label: Some("Tailscale".into()), icon: None, action: None, service: Some("tailscale".into()), hidden: None, removed: None },
            TileItem { id: "t-ssh".into(), type_: Some("service-light".into()), label: Some("SSH".into()), icon: None, action: None, service: Some("ssh".into()), hidden: None, removed: None },
        ],
        cards: Vec::new(), // compose-driven; merged at runtime (see merged_cards)
        frames: vec![
            FrameItem { id: "lf-files".into(), title: Some("文件".into()), url: Some("http://${FENG_IP}:5244".into()), container: Some("alist".into()), boot_hint: Some("首次约需 10~30 秒".into()), fullscreen: None, hidden: None, removed: None },
            FrameItem { id: "lf-media".into(), title: Some("影视".into()), url: Some("http://${FENG_IP}:8096".into()), container: Some("jellyfin".into()), boot_hint: None, fullscreen: None, hidden: None, removed: None },
            FrameItem { id: "lf-photos".into(), title: Some("相册".into()), url: Some("http://${FENG_IP}:2283".into()), container: Some("immich-server".into()), boot_hint: None, fullscreen: None, hidden: None, removed: None },
        ],
    }
}

/// Default card base: the compose-derived service list mapped to card items.
/// Kept here so GET (editor source) and /api/health|/api/docker see the same
/// merged list. Compose stays the single dynamic source of service discovery.
fn default_cards() -> Vec<CardItem> {
    crate::api::docker::compose_list()
        .into_iter()
        .map(|s| CardItem {
            id: s.key.clone(),
            name: Some(s.name.clone()),
            icon: Some(s.icon.clone()),
            desc: Some(s.desc.clone()),
            url: if s.port > 0 { Some(format!("http://${{FENG_IP}}:{}", s.port)) } else { None },
            port: Some(s.port),
            ctn: Some(s.ctn.clone()),
            group: Some("services".to_string()),
            sort: None,
            hidden: None,
            removed: None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Overlay merge (by id): override fields / append new / hidden / removed.
// ---------------------------------------------------------------------------

trait Item: Clone {
    fn id(&self) -> &str;
    fn removed(&self) -> bool;
    fn merge_from(&mut self, u: &Self);
}

impl Item for NavItem {
    fn id(&self) -> &str { &self.id }
    fn removed(&self) -> bool { self.removed == Some(true) }
    fn merge_from(&mut self, u: &Self) {
        if u.label.is_some() { self.label = u.label.clone(); }
        if u.icon.is_some() { self.icon = u.icon.clone(); }
        if u.target.is_some() { self.target = u.target.clone(); }
        if u.sort.is_some() { self.sort = u.sort.clone(); }
        if u.hidden.is_some() { self.hidden = u.hidden.clone(); }
    }
}

impl Item for TileItem {
    fn id(&self) -> &str { &self.id }
    fn removed(&self) -> bool { self.removed == Some(true) }
    fn merge_from(&mut self, u: &Self) {
        if u.type_.is_some() { self.type_ = u.type_.clone(); }
        if u.label.is_some() { self.label = u.label.clone(); }
        if u.icon.is_some() { self.icon = u.icon.clone(); }
        if u.action.is_some() { self.action = u.action.clone(); }
        if u.service.is_some() { self.service = u.service.clone(); }
        if u.hidden.is_some() { self.hidden = u.hidden.clone(); }
    }
}

impl Item for CardItem {
    fn id(&self) -> &str { &self.id }
    fn removed(&self) -> bool { self.removed == Some(true) }
    fn merge_from(&mut self, u: &Self) {
        if u.name.is_some() { self.name = u.name.clone(); }
        if u.icon.is_some() { self.icon = u.icon.clone(); }
        if u.desc.is_some() { self.desc = u.desc.clone(); }
        if u.url.is_some() { self.url = u.url.clone(); }
        if u.port.is_some() { self.port = u.port.clone(); }
        if u.ctn.is_some() { self.ctn = u.ctn.clone(); }
        if u.group.is_some() { self.group = u.group.clone(); }
        if u.sort.is_some() { self.sort = u.sort.clone(); }
        if u.hidden.is_some() { self.hidden = u.hidden.clone(); }
    }
}

impl Item for FrameItem {
    fn id(&self) -> &str { &self.id }
    fn removed(&self) -> bool { self.removed == Some(true) }
    fn merge_from(&mut self, u: &Self) {
        if u.title.is_some() { self.title = u.title.clone(); }
        if u.url.is_some() { self.url = u.url.clone(); }
        if u.container.is_some() { self.container = u.container.clone(); }
        if u.boot_hint.is_some() { self.boot_hint = u.boot_hint.clone(); }
        if u.fullscreen.is_some() { self.fullscreen = u.fullscreen.clone(); }
        if u.hidden.is_some() { self.hidden = u.hidden.clone(); }
    }
}

/// base overlaid by user: same id -> merge; removed -> dropped; new id -> appended.
fn overlay_items<T: Item>(base: Vec<T>, user: Vec<T>) -> Vec<T> {
    let mut out: Vec<T> = Vec::with_capacity(base.len());
    for mut b in base {
        match user.iter().find(|u| u.id() == b.id()) {
            Some(u) if u.removed() => continue,
            Some(u) => { b.merge_from(u); out.push(b); }
            None => out.push(b),
        }
    }
    for u in user {
        if u.removed() || out.iter().any(|o| o.id() == u.id()) { continue; }
        out.push(u);
    }
    out
}

/// Full merged config: defaults overlaid by the user file, cards replaced by
/// the compose-merged card wall so the editor sees exactly what the UI shows.
fn overlay_defaults(u: &UserConfig) -> UserConfig {
    let mut m = default_config();
    let mut nav = overlay_items(m.nav, u.nav.clone());
    nav.sort_by_key(|n| n.sort.unwrap_or(0));
    m.nav = nav;
    m.tiles = overlay_items(m.tiles, u.tiles.clone());
    m.cards = overlay_cards_from(u);
    m.frames = overlay_items(m.frames, u.frames.clone());
    if let Some(t) = &u.title { m.title = Some(t.clone()); }
    if let Some(v) = u.version { m.version = Some(v); }
    if let Some(t) = &u.theme { m.theme = Some(t.clone()); }
    m
}

/// Merged card wall (compose base + user overlay + user-only additions).
pub fn merged_cards() -> Vec<CardItem> {
    match load_user_config() {
        CfgLoad::Valid(u) => overlay_cards_from(&u),
        _ => default_cards(),
    }
}

fn overlay_cards_from(u: &UserConfig) -> Vec<CardItem> {
    let mut out = overlay_items(default_cards(), u.cards.clone());
    out.sort_by_key(|c| c.sort.unwrap_or(0));
    out
}

/// Service-light keys + labels from the merged tiles, or None when there is
/// no user config (services.rs keeps its built-in three then). This is how
/// the config adds/removes entries in the /api/services probe list.
pub fn user_service_lights() -> Option<Vec<(String, String)>> {
    match load_user_config() {
        CfgLoad::Valid(u) => {
            let mut m = default_config();
            m.tiles = overlay_items(m.tiles, u.tiles.clone());
            Some(
                m.tiles
                    .iter()
                    .filter(|t| t.type_.as_deref() == Some("service-light") && t.hidden != Some(true))
                    .filter_map(|t| {
                        t.service
                            .clone()
                            .map(|k| (k.clone(), t.label.clone().unwrap_or_else(|| k)))
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// File I/O: (mtime, load) cache, atomic tmp+rename write, .bak backup.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum CfgLoad {
    Missing,
    Valid(UserConfig),
    Invalid(String),
}

static CACHE: Mutex<Option<(SystemTime, CfgLoad)>> = Mutex::new(None);

fn invalidate_cache() {
    if let Ok(mut g) = CACHE.lock() {
        *g = None;
    }
}

fn config_path() -> std::path::PathBuf {
    crate::config::get().components_path.clone()
}

/// Read + parse + validate the user config file. Cached by mtime so repeated
/// GET / /api/docker / /api/health calls stay cheap; PUT/reset invalidate.
fn load_user_config() -> CfgLoad {
    let path = config_path();
    let meta = std::fs::metadata(&path);
    let mtime = meta.as_ref().ok().and_then(|m| m.modified().ok());
    {
        let g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, ref load)) = *g {
            if Some(t) == mtime {
                return load.clone();
            }
        }
    }
    let load = match meta {
        Err(_) => CfgLoad::Missing, // no file (or gone) -> defaults
        Ok(_) => match std::fs::read_to_string(&path) {
            Err(e) => CfgLoad::Invalid(format!("cannot read {}: {e}", path.display())),
            Ok(s) => match serde_json::from_str::<UserConfig>(&s) {
                Err(e) => CfgLoad::Invalid(format!("config parse error: {e}")),
                Ok(cfg) => {
                    let errs = validate(&cfg);
                    if errs.is_empty() {
                        CfgLoad::Valid(cfg)
                    } else {
                        let detail = errs
                            .iter()
                            .map(|e| format!("{}: {}", e.path, e.message))
                            .collect::<Vec<_>>()
                            .join("; ");
                        CfgLoad::Invalid(format!("config validation failed: {detail}"))
                    }
                }
            },
        },
    };
    if let Ok(mut g) = CACHE.lock() {
        *g = mtime.map(|t| (t, load.clone()));
    }
    load
}

// ---------------------------------------------------------------------------
// Validation: per-field errors so the editor can point at the exact row.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct FieldErr {
    pub path: String,
    pub message: String,
}

fn check_id(errs: &mut Vec<FieldErr>, path: &str, id: &str) {
    if id.is_empty() {
        errs.push(FieldErr { path: path.to_string(), message: "id is required".into() });
        return;
    }
    if id.len() > 64
        || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        errs.push(FieldErr {
            path: path.to_string(),
            message: "id must use [A-Za-z0-9._-] and be at most 64 chars".into(),
        });
    }
}

fn check_url(errs: &mut Vec<FieldErr>, path: &str, url: Option<&str>) {
    if let Some(u) = url {
        let dirty = u.chars().any(|c| c.is_control() || c == '\'' || c == '"' || c == '<' || c == '>');
        if (!u.starts_with("http://") && !u.starts_with("https://")) || dirty {
            errs.push(FieldErr {
                path: path.to_string(),
                message: "url must be http:// or https:// without quotes or control chars".into(),
            });
        }
    }
}

fn dup_check(errs: &mut Vec<FieldErr>, coll: &str, ids: &[String]) {
    let mut seen: HashSet<&str> = HashSet::new();
    for (i, id) in ids.iter().enumerate() {
        if !seen.insert(id.as_str()) {
            errs.push(FieldErr { path: format!("{coll}[{i}].id"), message: "duplicate id".into() });
        }
    }
}

fn validate(cfg: &UserConfig) -> Vec<FieldErr> {
    let mut errs = Vec::new();
    let mut frame_ids: HashSet<String> = HashSet::new();

    for (i, f) in cfg.frames.iter().enumerate() {
        let p = format!("frames[{i}]");
        check_id(&mut errs, &format!("{p}.id"), &f.id);
        if f.removed == Some(true) { continue; }
        check_url(&mut errs, &format!("{p}.url"), f.url.as_deref());
        if let Some(c) = &f.container { check_id(&mut errs, &format!("{p}.container"), c); }
        frame_ids.insert(f.id.clone());
    }

    for (i, n) in cfg.nav.iter().enumerate() {
        let p = format!("nav[{i}]");
        check_id(&mut errs, &format!("{p}.id"), &n.id);
        if n.removed == Some(true) { continue; }
        if let Some(t) = &n.target {
            if t.starts_with("page:") {
                if t.len() <= 5 { errs.push(FieldErr { path: format!("{p}.target"), message: "page name is empty".into() }); }
            } else if let Some(fid) = t.strip_prefix("iframe:") {
                if !frame_ids.contains(fid) {
                    errs.push(FieldErr { path: format!("{p}.target"), message: format!("iframe target {fid} not found in frames") });
                }
            } else if let Some(u) = t.strip_prefix("url:") {
                check_url(&mut errs, &format!("{p}.target"), Some(u));
            } else {
                errs.push(FieldErr { path: format!("{p}.target"), message: "target must start with page:/iframe:/url:".into() });
            }
        }
    }

    for (i, t) in cfg.tiles.iter().enumerate() {
        let p = format!("tiles[{i}]");
        check_id(&mut errs, &format!("{p}.id"), &t.id);
        if t.removed == Some(true) { continue; }
        match t.type_.as_deref() {
            Some("action") => match t.action.as_deref() {
                Some(a) if ["copy-url", "theme", "restart"].contains(&a) => {}
                Some(_) => errs.push(FieldErr { path: format!("{p}.action"), message: "action must be copy-url, theme or restart".into() }),
                None => errs.push(FieldErr { path: format!("{p}.action"), message: "action is required for type=action".into() }),
            },
            Some("service-light") => match &t.service {
                Some(s) => check_id(&mut errs, &format!("{p}.service"), s),
                None => errs.push(FieldErr { path: format!("{p}.service"), message: "service is required for type=service-light".into() }),
            },
            Some(other) => errs.push(FieldErr { path: format!("{p}.type"), message: format!("type must be action or service-light (got {other})") }),
            None => errs.push(FieldErr { path: format!("{p}.type"), message: "type is required".into() }),
        }
    }

    for (i, c) in cfg.cards.iter().enumerate() {
        let p = format!("cards[{i}]");
        check_id(&mut errs, &format!("{p}.id"), &c.id);
        if c.removed == Some(true) { continue; }
        check_url(&mut errs, &format!("{p}.url"), c.url.as_deref());
        if let Some(v) = &c.ctn { check_id(&mut errs, &format!("{p}.ctn"), v); }
    }

    dup_check(&mut errs, "nav", &cfg.nav.iter().map(|x| x.id.clone()).collect::<Vec<_>>());
    dup_check(&mut errs, "tiles", &cfg.tiles.iter().map(|x| x.id.clone()).collect::<Vec<_>>());
    dup_check(&mut errs, "cards", &cfg.cards.iter().map(|x| x.id.clone()).collect::<Vec<_>>());
    dup_check(&mut errs, "frames", &cfg.frames.iter().map(|x| x.id.clone()).collect::<Vec<_>>());
    errs
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn get_config() -> ApiResult<Json<serde_json::Value>> {
    use serde_json::json;
    Ok(Json(match load_user_config() {
        CfgLoad::Missing => json!({ "ok": true, "config": default_config(), "merged": false }),
        CfgLoad::Valid(u) => json!({ "ok": true, "config": overlay_defaults(&u), "merged": true }),
        CfgLoad::Invalid(e) => json!({ "ok": false, "error": e, "merged": false }),
    }))
}

/// Built-in default template (editor "restore default" preview source).
pub async fn default_config_handler() -> ApiResult<Json<serde_json::Value>> {
    use serde_json::json;
    Ok(Json(json!({ "ok": true, "config": default_config() })))
}

pub async fn put_config(body: String) -> ApiResult<Json<serde_json::Value>> {
    use serde_json::json;
    let cfg: UserConfig = match serde_json::from_str(&body) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Json(json!({ "ok": false, "errors": [{ "path": "", "message": format!("invalid JSON: {e}") }] })));
        }
    };
    let errs = validate(&cfg);
    if !errs.is_empty() {
        return Ok(Json(json!({ "ok": false, "errors": errs })));
    }

    // The editor saves what it sees (the merged config). Items the user
    // removed in a previous save are invisible there, so carry their
    // removed markers forward unless the new body re-adds the same id.
    // Same for top-level title/theme: the phase-1 editor form does not
    // collect them, so a hand-edited title must survive an editor save.
    let mut out = cfg;
    if let CfgLoad::Valid(old) = load_user_config() {
        preserve_removals(&old, &mut out);
        if out.title.is_none() {
            out.title = old.title.clone();
        }
        if out.theme.is_none() {
            out.theme = old.theme.clone();
        }
    }

    let path = config_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let text = serde_json::to_string_pretty(&out)
        .map_err(|e| ApiError(format!("serialize config failed: {e}")))?;
    if path.exists() {
        let _ = std::fs::copy(&path, path.with_extension("json.bak"));
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .map_err(|e| ApiError(format!("write {} failed: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| { let _ = std::fs::remove_file(&tmp); ApiError(format!("replace {} failed: {e}", path.display())) })?;
    invalidate_cache();
    Ok(Json(json!({ "ok": true, "message": format!("saved to {}", path.display()) })))
}

pub async fn reset_config() -> ApiResult<Json<serde_json::Value>> {
    use serde_json::json;
    let path = config_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    invalidate_cache();
    Ok(Json(json!({ "ok": true })))
}

fn preserve_removals(old: &UserConfig, out: &mut UserConfig) {
    for o in &old.nav {
        if o.removed == Some(true) && !out.nav.iter().any(|n| n.id == o.id) {
            out.nav.push(NavItem { id: o.id.clone(), removed: Some(true), ..Default::default() });
        }
    }
    for o in &old.tiles {
        if o.removed == Some(true) && !out.tiles.iter().any(|n| n.id == o.id) {
            out.tiles.push(TileItem { id: o.id.clone(), removed: Some(true), ..Default::default() });
        }
    }
    for o in &old.cards {
        if o.removed == Some(true) && !out.cards.iter().any(|n| n.id == o.id) {
            out.cards.push(CardItem { id: o.id.clone(), removed: Some(true), ..Default::default() });
        }
    }
    for o in &old.frames {
        if o.removed == Some(true) && !out.frames.iter().any(|n| n.id == o.id) {
            out.frames.push(FrameItem { id: o.id.clone(), removed: Some(true), ..Default::default() });
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests (pure logic; no disk access)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_matches_current_ui() {
        let d = default_config();
        assert_eq!(d.nav.len(), 13);
        assert_eq!(d.tiles.len(), 6);
        assert_eq!(d.frames.len(), 3);
        assert!(d.cards.is_empty());
        assert_eq!(d.title.as_deref(), Some("FlyGo · Tailscale 内网"));
        assert!(d.nav.iter().all(|n| n.target.as_deref().is_some()));
        assert!(d.tiles.iter().all(|t| t.type_.as_deref().is_some()));
        assert_eq!(d.frames[0].url.as_deref(), Some("http://${FENG_IP}:5244"));
    }

    #[test]
    fn overlay_override_append_hidden_removed() {
        let base = vec![
            CardItem { id: "a".into(), name: Some("A".into()), port: Some(80), ..Default::default() },
            CardItem { id: "b".into(), name: Some("B".into()), ..Default::default() },
        ];
        let user = vec![
            CardItem { id: "a".into(), name: Some("A2".into()), ..Default::default() },
            CardItem { id: "b".into(), removed: Some(true), ..Default::default() },
            CardItem { id: "c".into(), name: Some("C".into()), ..Default::default() },
        ];
        let out = overlay_items(base, user);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name.as_deref(), Some("A2")); // overridden
        assert_eq!(out[0].port, Some(80));              // untouched default kept
        assert_eq!(out[1].name.as_deref(), Some("C"));  // appended
    }

    #[test]
    fn validate_rejects_bad_fields_with_paths() {
        let cfg = UserConfig {
            nav: vec![NavItem { id: "x y".into(), target: Some("page:x".into()), ..Default::default() }],
            tiles: vec![TileItem { id: "t".into(), type_: Some("bogus".into()), ..Default::default() }],
            cards: vec![CardItem { id: "c".into(), url: Some("javascript:alert(1)".into()), ..Default::default() }],
            frames: vec![FrameItem { id: "f".into(), url: Some("ftp://x".into()), container: Some("../".into()), ..Default::default() }],
            ..Default::default()
        };
        let errs = validate(&cfg);
        assert!(errs.iter().any(|e| e.path == "nav[0].id"));
        assert!(errs.iter().any(|e| e.path == "tiles[0].type"));
        assert!(errs.iter().any(|e| e.path == "cards[0].url"));
        assert!(errs.iter().any(|e| e.path == "frames[0].url"));
        assert!(errs.iter().any(|e| e.path == "frames[0].container"));
    }

    #[test]
    fn validate_rejects_duplicate_ids_and_bad_iframe_ref() {
        let cfg = UserConfig {
            nav: vec![
                NavItem { id: "a".into(), target: Some("page:x".into()), ..Default::default() },
                NavItem { id: "a".into(), target: Some("page:y".into()), ..Default::default() },
                NavItem { id: "b".into(), target: Some("iframe:nope".into()), ..Default::default() },
            ],
            frames: vec![FrameItem { id: "f".into(), url: Some("http://x".into()), ..Default::default() }],
            ..Default::default()
        };
        let errs = validate(&cfg);
        assert!(errs.iter().any(|e| e.message == "duplicate id"));
        assert!(errs.iter().any(|e| e.message.contains("iframe target nope")));
    }

    #[test]
    fn removed_entries_skip_field_validation() {
        let cfg = UserConfig {
            cards: vec![CardItem { id: "gone".into(), removed: Some(true), url: Some("javascript:x".into()), ..Default::default() }],
            ..Default::default()
        };
        assert!(validate(&cfg).is_empty());
    }

    /// End-to-end API roundtrip over the real handlers: no-config GET returns
    /// the default template, PUT persists an overlay that the next GET merges,
    /// bad PUT returns per-field errors, POST reset restores the default.
    /// Uses a temp install root + temp components file; the real install and
    /// the running panel instance are never touched.
    #[tokio::test]
    async fn api_roundtrip_get_put_reset() {
        let tmp = std::env::temp_dir().join(format!("fengo-ctest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let comp = tmp.join("components.json");
        std::env::set_var("FENGPAGE_ROOT", &tmp);
        std::env::set_var("FENGPAGE_COMPONENTS", &comp);
        crate::config::init();

        let body_of = |v: serde_json::Value| async move {
            put_config(v.to_string())
                .await
                .unwrap_or_else(|e| panic!("put failed: {}", e.0))
                .0
        };

        // 1) no config file -> ok:true, default template, merged:false
        let j = get_config().await.unwrap_or_else(|e| panic!("get failed: {}", e.0)).0;
        assert_eq!(j["ok"], true);
        assert_eq!(j["merged"], false);
        let d = &j["config"];
        assert_eq!(d["nav"].as_array().unwrap().len(), 13);
        assert_eq!(d["tiles"].as_array().unwrap().len(), 6);
        assert_eq!(d["frames"].as_array().unwrap().len(), 3);

        // 2) PUT a small overlay -> file written, GET now merged
        let r = body_of(serde_json::json!({
            "nav": [{"id": "overview", "label": "总览X"}],
            "tiles": [{"id": "t-copy", "type": "action", "action": "theme", "label": "复制X"}],
            "cards": [{"id": "my-app", "name": "MyApp", "url": "http://10.0.0.5:8080", "port": 8080}],
            "frames": [{"id": "lf-files", "url": "http://${FENG_IP}:5555"}]
        }))
        .await;
        assert_eq!(r["ok"], true);
        assert!(comp.exists(), "components.json must be written");
        assert!(comp.with_extension("json.tmp").exists() == false, "tmp must be renamed away");

        let j2 = get_config().await.unwrap_or_else(|e| panic!("get failed: {}", e.0)).0;
        assert_eq!(j2["merged"], true);
        let cfg2 = &j2["config"];
        assert!(cfg2["nav"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["label"] == "总览X"));
        assert!(cfg2["tiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["label"] == "复制X"));
        assert!(cfg2["cards"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "my-app" && c["url"] == "http://10.0.0.5:8080"));
        assert!(cfg2["frames"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["url"] == "http://${FENG_IP}:5555"));

        // 3) invalid PUT -> ok:false with per-field errors, file untouched
        let rb = body_of(serde_json::json!({
            "nav": [{"id": "bad id", "target": "page:x"}],
            "cards": [{"id": "c", "url": "javascript:alert(1)"}]
        }))
        .await;
        assert_eq!(rb["ok"], false);
        let errs = rb["errors"].as_array().unwrap();
        assert!(errs.iter().any(|e| e["path"] == "nav[0].id"));
        assert!(errs.iter().any(|e| e["path"] == "cards[0].url"));
        let j3 = get_config().await.unwrap_or_else(|e| panic!("get failed: {}", e.0)).0;
        assert_eq!(j3["merged"], true, "bad PUT must not clobber the saved config");

        // 4) POST reset -> file gone, GET back to default template
        let rr = reset_config().await.unwrap_or_else(|e| panic!("reset failed: {}", e.0)).0;
        assert_eq!(rr["ok"], true);
        assert!(!comp.exists(), "reset must delete components.json");
        let j4 = get_config().await.unwrap_or_else(|e| panic!("get failed: {}", e.0)).0;
        assert_eq!(j4["merged"], false);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
