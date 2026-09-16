// FengDog control plane - identity + subscription + heartbeat backend.
//
// Rust + axum + SQLite, single binary. The control plane only tracks
// metadata (devices, activation codes, subscriptions, heartbeats); it
// never touches the data plane (files flow peer-to-peer via
// Tailscale/Syncthing).
//
// Configuration (environment variables):
//   FENGDOG_SECRET      JWT signing secret, REQUIRED (>= 16 chars).
//                       The binary refuses to start without it - no
//                       hardcoded keys anywhere.
//   FENGDOG_ADMIN_TOKEN bootstrap admin token (optional; without it the
//                       admin endpoints are disabled).
//   FENGDOG_DB          SQLite file path (default ./control-plane.db)
//   FENGDOG_BIND        bind address (default 127.0.0.1).
//                       Use 0.0.0.0 only behind a firewall / reverse
//                       proxy - this service is not TLS-terminating.
//   FENGDOG_PORT        port (default 9001); --port CLI flag wins.
//
// All code comments are pure ASCII; HTML/JSON payloads are UTF-8.

mod api;
mod auth;
mod db;
mod limit;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// Shared application state (cloned per request).
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub auth: auth::Auth,
    pub admin_token: Option<String>,
    pub limiter: limit::RateLimiter,
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            auth: self.auth.clone(),
            admin_token: self.admin_token.clone(),
            limiter: self.limiter.clone(),
        }
    }
}

impl AppState {
    /// Current Unix epoch seconds (UTC) - single clock source for the app.
    pub fn now(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // -- Security gate: the JWT secret must come from the environment. --
    let secret = std::env::var("FENGDOG_SECRET").unwrap_or_default();
    if secret.len() < 16 {
        eprintln!(
            "FATAL: FENGDOG_SECRET environment variable is required (>= 16 chars).\n\
             This binary never hardcodes keys - refusing to start without it."
        );
        std::process::exit(1);
    }

    let admin_token = std::env::var("FENGDOG_ADMIN_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    if admin_token.is_none() {
        tracing::warn!("FENGDOG_ADMIN_TOKEN not set - admin endpoints are disabled");
    }

    let db_path = std::env::var("FENGDOG_DB").unwrap_or_else(|_| "control-plane.db".to_string());
    let bind: std::net::IpAddr = std::env::var("FENGDOG_BIND")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let port = parse_port();

    let conn = match db::open_db(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("FATAL: cannot open database at {db_path}: {e}");
            std::process::exit(1);
        }
    };

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        auth: auth::Auth::new(&secret),
        admin_token,
        limiter: limit::RateLimiter::new(),
    };
    let app = api::build_app(state);

    let addr = SocketAddr::new(bind, port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("FATAL: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(
        "FengDog control plane listening on http://{addr} (db: {db_path})"
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("server error");
}

/// Port resolution: --port CLI flag > FENGDOG_PORT env > 9001.
fn parse_port() -> u16 {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--port" {
            if let Some(v) = args.next() {
                if let Ok(p) = v.parse::<u16>() {
                    return p;
                }
            }
        }
    }
    std::env::var("FENGDOG_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9001)
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutdown signal received, stopping");
}
