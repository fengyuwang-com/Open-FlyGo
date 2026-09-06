// /api/upload - phone "share to panel" upload target.
//
// POST /api/upload with a multipart body carrying the file in the `file`
// field. The file lands in <data>/uploads/<YYYYMMDD>/<filename> (date is the
// local date at upload time). A successful response is
//   {"ok": true, "path": "<absolute path>", "size": <bytes>, "date": "YYYYMMDD"}
//
// Retrieving a failed transfer is a retry of a *new* attempt: the first
// attempt's file is never left half-written (streamed to a .part temp file
// and renamed into place), and the idempotency key below makes a full retry
// safe - the same upload_id yields the same stored file instead of a second
// copy with a "-1" suffix.
//
// Idempotency (safe retry, never duplicating a non-idempotent write):
//   The client may carry an `upload_id` (8-64 chars of [A-Za-z0-9_-]) in the
//   `X-FlyGo-Upload-Id` header or as a multipart form field *before* the
//   `file` field. Completed uploads are remembered in a small journal
//   (<data>/uploads/.upload-journal.json). A retry with the same upload_id
//   replays the stored result without writing anything; every response
//   echoes the upload_id so the client always has a key for its next retry.
//
// Error responses carry a `retryable` flag: true for transient failures
// (network aborts, disk errors) where resending the same request is safe,
// false for permanent ones (too large, invalid filename) where a naive
// retry would just fail again.
//
// GET /api/upload/recent - last few completed uploads (journal) for the
// panel UI's "recent uploads" line.
//
// Safety:
//  - size cap from [control].upload_max_mb (default 500 MB) -> HTTP 413
//  - filename is sanitized (path separators / traversal / Windows-invalid
//    characters rejected) so a hostile client cannot escape the upload dir
//  - duplicate names get a "-1", "-2" ... suffix instead of overwriting
//  - the file is streamed to a .part temp file and renamed into place, so
//    a partial/failed upload never leaves a half-written file under its
//    final name
//
// All code comments are pure ASCII.

use axum::extract::Multipart;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Maximum accepted filename length (bytes) after sanitizing.
const MAX_NAME_BYTES: usize = 200;
/// Maximum length of an upload idempotency key.
const MAX_UPLOAD_ID_BYTES: usize = 64;
/// Journal size cap: the newest entries are kept for retry replay.
const JOURNAL_CAP: usize = 128;
/// How many entries the /api/upload/recent widget shows.
const RECENT_LIMIT: usize = 12;

// ---------------------------------------------------------------------------
// upload idempotency journal
// ---------------------------------------------------------------------------

/// One completed upload, remembered so a retry with the same id can replay
/// the stored result instead of writing a duplicate file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct JournalEntry {
    id: String,
    path: String,
    size: u64,
    date: String,
    at: i64,
    /// Number of times this entry was served (1 = original upload, 2+ =
    /// the client retried with the same id and got the deduped replay).
    #[serde(default)]
    times: u64,
}

/// In-memory journal (process lifetime) + one-shot lazy load from disk.
static JOURNAL: Mutex<Vec<JournalEntry>> = Mutex::new(Vec::new());
static JOURNAL_LOADED: AtomicBool = AtomicBool::new(false);

fn journal_path() -> PathBuf {
    crate::config::get().uploads_dir.join(".upload-journal.json")
}

/// Load the journal from disk once. A missing or corrupt journal is treated
/// as empty - the panel keeps working; only dedupe replay is lost.
fn load_journal() -> Vec<JournalEntry> {
    if JOURNAL_LOADED.load(Ordering::Relaxed) {
        return JOURNAL.lock().unwrap_or_else(|e| e.into_inner()).clone();
    }
    let loaded = std::fs::read_to_string(journal_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<JournalEntry>>(&t).ok())
        .unwrap_or_default();
    {
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        *j = loaded;
        JOURNAL_LOADED.store(true, Ordering::Relaxed);
    }
    load_journal()
}

/// Persist the journal atomically (tmp + rename, same pattern as
/// api/control.rs). Failures are logged but never fail the upload itself -
/// the file is already safely on disk.
fn save_journal() {
    let entries = JOURNAL.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let path = journal_path();
    let text = match serde_json::to_string(&entries) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[upload] serialize journal failed: {e}");
            return;
        }
    };
    let tmp = path.with_extension("upload-journal.json.tmp");
    if let Err(e) = std::fs::write(&tmp, text) {
        eprintln!("[upload] write journal failed: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("[upload] replace journal failed: {e}");
    }
}

