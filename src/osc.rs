//! Open Source Cloud, the platform side: signing in, what a token says about itself,
//! and the list of Open Live instances a workspace owns.
//!
//! There are two ways to sign in through the browser. [`browser_login`] does it
//! natively, for an app: OAuth 2.0 authorization code with PKCE (S256) and a loopback
//! redirect to `127.0.0.1` (RFC 8252), against OSC's `/api/connect` endpoints, and
//! keeps the refresh token. See docs/research/OSC_BROWSER_SIGN_IN.md for what OSC
//! offers. The resulting [`Credential`] is handed to the caller; `credential.rs` keeps
//! it in the OS keychain and [`refresh`]es it during a session.
//!
//! The command line still has `npx @osaas/cli login`, which signs in the same way and
//! saves only the access token to `~/.osc/token`, mode 0600. That token is exchanged
//! for a service token exactly like a personal access token, so the gateway reads it
//! from there and never has to hold a secret in its own settings. It is read again at
//! each exchange: a fresh login in another session takes effect without restarting a
//! running gateway. A login lasts an hour, though, so it suits setup; a show is
//! carried by a personal access token.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

/// What an operator types to sign in, or to sign in again once the token has expired.
pub const LOGIN_COMMAND: &str = "npx @osaas/cli login";

/// The OSC CLI's own environment variable, which it prefers over its saved token.
pub const ENV_TOKEN: &str = "OSC_ACCESS_TOKEN";

const CATALOG_URL: &str = "https://catalog.svc.prod.osaas.io/mysubscriptions";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// OSC's OAuth 2.0 server: `/api/connect/{register,authorize,token}` below it.
pub const CONNECT_URL: &str = "https://app.osaas.io";
/// The OSC CLI's public client id. Any loopback redirect, no secret. Used when a
/// client of our own cannot be registered; only the name on the consent page differs.
const FALLBACK_CLIENT_ID: &str = "cli";
/// What the OSC consent page shows for a client this gateway registers.
const CLIENT_NAME: &str = "Open Live Ingest";
/// Loopback redirects match on host and path, not port, so one registration covers
/// any port the listener gets.
const REGISTERED_REDIRECT_URI: &str = "http://127.0.0.1/callback";
/// How long the person has to finish signing in in the browser.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long one connection to the loopback listener may take to send its request.
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CALLBACK_MAX_HEAD: usize = 16 * 1024;

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

/// What a browser sign-in yields. Not stored here: keeping it is the caller's job.
///
/// `Debug` shows neither token, so a credential can sit in a struct that is logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// Bound to the workspace picked on the consent page; [`inspect`] reads which.
    pub access_token: String,
    /// Renews the access token (`grant_type=refresh_token`) without a new sign-in.
    pub refresh_token: Option<String>,
    /// When the access token expires, Unix seconds.
    pub expires_at: Option<u64>,
    /// The client the sign-in went through: a registered one, or `cli`.
    pub client_id: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("client_id", &self.client_id)
            .finish()
    }
}

/// Signs in to Open Source Cloud in the system browser, with no Node or OSC CLI.
/// The person signs in and picks the workspace there; the browser comes back to a
/// listener on `127.0.0.1` and the code is exchanged with the PKCE verifier. Fails
/// if the browser cannot be opened, the person declines, or nothing comes back
/// within five minutes.
pub async fn browser_login() -> Result<Credential> {
    browser_login_with(open_browser).await
}

/// [`browser_login`], with the caller opening the sign-in page: an app can show the
/// address as well, for when no browser opens.
pub async fn browser_login_with(open: impl FnOnce(&str) -> Result<()>) -> Result<Credential> {
    sign_in(CONNECT_URL, SIGN_IN_TIMEOUT, open).await
}

