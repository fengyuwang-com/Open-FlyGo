// /api/config - global configuration file (flygo.toml) read/write (P1).
//
// GET: merged effective configuration (env > file > default) with per-item
//   source annotation, the built-in defaults, the resolved file path and any
//   non-fatal parse/validation errors. Re-reads the file on every call so
//   the answer always matches the disk.
// PUT: validate a full TOML document, then write it atomically (temp file +
//   rename, same pattern as api/term.rs password update). Errors are
//   returned per field. P1 has no hot reload: changed values apply on the
//   next panel start (restart-effective knobs such as [server].addr are
//   documented in the design doc).
//
// Secrets never enter this file (term_token.env etc. keep their own files).
//
// All code comments are pure ASCII; JSON payloads are UTF-8.

use crate::settings;
use crate::log::{log_error, log_info};
use axum::Json;
use serde_json::{json, Value};

/// GET /api/config -> merged effective config + defaults + file info.
pub async fn get_config() -> Json<Value> {
    let s = settings::Settings::load();
    Json(json!({
        "ok": true,
        "file": s.path.to_string_lossy(),
        "exists": s.exists,
        "errors": s.errors,
        "default": settings::default_json(),
        "effective": settings::effective_json(&s),
    }))
}

/// PUT /api/config -> validate a full TOML document and write it atomically.
pub async fn put_config(body: String) -> Json<Value> {
    let text = body;
    let parsed = match toml::from_str::<settings::FileConfig>(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            let (line, col) = settings::span_line_col(&text, e.span());
            let field = settings::key_from_span(&text, e.span())
                .or_else(|| settings::key_from_message(e.message()))
                .unwrap_or_else(|| "flygo.toml".into());
            log_error!(
                "cfg.put",
                "E config write rejected: parse {field}@{line}:{col}: {}",
                e.message()
            );
            return Json(json!({
                "ok": false,
                "errors": [{"field": field, "message": e.message(), "line": line, "col": col}],
            }));
        }
    };
    let errs = settings::validate(&parsed);
    if !errs.is_empty() {
        log_error!("cfg.put", "E config write rejected: validation failed: {errs:?}");
        return Json(json!({ "ok": false, "errors": errs }));
    }

    // Atomic write: temp file + rename (mirrors api/term.rs
    // atomic_update_password; TOKEN/other keys never live here).
    let target = settings::config_target();
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log_error!("cfg.put", "E create dir {} failed: {e}", parent.display());
                return Json(json!({
                    "ok": false,
                    "errors": [{"field": "flygo.toml", "message": format!("create dir {} failed: {e}", parent.display())}],
                }));
            }
        }
    }
    let tmp = target.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, &text) {
        log_error!("cfg.put", "E write {} failed: {e}", tmp.display());
        return Json(json!({
            "ok": false,
            "errors": [{"field": "flygo.toml", "message": format!("write {} failed: {e}", tmp.display())}],
        }));
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        log_error!("cfg.put", "E replace {} failed: {e}", target.display());
        return Json(json!({
            "ok": false,
            "errors": [{"field": "flygo.toml", "message": format!("replace {} failed: {e}", target.display())}],
        }));
    }
    log_info!("cfg.put", "config written to {}", target.display());
    Json(json!({
        "ok": true,
        "file": target.to_string_lossy(),
        "note": "applies on the next panel start (P1 has no hot reload)",
    }))
}
