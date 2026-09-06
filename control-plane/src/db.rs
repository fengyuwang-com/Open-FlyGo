// db.rs - SQLite persistence for the FengDog control plane.
//
// All timestamps are Unix epoch seconds (UTC) stored as INTEGER.
// One connection behind a Mutex is plenty for this MVP workload.
// The control plane only stores metadata (identity + subscription +
// heartbeats); it never touches user data.

use rusqlite::{params, Connection, Row};
use serde_json::{json, Value};
use std::path::Path;

// ---------------------------------------------------------------------------
// Business constants
// ---------------------------------------------------------------------------

/// No heartbeat for this long => the device is considered offline.
pub const ONLINE_WINDOW_SECS: i64 = 15 * 60;
/// Days added to a fresh activation (trial).
pub const TRIAL_DAYS: i64 = 30;
/// Days added by the admin renew action.
pub const RENEW_DAYS: i64 = 365;
/// How many seconds count as "expiring" before the expiry date.
pub const EXPIRING_WINDOW_SECS: i64 = 30 * 24 * 3600;
/// Trial quota: 100 GiB.
pub const TRIAL_QUOTA_BYTES: i64 = 100 * 1024 * 1024 * 1024;
/// Active plan quota: 1 TiB.
pub const ACTIVE_QUOTA_BYTES: i64 = 1024 * 1024 * 1024 * 1024;

pub const PLAN_TRIAL: &str = "trial";
pub const PLAN_ACTIVE: &str = "active";
pub const PLAN_CANCELLED: &str = "cancelled";

// ---------------------------------------------------------------------------
// Standalone activation codes (admin-issued, redeemed at /api/activate)
// ---------------------------------------------------------------------------

/// Code kinds: trial = 30-day trial terms, pro = 1-year active plan.
pub const CODE_KIND_TRIAL: &str = "trial";
pub const CODE_KIND_PRO: &str = "pro";
/// Standalone code lifecycle: issued -> used | revoked.
pub const CODE_ISSUED: &str = "issued";
pub const CODE_USED: &str = "used";
pub const CODE_REVOKED: &str = "revoked";
/// A pro code carries the same terms as an admin renew (1 year, active).
pub const PRO_DAYS: i64 = RENEW_DAYS;

/// Activation codes: 8 chars from an unambiguous alphabet (no 0/O/1/I).
/// Shared by device-bound registration codes and admin-issued standalone
/// codes, so allocation must check both tables for collisions.
pub const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 8;