/// [`browser_login_with`], against the OAuth server at `base` (production's is
/// [`CONNECT_URL`]), waiting at most `wait` for the browser to come back. Returns
/// once the code is exchanged, so a caller that shows the page itself (the iPhone
/// app's authentication sheet) knows when to close it.
pub async fn sign_in(
    base: &str,
    wait: Duration,
    open: impl FnOnce(&str) -> Result<()>,
) -> Result<Credential> {
    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building HTTP client")?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("listening on 127.0.0.1 for the sign-in to come back")?;
    let port = listener.local_addr().context("loopback address")?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let client_id = match register_client(&http, base).await {
        Ok(id) => id,
        Err(err) => {
            debug!("signing in as the OSC CLI client: {err:#}");
            FALLBACK_CLIENT_ID.to_string()
        }
    };
    let verifier = random_string(32)?;
    let state = random_string(16)?;
    let url = authorize_url(
        base,
        &client_id,
        &redirect_uri,
        &code_challenge(&verifier),
        &state,
    )?;
    open(url.as_str()).context("opening the system browser to sign in")?;

    let code = tokio::time::timeout(wait, wait_for_code(&listener, &state))
        .await
        .map_err(|_| {
            anyhow!(
                "the Open Source Cloud sign-in did not finish within {} s",
                wait.as_secs()
            )
        })??;
    exchange_code(&http, base, &client_id, &code, &redirect_uri, &verifier).await
}

/// Registers a public client (RFC 7591), so the consent page names this gateway
/// rather than the OSC CLI.
async fn register_client(http: &reqwest::Client, base: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Registered {
        client_id: String,
    }
    let res = http
        .post(format!("{base}/api/connect/register"))
        .json(&serde_json::json!({
            "client_name": CLIENT_NAME,
            "redirect_uris": [REGISTERED_REDIRECT_URI],
        }))
        .send()
        .await
        .context("POST client registration")?;
    if !res.status().is_success() {
        bail!("client registration failed with HTTP {}", res.status());
    }
    let registered: Registered = res.json().await.context("decoding client registration")?;
    if registered.client_id.is_empty() {
        bail!("client registration returned no client_id");
    }
    Ok(registered.client_id)
}

fn authorize_url(
    base: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<reqwest::Url> {
    reqwest::Url::parse_with_params(
        &format!("{base}/api/connect/authorize"),
        [
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
        ],
    )
    .context("building the sign-in address")
}

/// Answers the browser on the loopback listener until it brings back a code or a
/// refusal for this sign-in. Anything else (a favicon request, a stale tab with
/// another state) gets an error page and the wait goes on. Connections are served
/// side by side, so a browser's idle preconnect does not hold up the real one.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String>>(1);
    // Dropped on return, which aborts any connection still being served.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting the sign-in redirect")?;
                let (tx, state) = (tx.clone(), state.to_string());
                connections.spawn(async move {
                    if let Some(outcome) = serve_callback(stream, &state).await {
                        tx.send(outcome).await.ok();
                    }
                });
            }
            Some(outcome) = rx.recv() => return outcome,
        }
    }
}

/// What one request to the loopback listener means for the sign-in.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Code(String),
    Refused(String),
    /// Our path, but not this sign-in's state: a stale or forged redirect.
    Stale,
    NotFound,
}

async fn serve_callback(mut stream: TcpStream, state: &str) -> Option<Result<String>> {
    let target = tokio::time::timeout(CALLBACK_READ_TIMEOUT, read_request_target(&mut stream))
        .await
        .ok()??;
    let callback = parse_callback(&target, state);
    // Fixed pages: nothing from the request is reflected back.
    let (status, page, outcome) = match callback {
        Callback::Code(code) => (
            "200 OK",
            "You are signed in to Open Source Cloud. You can close this window and go back to Open Live Ingest.",
            Some(Ok(code)),
        ),
        Callback::Refused(error) => (
            "200 OK",
            "The Open Source Cloud sign-in was not completed. You can close this window and try again from Open Live Ingest.",
            Some(Err(anyhow!("the Open Source Cloud sign-in was refused ({error})"))),
        ),
        Callback::Stale => (
            "400 Bad Request",
            "This sign-in is not the one Open Live Ingest is waiting for. Start the sign-in again from Open Live Ingest.",
            None,
        ),
        Callback::NotFound => ("404 Not Found", "Not found.", None),
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Open Live Ingest</title></head><body><p>{page}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.ok();
    stream.shutdown().await.ok();
    outcome
}

/// The target of a `GET`, read up to the end of its head. None for anything else.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > CALLBACK_MAX_HEAD {
            return None;
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = head.split(|&b| b == b'\r').next()?;
    let mut parts = std::str::from_utf8(line).ok()?.split(' ');
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_string)
}

