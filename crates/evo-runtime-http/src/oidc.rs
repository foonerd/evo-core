// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! OAuth 2.0 / OpenID Connect verifier for fleet operator SSO.
//!
//! Vendor fleets that front a corporate IdP (Okta, Auth0,
//! Azure AD, Google Workspace, Keycloak, …) configure their
//! devices with an OIDC issuer URL + audience; this module
//! exchanges the issuer's well-known discovery document for
//! the JWKS and then validates every presented JWT bearer
//! token against:
//!
//! - The configured `iss` (issuer) claim.
//! - The configured `aud` (audience) claim — the device's
//!   client identifier registered with the IdP.
//! - The signature, verified against the JWKS key matching
//!   the JWT's `kid` header.
//! - The `exp` (expiry) and `nbf` (not-before) claims.
//!
//! Successfully-validated JWTs surface as a
//! [`OidcPrincipal`] carrying the operator's subject id and
//! the configured group / scope mapping; the runtime's bearer-
//! token middleware composes this with the framework's native
//! token validator so deployments can present either form on
//! the wire.
//!
//! JWKS caching is on by default with a 10-minute refresh
//! interval — short enough to pick up rotated IdP keys, long
//! enough to keep JWKS-fetch traffic off the hot path.

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Default JWKS refresh interval. The verifier serves cached
/// keys for this long before re-fetching from the issuer's
/// `jwks_uri`. 10 minutes balances rotation responsiveness
/// against the load a fleet places on the IdP.
pub const DEFAULT_JWKS_REFRESH: Duration = Duration::from_secs(600);

/// Default request timeout for JWKS / discovery fetches. The
/// IdP is on the open internet; a slow response shouldn't
/// stall the verifier forever.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Construction-time configuration for the OIDC verifier.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// The OIDC issuer URL (no trailing slash, no
    /// `/.well-known/openid-configuration`). The verifier
    /// appends the discovery path itself.
    pub issuer: String,

    /// Required `aud` claim value. Most IdPs use the
    /// device's client-id here; fleet deployments typically
    /// register one client-id per device class.
    pub audience: String,

    /// Optional JWT claim that carries the operator's group
    /// list. When supplied, the verifier maps the claim's
    /// value (string or string list) into the principal's
    /// group set so the framework can derive scopes from it.
    /// Defaults to `"groups"` (the OIDC standard).
    pub group_claim: String,

    /// JWKS refresh interval. Defaults to
    /// [`DEFAULT_JWKS_REFRESH`].
    pub jwks_refresh: Duration,

    /// HTTP fetch timeout for discovery + JWKS. Defaults to
    /// [`DEFAULT_FETCH_TIMEOUT`].
    pub fetch_timeout: Duration,
}

impl OidcConfig {
    /// Construct with sensible defaults except the supplied
    /// issuer + audience.
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            group_claim: "groups".to_string(),
            jwks_refresh: DEFAULT_JWKS_REFRESH,
            fetch_timeout: DEFAULT_FETCH_TIMEOUT,
        }
    }
}

/// Errors raised by [`OidcVerifier`] construction or
/// validation.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// The discovery document URL could not be reached, or
    /// the response was not a valid JSON document with the
    /// expected fields.
    #[error("OIDC discovery fetch failed: {0}")]
    Discovery(String),
    /// The JWKS endpoint could not be reached, or the
    /// response was not a valid JSON document.
    #[error("JWKS fetch failed: {0}")]
    Jwks(String),
    /// The presented JWT did not decode (malformed header,
    /// missing claims, …).
    #[error("JWT decode failed: {0}")]
    JwtDecode(String),
    /// The JWT signature failed verification or a claim
    /// validation (issuer, audience, expiry, …) refused.
    #[error("JWT validation failed: {0}")]
    JwtValidation(String),
    /// The JWT's `kid` header pointed at a key not present in
    /// the JWKS cache (even after a refresh).
    #[error("JWT key id {kid:?} not in JWKS")]
    UnknownKeyId {
        /// The `kid` value the JWT presented.
        kid: String,
    },
    /// The JWT's algorithm header is not in the framework's
    /// allow-list. Refusing unfamiliar algorithms denies the
    /// `alg=none` and `alg=HS256` confusion vectors at the
    /// boundary.
    #[error("JWT algorithm {alg:?} is not in the verifier's allow-list")]
    UnsupportedAlgorithm {
        /// The algorithm string the JWT presented.
        alg: String,
    },
}

/// A successfully-validated OIDC principal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcPrincipal {
    /// Subject id (`sub` claim).
    pub subject: String,
    /// Optional preferred username (`preferred_username` /
    /// `email` / etc.; the verifier falls back to `sub` when
    /// absent).
    pub username: String,
    /// Group membership extracted from the configured
    /// `group_claim`. Empty when the claim is absent.
    pub groups: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

#[derive(Debug, Deserialize)]
struct JwksDocument {
    keys: Vec<JwkRecord>,
}