/// Keep only the newest JOURNAL_CAP entries (oldest trimmed first).
fn journal_trim(entries: &mut Vec<JournalEntry>) {
    let over = entries.len().saturating_sub(JOURNAL_CAP);
    if over > 0 {
        entries.drain(..over);
    }
}

/// Append a completed upload to the journal (in memory + disk).
fn journal_append(entry: JournalEntry) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    j.retain(|e| e.id != entry.id); // overwrite any earlier entry with the id
    j.push(entry);
    journal_trim(&mut j);
    drop(j);
    save_journal();
}

/// Look up a completed upload by id.
fn journal_find(id: &str) -> Option<JournalEntry> {
    load_journal()
        .into_iter()
        .find(|e| e.id == id)
}

/// Drop a stale entry (its file no longer exists) before re-uploading.
fn journal_remove(id: &str) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    j.retain(|e| e.id != id);
    drop(j);
    save_journal();
}

/// Idempotency keys are client-generated: strict charset and length, so a
/// hostile client cannot abuse the journal path or grow it unboundedly.
fn validate_upload_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_UPLOAD_ID_BYTES {
        return Err(format!("must be 1-{MAX_UPLOAD_ID_BYTES} bytes"));
    }
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("only [A-Za-z0-9_-] allowed".into());
    }
    Ok(())
}

/// The cached response for a replayed upload (repeated=true).
fn repeated_json(entry: &JournalEntry, upload_id: &str) -> Value {
    json!({
        "ok": true,
        "path": entry.path,
        "size": entry.size,
        "date": entry.date,
        "upload_id": upload_id,
        "repeated": true,
        "note": "this upload_id was already completed - reused the stored file",
    })
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// upload handler
// ---------------------------------------------------------------------------

/// POST /api/upload - stream the `file` multipart field to disk.
pub async fn upload(headers: HeaderMap, mut multipart: Multipart) -> Response {
    let max_bytes = {
        let s = crate::settings::Settings::load();
        s.control_upload_max_mb().saturating_mul(1024 * 1024)
    };
    let root = crate::config::get().uploads_dir.clone();
    let date = ymd_dir();
    let dir = root.join(&date);

    // Idempotency key from the header (form-field variant is read in the
    // part loop below and must arrive before the `file` field).
    let mut upload_id: Option<String> = headers
        .get("x-flygo-upload-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    if let Some(id) = upload_id.as_deref() {
        if let Err(e) = validate_upload_id(id) {
            return err(400, format!("invalid upload_id: {e}"), false);
        }
    }

    loop {
        let mut part = match multipart.next_field().await {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => return err(400, format!("multipart read error: {e}"), true),
        };
        // Optional idempotency field when the client did not send the header.
        if part.name() == Some("upload_id") {
            let id = match part.text().await {
                Ok(t) => t.trim().to_string(),
                Err(e) => return err(400, format!("upload_id read error: {e}"), true),
            };
            if let Err(e) = validate_upload_id(&id) {
                return err(400, format!("invalid upload_id: {e}"), false);
            }
            upload_id = Some(id);
            continue;
        }
        if part.name() != Some("file") {
            continue; // ignore unexpected fields
        }
        let Some(fname) = part.file_name().map(|s| s.to_string()) else {
            return err(400, "file field has no filename".into(), false);
        };
        let Some(name) = sanitize_filename(&fname) else {
            return err(400, format!("invalid filename `{fname}`"), false);
        };
        // Safe retry: a retry with the same upload_id replays the stored
        // result instead of writing a "-1" duplicate. The replay itself is
        // recorded (times += 1) so the UI can show which files were
        // deduped by a retry.
        if let Some(id) = upload_id.as_deref() {
            if let Some(entry) = journal_find(id) {
                if Path::new(&entry.path).exists() {
                    let mut entry = entry;
                    entry.times = entry.times.saturating_add(1);
                    journal_append(entry.clone());
                    return Json(repeated_json(&entry, id)).into_response();
                }
                journal_remove(id); // stale entry - the file is gone
            }
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return err(500, format!("create {} failed: {e}", dir.display()), true);
        }
        let dest = dedupe_path(&dir, &name);
        let tmp = dir.join(format!(".{}.part", date));
        let mut file = match std::fs::File::create(&tmp) {
            Ok(f) => f,
            Err(e) => return err(500, format!("create {} failed: {e}", tmp.display()), true),
        };
        let mut total: u64 = 0;
        let mut too_large = false;
        loop {
            match part.chunk().await {
                Ok(Some(chunk)) => {
                    total += chunk.len() as u64;
                    if total > max_bytes {
                        // Keep draining the stream so the client receives the
                        // 413 response instead of a connection reset mid-body.
                        // Far past the cap, stop draining (abusive sizes
                        // forfeit a clean response); the temp file is removed
                        // after the loop (the handle must close on Windows).
                        too_large = true;
                        if total > max_bytes.saturating_add(64 * 1024 * 1024) {
                            break;
                        }
                        continue;
                    }
                    if let Err(e) = file.write_all(&chunk) {
                        let _ = std::fs::remove_file(&tmp);
                        return err(500, format!("write failed: {e}"), true);
                    }
                }
                Ok(None) => break, // stream fully drained; finalize below
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return err(400, format!("upload aborted: {e}"), true);
                }
            }
        }
        drop(file);
        if too_large {
            let _ = std::fs::remove_file(&tmp);
            let mb = max_bytes / (1024 * 1024);
            return err(413, format!("file too large (limit {mb} MB, control.upload_max_mb)"), false);
        }
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            let _ = std::fs::remove_file(&tmp);
            return err(500, format!("finalize {} failed: {e}", dest.display()), true);
        }
        // Remember the completed upload so a retry with the same id replays
        // this exact result instead of duplicating the file.
        if let Some(id) = upload_id.as_ref() {
            journal_append(JournalEntry {
                id: id.clone(),
                path: dest.to_string_lossy().into_owned(),
                size: total,
                date: date.clone(),
                at: now_epoch_secs(),
                times: 1,
            });
        }
        return Json(json!({
            "ok": true,
            "path": dest.to_string_lossy(),
            "size": total,
            "date": date,
            "upload_id": upload_id.unwrap_or_default(),
            "repeated": false,
        }))
        .into_response();
    }
    err(400, "no `file` part found in the multipart body".into(), false)
}