fn parse_callback(target: &str, state: &str) -> Callback {
    let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::NotFound;
    };
    if url.path() != "/callback" {
        return Callback::NotFound;
    }
    let (mut got_state, mut code, mut error) = (None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" => got_state = Some(value.into_owned()),
            "code" => code = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if got_state.as_deref() != Some(state) {
        return Callback::Stale;
    }
    match (code, error) {
        (_, Some(error)) => Callback::Refused(error_code(&error)),
        (Some(code), None) if !code.is_empty() => Callback::Code(code),
        _ => Callback::Stale,
    }
}

/// An OAuth error code, safe to put in a message: RFC 6749 codes are short and plain,
/// so anything else is replaced rather than echoed.
fn error_code(raw: &str) -> String {
    let plain = !raw.is_empty()
        && raw.len() <= 64
        && raw.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    if plain {
        raw.to_string()
    } else {
        "unrecognised error".to_string()
    }
}

/// Trades the code for tokens, proving with the verifier that this is the process
/// that started the sign-in. A public client: no secret is sent.
async fn exchange_code(
    http: &reqwest::Client,
    base: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<Credential> {
    let grant = token_request(
        http,
        base,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ],
    )
    .await
    .map_err(|err| match err {
        TokenError::Refused { status, error } => {
            anyhow!("the Open Source Cloud sign-in could not be completed: HTTP {status} ({error})")
        }
        TokenError::Unavailable(err) => err.context("completing the Open Source Cloud sign-in"),
    })?;
    let expires_at = grant
        .expires_in
        .map(|secs| now().as_secs() + secs)
        .or_else(|| inspect(&grant.access_token)?.expires_at);
    debug!("signed in to Open Source Cloud");
    Ok(Credential {
        access_token: grant.access_token,
        refresh_token: grant.refresh_token,
        expires_at,
        client_id: client_id.to_string(),
    })
}

/// What a token request yields: the access token, its successor's means, and how long
/// it lasts as the server said it (seconds from now), not yet turned into a time.
#[derive(Clone, PartialEq, Eq)]
pub struct Grant {
    pub access_token: String,
    /// A refresh returns a new one, which replaces the one it was made with.
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Why a token request failed, split by what to do about it.
#[derive(Debug)]
pub enum TokenError {
    /// The server refused the grant: `invalid_grant` (expired, revoked or already
    /// used) or `403 access_denied` (a suspended workspace), or any other 4xx but
    /// `408` and `429`. Trying again with the same grant will not help; the person has
    /// to sign in again.
    Refused {
        status: reqwest::StatusCode,
        /// The OAuth error code only, never the body around it.
        error: String,
    },
    /// The server could not be reached, answered 5xx (a `503` while it checks the
    /// workspace), `408 Request Timeout` or `429 Too Many Requests`, or answered
    /// something unreadable. Worth trying again later.
    Unavailable(anyhow::Error),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Refused { status, error } => {
                write!(
                    f,
                    "Open Source Cloud refused the token request: HTTP {status} ({error})"
                )
            }
            TokenError::Unavailable(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for TokenError {}

/// Renews an access token from a refresh token (`grant_type=refresh_token`). OSC
/// needs no client id or secret for it, and answers with a new pair: the refresh
/// token in the result replaces `refresh_token`, which may stop working.
pub async fn refresh(
    http: &reqwest::Client,
    base: &str,
    refresh_token: &str,
) -> Result<Grant, TokenError> {
    let grant = token_request(
        http,
        base,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )
    .await?;
    debug!("refreshed the Open Source Cloud sign-in");
    Ok(grant)
}

/// One `POST /api/connect/token`. Errors carry the status and the OAuth error code,
/// never the body: it can repeat the grant.
async fn token_request(
    http: &reqwest::Client,
    base: &str,
    form: &[(&str, &str)],
) -> Result<Grant, TokenError> {
    #[derive(Deserialize)]
    struct Tokens {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
    }
    #[derive(Deserialize)]
    struct OauthError {
        error: String,
    }
    let res = http
        .post(format!("{base}/api/connect/token"))
        .header("accept", "application/json")
        .form(form)
        .send()
        .await
        .map_err(|err| TokenError::Unavailable(anyhow!(err).context("POST token")))?;
    let status = res.status();
    // A timeout or a rate limit says nothing about the grant: it may work later.
    if status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        return Err(TokenError::Unavailable(anyhow!(
            "the Open Source Cloud token endpoint answered HTTP {status}"
        )));
    }
    if !status.is_success() {
        let error = res
            .json::<OauthError>()
            .await
            .map(|e| error_code(&e.error))
            .unwrap_or_else(|_| "no error code".to_string());
        return Err(TokenError::Refused { status, error });
    }
    let tokens: Tokens = res
        .json()
        .await
        .map_err(|err| TokenError::Unavailable(anyhow!(err).context("decoding tokens")))?;
    if tokens.access_token.is_empty() {
        return Err(TokenError::Unavailable(anyhow!(
            "Open Source Cloud returned no access token"
        )));
    }
    Ok(Grant {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token.filter(|t| !t.is_empty()),
        expires_in: tokens.expires_in,
    })
}

/// `bytes` from the OS random source, base64url: 32 bytes give a 43 character PKCE
/// verifier (RFC 7636 §4.1).
fn random_string(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|err| anyhow!("the OS random source failed: {err}"))?;
    Ok(base64url_encode(&buf))
}

/// The S256 code challenge for a verifier (RFC 7636 §4.2).
fn code_challenge(verifier: &str) -> String {
    base64url_encode(&Sha256::digest(verifier.as_bytes()))
}

/// Opens `url` in the default browser. macOS `open`, Windows the URL handler through
/// `rundll32` (no `cmd` in between to read `&` and `%` in the address), elsewhere
/// `xdg-open`. Returns once the opener has started; it is reaped in the background.
pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = std::process::Command::new("rundll32");
        cmd.arg("url.dll,FileProtocolHandler");
        cmd
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut cmd = std::process::Command::new("xdg-open");
    let mut child = cmd
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("starting the system browser")?;
    std::thread::spawn(move || child.wait());
    Ok(())
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

fn base64url_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
        }
    }
    out
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Instance {
    pub name: String,
    pub url: String,
}

