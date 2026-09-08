//! Client for the Open Live `/api/v1/sources` API, and the two ways of
//! authenticating against it.
//!
//! The credential must never reach the logs: errors are built from status codes, and
//! request bodies are never dumped because the source address carries the passphrase.

use crate::config::{mask_passphrase, AuthMode};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::debug;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_EXCHANGE_URL: &str = "https://token.svc.prod.osaas.io/servicetoken";
const OPEN_LIVE_SERVICE_ID: &str = "eyevinn-open-live";
/// Refresh this long before expiry, so a token never dies mid-request.
const REFRESH_BUFFER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SourcePayload {
    pub name: String,
    /// The address the cloud Strom's input block gets.
    pub address: String,
    #[serde(rename = "streamType")]
    pub stream_type: String,
    /// `active` means "safe to assign to a production". A source assigned while its
    /// feed never arrives stops the cloud flow from reaching playing at all, so this
    /// has to mean the uplink is delivering, not merely that the flow runs.
    pub status: String,
    /// SRT receiver latency in ms. Open Live takes the maximum across a production's
    /// sources when generating the flow.
    pub latency: u32,
    #[serde(rename = "liveCamera")]
    pub live_camera: bool,
}

impl SourcePayload {
    pub fn new(name: &str, address: &str, active: bool, latency: u32) -> Self {
        Self {
            name: name.to_string(),
            address: address.to_string(),
            stream_type: "srt".to_string(),
            status: if active { "active" } else { "inactive" }.to_string(),
            latency,
            live_camera: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub id: String,
    pub name: String,
    /// Returned with any passphrase masked, which is why the gateway never reads the
    /// passphrase back and compares addresses masked.
    pub address: String,
    #[serde(rename = "streamType", default)]
    pub stream_type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub latency: Option<u32>,
}

/// Whether a stored source needs a PATCH to match what the gateway wants. Only what
/// differs is written: Open Live keeps a CouchDB revision per write, so a needless
/// PATCH every tick would add thousands of revisions a day per source.
pub fn drifted(stored: &Source, desired: &SourcePayload) -> bool {
    stored.name != desired.name
        || stored.stream_type != desired.stream_type
        || stored.status != desired.status
        // Not reported at all is not drift; an older Open Live simply lacks the field.
        || stored.latency.is_some_and(|l| l != desired.latency)
        || mask_passphrase(&stored.address) != mask_passphrase(&desired.address)
}

pub struct OpenLiveClient {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
}

impl OpenLiveClient {
    pub fn new(base_url: &str, auth_mode: AuthMode, api_key: Option<&str>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .context("building HTTP client")?,
            base_url: base_url.trim_end_matches('/').to_string(),
            auth: Auth::new(auth_mode, api_key.filter(|k| !k.trim().is_empty())),
        })
    }

    /// The cloud Strom's hostname, so a venue needs only the Open Live address.
    /// Older deployments lack the route, so a 404 is "unknown", not an error.
    pub async fn cloud_strom_host(&self) -> Result<Option<String>> {
        let res = self
            .get("/api/v1/server-info")
            .await?
            .send()
            .await
            .context("GET server-info")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: serde_json::Value = ok(res, "GET server-info")?
            .json()
            .await
            .context("decoding server-info")?;
        Ok(body
            .get("stromHost")
            .and_then(|v| v.as_str())
            .filter(|h| !h.is_empty())
            .map(str::to_string))
    }

    pub async fn list_sources(&self) -> Result<Vec<Source>> {
        let res = self
            .get("/api/v1/sources")
            .await?
            .send()
            .await
            .context("GET sources")?;
        ok(res, "GET sources")?
            .json()
            .await
            .context("decoding sources")
    }

    pub async fn create_source(&self, payload: &SourcePayload) -> Result<Source> {
        let res = self
            .auth(self.http.post(format!("{}/api/v1/sources", self.base_url)))
            .await?
            .json(payload)
            .send()
            .await
            .context("POST source")?;
        ok(res, "POST source")?
            .json()
            .await
            .context("decoding created source")
    }

    pub async fn patch_source(&self, id: &str, payload: &SourcePayload) -> Result<()> {
        let res = self
            .auth(
                self.http
                    .patch(format!("{}/api/v1/sources/{id}", self.base_url)),
            )
            .await?
            .json(payload)
            .send()
            .await
            .context("PATCH source")?;
        ok(res, "PATCH source")?;
        Ok(())
    }

    pub async fn delete_source(&self, id: &str) -> Result<()> {
        let res = self
            .auth(
                self.http
                    .delete(format!("{}/api/v1/sources/{id}", self.base_url)),
            )
            .await?
            .send()
            .await
            .context("DELETE source")?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        ok(res, "DELETE source")?;
        Ok(())
    }

    async fn get(&self, path: &str) -> Result<reqwest::RequestBuilder> {
        self.auth(self.http.get(format!("{}{path}", self.base_url)))
            .await
    }