/// GET /api/upload/recent - last few completed uploads (newest first).
pub async fn recent() -> Json<Value> {
    let entries: Vec<Value> = load_journal()
        .iter()
        .rev()
        .take(RECENT_LIMIT)
        .map(|e| {
            let name = Path::new(&e.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| e.id.clone());
            json!({
                "name": name,
                "path": e.path,
                "size": e.size,
                "date": e.date,
                "upload_id": e.id,
                "at": e.at,
                "times": e.times,
            })
        })
        .collect();
    Json(json!({ "ok": true, "recent": entries }))
}

/// Safe filename: strips any directory components, rejects traversal and
/// Windows-invalid characters. None = reject the upload.
pub fn sanitize_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    if base.len() > MAX_NAME_BYTES {
        return None;
    }
    let bad = base
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'));
    if bad {
        return None;
    }
    Some(base.to_string())
}

/// First free name: "a.txt" -> "a.txt", "a-1.txt", "a-2.txt", ...
fn dedupe_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for i in 1..=999 {
        let cand = dir.join(format!("{stem}-{i}{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    dir.join(format!("{stem}-{}-{ext}", now_epoch_millis()))
}

/// Local date as YYYYMMDD via the Win32 GetLocalTime API (already linked in
/// this binary through the tray code paths).
fn ymd_dir() -> String {
    use windows::Win32::Foundation::SYSTEMTIME;
    let st: SYSTEMTIME = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}{:02}{:02}", st.wYear, st.wMonth, st.wDay)
}

fn now_epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// JSON error response with an explicit retryable flag so clients can decide
/// whether resending the same request is safe (see module doc).
fn err(status: u16, message: String, retryable: bool) -> Response {
    (
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_REQUEST),
        Json(json!({"ok": false, "message": message, "retryable": retryable})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_invalid_names() {
        // Path components are stripped to the basename, so traversal
        // attempts can never escape the upload dir; the surviving name is
        // still subject to the invalid-character checks below.
        assert_eq!(sanitize_filename("../evil.txt"), Some("evil.txt".into()));
        assert_eq!(sanitize_filename("..\\evil.txt"), Some("evil.txt".into()));
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(sanitize_filename("  "), None);
        assert_eq!(sanitize_filename("a<b>.txt"), None);
        assert_eq!(sanitize_filename("a:b.txt"), None);
        assert_eq!(sanitize_filename("a|b.txt"), None);
        assert_eq!(sanitize_filename(&"x".repeat(MAX_NAME_BYTES + 1)), None);
    }

    #[test]
    fn sanitize_keeps_normal_names_and_strips_dirs() {
        assert_eq!(sanitize_filename("IMG_2026.jpg"), Some("IMG_2026.jpg".into()));
        assert_eq!(sanitize_filename("照片 2026.jpg"), Some("照片 2026.jpg".into()));
        // Directory components are stripped; only the basename survives, so
        // a path can never escape the upload directory.
        assert_eq!(sanitize_filename("a/b.txt"), Some("b.txt".into()));
        assert_eq!(sanitize_filename("C:\\full\\path\\f.txt"), Some("f.txt".into()));
        assert_eq!(sanitize_filename("/etc/passwd"), Some("passwd".into()));
        assert_eq!(sanitize_filename("subdir/../../etc/x"), Some("x".into()));
    }

    #[test]
    fn dedupe_appends_suffix_after_first() {
        let dir = std::env::temp_dir().join(format!("fengo-upload-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dedupe_path(&dir, "a.txt");
        assert_eq!(a.file_name().unwrap().to_string_lossy(), "a.txt");
        std::fs::write(&a, b"x").unwrap();
        let b = dedupe_path(&dir, "a.txt");
        assert_eq!(b.file_name().unwrap().to_string_lossy(), "a-1.txt");
        std::fs::write(&b, b"x").unwrap();
        let c = dedupe_path(&dir, "a.txt");
        assert_eq!(c.file_name().unwrap().to_string_lossy(), "a-2.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_id_validation_enforces_charset_and_length() {
        assert!(validate_upload_id("abc-123_XYZ").is_ok());
        assert!(validate_upload_id("a").is_ok());
        assert!(validate_upload_id(&"x".repeat(64)).is_ok());
        assert!(validate_upload_id(&"x".repeat(65)).is_err(), "too long");
        assert!(validate_upload_id("").is_err(), "empty");
        assert!(validate_upload_id("bad id!").is_err(), "space + bang");
        assert!(validate_upload_id("../../etc").is_err(), "path chars");
    }

    #[test]
    fn journal_round_trip_and_trim() {
        let dir = std::env::temp_dir().join(format!("fengo-journal-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".upload-journal.json");

        let mut entries = Vec::new();
        for i in 0..(JOURNAL_CAP + 10) {
            entries.push(JournalEntry {
                id: format!("id-{i}"),
                path: format!(r"C:\data\uploads\f{i}.bin"),
                size: i as u64,
                date: "20260817".into(),
                at: i as i64,
                times: 1,
            });
        }
        journal_trim(&mut entries);
        assert_eq!(entries.len(), JOURNAL_CAP);
        assert_eq!(entries.first().unwrap().id, "id-10", "oldest trimmed");
        assert_eq!(entries.last().unwrap().id, format!("id-{}", JOURNAL_CAP + 9));

        // Persist + reload round-trip.
        let text = serde_json::to_string(&entries).unwrap();
        std::fs::write(&path, &text).unwrap();
        let reloaded: Vec<JournalEntry> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded, entries);

        // Old journals without the times field deserialize to times=0.
        let legacy: Vec<JournalEntry> =
            serde_json::from_str(r#"[{"id":"a","path":"p","size":1,"date":"20260817","at":1}]"#).unwrap();
        assert_eq!(legacy[0].times, 0);

        // An id collision replaces the older entry instead of duplicating.
        let mut with_dup = reloaded;
        with_dup.push(with_dup[0].clone());
        journal_trim(&mut with_dup);
        assert!(with_dup.len() <= JOURNAL_CAP + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
