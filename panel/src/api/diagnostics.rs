// GET /api/health/diagnostics - one-click "deployment drift" self check.
//
// Answers, from the browser with a single GET:
//   - is the panel alive and are the backend channels answering?
//   - which panel version is the running exe serving?
//   - where is the running exe, and when was it last written?
//   - does the served UI still match the embedded source (md5 of the
//     include_str! bundle)?
//
// The embedded HTML is included at compile time (include_str!, same source
// as main.rs's INDEX_HTML), so the running binary can never show the raw
// source file on disk. Instead this reports the md5 of the exact bytes the
// running exe embeds - if the on-disk bundle differs, a freshly built exe
// will report a different hash, which is the drift signal.
//
// Endpoint probes call the real handler functions in-process (bypassing the
// HTTP/auth layer on purpose) and time them, so the numbers reflect the
// actual backend channels without depending on a browser session.
//
// All code comments are pure ASCII.

use crate::ApiResult;
use axum::Json;
use serde::Serialize;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The embedded UI bundle served by GET /. Must match main.rs INDEX_HTML
/// (same file, same include_str!), but each module carries its own copy so
/// the diagnostics payload is self-contained. A mismatch here vs. the on-disk
/// file is exactly the drift this endpoint surfaces.
const INDEX_HTML: &str = include_str!("../../static/index.html");

#[derive(Serialize)]
pub struct EndpointProbe {
    pub status: u16,
    pub ok: bool,
    pub latency_ms: u64,
}

#[derive(Serialize)]
pub struct Diagnostics {
    pub panel_version: String,
    pub exe_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_mtime_unix: Option<u64>,
    pub embedded_html_md5: String,
    pub endpoints: Vec<EndpointProbe>,
    pub ok: bool,
    pub timestamp: u64,
}

/// GET /api/health/diagnostics
pub async fn diagnostics() -> ApiResult<Json<Diagnostics>> {
    let panel_version = read_version();
    let (exe_path, exe_mtime_unix) = exe_info();
    let embedded_html_md5 = md5_hex(INDEX_HTML.as_bytes());

    // Probe the key backend channels in-process and time each one. Each probe
    // calls the real handler and reports (status, ok, latency).
    let mut endpoints = Vec::new();
    endpoints.push(time_probe(api_system()).await);
    endpoints.push(time_probe(api_health()).await);
    endpoints.push(time_probe(api_services()).await);

    let ok = panel_version != "unknown"
        && !embedded_html_md5.is_empty()
        && !exe_path.is_empty()
        && endpoints.iter().all(|e| e.ok);

    let d = Diagnostics {
        panel_version,
        exe_path,
        exe_mtime_unix,
        embedded_html_md5,
        endpoints,
        ok,
        timestamp: now_secs(),
    };
    Ok(Json(d))
}

/// Time one endpoint call and normalize (Result<u16 marker>) to a probe.
async fn time_probe(call: impl std::future::Future<Output = Result<u16, u16>>) -> EndpointProbe {
    let start = Instant::now();
    let r = call.await;
    let ms = start.elapsed().as_millis() as u64;
    match r {
        Ok(status) => EndpointProbe { status, ok: true, latency_ms: ms },
        Err(e) => EndpointProbe { status: e, ok: false, latency_ms: ms },
    }
}

async fn api_system() -> Result<u16, u16> {
    match crate::api::system::system().await {
        Ok(_) => Ok(200),
        Err(_) => Err(500),
    }
}

async fn api_health() -> Result<u16, u16> {
    match crate::api::health::health().await {
        Ok(_) => Ok(200),
        Err(_) => Err(500),
    }
}

async fn api_services() -> Result<u16, u16> {
    match crate::api::health::services_health().await {
        Ok(_) => Ok(200),
        Err(_) => Err(500),
    }
}

/// Panel version from the install-root Version file (<root>/Version);
/// "unknown" when unreadable. Mirrors the read_version() used by the
/// heartbeat (api/control.rs), kept as a tiny local copy to avoid coupling.
fn read_version() -> String {
    let path = crate::config::get().root.join("Version");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Running executable path plus its filesystem mtime (approximate build time,
/// in UTC epoch seconds). mtime is None when the metadata is unreadable.
fn exe_info() -> (String, Option<u64>) {
    let path = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mtime = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    (path, mtime)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// minimal MD5 (RFC 1321) - pure Rust, no external dependency
// ---------------------------------------------------------------------------

const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a,
    0xa8304613, 0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
    0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340,
    0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8,
    0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
    0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
    0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92,
    0xffeff47d, 0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
    0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

fn md5_hex(input: &[u8]) -> String {
    // Message length in bits, mod 2^64.
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f
                .wrapping_add(a)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = Vec::with_capacity(16);
    for w in [a0, b0, c0, d0] {
        out.extend_from_slice(&w.to_le_bytes());
    }
    let mut hex = String::with_capacity(32);
    for b in out {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn embedded_html_present_and_hashes() {
        // Sanity: the bundle must exist (non-empty) and produce a stable hash
        // across builds of the same source bytes.
        assert!(!INDEX_HTML.is_empty());
        let h1 = md5_hex(INDEX_HTML.as_bytes());
        let h2 = md5_hex(INDEX_HTML.as_bytes());
        assert_eq!(h1.len(), 32);
        assert_eq!(h1, h2);
    }
}