    /// In OSC mode this may perform a token exchange, so it is awaited per request.
    async fn auth(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        Ok(match self.auth.bearer(&self.http).await? {
            Some(token) => req.bearer_auth(token),
            None => req,
        })
    }
}

fn ok(res: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = res.status();
    if !status.is_success() {
        bail!("{what} failed with HTTP {status}");
    }
    Ok(res)
}

/// The bearer token to present: a static key as-is, or an OSC personal access token
/// exchanged for a cached, short-lived service token. Callers serialise on the cache,
/// so a burst of requests cannot fire a burst of exchanges into OSC's rate limit.
enum Auth {
    Direct(Option<String>),
    Osc {
        pat: String,
        cache: Mutex<Option<CachedToken>>,
    },
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: Duration,
}

#[derive(Debug, Deserialize)]
struct ServiceTokenResponse {
    token: String,
    /// Unix seconds.
    expiry: u64,
}

impl Auth {
    fn new(mode: AuthMode, key: Option<&str>) -> Self {
        match mode {
            AuthMode::Direct => Auth::Direct(key.map(str::to_string)),
            AuthMode::Osc => Auth::Osc {
                pat: key.unwrap_or_default().to_string(),
                cache: Mutex::new(None),
            },
        }
    }

    async fn bearer(&self, http: &reqwest::Client) -> Result<Option<String>> {
        match self {
            Auth::Direct(key) => Ok(key.clone()),
            Auth::Osc { pat, cache } => {
                let mut guard = cache.lock().await;
                if let Some(cached) = guard.as_ref().filter(|c| !is_expiring(c)) {
                    return Ok(Some(cached.token.clone()));
                }
                let fresh = exchange(http, pat).await?;
                let token = fresh.token.clone();
                *guard = Some(fresh);
                Ok(Some(token))
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
    // Never include the body: it can echo the token back.
    let body: ServiceTokenResponse = ok(res, "OSC token exchange")?
        .json()
        .await
        .context("decoding service token")?;
    debug!("exchanged the OSC token for a service access token");
    Ok(CachedToken {
        token: body.token,
        expires_at: Duration::from_secs(body.expiry),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bearer_of(auth: &Auth) -> Option<String> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(auth.bearer(&reqwest::Client::new()))
            .unwrap()
    }

    #[test]
    fn direct_mode_sends_the_key_as_is_or_nothing() {
        assert_eq!(
            bearer_of(&Auth::new(AuthMode::Direct, Some("k"))).as_deref(),
            Some("k")
        );
        assert_eq!(bearer_of(&Auth::new(AuthMode::Direct, None)), None);
    }

    /// A token inside the refresh buffer must count as expiring, or a request can go
    /// out holding a token that dies in flight.
    #[test]
    fn a_token_expiring_within_the_buffer_is_refreshed() {
        let soon = CachedToken {
            token: "t".into(),
            expires_at: now() + Duration::from_secs(60),
        };
        assert!(is_expiring(&soon));
        let fresh = CachedToken {
            token: "t".into(),
            expires_at: now() + Duration::from_secs(3600),
        };
        assert!(!is_expiring(&fresh));
    }

    fn stored(address: &str, status: &str, latency: Option<u32>) -> Source {
        Source {
            id: "src-1".into(),
            name: "Venue — cam".into(),
            address: address.into(),
            stream_type: "srt".into(),
            status: status.into(),
            latency,
        }
    }

    #[test]
    fn an_identical_source_does_not_drift_but_status_port_and_latency_do() {
        let want = SourcePayload::new("Venue — cam", "srt://:9000?mode=listener", true, 200);
        assert!(!drifted(
            &stored("srt://:9000?mode=listener", "active", Some(200)),
            &want
        ));
        assert!(drifted(
            &stored("srt://:9000?mode=listener", "inactive", Some(200)),
            &want
        ));
        assert!(drifted(
            &stored("srt://:9001?mode=listener", "active", Some(200)),
            &want
        ));
        assert!(drifted(
            &stored("srt://:9000?mode=listener", "active", Some(400)),
            &want
        ));
    }

    /// Open Live masks the passphrase on read; comparing raw would PATCH forever. An
    /// Open Live that does not report latency at all would do the same.
    #[test]
    fn masked_passphrases_and_missing_latency_are_not_drift() {
        let want = SourcePayload::new(
            "Venue — cam",
            "srt://:9000?mode=listener&passphrase=s3cret",
            true,
            200,
        );
        assert!(!drifted(
            &stored(
                "srt://:9000?mode=listener&passphrase=***",
                "active",
                Some(200)
            ),
            &want
        ));
        assert!(!drifted(
            &stored("srt://:9000?mode=listener&passphrase=***", "active", None),
            &want
        ));
        assert!(drifted(
            &stored(
                "srt://:9002?mode=listener&passphrase=***",
                "active",
                Some(200)
            ),
            &want
        ));
    }
}