pub fn gen_activation_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..CODE_LEN)
        .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
        .collect()
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS devices (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  hardware_id       TEXT NOT NULL UNIQUE,
  name              TEXT NOT NULL,
  node_kind         TEXT NOT NULL,
  activation_code   TEXT UNIQUE,
  activated_at      INTEGER,
  expires_at        INTEGER,
  plan              TEXT NOT NULL DEFAULT 'trial',
  quota_bytes       INTEGER NOT NULL DEFAULT 0,
  last_heartbeat_at INTEGER,
  disk_free_bytes   INTEGER,
  version           TEXT,
  created_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_devices_code ON devices (activation_code);
CREATE TABLE IF NOT EXISTS activation_codes (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  code              TEXT NOT NULL UNIQUE,
  kind              TEXT NOT NULL,
  status            TEXT NOT NULL DEFAULT 'issued',
  created_at        INTEGER NOT NULL,
  used_at           INTEGER,
  used_by_device_id INTEGER
);
CREATE INDEX IF NOT EXISTS idx_codes_status ON activation_codes (status);
CREATE TABLE IF NOT EXISTS pairings (
  id                    INTEGER PRIMARY KEY AUTOINCREMENT,
  code                  TEXT NOT NULL UNIQUE,
  inviter_device_id     INTEGER NOT NULL,
  inviter_syncthing_id  TEXT NOT NULL,
  accepter_device_id    INTEGER,
  accepter_syncthing_id TEXT,
  created_at            INTEGER NOT NULL,
  expires_at            INTEGER NOT NULL,
  matched_at            INTEGER,
  status                TEXT NOT NULL DEFAULT 'pending'
);
CREATE INDEX IF NOT EXISTS idx_pairings_inviter ON pairings (inviter_device_id);
";

// ---------------------------------------------------------------------------
// Mutual-backup pairing constants (tech plan 4.2; signaling relay only)
// ---------------------------------------------------------------------------

/// Invite codes live 10 minutes - enough to read a code across the room,
/// short enough that a leaked code is nearly worthless.
pub const PAIR_TTL_SECS: i64 = 600;
/// Invite codes are exactly 6 digits (numeric so they are easy to relay
/// by voice; the 10^6 space with a 10-minute TTL is fine for household use).
pub const PAIR_CODE_LEN: usize = 6;

pub const PAIR_PENDING: &str = "pending";
pub const PAIR_MATCHED: &str = "matched";
pub const PAIR_EXPIRED: &str = "expired";

/// Open (or create) the SQLite database and apply the schema.
pub fn open_db(path: &str) -> Result<Connection, Box<dyn std::error::Error>> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Device model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Device {
    pub id: i64,
    pub hardware_id: String,
    pub name: String,
    pub node_kind: String,
    pub activation_code: Option<String>,
    pub activated_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub plan: String,
    pub quota_bytes: i64,
    pub last_heartbeat_at: Option<i64>,
    pub disk_free_bytes: Option<i64>,
    pub version: Option<String>,
    pub created_at: i64,
}

impl Device {
    /// Minimal subscription state machine:
    /// pending -> trial -> active -> expiring -> expired; cancel is terminal.
    /// The "expiring" marker (30 days before expiry) only applies to the
    /// 1-year active plan; a trial simply expires at its end.
    pub fn subscription_state(&self, now: i64) -> &'static str {
        if self.activated_at.is_none() {
            return "pending";
        }
        if self.plan == PLAN_CANCELLED {
            return "cancelled";
        }
        let exp = self.expires_at.unwrap_or(0);
        if now > exp {
            return "expired";
        }
        if self.plan == PLAN_ACTIVE && exp - now <= EXPIRING_WINDOW_SECS {
            return "expiring";
        }
        if self.plan == PLAN_ACTIVE {
            "active"
        } else {
            "trial"
        }
    }

    /// Online = a heartbeat within ONLINE_WINDOW_SECS.
    pub fn online(&self, now: i64) -> bool {
        match self.last_heartbeat_at {
            Some(t) => now - t <= ONLINE_WINDOW_SECS,
            None => false,
        }
    }

    /// API-facing JSON shape. The activation code is only exposed to the
    /// admin (include_code = true); device-facing endpoints never see it.
    pub fn to_json(&self, now: i64, include_code: bool) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("device_id".into(), json!(self.id));
        m.insert("hardware_id".into(), json!(self.hardware_id));
        m.insert("name".into(), json!(self.name));
        m.insert("node_kind".into(), json!(self.node_kind));
        m.insert("plan".into(), json!(self.plan));
        m.insert(
            "subscription_state".into(),
            json!(self.subscription_state(now)),
        );
        m.insert("activated_at".into(), json!(self.activated_at));
        m.insert("expires_at".into(), json!(self.expires_at));
        m.insert("quota_bytes".into(), json!(self.quota_bytes));
        m.insert("last_heartbeat_at".into(), json!(self.last_heartbeat_at));
        m.insert("disk_free_bytes".into(), json!(self.disk_free_bytes));
        m.insert("online".into(), json!(self.online(now)));
        m.insert("version".into(), json!(self.version));
        m.insert("created_at".into(), json!(self.created_at));
        if include_code {
            m.insert("activation_code".into(), json!(self.activation_code));
        }
        Value::Object(m)
    }
}

