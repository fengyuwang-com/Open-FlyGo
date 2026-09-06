// auth.rs - JWT (HS256) issuing, verification and request extractors.
//
// The signing secret comes from the FENGDOG_SECRET environment variable
// (required at startup). Nothing is hardcoded in this binary.

use crate::api::{ApiError, ApiResult};
use crate::AppState;
use axum::async_trait;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

pub const ROLE_DEVICE: &str = "device";
pub const ROLE_ADMIN: &str = "admin";

/// Node tokens live one week; nodes re-activate (or re-register) afterwards.
const DEVICE_TOKEN_TTL_SECS: i64 = 7 * 24 * 3600;
/// Admin session tokens live 12 hours.
const ADMIN_TOKEN_TTL_SECS: i64 = 12 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,  // device id as string, or "admin"
    pub hid: String,  // hardware id (device tokens only)
    pub role: String, // ROLE_DEVICE | ROLE_ADMIN
    pub iat: i64,
    pub exp: i64,
}

/// HS256 signer/verifier. Clone rebuilds the keys from the same secret.
#[derive(Clone)]
pub struct Auth {
    secret: String,
}

impl Auth {
    pub fn new(secret: &str) -> Self {
        Self {
            secret: secret.to_string(),
        }
    }

    pub fn issue_device_token(&self, device_id: i64, hardware_id: &str, now: i64) -> ApiResult<String> {
        let claims = Claims {
            sub: device_id.to_string(),
            hid: hardware_id.to_string(),
            role: ROLE_DEVICE.to_string(),
            iat: now,
            exp: now + DEVICE_TOKEN_TTL_SECS,
        };
        self.encode(&claims)
    }

    pub fn issue_admin_token(&self, now: i64) -> ApiResult<String> {
        let claims = Claims {
            sub: "admin".to_string(),
            hid: String::new(),
            role: ROLE_ADMIN.to_string(),
            iat: now,
            exp: now + ADMIN_TOKEN_TTL_SECS,
        };
        self.encode(&claims)
    }

    /// Verify signature + expiry. Any failure is reported as 401.
    pub fn verify(&self, token: &str) -> Result<Claims, ApiError> {
        let key = DecodingKey::from_secret(self.secret.as_bytes());
        decode::<Claims>(token, &key, &Validation::new(Algorithm::HS256))
            .map(|d| d.claims)
            .map_err(|_| ApiError::unauthorized("invalid or expired token"))
    }

    fn encode(&self, claims: &Claims) -> ApiResult<String> {
        let key = EncodingKey::from_secret(self.secret.as_bytes());
        encode(&Header::default(), claims, &key)
            .map_err(|e| ApiError::internal(format!("jwt encode failed: {e}")))
    }
}

/// Constant-time string compare for the admin bootstrap token.
pub fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer_from_parts(parts: &Parts) -> Result<String, ApiError> {
    let header = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or_else(|| ApiError::unauthorized("missing authorization header"))?;
    let raw = header
        .to_str()
        .map_err(|_| ApiError::unauthorized("invalid authorization header"))?;
    let token = raw
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::unauthorized("expected 'Bearer <token>'"))?;
    if token.is_empty() {
        return Err(ApiError::unauthorized("empty token"));
    }
    Ok(token.to_string())
}

/// Device-scoped auth: a valid JWT with role = device.
pub struct AuthDevice(pub Claims);

#[async_trait]
impl FromRequestParts<AppState> for AuthDevice {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_from_parts(parts)?;
        let claims = state.auth.verify(&token)?;
        if claims.role != ROLE_DEVICE {
            return Err(ApiError::forbidden("token is not a device token"));
        }
        Ok(AuthDevice(claims))
    }
}

/// Admin-scoped auth: either a valid JWT with role = admin, or the raw
/// FENGDOG_ADMIN_TOKEN bootstrap token presented as a Bearer token.
pub struct AuthAdmin(pub Claims);
// The claims payload is kept for future admin audit trails.
#[allow(dead_code)]
impl AuthAdmin {
    pub fn claims(&self) -> &Claims {
        &self.0
    }
}

#[async_trait]
impl FromRequestParts<AppState> for AuthAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_from_parts(parts)?;
        if let Ok(claims) = state.auth.verify(&token) {
            if claims.role == ROLE_ADMIN {
                return Ok(AuthAdmin(claims));
            }
            // Valid JWT but wrong role: authenticated, not authorized.
            return Err(ApiError::forbidden("admin role required"));
        }
        // Not a valid JWT: maybe the raw bootstrap token was passed directly.
        if let Some(expected) = &state.admin_token {
            if ct_eq(&token, expected) {
                let now = state.now();
                return Ok(AuthAdmin(Claims {
                    sub: "admin".to_string(),
                    hid: String::new(),
                    role: ROLE_ADMIN.to_string(),
                    iat: now,
                    exp: now + ADMIN_TOKEN_TTL_SECS,
                }));
            }
        }
        Err(ApiError::unauthorized("admin authentication failed"))
    }
}
