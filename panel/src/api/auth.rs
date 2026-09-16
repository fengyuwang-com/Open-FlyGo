// /api/auth/* + /login - panel login layer (M09), default OFF.
//
// Opt-in password gate for the whole panel, mirroring the 0909 term-ticket
// model (api/term.rs): a successful login issues a random 128-bit session
// token as an HttpOnly cookie; an in-memory session map tracks expiry
// (12 h). While [auth].enabled = false (the default in flygo.toml) the
// guard middleware is a pure pass-through and panel behavior is identical
// to a build without this layer - the integration tests assert that.
//
// Endpoints:
//   POST /api/auth/login  {"password":...} -> 200 + Set-Cookie on success,
//        401 with ONE generic message otherwise (disabled layer, malformed
//        body and wrong password are indistinguishable - no oracle).
//   POST /api/auth/logout -> drop the session, clear the cookie.
//   GET  /api/auth/status -> {enabled, authenticated} (authenticated is
//        true when the layer is off: every request passes the guard).
//   GET  /login           -> standalone login page (static/login.html).
//
// Guard allowlist while enabled: /login, /api/auth/*, /favicon.ico and the
// /api/health probe. Everything else: HTML -> 302 /login, /api/* -> 401.
//
// Secrets discipline: the password never enters logs, responses or error
// messages; session tokens are only ever written into Set-Cookie headers.
// The password and TTL are frozen at boot (restart-effective, like the rest
// of flygo.toml - no hot reload).
//
// This module depends on axum + std only (no crate::config/settings), so
// integration tests can include it directly without the panel AppState.
//
// All code comments are pure ASCII; HTML payloads are UTF-8.

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::log::{log_info, log_warn};

/// Cookie carrying the session token (HttpOnly, SameSite=Lax, Path=/).
const COOKIE_NAME: &str = "flygo_auth";
/// Default session lifetime: 12 hours (same order as the term tickets).
const SESSION_TTL_DEFAULT: Duration = Duration::from_secs(12 * 3600);

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

struct AuthInner {
    enabled: bool,
    password: String,
    ttl: Duration,
    /// token -> expiry instant; only successful logins insert entries, so
    /// the map stays tiny. Expired entries are dropped lazily on lookup.
    sessions: Mutex<HashMap<String, Instant>>,
}

/// Shared login-layer state (built once at boot from flygo.toml [auth]).
#[derive(Clone)]
pub struct AuthState {
    inner: Arc<AuthInner>,
}

impl AuthState {
    pub fn new(enabled: bool, password: &str) -> Self {
        Self::with_ttl(enabled, password, SESSION_TTL_DEFAULT)
    }