fn row_to_device(row: &Row) -> rusqlite::Result<Device> {
    Ok(Device {
        id: row.get("id")?,
        hardware_id: row.get("hardware_id")?,
        name: row.get("name")?,
        node_kind: row.get("node_kind")?,
        activation_code: row.get("activation_code")?,
        activated_at: row.get("activated_at")?,
        expires_at: row.get("expires_at")?,
        plan: row.get("plan")?,
        quota_bytes: row.get("quota_bytes")?,
        last_heartbeat_at: row.get("last_heartbeat_at")?,
        disk_free_bytes: row.get("disk_free_bytes")?,
        version: row.get("version")?,
        created_at: row.get("created_at")?,
    })
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

pub fn find_by_hardware_id(conn: &Connection, hid: &str) -> Option<Device> {
    conn.query_row(
        "SELECT * FROM devices WHERE hardware_id = ?1",
        params![hid],
        row_to_device,
    )
    .ok()
}

pub fn find_by_activation_code(conn: &Connection, code: &str) -> Option<Device> {
    conn.query_row(
        "SELECT * FROM devices WHERE activation_code = ?1",
        params![code],
        row_to_device,
    )
    .ok()
}

pub fn find_by_id(conn: &Connection, id: i64) -> Option<Device> {
    conn.query_row("SELECT * FROM devices WHERE id = ?1", params![id], row_to_device)
        .ok()
}

/// Create a pending device (register step). The caller retries on UNIQUE
/// collisions of the activation code.
pub fn insert_device(
    conn: &Connection,
    hardware_id: &str,
    name: &str,
    node_kind: &str,
    code: &str,
    now: i64,
) -> rusqlite::Result<Device> {
    conn.execute(
        "INSERT INTO devices (hardware_id, name, node_kind, activation_code, plan, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![hardware_id, name, node_kind, code, PLAN_TRIAL, now],
    )?;
    find_by_hardware_id(conn, hardware_id)
        .ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// Activate with explicit terms. Device-bound registration codes use the
/// default trial terms; standalone admin-issued codes use trial or pro.
pub fn activate_plan(
    conn: &Connection,
    id: i64,
    days: i64,
    plan: &str,
    quota: i64,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE devices SET activated_at = ?1, expires_at = ?2,
                           plan = ?3, quota_bytes = ?4 WHERE id = ?5",
        params![now, now + days * 86400, plan, quota, id],
    )
}

/// Record a heartbeat from the node.
pub fn touch_heartbeat(
    conn: &Connection,
    id: i64,
    disk_free: Option<i64>,
    version: Option<&str>,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE devices SET last_heartbeat_at = ?1, disk_free_bytes = ?2,
                           version = ?3 WHERE id = ?4",
        params![now, disk_free, version, id],
    )
}

/// Admin renew: extend expiry from max(now, current expiry).
pub fn renew(
    conn: &Connection,
    id: i64,
    days: i64,
    plan: &str,
    quota: i64,
    now: i64,
) -> rusqlite::Result<usize> {
    let cur: Option<i64> = conn
        .query_row("SELECT expires_at FROM devices WHERE id = ?1", params![id], |r| r.get(0))
        .ok();
    let base = cur.unwrap_or(now).max(now);
    conn.execute(
        "UPDATE devices SET expires_at = ?1, plan = ?2, quota_bytes = ?3 WHERE id = ?4",
        params![base + days * 86400, plan, quota, id],
    )
}

/// Admin cancel: terminate the subscription now (terminal state).
pub fn cancel(conn: &Connection, id: i64, now: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE devices SET plan = ?1, expires_at = ?2 WHERE id = ?3",
        params![PLAN_CANCELLED, now, id],
    )
}

/// All devices, newest first (admin listing).
pub fn list_all(conn: &Connection) -> rusqlite::Result<Vec<Device>> {
    let mut stmt = conn.prepare("SELECT * FROM devices ORDER BY id DESC")?;
    let rows = stmt.query_map([], row_to_device)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Standalone activation codes (admin-issued; redeemed at /api/activate)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ActivationCode {
    pub id: i64,
    pub code: String,
    pub kind: String,
    pub status: String,
    pub created_at: i64,
    pub used_at: Option<i64>,
    pub used_by_device_id: Option<i64>,
}

impl ActivationCode {
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "kind": self.kind,
            "status": self.status,
            "created_at": self.created_at,
            "used_at": self.used_at,
            "used_by_device_id": self.used_by_device_id,
        })
    }
}

fn row_to_code(row: &Row) -> rusqlite::Result<ActivationCode> {
    Ok(ActivationCode {
        id: row.get("id")?,
        code: row.get("code")?,
        kind: row.get("kind")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        used_at: row.get("used_at")?,
        used_by_device_id: row.get("used_by_device_id")?,
    })
}

/// Create an issued code. The caller generates the code and retries on
/// UNIQUE collisions (against both this table and devices.activation_code).
pub fn insert_code(
    conn: &Connection,
    code: &str,
    kind: &str,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO activation_codes (code, kind, status, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![code, kind, CODE_ISSUED, now],
    )
}

pub fn find_code(conn: &Connection, code: &str) -> Option<ActivationCode> {
    conn.query_row(
        "SELECT * FROM activation_codes WHERE code = ?1",
        params![code],
        row_to_code,
    )
    .ok()
}

/// Mark a code as used by a device (single redemption).
pub fn mark_code_used(
    conn: &Connection,
    id: i64,
    device_id: i64,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE activation_codes SET status = ?1, used_at = ?2, used_by_device_id = ?3
         WHERE id = ?4 AND status = ?5",
        params![CODE_USED, now, device_id, id, CODE_ISSUED],
    )
}

