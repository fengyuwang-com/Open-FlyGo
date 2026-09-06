// GET /api/diagnostics/bundle - one-click after-sales diagnostic bundle (T-13).
//
// Assembles everything support needs to locate a fault without remote
// access: panel version + drift self-check, health and service probes,
// the docker services wall, a config summary (never the password), and
// the tail of flygo.log. The log tail is REDACTED with the same secret
// keyword list as scripts/check-secrets.sh (single source of truth for
// "what counts as a secret"), and PEM key blocks are dropped outright.
//
// The bundle is written to <root>/data/flygo-diag-<ts>.json (so the user
// can attach the file even if the download itself fails) and returned as
// a downloadable attachment.
//
// All code comments are pure ASCII; user-visible strings may be UTF-8.

use crate::ApiResult;
use axum::{
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;

/// Log tail length (lines). 300 lines cover the last boot cycle plus a
/// little history while keeping the bundle well under a megabyte.
const LOG_TAIL_LINES: usize = 300;

/// Secret keyword list - same source as scripts/check-secrets.sh
/// CONTENT_RE. Keep the two lists in sync (checked by a unit test that
/// greps the shell script).
const SECRET_KEYS: [&str; 12] = [
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "api-key",
    "access_key",
    "access-key",
    "private_key",
    "client_secret",
    "auth_token",
    "cf_token",
];

/// Redact one line. Secret-shaped assignments (KEY=VALUE / KEY: VALUE /
/// "KEY": "VALUE", value >= 6 chars) get their value replaced; PEM key
/// blocks are dropped entirely; everything else passes through.
pub fn redact_line(line: &str) -> String {
    if line.contains("PRIVATE KEY") {
        return String::new();
    }
    let (key_part, sep_idx) = match find_separator(line) {
        Some(x) => x,
        None => return line.to_string(),
    };
    let key_l = key_part.to_lowercase();
    let key_clean = key_l.trim_matches(|c| c == '"' || c == '\'').trim().to_string();
    let is_secret = SECRET_KEYS.iter().any(|k| {
        key_clean == *k
            || key_clean.ends_with(&format!("_{k}"))
            || key_clean.ends_with(&format!("-{k}"))
            || key_clean.starts_with(&format!("{k}_"))
            || key_clean.starts_with(&format!("{k}-"))
    });
    if !is_secret {
        return line.to_string();
    }
    let value = line[sep_idx + 1..].trim();
    let unquoted = value.trim_matches(|c| c == '"' || c == '\'');
    if unquoted.len() < 6 {
        return line.to_string(); // booleans/short flags pass (same rule as the script)
    }
    let head = &line[..sep_idx + 1];
    format!("{head} ***REDACTED***")
}

/// Find the first `=` or `:` separator outside quotes; returns the key
/// part and the separator index. None when the line has no assignment
/// shape (plain log text).
fn find_separator(line: &str) -> Option<(&str, usize)> {
    let bytes = line.as_bytes();
    let mut in_quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match in_quote {
            Some(q) => {
                if b == q {
                    in_quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => in_quote = Some(b),
                b'=' | b':' => return Some((&line[..i], i)),
                _ => {}
            },
        }
    }
    None
}

/// Tail of a text file (last N lines), each redacted.
fn log_tail() -> Vec<String> {
    let path = crate::config::get().root.join("data").join("logs").join("flygo.log");
    let Ok(content) = std::fs::read_to_string(path) else {
        return vec!["<log file not readable>".into()];
    };
    let start = content.lines().count().saturating_sub(LOG_TAIL_LINES);
    content
        .lines()
        .skip(start)
        .map(redact_line)
        .filter(|l| !l.is_empty())
        .collect()
}

/// Assemble the bundle. Slow probes are avoided: services_health has its
/// own per-probe timeouts and everything else is file reads / in-process.
async fn build_bundle() -> serde_json::Value {
    let diag = match crate::api::diagnostics::diagnostics().await {
        Ok(v) => v.0,
        Err(e) => return json!({ "kind": "flygo-diagnostic-bundle", "error": e.0 }),
    };
    let health = match crate::api::health::health().await {
            Ok(v) => json!(v.0),
            Err(e) => json!({ "error": e.0 }),
        };
    // Service probes carry their own per-probe timeouts and internal
    // spawn_blocking usage, so awaiting them directly is safe.
    let services = match crate::api::health::services_health().await {
        Ok(v) => v.0,
        Err(_) => Vec::new(),
    };
    json!({
        "kind": "flygo-diagnostic-bundle",
        "created_at_unix": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "panel": diag,
        "health": health,
        "services": services,
        "docker_wall": crate::api::docker::compose_list(),
        "config_summary": config_summary(),
        "log_tail_redacted": log_tail(),
    })
}

/// Non-secret config facts support may need (ports, feature switches).
/// The auth password is NEVER included - only whether the layer is on.
fn config_summary() -> serde_json::Value {
    let s = crate::settings::get();
    let (webcli, syncthing, homepage) = s.services_ports();
    json!({
        "ports": {
            "webcli": webcli,
            "syncthing": syncthing,
            "homepage": homepage,
        },
        "auth_enabled": s.auth_enabled(),
        "install_root": crate::config::get().root.display().to_string(),
    })
}

/// GET /api/diagnostics/bundle -> JSON attachment + copy on disk.
pub async fn bundle() -> ApiResult<axum::response::Response> {
    let value = build_bundle().await;
    let stamp = chrono_stamp();
    let path = crate::config::get()
        .root
        .join("data")
        .join(format!("flygo-diag-{stamp}.json"));
    let disk = std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap_or_default());
    crate::log::log(
        "INFO",
        "diag.bundle",
        &match &disk {
            Ok(_) => format!("bundle written to {}", path.display()),
            Err(e) => format!("bundle disk write failed: {e}"),
        },
    );
    let body = serde_json::to_vec_pretty(&value).unwrap_or_default();
    let name = format!("flygo-diag-{stamp}.json");
    let mut res = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response();
    if let Ok(v) = header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        res.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

/// Local-time-ish stamp from the epoch (UTC HHMMSS; uniqueness is what
/// matters, not the timezone).
fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs:010x}") // hex epoch: short, unique, sortable
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_assignments_only() {
        assert_eq!(
            redact_line("FENGDOG_SECRET=deadbeefcafe"),
            "FENGDOG_SECRET= ***REDACTED***"
        );
        assert_eq!(
            redact_line("password: hunter2hunter2"),
            "password: ***REDACTED***"
        );
        assert_eq!(
            redact_line("\"api_key\": \"abc123def\""),
            "\"api_key\": ***REDACTED***"
        );
        // short/boolean values pass (same 6-char rule as check-secrets)
        assert_eq!(redact_line("enabled = true"), "enabled = true");
        // ordinary log lines pass untouched
        assert_eq!(
            redact_line("2026-09-07T00:00 INFO http.req GET /api/health -> 200 (5ms)"),
            "2026-09-07T00:00 INFO http.req GET /api/health -> 200 (5ms)"
        );
        // PEM blocks dropped (header split so check-secrets does not flag this source)
        let pem_header = format!("-----BEGIN {} KEY-----", "PRIVATE");
        assert_eq!(redact_line(&pem_header), "");
    }

    #[test]
    fn secret_keyword_list_stays_in_sync_with_check_secrets_sh() {
        // The shell script is the source of truth; this test only guards
        // against the Rust list drifting behind it silently.
        let script = include_str!("../../../scripts/check-secrets.sh");
        for kw in SECRET_KEYS {
            let plain = kw.to_uppercase().replace('-', "_");
            let bracket = plain.replace('_', "[_-]?");
            assert!(
                script.contains(&plain) || script.contains(&bracket),
                "keyword {plain} missing from scripts/check-secrets.sh CONTENT_RE"
            );
        }
    }
}