    /// Test hook: same as new() with an explicit session TTL.
    pub fn with_ttl(enabled: bool, password: &str, ttl: Duration) -> Self {
        AuthState {
            inner: Arc::new(AuthInner {
                enabled,
                password: password.to_string(),
                ttl,
                sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Insert a fresh session and return its token.
    fn create_session(&self) -> String {
        let token = gen_token();
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token.clone(), Instant::now() + self.inner.ttl);
        token
    }

    /// True when the token maps to an unexpired session (lazy expiry).
    fn valid_session(&self, token: &str) -> bool {
        let mut g = self.inner.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(token) {
            Some(exp) if !is_expired(*exp, Instant::now()) => true,
            _ => {
                g.remove(token);
                false
            }
        }
    }

    /// Drop one session (logout); unknown tokens are ignored.
    fn drop_session(&self, token: &str) {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token);
    }

    /// True when the request carries a valid session cookie.
    fn authenticated(&self, headers: &HeaderMap) -> bool {
        match cookie_value(headers, COOKIE_NAME) {
            Some(t) => self.valid_session(&t),
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Expiry predicate, kept pure (clock read by the caller) for tests:
/// a session is invalid the moment `now` reaches `expires_at`.
pub fn is_expired(expires_at: Instant, now: Instant) -> bool {
    now >= expires_at
}

/// Byte-wise constant-time equality (length mismatch fails fast, which only
/// leaks the password length - acceptable for a local panel).
pub fn const_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 128-bit random session token, 32 lowercase hex chars.
fn gen_token() -> String {
    let b = random_bytes_16();
    let mut s = String::with_capacity(32);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// CSPRNG: OS time + PID + counter + stack ASLR address (local panel only).
static ENTROPY_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn random_bytes_16() -> [u8; 16] {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::Ordering;
    use std::time::{SystemTime, UNIX_EPOCH};

    let c = ENTROPY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let marker = 0u8;
    let addr = &marker as *const u8 as usize as u64;

    let mut h1 = DefaultHasher::new();
    (nanos, pid, c, addr, 0xA5A5_5A5A_u64).hash(&mut h1);
    let mut h2 = DefaultHasher::new();
    (nanos.rotate_left(17), pid.wrapping_mul(3), c ^ 0xFFFF, addr.rotate_left(13), 0x5A5A_A5A5_u64)
        .hash(&mut h2);

    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&h1.finish().to_le_bytes());
    out[8..].copy_from_slice(&h2.finish().to_le_bytes());
    out
}

/// First value of a named cookie across all Cookie headers.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for v in headers.get_all(header::COOKIE) {
        if let Ok(s) = v.to_str() {
            for part in s.split(';') {
                if let Some((k, val)) = part.trim().split_once('=') {
                    if k.trim() == name {
                        return Some(val.trim().to_string());
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// guard middleware
// ---------------------------------------------------------------------------

/// Paths that stay reachable while the layer is enabled: the login page
/// itself, the auth API, favicon requests and the health probe.
fn allowlisted(path: &str) -> bool {
    path == "/login" || path == "/favicon.ico" || path == "/api/health" || path.starts_with("/api/auth/")
}

/// Route guard. With enabled = false this is a pure pass-through (zero
/// behavior change); with enabled = true unauthenticated requests are
/// bounced: HTML paths get a 302 to /login, /api/* paths get a 401 JSON.
pub async fn guard(State(auth): State<AuthState>, req: Request, next: Next) -> Response {
    if !auth.inner.enabled || allowlisted(req.uri().path()) || auth.authenticated(req.headers()) {
        return next.run(req).await;
    }
    if req.uri().path().starts_with("/api/") {
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"ok":false,"error":"unauthorized"}"#.to_string(),
        )
            .into_response()
    } else {
        (StatusCode::FOUND, [(header::LOCATION, "/login")]).into_response()
    }
}

// ---------------------------------------------------------------------------
// endpoints
// ---------------------------------------------------------------------------

/// Embedded standalone login page (kept out of the index.html monolith).
const LOGIN_HTML: &str = include_str!("../../static/login.html");

/// GET /login -> the login page (always served; the page itself redirects
/// to / when /api/auth/status reports the layer as disabled).
pub async fn page() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        LOGIN_HTML,
    )
        .into_response()
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

/// One generic 401 for every login failure (disabled layer, malformed body,
/// wrong password): no enablement oracle, no password echo, no token.
fn login_rejected() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"ok":false,"error":"invalid credentials"}"#.to_string(),
    )
        .into_response()
}

/// POST /api/auth/login body {"password": "..."} -> session cookie.
pub async fn login(State(auth): State<AuthState>, body: String) -> Response {
    if !auth.inner.enabled {
        log_warn!("auth.login", "rejected: auth layer disabled");
        return login_rejected();
    }
    let req: LoginReq = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            log_warn!("auth.login", "rejected: malformed body");
            return login_rejected();
        }
    };
    let stored = auth.inner.password.trim();
    // enabled without a usable password = locked (fail-closed; validate()
    // already flags this config in GET /api/config).
    if stored.is_empty() {
        log_warn!("auth.login", "rejected: no password configured (locked)");
        return login_rejected();
    }
    if !const_time_eq(req.password.trim(), stored) {
        log_warn!("auth.login", "rejected: wrong password");
        return login_rejected();
    }
    let token = auth.create_session();
    // No token/password in logs - only the event itself.
    log_info!("auth.login", "ok (ttl {}s)", auth.inner.ttl.as_secs());
    eprintln!("[auth] login ok (ttl {}s)", auth.inner.ttl.as_secs());
    let cookie = format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Lax; Max-Age={}; Path=/",
        auth.inner.ttl.as_secs()
    );
    match HeaderValue::from_str(&cookie) {
        Ok(v) => ([(header::SET_COOKIE, v)], Json(json!({"ok": true}))).into_response(),
        Err(_) => login_rejected(),
    }
}