/// Void a still-issued code (admin revoke; idempotent for revoked codes).
pub fn revoke_code(conn: &Connection, id: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE activation_codes SET status = ?1 WHERE id = ?2 AND status = ?3",
        params![CODE_REVOKED, id, CODE_ISSUED],
    )
}

/// Latest codes, newest first (admin listing). Capped so the page stays
/// light even after long operation.
pub fn list_codes(conn: &Connection) -> rusqlite::Result<Vec<ActivationCode>> {
    let mut stmt = conn.prepare("SELECT * FROM activation_codes ORDER BY id DESC LIMIT 500")?;
    let rows = stmt.query_map([], row_to_code)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Mutual-backup pairing (signaling relay: invite codes + Syncthing device
// IDs only; the control plane never touches the data plane)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Pairing {
    pub id: i64,
    /// The code is only read at accept time via find_pairing_by_code; kept
    /// on the struct for completeness/debugging (schema mirror).
    #[allow(dead_code)]
    pub code: String,
    pub inviter_device_id: i64,
    pub inviter_syncthing_id: String,
    #[allow(dead_code)]
    pub accepter_device_id: Option<i64>,
    pub accepter_syncthing_id: Option<String>,
    #[allow(dead_code)]
    pub created_at: i64,
    pub expires_at: i64,
    pub matched_at: Option<i64>,
    pub status: String,
}

impl Pairing {
    /// A pending pairing that is past its expiry is logically expired, no
    /// matter what the status column still says (lazy transition).
    pub fn is_expired(&self, now: i64) -> bool {
        self.status == PAIR_PENDING && now > self.expires_at
    }
}

fn row_to_pairing(row: &Row) -> rusqlite::Result<Pairing> {
    Ok(Pairing {
        id: row.get("id")?,
        code: row.get("code")?,
        inviter_device_id: row.get("inviter_device_id")?,
        inviter_syncthing_id: row.get("inviter_syncthing_id")?,
        accepter_device_id: row.get("accepter_device_id")?,
        accepter_syncthing_id: row.get("accepter_syncthing_id")?,
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
        matched_at: row.get("matched_at")?,
        status: row.get("status")?,
    })
}

/// Create a pending pairing. The caller generates the code and retries on
/// UNIQUE collisions.
pub fn insert_pairing(
    conn: &Connection,
    code: &str,
    inviter_device_id: i64,
    inviter_syncthing_id: &str,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO pairings (code, inviter_device_id, inviter_syncthing_id,
                              created_at, expires_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![code, inviter_device_id, inviter_syncthing_id, now, now + PAIR_TTL_SECS, PAIR_PENDING],
    )
}

/// Expire the still-pending invites of a device (a fresh invite supersedes
/// any older one, so at most one code per inviter is ever redeemable).
pub fn supersede_pending(conn: &Connection, inviter_device_id: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE pairings SET status = ?1 WHERE inviter_device_id = ?2 AND status = ?3",
        params![PAIR_EXPIRED, inviter_device_id, PAIR_PENDING],
    )
}

pub fn find_pairing_by_code(conn: &Connection, code: &str) -> Option<Pairing> {
    conn.query_row(
        "SELECT * FROM pairings WHERE code = ?1",
        params![code],
        row_to_pairing,
    )
    .ok()
}

/// The inviter's newest pairing (any status) - what GET /api/pair/poll reports.
pub fn latest_pairing_for_inviter(conn: &Connection, inviter_device_id: i64) -> Option<Pairing> {
    conn.query_row(
        "SELECT * FROM pairings WHERE inviter_device_id = ?1 ORDER BY id DESC LIMIT 1",
        params![inviter_device_id],
        row_to_pairing,
    )
    .ok()
}

/// Accept: record the accepter and flip the pairing to matched.
pub fn match_pairing(
    conn: &Connection,
    id: i64,
    accepter_device_id: i64,
    accepter_syncthing_id: &str,
    now: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE pairings SET accepter_device_id = ?1, accepter_syncthing_id = ?2,
                            matched_at = ?3, status = ?4 WHERE id = ?5",
        params![accepter_device_id, accepter_syncthing_id, now, PAIR_MATCHED, id],
    )
}

/// Persist the lazy expiry transition.
pub fn expire_pairing(conn: &Connection, id: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE pairings SET status = ?1 WHERE id = ?2 AND status = ?3",
        params![PAIR_EXPIRED, id, PAIR_PENDING],
    )
}
