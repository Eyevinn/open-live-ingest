//! Open Source Cloud, the platform side: the OSC CLI's login, what a token says about
//! itself, and the list of Open Live instances a workspace owns.
//!
//! `npx @osaas/cli login` signs in through the browser and saves an access token to
//! `~/.osc/token`, mode 0600. That token is exchanged for a service token exactly like
//! a personal access token, so the gateway reads it from there and never has to hold
//! a secret in its own settings. It is read again at each exchange: a fresh login in
//! another session takes effect without restarting a running gateway. A login lasts
//! an hour, though, so it suits setup; a show is carried by a personal access token.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What an operator types to sign in, or to sign in again once the token has expired.
pub const LOGIN_COMMAND: &str = "npx @osaas/cli login";

/// The OSC CLI's own environment variable, which it prefers over its saved token.
pub const ENV_TOKEN: &str = "OSC_ACCESS_TOKEN";

const CATALOG_URL: &str = "https://catalog.svc.prod.osaas.io/mysubscriptions";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Where `login` saves the production token.
pub fn token_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".osc").join("token"))
}

/// The token in the OSC CLI's environment variable, if one is set.
pub fn env_token() -> Option<String> {
    std::env::var(ENV_TOKEN)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// The token the OSC CLI's `login` saved, if the operator has logged in here.
pub fn login_token() -> Option<String> {
    let path = token_path()?;
    token_from(None, &path)
}

/// The token the OSC CLI would use itself: its environment variable first, then the
/// saved login. None when neither exists.
pub fn saved_token() -> Option<String> {
    let from_env = std::env::var(ENV_TOKEN).ok();
    let path = token_path()?;
    token_from(from_env.as_deref(), &path)
}

fn token_from(env: Option<&str>, path: &Path) -> Option<String> {
    if let Some(token) = env.map(str::trim).filter(|t| !t.is_empty()) {
        return Some(token.to_string());
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let token = raw.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Where the login is, for a prompt or a message. Never the token itself.
pub fn describe_location() -> String {
    if env_token().is_some() {
        return ENV_TOKEN.to_string();
    }
    token_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.osc/token".to_string())
}

/// Runs the OSC CLI's login on the operator's terminal. It opens a browser on this
/// machine, where the operator also picks the workspace, and waits for the redirect,
/// so it works on a laptop and not over a bare SSH session.
pub async fn login() -> Result<()> {
    let status = tokio::task::spawn_blocking(|| {
        std::process::Command::new("npx")
            .args(["--yes", "@osaas/cli", "login"])
            .status()
    })
    .await
    .context("login task")?;
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => bail!("`{LOGIN_COMMAND}` exited with {status}"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("npx was not found on PATH; the OSC CLI needs Node.js")
        }
        Err(err) => Err(err).context("running the OSC CLI"),
    }
}

/// What a token says about itself. Both a console personal access token and a CLI
/// login are JWTs bound to one workspace, and only the login carries an expiry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TokenInfo {
    pub workspace: Option<String>,
    /// Unix seconds.
    pub expires_at: Option<u64>,
}

impl TokenInfo {
    /// How long the token has left, or None for one that never expires.
    pub fn remaining(&self) -> Option<Duration> {
        let exp = Duration::from_secs(self.expires_at?);
        Some(exp.saturating_sub(now()))
    }

    pub fn is_expired(&self) -> bool {
        self.remaining().is_some_and(|left| left.is_zero())
    }
}

/// Reads the claims without verifying anything: this is for telling an operator
/// which workspace they are in, not for trusting the token. Garbage gives None.
pub fn inspect(token: &str) -> Option<TokenInfo> {
    #[derive(Deserialize)]
    struct Claims {
        #[serde(rename = "tenantId")]
        tenant_id: Option<String>,
        exp: Option<u64>,
    }
    let payload = token.split('.').nth(1)?;
    let claims: Claims = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    Some(TokenInfo {
        workspace: claims.tenant_id,
        expires_at: claims.exp,
    })
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for c in input.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

pub fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// An Open Live instance as the platform lists it. The record also carries the
/// instance's own secrets, so only the name and address are ever decoded.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Instance {
    pub name: String,
    pub url: String,
}

/// The Open Live instances in the token's workspace, the same way `osc list` finds
/// them: the catalog says where the service's API is, and a service token lists it.
pub async fn open_live_instances(token: &str) -> Result<Vec<Instance>> {
    #[derive(Deserialize)]
    struct Subscription {
        #[serde(rename = "serviceId")]
        service_id: String,
        #[serde(rename = "apiUrl")]
        api_url: String,
    }
    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building HTTP client")?;
    let res = http
        .get(CATALOG_URL)
        .header("x-pat-jwt", format!("Bearer {token}"))
        .send()
        .await
        .context("GET subscriptions")?;
    // Errors carry the status only: a body could echo the token.
    if !res.status().is_success() {
        bail!("listing subscriptions failed with HTTP {}", res.status());
    }
    let subscriptions: Vec<Subscription> = res.json().await.context("decoding subscriptions")?;
    let api_url = subscriptions
        .into_iter()
        .find(|s| s.service_id == crate::openlive::OPEN_LIVE_SERVICE_ID)
        .map(|s| s.api_url)
        .context("this workspace has no Open Live subscription")?;

    let service_token = crate::openlive::service_token(&http, token).await?;
    let res = http
        .get(&api_url)
        .header("x-jwt", format!("Bearer {service_token}"))
        .send()
        .await
        .context("GET instances")?;
    if !res.status().is_success() {
        bail!(
            "listing Open Live instances failed with HTTP {}",
            res.status()
        );
    }
    res.json().await.context("decoding instances")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_token(name: &str, content: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("olg-osc-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        if let Some(content) = content {
            std::fs::write(&path, content).unwrap();
        }
        path
    }

    /// The CLI itself prefers its environment variable over the saved login, and a
    /// gateway reading the same store has to agree with it.
    #[test]
    fn the_environment_variable_wins_over_the_saved_login() {
        let path = temp_token("env-wins", Some("from-file"));
        assert_eq!(
            token_from(Some("from-env"), &path).as_deref(),
            Some("from-env")
        );
        assert_eq!(
            token_from(Some("  "), &path).as_deref(),
            Some("from-file"),
            "a blank variable is no credential"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A token copied over by hand tends to carry a trailing newline, and the CLI
    /// trims its own file, so a byte-exact read would present a token that never works.
    #[test]
    fn the_saved_login_is_trimmed_and_an_empty_one_is_none() {
        let path = temp_token("trimmed", Some("  the-token\n"));
        assert_eq!(token_from(None, &path).as_deref(), Some("the-token"));
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(token_from(None, &path), None);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn no_login_is_none_rather_than_an_error() {
        let path = temp_token("missing", None);
        assert_eq!(token_from(None, &path), None);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// The operator picks the workspace in the browser, so setup has to be able to
    /// read back which one the token landed in, and whether it is a one-hour login.
    #[test]
    fn a_tokens_workspace_and_expiry_are_read_from_its_claims() {
        // {"tenantId":"ludde","exp":1788956652,"iat":1788953052}
        let login = "h.eyJ0ZW5hbnRJZCI6Imx1ZGRlIiwiZXhwIjoxNzg4OTU2NjUyLCJpYXQiOjE3ODg5NTMwNTJ9.s";
        assert_eq!(
            inspect(login),
            Some(TokenInfo {
                workspace: Some("ludde".into()),
                expires_at: Some(1788956652),
            })
        );
        // {"tenantId":"eyevinnlab","patId":"x"}: a console token, which never expires.
        let pat = "h.eyJ0ZW5hbnRJZCI6ImV5ZXZpbm5sYWIiLCJwYXRJZCI6IngifQ.s";
        let info = inspect(pat).unwrap();
        assert_eq!(info.workspace.as_deref(), Some("eyevinnlab"));
        assert_eq!(info.remaining(), None);
        assert!(!info.is_expired());
        assert_eq!(inspect("not a jwt"), None);
        assert_eq!(inspect("a.b.c"), None);
    }

    #[test]
    fn an_old_login_counts_as_expired() {
        let old = TokenInfo {
            workspace: None,
            expires_at: Some(1_000_000),
        };
        assert!(old.is_expired());
        let fresh = TokenInfo {
            workspace: None,
            expires_at: Some(now().as_secs() + 3600),
        };
        assert!(!fresh.is_expired());
    }

    /// The platform's instance record carries the instance's Strom token. Decoding
    /// only the two fields setup needs is what keeps it out of memory and messages.
    #[test]
    fn an_instance_record_yields_only_name_and_url() {
        let raw = r#"[{"name":"venue","url":"https://x.osaas.io","StromUrl":"s","StromAccessToken":"secret","_links":{}}]"#;
        let list: Vec<Instance> = serde_json::from_str(raw).unwrap();
        assert_eq!(
            list,
            vec![Instance {
                name: "venue".into(),
                url: "https://x.osaas.io".into(),
            }]
        );
        assert!(!format!("{list:?}").contains("secret"));
    }
}
