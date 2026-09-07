//! Open Live authentication.
//!
//! Two modes, mirroring how Open Live itself authenticates against Strom:
//!
//! - `direct` — the configured key is used as the bearer token as-is. For a
//!   self-hosted Open Live protected by a static `API_KEY`.
//! - `osc` — the configured key is an OSC Personal Access Token, exchanged for a
//!   short-lived Service Access Token via the OSC token service. Required for an
//!   OSC-hosted instance, where the reverse proxy in front of it rejects a PAT.
//!
//! The SAT is cached and refreshed before expiry. Concurrent callers serialise on
//! the cache mutex, so a burst of reconciles cannot fire a burst of exchanges and
//! run into OSC's per-PAT rate limit.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::debug;

const TOKEN_EXCHANGE_URL: &str = "https://token.svc.prod.osaas.io/servicetoken";
const OPEN_LIVE_SERVICE_ID: &str = "eyevinn-open-live";
/// Refresh this long before expiry, so a token never expires mid-request.
const REFRESH_BUFFER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Deserialize)]
struct ServiceTokenResponse {
    token: String,
    /// Unix seconds.
    expiry: u64,
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: Duration,
}

pub struct Auth {
    inner: Inner,
}

enum Inner {
    /// Static bearer token.
    Direct(String),
    /// OSC PAT exchanged for a short-lived SAT.
    Osc {
        pat: String,
        cache: Mutex<Option<CachedToken>>,
    },
}

impl Auth {
    pub fn new(mode: &str, key: &str) -> Result<Self> {
        let inner = match mode {
            "direct" => Inner::Direct(key.to_string()),
            "osc" => Inner::Osc {
                pat: key.to_string(),
                cache: Mutex::new(None),
            },
            other => {
                bail!("unknown open_live.auth_mode {other:?} (expected \"direct\" or \"osc\")")
            }
        };
        Ok(Self { inner })
    }

    /// The bearer token to present on the next request.
    pub async fn bearer(&self, http: &reqwest::Client) -> Result<String> {
        match &self.inner {
            Inner::Direct(key) => Ok(key.clone()),
            Inner::Osc { pat, cache } => {
                let mut guard = cache.lock().await;
                if let Some(cached) = guard.as_ref() {
                    if !is_expiring(cached) {
                        return Ok(cached.token.clone());
                    }
                }
                let fresh = exchange(http, pat).await?;
                let token = fresh.token.clone();
                *guard = Some(fresh);
                Ok(token)
            }
        }
    }
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn is_expiring(cached: &CachedToken) -> bool {
    now() + REFRESH_BUFFER >= cached.expires_at
}

async fn exchange(http: &reqwest::Client, pat: &str) -> Result<CachedToken> {
    let res = http
        .post(TOKEN_EXCHANGE_URL)
        .header("x-pat-jwt", format!("Bearer {pat}"))
        .header("accept", "application/json")
        .json(&serde_json::json!({ "serviceId": OPEN_LIVE_SERVICE_ID }))
        .send()
        .await
        .context("POST service token")?;

    let status = res.status();
    if !status.is_success() {
        // Never include the body: it can echo the PAT back.
        bail!("OSC token exchange failed with HTTP {status}");
    }

    let body: ServiceTokenResponse = res.json().await.context("decoding service token")?;
    debug!("exchanged PAT for a service access token");

    Ok(CachedToken {
        token: body.token,
        expires_at: Duration::from_secs(body.expiry),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_mode_returns_the_key_unchanged() {
        let auth = Auth::new("direct", "static-key").unwrap();
        let http = reqwest::Client::new();
        let token = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(auth.bearer(&http))
            .unwrap();
        assert_eq!(token, "static-key");
    }

    #[test]
    fn an_unknown_mode_is_rejected_rather_than_silently_defaulted() {
        assert!(Auth::new("oauth", "key").is_err());
    }

    /// A token inside the refresh buffer must be treated as expiring, or a request
    /// can go out holding a token that dies in flight.
    #[test]
    fn a_token_expiring_within_the_buffer_is_refreshed() {
        let almost = CachedToken {
            token: "t".to_string(),
            expires_at: now() + Duration::from_secs(60),
        };
        assert!(is_expiring(&almost));

        let fresh = CachedToken {
            token: "t".to_string(),
            expires_at: now() + Duration::from_secs(3600),
        };
        assert!(!is_expiring(&fresh));
    }
}