/// Where Open Source Cloud answers: its OAuth server, the catalog of a workspace's
/// subscriptions, and the exchange for service tokens. Production unless a test
/// points them at a fake; public so an app's own tests can do the same through
/// [`crate::api::osc_sign_in_at`] and [`crate::api::workspace_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The OAuth server: `/api/connect/{register,authorize,token}` below it.
    pub connect: String,
    /// The list of a workspace's subscriptions.
    pub catalog: String,
    /// The exchange of a personal access token for a service token.
    pub token_exchange: String,
}

impl Endpoints {
    pub fn production() -> Self {
        Endpoints {
            connect: CONNECT_URL.to_string(),
            catalog: CATALOG_URL.to_string(),
            token_exchange: crate::openlive::TOKEN_EXCHANGE_URL.to_string(),
        }
    }
}

/// The Open Live instances in the token's workspace, the same way `osc list` finds
/// them: the catalog says where the service's API is, and a service token lists it.
pub async fn open_live_instances(token: &str) -> Result<Vec<Instance>> {
    open_live_instances_at(&Endpoints::production(), token).await
}

pub(crate) async fn open_live_instances_at(
    endpoints: &Endpoints,
    token: &str,
) -> Result<Vec<Instance>> {
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
        .get(&endpoints.catalog)
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

    let service_token =
        crate::openlive::service_token_at(&http, &endpoints.token_exchange, token).await?;
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

/// A fake Open Source Cloud: `/api/connect` registration, and a token endpoint that
/// checks the code, the redirect and the PKCE verifier against what the fake browser
/// saw on the authorize page, or renews [`REFRESH`] and [`RENEWED`]; and the catalog,
/// service-token exchange and instance list, which take only [`ACCESS`].
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub const CODE: &str = "the-code";
    /// A token for the workspace [`WORKSPACE`]: `{"tenantId":"venue-ws"}`.
    pub const ACCESS: &str = "h.eyJ0ZW5hbnRJZCI6InZlbnVlLXdzIn0.s";
    pub const WORKSPACE: &str = "venue-ws";
    pub const REFRESH: &str = "refresh-secret";
    /// What a refresh returns in place of the refresh token it was given.
    pub const RENEWED: &str = "refresh-renewed";
    const SERVICE_TOKEN: &str = "service-token";

    #[derive(Default)]
    pub struct Seen {
        pub registrations: Vec<serde_json::Value>,
        pub authorize: Vec<(String, String)>,
        pub token_forms: Vec<Vec<(String, String)>>,
        /// The token each catalog request presented.
        pub catalog_tokens: Vec<String>,
    }

    impl Seen {
        pub fn param(&self, key: &str) -> Option<String> {
            self.authorize
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        }
    }

    pub struct Osc {
        pub base: String,
        pub seen: Arc<Mutex<Seen>>,
    }

    impl Osc {
        /// Every Open Source Cloud endpoint, on this fake.
        pub fn endpoints(&self) -> Endpoints {
            Endpoints {
                connect: self.base.clone(),
                catalog: format!("{}/mysubscriptions", self.base),
                token_exchange: format!("{}/servicetoken", self.base),
            }
        }
    }

    /// `register` false answers registration with HTTP 500; `token_error` answers
    /// the token request with that OAuth error.
    pub async fn osc(register: bool, token_error: Option<&'static str>) -> Osc {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let shared = seen.clone();
        let base_for_answers = base.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let seen = shared.clone();
                let base = base_for_answers.clone();
                tokio::spawn(answer(stream, seen, base, register, token_error));
            }
        });
        Osc { base, seen }
    }

    async fn answer(
        mut stream: TcpStream,
        seen: Arc<Mutex<Seen>>,
        base: String,
        register: bool,
        token_error: Option<&'static str>,
    ) {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let (head, body) = loop {
            let n = stream.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&raw[..end]).to_string();
            let length = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while raw.len() < end + 4 + length {
                let n = stream.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            break (head, raw[end + 4..end + 4 + length].to_vec());
        };
        let request_line = head.lines().next().unwrap_or_default().to_string();
        let header = |name: &str| {
            head.lines().skip(1).find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| v.trim().to_string())
            })
        };
        let bearer = |name: &str, token: &str| header(name) == Some(format!("Bearer {token}"));
        let unauthorized = ("401 Unauthorized", "{}".to_string());
        let (status, reply) = if request_line.starts_with("POST /api/connect/register ") {
            seen.lock()
                .unwrap()
                .registrations
                .push(serde_json::from_slice(&body).unwrap());
            if register {
                (
                    "201 Created",
                    r#"{"client_id":"registered-id","token_endpoint_auth_method":"none"}"#
                        .to_string(),
                )
            } else {
                ("500 Internal Server Error", "{}".to_string())
            }
        } else if request_line.starts_with("POST /api/connect/token ") {
            let form: Vec<(String, String)> =
                reqwest::Url::parse(&format!("http://x/?{}", String::from_utf8_lossy(&body)))
                    .unwrap()
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
            let mut seen = seen.lock().unwrap();
            let get = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
            let pkce_ok =
                get("code_verifier").map(|v| code_challenge(&v)) == seen.param("code_challenge");
            let valid = get("grant_type").as_deref() == Some("authorization_code")
                && get("code").as_deref() == Some(CODE)
                && get("redirect_uri") == seen.param("redirect_uri")
                && get("client_id") == seen.param("client_id")
                && get("client_secret").is_none()
                && pkce_ok;
            seen.token_forms.push(form.clone());
            let refresh = get("grant_type").as_deref() == Some("refresh_token");
            if refresh && token_error.is_none() {
                match get("refresh_token").as_deref() {
                    Some(REFRESH | RENEWED) => (
                        "200 OK",
                        format!(
                            r#"{{"access_token":"{ACCESS}","refresh_token":"{RENEWED}","token_type":"Bearer","expires_in":3600}}"#
                        ),
                    ),
                    _ => (
                        "400 Bad Request",
                        r#"{"error":"invalid_grant"}"#.to_string(),
                    ),
                }
            } else {
                match (token_error, valid) {
                    (None, true) => (
                        "200 OK",
                        format!(
                            r#"{{"access_token":"{ACCESS}","refresh_token":"{REFRESH}","token_type":"Bearer","expires_in":3600,"scope":"claudeai"}}"#
                        ),
                    ),
                    (Some(error), _) => (
                        "400 Bad Request",
                        format!(
                            r#"{{"error":"{error}","error_description":"code {CODE} for {ACCESS}"}}"#
                        ),
                    ),
                    (None, false) => (
                        "400 Bad Request",
                        r#"{"error":"invalid_request"}"#.to_string(),
                    ),
                }
            }
        } else if request_line.starts_with("GET /mysubscriptions ") {
            let presented = header("x-pat-jwt").unwrap_or_default();
            seen.lock().unwrap().catalog_tokens.push(presented);
            if bearer("x-pat-jwt", ACCESS) {
                (
                    "200 OK",
                    format!(
                        r#"[{{"serviceId":"another-service","apiUrl":"{base}/nope"}},{{"serviceId":"{}","apiUrl":"{base}/instances"}}]"#,
                        crate::openlive::OPEN_LIVE_SERVICE_ID
                    ),
                )
            } else {
                unauthorized
            }
        } else if request_line.starts_with("POST /servicetoken ") {
            if bearer("x-pat-jwt", ACCESS) {
                (
                    "200 OK",
                    format!(
                        r#"{{"token":"{SERVICE_TOKEN}","expiry":{}}}"#,
                        now().as_secs() + 3600
                    ),
                )
            } else {
                unauthorized
            }
        } else if request_line.starts_with("GET /instances ") {
            if bearer("x-jwt", SERVICE_TOKEN) {
                (
                    "200 OK",
                    r#"[{"name":"venue","url":"https://venue.example.osaas.io","StromAccessToken":"secret"}]"#
                        .to_string(),
                )
            } else {
                unauthorized
            }
        } else {
            ("404 Not Found", "{}".to_string())
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        stream.write_all(response.as_bytes()).await.ok();
    }

    /// A browser on the authorize page: records what it was given, then, as the
    /// person finishes, loads a favicon and a stale tab's redirect before the real
    /// one, and reports the HTTP status of each.
    pub fn browser(
        seen: Arc<Mutex<Seen>>,
        outcome: &'static str,
    ) -> (
        impl FnOnce(&str) -> Result<()>,
        tokio::sync::oneshot::Receiver<Vec<u16>>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let open = move |url: &str| -> Result<()> {
            let url = reqwest::Url::parse(url)?;
            assert_eq!(url.path(), "/api/connect/authorize");
            let params: Vec<(String, String)> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            seen.lock().unwrap().authorize = params;
            let (redirect, state) = {
                let seen = seen.lock().unwrap();
                (
                    seen.param("redirect_uri").unwrap(),
                    seen.param("state").unwrap(),
                )
            };
            tokio::spawn(async move {
                let http = reqwest::Client::new();
                let base = redirect.trim_end_matches("/callback").to_string();
                let mut statuses = Vec::new();
                for url in [
                    format!("{base}/favicon.ico"),
                    format!("{redirect}?code=stale&state=not-this-one"),
                    format!("{redirect}?{outcome}&state={state}"),
                ] {
                    let res = http.get(&url).send().await.unwrap();
                    statuses.push(res.status().as_u16());
                }
                tx.send(statuses).ok();
            });
            Ok(())
        };
        (open, rx)
    }
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

    /// RFC 7636's verifier rules, and a challenge checked against an independent
    /// SHA-256 (`openssl dgst -sha256 -binary | base64url`).
    #[test]
    fn the_pkce_challenge_is_base64url_sha256_of_the_verifier() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mJ92IjeBTN3ccN3hx8bxaL4KF9IPaU"),
            "Nz5cp7Hk28pVk_GDEdhsI4h3a7vWvLxxgN5HRQ60pak"
        );
        let verifier = random_string(32).unwrap();
        assert_eq!(verifier.len(), 43, "RFC 7636 wants 43 to 128 characters");
        assert!(verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(verifier, random_string(32).unwrap());
    }

    #[test]
    fn base64url_encoding_round_trips_without_padding() {
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(
            base64url_decode(&base64url_encode(&bytes)).as_deref(),
            Some(&bytes[..])
        );
    }

    #[test]
    fn only_this_sign_ins_callback_counts() {
        assert_eq!(
            parse_callback("/callback?code=c1&state=s1", "s1"),
            Callback::Code("c1".into())
        );
        assert_eq!(
            parse_callback("/callback?error=access_denied&state=s1", "s1"),
            Callback::Refused("access_denied".into())
        );
        assert_eq!(
            parse_callback("/callback?code=c1&state=other", "s1"),
            Callback::Stale
        );
        assert_eq!(parse_callback("/callback?code=c1", "s1"), Callback::Stale);
        assert_eq!(parse_callback("/callback?state=s1", "s1"), Callback::Stale);
        assert_eq!(parse_callback("/favicon.ico", "s1"), Callback::NotFound);
        assert_eq!(
            parse_callback("/callback?error=%3Cscript%3E&state=s1", "s1"),
            Callback::Refused("unrecognised error".into()),
            "an error is only repeated when it is a plain OAuth code"
        );
    }

    #[tokio::test]
    async fn a_browser_sign_in_registers_a_client_and_keeps_the_refresh_token() {
        let osc = fake::osc(true, None).await;
        let (open, statuses) = fake::browser(osc.seen.clone(), "code=the-code");
        let credential = sign_in(&osc.base, Duration::from_secs(10), open)
            .await
            .unwrap();

        assert_eq!(credential.access_token, fake::ACCESS);
        assert_eq!(credential.refresh_token.as_deref(), Some(fake::REFRESH));
        assert_eq!(credential.client_id, "registered-id");
        let expires_at = credential.expires_at.unwrap();
        assert!(expires_at.abs_diff(now().as_secs() + 3600) <= 5);
        assert_eq!(statuses.await.unwrap(), vec![404, 400, 200]);

        let seen = osc.seen.lock().unwrap();
        assert_eq!(
            seen.registrations,
            vec![serde_json::json!({
                "client_name": "Open Live Ingest",
                "redirect_uris": ["http://127.0.0.1/callback"],
            })]
        );
        assert_eq!(seen.param("response_type").as_deref(), Some("code"));
        assert_eq!(seen.param("client_id").as_deref(), Some("registered-id"));
        assert_eq!(seen.param("code_challenge_method").as_deref(), Some("S256"));
        let redirect = seen.param("redirect_uri").unwrap();
        assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"));
        assert_eq!(seen.token_forms.len(), 1, "one exchange, accepted");

        let shown = format!("{credential:?}");
        assert!(!shown.contains(fake::ACCESS) && !shown.contains(fake::REFRESH));
        assert!(shown.contains("registered-id"));
    }

    #[tokio::test]
    async fn without_a_registration_the_sign_in_goes_through_the_cli_client() {
        let osc = fake::osc(false, None).await;
        let (open, _) = fake::browser(osc.seen.clone(), "code=the-code");
        let credential = sign_in(&osc.base, Duration::from_secs(10), open)
            .await
            .unwrap();
        assert_eq!(credential.client_id, "cli");
        assert_eq!(credential.access_token, fake::ACCESS);
        assert_eq!(
            osc.seen.lock().unwrap().param("client_id").as_deref(),
            Some("cli")
        );
    }

    #[tokio::test]
    async fn a_declined_sign_in_fails_without_an_exchange() {
        let osc = fake::osc(true, None).await;
        let (open, statuses) = fake::browser(osc.seen.clone(), "error=access_denied");
        let err = sign_in(&osc.base, Duration::from_secs(10), open)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("access_denied"), "{err:#}");
        assert_eq!(statuses.await.unwrap(), vec![404, 400, 200]);
        assert!(osc.seen.lock().unwrap().token_forms.is_empty());
    }

    /// The token endpoint's body can repeat the code and more; only its error code
    /// and the status reach the message.
    #[tokio::test]
    async fn a_refused_exchange_reports_the_error_code_and_nothing_else() {
        let osc = fake::osc(true, Some("invalid_grant")).await;
        let (open, _) = fake::browser(osc.seen.clone(), "code=the-code");
        let err = sign_in(&osc.base, Duration::from_secs(10), open)
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("HTTP 400") && message.contains("invalid_grant"),
            "{message}"
        );
        assert!(
            !message.contains(fake::CODE) && !message.contains(fake::ACCESS),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_sign_in_nobody_finishes_times_out() {
        let osc = fake::osc(true, None).await;
        let err = sign_in(&osc.base, Duration::from_millis(200), |_| Ok(()))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("did not finish"), "{err:#}");
    }

    #[tokio::test]
    async fn a_browser_that_cannot_open_fails_the_sign_in() {
        let osc = fake::osc(true, None).await;
        let err = sign_in(&osc.base, Duration::from_secs(10), |_| bail!("no browser"))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("opening the system browser"),
            "{err:#}"
        );
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
