// OpenList local-storage auto-sync ("a plugged disk shows up in the list").
//
// Background: the container layer is already generic (/mnt bind-mounted, so
// every present drive is visible inside without compose edits), but OpenList
// keeps one storage record per drive in its own DB - a new drive stays
// invisible until a record exists. This module ensures one Local storage per
// present fixed/removable drive on every hotplug pass:
// mount /<LETTER> -> /mnt/<lower letter>, same shape as the existing
// /C /D /E records.
//
// Quiet + safe:
//   - zero I/O when the SYNCED cache covers every present drive;
//     failures only log, never propagate to the caller.
//   - the password never enters logs/responses/errors; a failed login starts
//     a 30-minute cooldown (OpenList rate-limits with 429 on brute force).
//   - server unreachable (container down) retries next round, no cooldown.
//   - switch + credentials come from [openlist] (env FENGPAGE_OPENLIST_*
//     wins), default OFF.
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Per-request timeout (OpenList is local, 10s is generous).
const REST_TIMEOUT_SECS: u64 = 10;
/// Auth-failure cooldown (OpenList counts bad logins, then answers 429).
const AUTH_COOLDOWN_SECS: u64 = 1800;
/// List page size (personal scale, one page is enough).
const LIST_PER_PAGE: u32 = 100;

/// Synced drive letters (uppercase). Process-local memory: a full hit means
/// zero I/O for the whole pass.
fn synced() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}
/// Auth-failure cooldown deadline. None = not triggered.
fn cooldown_until() -> &'static Mutex<Option<Instant>> {
    static C: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(REST_TIMEOUT_SECS))
        .build()
}

/// Drive letter -> OpenList mount path (existing convention: /C /D /E).
/// pub(crate): shared by creation logic and unit tests.
pub(crate) fn mount_path_for(letter: &str) -> String {
    format!("/{}", letter.trim().to_uppercase())
}

/// Drive letter -> in-container root (compose bind-mounts /mnt as a whole).
/// pub(crate): shared by creation logic and unit tests.
pub(crate) fn root_for(letter: &str) -> String {
    format!("/mnt/{}", letter.trim().to_lowercase())
}

/// Local-driver addition JSON (same shape as the live /E record, root swapped).
/// pub(crate): unit-tested field shape.
pub(crate) fn addition_json(root: &str) -> serde_json::Value {
    serde_json::json!({
        "root_folder_path": root,
        "directory_size": false,
        "thumbnail": false,
        "thumb_cache_folder": "",
        "thumb_concurrency": "",
        "video_thumb_pos": "20%",
        "show_hidden": false,
        "mkdir_perm": "",
        "recycle_bin_path": "",
    })
}

/// Parse a storage list into (mount_path, root_folder_path). The addition
/// field is a JSON string; malformed rows are skipped, never fatal.
/// pub(crate): unit-tested.
pub(crate) fn parse_storage_list(v: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let items = v
        .get("data")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_array());
    let Some(items) = items else { return out };
    for s in items {
        let mp = s.get("mount_path").and_then(|m| m.as_str()).unwrap_or("");
        if mp.is_empty() {
            continue;
        }
        let root = s
            .get("addition")
            .and_then(|a| a.as_str())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
            .and_then(|a| a.get("root_folder_path").and_then(|r| r.as_str()).map(|r| r.to_string()))
            .unwrap_or_default();
        out.push((mp.to_string(), root));
    }
    out
}

fn authed(agent: &ureq::Agent, base: &str, token: &str, path: &str) -> Result<serde_json::Value, String> {
    agent
        .get(&format!("{base}{path}"))
        .set("Authorization", token)
        .call()
        .map_err(|e| format!("GET {path}: {e}"))?
        .into_json::<serde_json::Value>()
        .map_err(|e| format!("GET {path}: bad json ({e})"))
}