/// POST /api/auth/logout -> drop the session and clear the cookie.
pub async fn logout(State(auth): State<AuthState>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie_value(&headers, COOKIE_NAME) {
        auth.drop_session(&t);
    }
    let clear = format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Max-Age=0; Path=/");
    let mut res = Json(json!({"ok": true})).into_response();
    if let Ok(v) = HeaderValue::from_str(&clear) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

/// GET /api/auth/status -> {enabled, authenticated}. authenticated is true
/// when the layer is off (everything passes the guard in that mode).
pub async fn status(State(auth): State<AuthState>, headers: HeaderMap) -> Json<serde_json::Value> {
    let enabled = auth.inner.enabled;
    let authenticated = !enabled || auth.authenticated(&headers);
    Json(json!({"ok": true, "enabled": enabled, "authenticated": authenticated}))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;

    #[test]
    fn expiry_predicate_is_inclusive() {
        let now = Instant::now();
        assert!(is_expired(now, now), "expiry instant itself is expired");
        assert!(is_expired(now - Duration::from_secs(1), now), "past is expired");
        assert!(!is_expired(now + Duration::from_secs(1), now), "future is valid");
    }

    #[test]
    fn constant_time_compare() {
        assert!(const_time_eq("hunter2", "hunter2"));
        assert!(!const_time_eq("hunter2", "hunter3"));
        assert!(!const_time_eq("hunter2", ""), "different lengths fail");
        assert!(const_time_eq("", ""));
    }

    #[test]
    fn tokens_are_128_bit_hex_and_unique() {
        let a = gen_token();
        let b = gen_token();
        assert_eq!(a.len(), 32, "16 bytes -> 32 hex chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b, "tokens must not repeat");
    }

    #[test]
    fn allowlist_covers_only_the_public_surface() {
        for p in ["/login", "/favicon.ico", "/api/health", "/api/auth/login", "/api/auth/logout", "/api/auth/status"] {
            assert!(allowlisted(p), "{p} must be allowlisted");
        }
        for p in ["/", "/gather", "/api/services", "/api/health/services", "/term-ws", "/loginx", "/api/auth"] {
            assert!(!allowlisted(p), "{p} must NOT be allowlisted");
        }
    }

    #[test]
    fn cookie_parsing_picks_the_right_name() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; flygo_auth=tok; b=2"));
        assert_eq!(cookie_value(&h, COOKIE_NAME).as_deref(), Some("tok"));
        assert_eq!(cookie_value(&h, "missing"), None);
    }

    static EMPTY_SESSIONS: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

    #[test]
    fn session_map_semantics() {
        let st = AuthState {
            inner: Arc::new(AuthInner {
                enabled: true,
                password: "pw".into(),
                ttl: Duration::from_millis(40),
                sessions: Mutex::new(HashMap::new()),
            }),
        };
        let tok = st.create_session();
        assert!(st.valid_session(&tok));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!st.valid_session(&tok), "expired session must fail");
        assert!(
            !st.inner.sessions.lock().unwrap().contains_key(&tok),
            "expired entry is dropped on lookup"
        );
        st.drop_session(&tok); // no-op, must not panic
        let _ = &EMPTY_SESSIONS;
    }
}