#[derive(Debug, Deserialize, Clone)]
struct JwkRecord {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    // `alg` and `crv` are part of the JWK schema but the
    // decoding-key construction picks the right primitive
    // from `kty` + components; the dedicated fields are
    // captured for diagnostic logging if a future verifier
    // surface wants them.
    #[serde(default)]
    #[allow(dead_code)]
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    crv: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    sub: String,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// The OIDC verifier. Holds the cached discovery document +
/// JWKS and validates JWTs against them.
pub struct OidcVerifier {
    config: OidcConfig,
    http: reqwest::Client,
    cache: RwLock<CachedKeys>,
}

#[derive(Debug, Default)]
struct CachedKeys {
    jwks_uri: Option<String>,
    keys: HashMap<String, JwkRecord>,
    last_refresh: Option<Instant>,
}

impl OidcVerifier {
    /// Construct the verifier and trigger a first discovery
    /// fetch. The fetch is lazy — failures here surface as
    /// `OidcError::Discovery` and are bounded; the runtime
    /// caller logs them and continues (the OIDC integration
    /// is optional).
    pub async fn new(config: OidcConfig) -> Result<Arc<Self>, OidcError> {
        let http = reqwest::Client::builder()
            .timeout(config.fetch_timeout)
            .build()
            .map_err(|e| OidcError::Discovery(e.to_string()))?;
        let verifier = Arc::new(Self {
            config,
            http,
            cache: RwLock::new(CachedKeys::default()),
        });
        verifier.refresh_jwks().await?;
        Ok(verifier)
    }

    /// Force a fresh discovery + JWKS fetch. Called at boot
    /// and whenever the cached snapshot ages past
    /// `jwks_refresh`.
    pub async fn refresh_jwks(&self) -> Result<(), OidcError> {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        );
        let discovery: DiscoveryDocument = self
            .http
            .get(&discovery_url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| OidcError::Discovery(error_chain_to_string(&e)))?
            .json()
            .await
            .map_err(|e| OidcError::Discovery(error_chain_to_string(&e)))?;
        if discovery.issuer.trim_end_matches('/')
            != self.config.issuer.trim_end_matches('/')
        {
            return Err(OidcError::Discovery(format!(
                "issuer mismatch: discovery reports {:?}, config asks for {:?}",
                discovery.issuer, self.config.issuer
            )));
        }
        let jwks: JwksDocument = self
            .http
            .get(&discovery.jwks_uri)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| OidcError::Jwks(error_chain_to_string(&e)))?
            .json()
            .await
            .map_err(|e| OidcError::Jwks(error_chain_to_string(&e)))?;
        let mut map = HashMap::with_capacity(jwks.keys.len());
        for k in jwks.keys {
            if let Some(kid) = k.kid.clone() {
                map.insert(kid, k);
            }
        }
        let mut guard = self.cache.write().await;
        guard.jwks_uri = Some(discovery.jwks_uri);
        guard.keys = map;
        guard.last_refresh = Some(Instant::now());
        tracing::info!(
            issuer = %self.config.issuer,
            key_count = guard.keys.len(),
            "OIDC JWKS refreshed",
        );
        Ok(())
    }

    /// Validate a presented JWT. Returns the matching
    /// [`OidcPrincipal`] on success.
    pub async fn validate(
        &self,
        jwt: &str,
    ) -> Result<OidcPrincipal, OidcError> {
        // Header parse — gives us the `kid` and `alg`.
        let header = decode_header(jwt)
            .map_err(|e| OidcError::JwtDecode(e.to_string()))?;
        let alg = header.alg;
        if !is_allowed_algorithm(alg) {
            return Err(OidcError::UnsupportedAlgorithm {
                alg: format!("{alg:?}"),
            });
        }
        let kid = header.kid.clone().ok_or_else(|| {
            OidcError::JwtValidation(
                "JWT header missing required `kid` claim".into(),
            )
        })?;

        // Refresh the JWKS if the cache is stale or the kid
        // is unknown. One refresh per validation worst case;
        // the hot path stays in the cache.
        let needs_refresh = {
            let guard = self.cache.read().await;
            let stale = guard
                .last_refresh
                .map(|t| t.elapsed() > self.config.jwks_refresh);
            stale.unwrap_or(true) || !guard.keys.contains_key(&kid)
        };
        if needs_refresh {
            self.refresh_jwks().await?;
        }

        let jwk =
            {
                let guard = self.cache.read().await;
                guard.keys.get(&kid).cloned().ok_or_else(|| {
                    OidcError::UnknownKeyId { kid: kid.clone() }
                })?
            };
        let key = decoding_key_from_jwk(&jwk)?;
        let mut validation = Validation::new(alg);
        validation.set_issuer(&[self.config.issuer.as_str()]);
        validation.set_audience(&[self.config.audience.as_str()]);
        let token_data = decode::<JwtClaims>(jwt, &key, &validation)
            .map_err(|e| OidcError::JwtValidation(e.to_string()))?;

        let claims = token_data.claims;
        let username = claims
            .preferred_username
            .clone()
            .or_else(|| claims.email.clone())
            .unwrap_or_else(|| claims.sub.clone());
        let groups = extract_groups(&claims.extra, &self.config.group_claim);

        Ok(OidcPrincipal {
            subject: claims.sub,
            username,
            groups,
        })
    }
}