/// Sync every present drive in one pass (shared by the hotplug poller and the
/// manual endpoint). Returns one report line per action; a full SYNCED hit
/// returns empty with zero I/O.
pub fn sync_all_present_drives() -> Vec<String> {
    if !crate::settings::get().openlist_enabled() {
        return Vec::new();
    }
    let letters: Vec<String> = crate::smb::list_drives()
        .into_iter()
        .filter(|d| d.mounted && (d.kind == "fixed" || d.kind == "removable"))
        .map(|d| d.letter.trim().to_uppercase())
        .filter(|l| l.len() == 1)
        .collect();
    if letters.is_empty() {
        return Vec::new();
    }
    {
        let synced = synced().lock().unwrap_or_else(|e| e.into_inner());
        if letters.iter().all(|l| synced.contains(l)) {
            return Vec::new();
        }
    }
    // One session for all drives: single probe + login + list, create misses.
    match sync_session(&letters) {
        Ok(lines) => lines,
        Err(e) => vec![format!("openlist sync skipped: {e}")],
    }
}

/// Single-session sync (blocking; the caller must be in spawn_blocking/thread).
fn sync_session(letters: &[String]) -> Result<Vec<String>, String> {
    let base = crate::settings::get().openlist_base_url();
    let base = base.trim_end_matches('/');
    let username = crate::settings::get().openlist_username();
    let password = crate::settings::get().openlist_password();
    if username.trim().is_empty() || password.is_empty() {
        return Err("no credentials ([openlist].username/password)".into());
    }
    let ag = agent();
    // 1) liveness probe (fast fail while the container is down, no cooldown).
    ag.get(&format!("{base}/api/public/settings"))
        .call()
        .map_err(|_| "server unreachable (container down?)".to_string())?;
    // 2) cooldown gate.
    if let Some(until) = *cooldown_until().lock().unwrap_or_else(|e| e.into_inner()) {
        if Instant::now() < until {
            return Err("auth cooling down after a failed login".into());
        }
    }
    // 3) login (the password never enters an error string).
    let login: serde_json::Value = ag
        .post(&format!("{base}/api/auth/login"))
        .send_json(serde_json::json!({ "username": username, "password": password }))
        .map_err(|e| format!("login request failed ({e})"))?
        .into_json()
        .map_err(|e| format!("login bad response ({e})"))?;
    if login.get("code").and_then(|c| c.as_i64()) != Some(200) {
        *cooldown_until().lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Instant::now() + Duration::from_secs(AUTH_COOLDOWN_SECS));
        let msg = login.get("message").and_then(|m| m.as_str()).unwrap_or("rejected");
        return Err(format!("login rejected ({msg})"));
    }
    let token = login
        .get("data")
        .and_then(|d| d.get("token"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return Err("login gave no token".into());
    }
    // 4) existing records.
    let list = authed(&ag, base, &token, &format!("/api/admin/storage/list?page=1&per_page={LIST_PER_PAGE}"))?;
    let have = parse_storage_list(&list);
    let max_order = list
        .get("data")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(|s| s.get("order").and_then(|o| o.as_i64())).max().unwrap_or(-1))
        .unwrap_or(-1);
    // 5) clone one live Local record as the create template (follows the
    // server schema instead of hardcoding every field).
    let template = list
        .get("data")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .filter(|s| s.get("driver").and_then(|d| d.as_str()) == Some("Local"))
                .filter_map(|s| s.get("id").and_then(|i| i.as_i64()))
                .next()
        })
        .and_then(|id| {
            authed(&ag, base, &token, &format!("/api/admin/storage/get?id={id}")).ok()
        })
        .and_then(|full| full.get("data").cloned());
    let mut out = Vec::new();
    let mut order = max_order + 1;
    for l in letters {
        let root = root_for(l);
        if have.iter().any(|(_, r)| r == &root) {
            synced().lock().unwrap_or_else(|e| e.into_inner()).insert(l.clone());
            continue;
        }
        match create_storage(&ag, base, &token, l, &root, order, template.as_ref()) {
            Ok(id) => {
                synced().lock().unwrap_or_else(|e| e.into_inner()).insert(l.clone());
                order += 1;
                out.push(format!("{l}: created storage id {id}"));
            }
            Err(e) => out.push(format!("{l}: create failed ({e})")),
        }
    }
    Ok(out)
}

/// Create one Local storage. Prefers cloning the live template (only
/// mount/order/addition.root replaced); falls back to a minimal body.
fn create_storage(
    ag: &ureq::Agent,
    base: &str,
    token: &str,
    letter: &str,
    root: &str,
    order: i64,
    template: Option<&serde_json::Value>,
) -> Result<i64, String> {
    let mut body = template.cloned().unwrap_or_else(|| {
        serde_json::json!({
            "order": 0, "driver": "Local", "cache_expiration": 30,
            "status": "work", "remark": "", "disabled": false,
            "order_by": "name", "order_direction": "asc",
            "extract_folder": "", "web_proxy": false, "webdav_policy": "native",
            "proxy_range": false, "down_proxy_url": "",
        })
    });
    if let Some(o) = body.as_object_mut() {
        o.remove("id");
        o.remove("modified");
        o.remove("status");
        o.insert("mount_path".into(), serde_json::Value::String(mount_path_for(letter)));
        o.insert("order".into(), serde_json::Value::Number(order.into()));
        o.insert("driver".into(), serde_json::Value::String("Local".into()));
        o.insert("addition".into(), serde_json::Value::String(addition_json(root).to_string()));
    }
    let resp: serde_json::Value = ag
        .post(&format!("{base}/api/admin/storage/create"))
        .set("Authorization", token)
        .send_json(body)
        .map_err(|e| format!("create request failed ({e})"))?
        .into_json()
        .map_err(|e| format!("create bad response ({e})"))?;
    if resp.get("code").and_then(|c| c.as_i64()) != Some(200) {
        let msg = resp.get("message").and_then(|m| m.as_str()).unwrap_or("rejected");
        return Err(msg.to_string());
    }
    resp.get("data")
        .and_then(|d| d.get("id"))
        .and_then(|i| i.as_i64())
        .ok_or_else(|| "create gave no id".to_string())
}

/// POST /api/openlist/sync -> one full manual sync (besides the hotplug poll).
pub async fn sync() -> crate::ApiResult<axum::Json<serde_json::Value>> {
    let report = tokio::task::spawn_blocking(sync_all_present_drives)
        .await
        .unwrap_or_else(|e| vec![format!("worker failed ({e})")]);
    Ok(axum::Json(serde_json::json!({ "ok": true, "report": report })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_mapping_matches_existing_convention() {
        assert_eq!(mount_path_for("c"), "/C");
        assert_eq!(mount_path_for(" D "), "/D");
        assert_eq!(root_for("C"), "/mnt/c");
        assert_eq!(root_for("e"), "/mnt/e");
    }

    #[test]
    fn addition_shape_matches_live_e_entry() {
        let a = addition_json("/mnt/e");
        assert_eq!(a["root_folder_path"], "/mnt/e");
        assert_eq!(a["thumbnail"], false);
        assert_eq!(a["show_hidden"], false);
        assert_eq!(a["video_thumb_pos"], "20%");
    }

    #[test]
    fn storage_list_parse_skips_bad_rows() {
        let v = serde_json::json!({
            "code": 200,
            "data": { "content": [
                { "mount_path": "/E", "addition": "{\"root_folder_path\":\"/mnt/e\"}" },
                { "mount_path": "/BaiduNetDisk", "addition": "{\"root_folder_path\":\"/\"}" },
                { "mount_path": "", "addition": "{}" },
                { "mount_path": "/Broken", "addition": "not-json" },
            ]}
        });
        let got = parse_storage_list(&v);
        assert_eq!(got.len(), 3);
        assert!(got.contains(&("/E".to_string(), "/mnt/e".to_string())));
        assert!(got.contains(&("/Broken".to_string(), String::new())));
        assert!(parse_storage_list(&serde_json::json!({})).is_empty());
    }
}