/// Walk a `std::error::Error` source chain into a single
/// diagnostic string. reqwest's outer `Display` impl typically
/// reports only `error sending request` even when the
/// underlying problem (DNS failure, TLS rejection, connection
/// refused) is recorded in the source chain.
fn error_chain_to_string(err: &dyn std::error::Error) -> String {
    let mut s = err.to_string();
    let mut cur = err.source();
    while let Some(next) = cur {
        s.push_str(": ");
        s.push_str(&next.to_string());
        cur = next.source();
    }
    s
}

fn is_allowed_algorithm(alg: Algorithm) -> bool {
    matches!(
        alg,
        Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::PS256
            | Algorithm::PS384
            | Algorithm::PS512
            | Algorithm::ES256
            | Algorithm::ES384
            | Algorithm::EdDSA
    )
}

fn decoding_key_from_jwk(jwk: &JwkRecord) -> Result<DecodingKey, OidcError> {
    match jwk.kty.as_str() {
        "RSA" => {
            let n = jwk.n.as_deref().ok_or_else(|| {
                OidcError::JwtValidation("RSA JWK missing `n`".into())
            })?;
            let e = jwk.e.as_deref().ok_or_else(|| {
                OidcError::JwtValidation("RSA JWK missing `e`".into())
            })?;
            DecodingKey::from_rsa_components(n, e)
                .map_err(|e| OidcError::JwtValidation(e.to_string()))
        }
        "EC" => {
            let x = jwk.x.as_deref().ok_or_else(|| {
                OidcError::JwtValidation("EC JWK missing `x`".into())
            })?;
            let y = jwk.y.as_deref().ok_or_else(|| {
                OidcError::JwtValidation("EC JWK missing `y`".into())
            })?;
            DecodingKey::from_ec_components(x, y)
                .map_err(|e| OidcError::JwtValidation(e.to_string()))
        }
        "OKP" => {
            let x = jwk.x.as_deref().ok_or_else(|| {
                OidcError::JwtValidation("OKP JWK missing `x`".into())
            })?;
            DecodingKey::from_ed_components(x)
                .map_err(|e| OidcError::JwtValidation(e.to_string()))
        }
        other => Err(OidcError::JwtValidation(format!(
            "unsupported JWK key type: {other}"
        ))),
    }
}

fn extract_groups(
    extra: &HashMap<String, serde_json::Value>,
    claim: &str,
) -> Vec<String> {
    let Some(value) = extra.get(claim) else {
        return Vec::new();
    };
    match value {
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        serde_json::Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_new_applies_defaults() {
        let cfg =
            OidcConfig::new("https://issuer.example.com", "evo-device-class");
        assert_eq!(cfg.group_claim, "groups");
        assert_eq!(cfg.jwks_refresh, DEFAULT_JWKS_REFRESH);
        assert_eq!(cfg.fetch_timeout, DEFAULT_FETCH_TIMEOUT);
    }

    #[test]
    fn allowed_algorithms_cover_asymmetric_set() {
        for alg in [
            Algorithm::RS256,
            Algorithm::ES256,
            Algorithm::EdDSA,
            Algorithm::PS512,
        ] {
            assert!(is_allowed_algorithm(alg), "{alg:?}");
        }
    }

    #[test]
    fn unsigned_jwt_algorithm_is_refused() {
        // HS256 + the no-op `none` family are not in the
        // allow-list — they are the canonical JWT confusion
        // vectors and the framework refuses them outright.
        assert!(!is_allowed_algorithm(Algorithm::HS256));
        assert!(!is_allowed_algorithm(Algorithm::HS384));
        assert!(!is_allowed_algorithm(Algorithm::HS512));
    }

    #[test]
    fn extract_groups_handles_string_list() {
        let mut m = HashMap::new();
        m.insert(
            "groups".to_string(),
            serde_json::json!(["admins", "operators"]),
        );
        assert_eq!(
            extract_groups(&m, "groups"),
            vec!["admins".to_string(), "operators".to_string()]
        );
    }

    #[test]
    fn extract_groups_handles_single_string() {
        let mut m = HashMap::new();
        m.insert("role".to_string(), serde_json::json!("ops-lead"));
        assert_eq!(extract_groups(&m, "role"), vec!["ops-lead".to_string()]);
    }

    #[test]
    fn extract_groups_absent_yields_empty() {
        let m = HashMap::new();
        assert!(extract_groups(&m, "groups").is_empty());
    }
}
