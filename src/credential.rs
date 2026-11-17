//! The Open Source Cloud credential an app keeps in the OS keychain, and keeping a
//! session of several hours signed in with it.
//!
//! After a browser sign-in ([`osc::browser_login`]) what is kept is OSC's refresh
//! token: it lasts 30 days, and each refresh returns a new one that replaces it, so
//! the 30 days slide. The one-hour access token is never stored; [`KeptCredential`]
//! renews it from the refresh token a few minutes before it expires, as often as a
//! show needs. A personal access token pasted instead is kept as it is.
//!
//! The settings file holds only the marker `open_live.credential = "keychain"`, never
//! the secret. The command line's own sources, `OSC_ACCESS_TOKEN` and the OSC CLI's
//! `~/.osc/token`, are untouched by this module.
//!
//! See docs/research/OSC_BROWSER_SIGN_IN.md for what OSC's token endpoint does.

use crate::keychain::SecretStore;
use crate::osc::{self, TokenError};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// The keychain entry's name under [`crate::keychain::SERVICE`].
pub const ACCOUNT: &str = "osc";

/// Renew the access token this long before it expires, so none dies mid-request.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// How far ahead [`KeptCredential::ends_within`] looks when a session starts: a
/// credential that cannot be renewed and ends sooner than this is worth a warning
/// before going live. Longer than any show this gateway is meant for.
pub const SESSION_HORIZON: Duration = Duration::from_secs(12 * 60 * 60);

/// What OSC says an access token lasts, for a reply that says nothing.
const ASSUMED_ACCESS_LIFETIME: u64 = 3600;

/// What to tell someone whose sign-in can no longer be used.
pub const SIGN_IN_AGAIN: &str =
    "sign in to Open Source Cloud again, or use a personal access token instead";

/// What is kept in the keychain, as JSON.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredCredential {
    /// A browser sign-in, renewed from its refresh token.
    SignIn {
        refresh_token: String,
        /// The client the sign-in went through. Not needed to refresh; kept so a new
        /// sign-in can tell which client it was.
        client_id: String,
    },
    /// A personal access token, or a sign-in that came with no refresh token. Used as
    /// it is until `expires_at`, when it has one.
    Token {
        token: String,
        /// Unix seconds. When unset, the token's own `exp` claim, if any, applies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<u64>,
    },
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoredCredential::SignIn { client_id, .. } => f
                .debug_struct("SignIn")
                .field("refresh_token", &"<redacted>")
                .field("client_id", client_id)
                .finish(),
            StoredCredential::Token { expires_at, .. } => f
                .debug_struct("Token")
                .field("token", &"<redacted>")
                .field("expires_at", expires_at)
                .finish(),
        }
    }
}

impl StoredCredential {
    /// What to keep from a browser sign-in: its refresh token, or, without one, the
    /// access token until it expires.
    pub fn from_sign_in(credential: &osc::Credential) -> Self {
        match &credential.refresh_token {
            Some(refresh_token) => StoredCredential::SignIn {
                refresh_token: refresh_token.clone(),
                client_id: credential.client_id.clone(),
            },
            None => StoredCredential::Token {
                token: credential.access_token.clone(),
                expires_at: credential.expires_at,
            },
        }
    }

    /// A pasted personal access token, trimmed.
    pub fn from_token(token: &str) -> Result<Self> {
        let token = token.trim();
        if token.is_empty() {
            bail!("no Open Source Cloud token");
        }
        Ok(StoredCredential::Token {
            token: token.to_string(),
            expires_at: None,
        })
    }

    /// When this stops working with no way to renew it, Unix seconds. None for a
    /// sign-in that refreshes, or a token that never expires.
    pub fn ends_at(&self) -> Option<u64> {
        match self {
            StoredCredential::SignIn { .. } => None,
            StoredCredential::Token { token, expires_at } => {
                expires_at.or_else(|| osc::inspect(token)?.expires_at)
            }
        }
    }
}

/// The Open Source Cloud credential an app hands the library, in place of what the
/// command line reads (`open_live.api_key`, `OSC_ACCESS_TOKEN`, `~/.osc/token`).
///
/// `Debug` shows no token.
#[derive(Clone)]
pub enum OscCredential {
    /// A token in hand: a pasted personal access token, or the access token of a
    /// sign-in just made ([`osc::Credential::access_token`]). Used as it is.
    Token(String),
    /// What [`keep`] stored in this keychain: a sign-in, renewed from its refresh
    /// token as it is used, or a kept personal access token.
    Keychain(Arc<dyn SecretStore>),
}

impl fmt::Debug for OscCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OscCredential::Token(_) => f.write_str("Token(<redacted>)"),
            OscCredential::Keychain(_) => f.write_str("Keychain"),
        }
    }
}

impl From<&str> for OscCredential {
    fn from(token: &str) -> Self {
        OscCredential::Token(token.to_string())
    }
}

impl From<String> for OscCredential {
    fn from(token: String) -> Self {
        OscCredential::Token(token)
    }
}

impl From<&osc::Credential> for OscCredential {
    fn from(sign_in: &osc::Credential) -> Self {
        OscCredential::Token(sign_in.access_token.clone())
    }
}

impl From<&OscCredential> for OscCredential {
    fn from(credential: &OscCredential) -> Self {
        credential.clone()
    }
}

impl OscCredential {
    /// An access token to present to Open Source Cloud now: a token in hand,
    /// trimmed, refused when blank or expired; or the keychain's, renewed when it
    /// is a sign-in near expiry, with the new refresh token written back.
    pub async fn access_token(&self) -> Result<String> {
        self.access_token_at(osc::CONNECT_URL).await
    }

    /// [`access_token`](Self::access_token), renewing against the OAuth server at
    /// `base`.
    pub(crate) async fn access_token_at(&self, base: &str) -> Result<String> {
        match self {
            OscCredential::Token(token) => {
                let token = token.trim();
                if token.is_empty() {
                    bail!("no Open Source Cloud credential");
                }
                if osc::inspect(token).is_some_and(|i| i.is_expired()) {
                    bail!("that Open Source Cloud token has expired; sign in again");
                }
                Ok(token.to_string())
            }
            OscCredential::Keychain(store) => {
                KeptCredential::with(store.clone(), base, Arc::new(|| osc::now().as_secs()))?
                    .access_token()
                    .await
            }
        }
    }
}

/// Keeps `credential` in `store`, replacing what was there.
pub fn keep(store: &dyn SecretStore, credential: &StoredCredential) -> Result<()> {
    let json = serde_json::to_string(credential).context("encoding the credential")?;
    store
        .set(ACCOUNT, &json)
        .context("storing the Open Source Cloud credential in the keychain")
}

/// The credential in `store`, or None when there is none.
pub fn load(store: &dyn SecretStore) -> Result<Option<StoredCredential>> {
    let Some(json) = store
        .get(ACCOUNT)
        .context("reading the Open Source Cloud credential from the keychain")?
    else {
        return Ok(None);
    };
    // The error says nothing of the content: it is a secret even when malformed.
    serde_json::from_str(&json).map(Some).map_err(|_| {
        anyhow!("the Open Source Cloud credential in the keychain is unreadable; {SIGN_IN_AGAIN}")
    })
}

/// Removes the credential from `store`, if there is one.
pub fn forget(store: &dyn SecretStore) -> Result<()> {
    store
        .delete(ACCOUNT)
        .context("removing the Open Source Cloud credential from the keychain")
}

/// The time, as Unix seconds. A function so tests can move it.
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Clone)]
struct Access {
    token: String,
    expires_at: u64,
}

/// A refresh token OSC issued that could not be written to the keychain (locked
/// after sleep, say). OSC retired the one before it, which the keychain still holds,
/// so this is the only copy that still works until a write succeeds.
struct Unsaved {
    /// The retired refresh token the keychain still holds.
    stale: String,
    refresh_token: String,
    client_id: String,
}

/// What [`KeptCredential`] holds in memory, behind one lock.
#[derive(Default)]
struct State {
    access: Option<Access>,
    unsaved: Option<Unsaved>,
}

/// The credential in the keychain, as a source of access tokens for as long as the
/// gateway runs. The current access token is held in memory only.
///
/// The keychain is read again before each refresh, so a refresh done by another
/// process on the same machine (the app and a CLI `status`, say) is picked up rather
/// than undone. A renewed refresh token the keychain would not take is held in
/// memory, used while the keychain still holds the one it replaced, and written
/// again on each call until the keychain takes it.
pub struct KeptCredential {
    store: Arc<dyn SecretStore>,
    base: String,
    http: reqwest::Client,
    clock: Clock,
    state: Mutex<State>,
}

impl fmt::Debug for KeptCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeptCredential")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl KeptCredential {
    /// Against Open Source Cloud itself.
    pub fn new(store: Arc<dyn SecretStore>) -> Result<Self> {
        Self::with(store, osc::CONNECT_URL, Arc::new(|| osc::now().as_secs()))
    }

    fn with(store: Arc<dyn SecretStore>, base: &str, clock: Clock) -> Result<Self> {
        Ok(Self {
            store,
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("building HTTP client")?,
            clock,
            state: Mutex::new(State::default()),
        })
    }

    /// An access token with at least [`REFRESH_MARGIN`] left, refreshed when needed,
    /// with the new refresh token written back to the keychain.
    ///
    /// When OSC cannot be reached or answers 5xx, 408 or 429, the current access
    /// token is used for as long as it is still valid, and the refresh is tried again
    /// on the next call. A refused refresh (`invalid_grant`, `403`) does the same, and
    /// fails with [`SIGN_IN_AGAIN`] once the current access token has expired.
    pub async fn access_token(&self) -> Result<String> {
        let mut state = self.state.lock().await;
        // After the lock: another caller's refresh may have taken a while.
        let now = (self.clock)();
        self.save_unsaved(&mut state.unsaved);
        if let Some(access) = state.access.as_ref().filter(|a| fresh(a.expires_at, now)) {
            return Ok(access.token.clone());
        }
        let stored = match load(self.store.as_ref()) {
            Ok(stored) => stored,
            // The keychain cannot be read, but the newest refresh token is in hand.
            Err(err) if state.unsaved.is_some() => {
                warn!(err = %format_args!("{err:#}"), "could not read the Open Source Cloud sign-in from the keychain; renewing with the one held in memory");
                None
            }
            Err(err) => return Err(err),
        };
        // The refresh token to use, its client, and the refresh token the keychain
        // holds: the same one, unless the newest could not be written back.
        let ends = stored.as_ref().and_then(StoredCredential::ends_at);
        let (refresh_token, client_id, in_keychain) = match (stored, &state.unsaved) {
            (Some(StoredCredential::Token { token, .. }), _) => {
                if ends.is_some_and(|end| end <= now) {
                    bail!(
                        "the Open Source Cloud token in the keychain has expired; {SIGN_IN_AGAIN}"
                    );
                }
                return Ok(token);
            }
            (Some(StoredCredential::SignIn { refresh_token, .. }), Some(unsaved))
                if refresh_token == unsaved.stale =>
            {
                (
                    unsaved.refresh_token.clone(),
                    unsaved.client_id.clone(),
                    refresh_token,
                )
            }
            (
                Some(StoredCredential::SignIn {
                    refresh_token,
                    client_id,
                }),
                _,
            ) => (refresh_token.clone(), client_id, refresh_token),
            (None, Some(unsaved)) => (
                unsaved.refresh_token.clone(),
                unsaved.client_id.clone(),
                unsaved.stale.clone(),
            ),
            (None, None) => {
                bail!("there is no Open Source Cloud credential in the keychain; {SIGN_IN_AGAIN}")
            }
        };

        let (grant, in_keychain) = match osc::refresh(&self.http, &self.base, &refresh_token).await
        {
            Ok(grant) => (grant, in_keychain),
            Err(TokenError::Refused { status, error }) => {
                // Another process may have refreshed in between and so retired this
                // refresh token; if so, the keychain holds its successor.
                match self
                    .reread_after_refused(&[&refresh_token, &in_keychain])
                    .await?
                {
                    Some(renewed) => renewed,
                    None => {
                        let reason = format!(
                                "Open Source Cloud refused to renew the sign-in (HTTP {status}, {error})"
                            );
                        return serve_until_expiry(&mut state.access, now, &reason)
                            .ok_or_else(|| anyhow!("{reason}; {SIGN_IN_AGAIN}"));
                    }
                }
            }
            Err(TokenError::Unavailable(err)) => {
                let reason = format!("could not renew the Open Source Cloud sign-in: {err:#}");
                return serve_until_expiry(&mut state.access, now, &reason)
                    .ok_or_else(|| err.context("renewing the Open Source Cloud sign-in"));
            }
        };

        if let Some(next) = &grant.refresh_token {
            let renewed = StoredCredential::SignIn {
                refresh_token: next.clone(),
                client_id: client_id.clone(),
            };
            match keep(self.store.as_ref(), &renewed) {
                Ok(()) => state.unsaved = None,
                Err(err) => {
                    // OSC has retired the refresh token the keychain holds: the next
                    // refresh needs this one, so it is held here and written again on
                    // the next call.
                    warn!(err = %format_args!("{err:#}"), "could not store the renewed Open Source Cloud sign-in; holding it in memory and retrying");
                    state.unsaved = Some(Unsaved {
                        stale: in_keychain,
                        refresh_token: next.clone(),
                        client_id,
                    });
                }
            }
        }
        let expires_at = expiry(&grant, now);
        debug!(
            "the Open Source Cloud sign-in is renewed for {} s",
            expires_at.saturating_sub(now)
        );
        state.access = Some(Access {
            token: grant.access_token.clone(),
            expires_at,
        });
        Ok(grant.access_token)
    }

    /// Writes a refresh token held in memory to the keychain, if the keychain still
    /// holds the one it replaced. When the keychain holds anything else, another
    /// process or a new sign-in has replaced it, and the keychain wins.
    fn save_unsaved(&self, unsaved: &mut Option<Unsaved>) {
        let Some(pending) = unsaved.as_ref() else {
            return;
        };
        match load(self.store.as_ref()) {
            Ok(Some(StoredCredential::SignIn { refresh_token, .. }))
                if refresh_token == pending.stale =>
            {
                let renewed = StoredCredential::SignIn {
                    refresh_token: pending.refresh_token.clone(),
                    client_id: pending.client_id.clone(),
                };
                match keep(self.store.as_ref(), &renewed) {
                    Ok(()) => {
                        debug!("the renewed Open Source Cloud sign-in is now in the keychain");
                        *unsaved = None;
                    }
                    Err(err) => {
                        debug!(err = %format_args!("{err:#}"), "the renewed Open Source Cloud sign-in still cannot be stored")
                    }
                }
            }
            Ok(_) => *unsaved = None,
            // Unreadable for now: try again on the next call.
            Err(_) => {}
        }
    }

    /// After a refused refresh: if the keychain now holds a refresh token other than
    /// those `tried` (or known stale), a refresh with that one, and that token.
    async fn reread_after_refused(&self, tried: &[&str]) -> Result<Option<(osc::Grant, String)>> {
        let Ok(Some(StoredCredential::SignIn { refresh_token, .. })) = load(self.store.as_ref())
        else {
            return Ok(None);
        };
        if tried.contains(&refresh_token.as_str()) {
            return Ok(None);
        }
        match osc::refresh(&self.http, &self.base, &refresh_token).await {
            Ok(grant) => Ok(Some((grant, refresh_token))),
            Err(TokenError::Refused { .. }) => Ok(None),
            Err(TokenError::Unavailable(err)) => {
                Err(err.context("renewing the Open Source Cloud sign-in"))
            }
        }
    }

    /// Drops the access token in hand, so the next call refreshes. For when a service
    /// refused it although it had not expired: revoked, say.
    pub async fn discard_access(&self) {
        self.state.lock().await.access = None;
    }

    /// When the credential stops working with no way to renew it, if that is within
    /// `within` from now (or already past): Unix seconds. None for a sign-in that
    /// refreshes, a token that never expires or lasts longer, and no credential.
    pub fn ends_within(&self, within: Duration) -> Option<u64> {
        let end = load(self.store.as_ref()).ok()??.ends_at()?;
        (end <= (self.clock)() + within.as_secs()).then_some(end)
    }
}

/// The access token in hand, while it has not expired, after a renewal failed for
/// `reason`: logged, and the renewal is tried again on the next call. None once it
/// has expired, which also drops it.
fn serve_until_expiry(access: &mut Option<Access>, now: u64, reason: &str) -> Option<String> {
    match access.as_ref().filter(|a| a.expires_at > now) {
        Some(a) => {
            warn!(
                "{reason}; the current sign-in lasts another {} s and the renewal is retried",
                a.expires_at - now
            );
            Some(a.token.clone())
        }
        None => {
            *access = None;
            None
        }
    }
}

/// Whether an access token expiring at `expires_at` has more than [`REFRESH_MARGIN`]
/// left at `now`.
fn fresh(expires_at: u64, now: u64) -> bool {
    expires_at > now + REFRESH_MARGIN.as_secs()
}

/// When a granted access token expires: what the server said, else the token's own
/// claim, else OSC's usual hour.
fn expiry(grant: &osc::Grant, now: u64) -> u64 {
    grant
        .expires_in
        .map(|secs| now + secs)
        .or_else(|| osc::inspect(&grant.access_token)?.expires_at)
        .unwrap_or(now + ASSUMED_ACCESS_LIFETIME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryKeychain;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A fake OSC token endpoint for `grant_type=refresh_token`. Each refresh token it
    /// issued works once and gives `access-<n>` / `refresh-<n>`, lasting an hour.
    /// `mode` makes it refuse (`invalid_grant`) or be unavailable (`503`) instead.
    struct FakeOsc {
        base: String,
        state: Arc<StdMutex<Server>>,
    }

    #[derive(Default)]
    struct Server {
        /// Refresh tokens still valid.
        valid: Vec<String>,
        issued: u64,
        /// The refresh tokens presented, in order.
        presented: Vec<String>,
        mode: Mode,
    }

    #[derive(Default, Clone, Copy, PartialEq)]
    enum Mode {
        #[default]
        Ok,
        Refuse,
        Unavailable,
        /// Answers with this status line and an empty JSON body.
        Status(&'static str),
    }

    impl FakeOsc {
        async fn start(first_refresh: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let state = Arc::new(StdMutex::new(Server {
                valid: vec![first_refresh.to_string()],
                ..Server::default()
            }));
            let shared = state.clone();
            tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    tokio::spawn(answer(stream, shared.clone()));
                }
            });
            FakeOsc { base, state }
        }

        fn set_mode(&self, mode: Mode) {
            self.state.lock().unwrap().mode = mode;
        }

        fn presented(&self) -> Vec<String> {
            self.state.lock().unwrap().presented.clone()
        }
    }

    async fn answer(mut stream: TcpStream, state: Arc<StdMutex<Server>>) {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let (head, body) = loop {
            let n = stream.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
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
        let form: Vec<(String, String)> =
            reqwest::Url::parse(&format!("http://x/?{}", String::from_utf8_lossy(&body)))
                .unwrap()
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
        let get = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let (status, reply) = {
            let mut server = state.lock().unwrap();
            assert!(head.starts_with("POST /api/connect/token "), "{head}");
            assert_eq!(get("grant_type").as_deref(), Some("refresh_token"));
            assert_eq!(
                get("client_secret"),
                None,
                "a public client sends no secret"
            );
            let presented = get("refresh_token").unwrap_or_default();
            server.presented.push(presented.clone());
            match server.mode {
                Mode::Unavailable => ("503 Service Unavailable", "{}".to_string()),
                Mode::Status(status) => (status, "{}".to_string()),
                Mode::Refuse => (
                    "400 Bad Request",
                    format!(r#"{{"error":"invalid_grant","error_description":"{presented}"}}"#),
                ),
                Mode::Ok => match server.valid.iter().position(|t| *t == presented) {
                    Some(i) => {
                        server.valid.remove(i);
                        server.issued += 1;
                        let n = server.issued;
                        server.valid.push(format!("refresh-{n}"));
                        (
                            "200 OK",
                            format!(
                                r#"{{"access_token":"access-{n}","refresh_token":"refresh-{n}","token_type":"Bearer","expires_in":3600}}"#
                            ),
                        )
                    }
                    None => (
                        "400 Bad Request",
                        format!(r#"{{"error":"invalid_grant","error_description":"{presented}"}}"#),
                    ),
                },
            }
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        stream.write_all(response.as_bytes()).await.ok();
    }

    const START: u64 = 1_800_000_000;

    fn fake_clock() -> (Arc<AtomicU64>, Clock) {
        let time = Arc::new(AtomicU64::new(START));
        let read = time.clone();
        (time, Arc::new(move || read.load(Ordering::SeqCst)))
    }

    fn signed_in(store: &MemoryKeychain, refresh: &str) {
        signed_in_to(store, refresh)
    }

    fn signed_in_to(store: &dyn SecretStore, refresh: &str) {
        keep(
            store,
            &StoredCredential::SignIn {
                refresh_token: refresh.to_string(),
                client_id: "registered-id".to_string(),
            },
        )
        .unwrap();
    }

    fn stored_refresh(store: &dyn SecretStore) -> Option<String> {
        match load(store).unwrap()? {
            StoredCredential::SignIn { refresh_token, .. } => Some(refresh_token),
            StoredCredential::Token { .. } => None,
        }
    }

    /// #65 AC2: four hours on a fake clock, asking for a token every minute as a
    /// running gateway might. Every token handed out has time left, a refresh happens
    /// only near the end of each hour, and the keychain always holds the newest
    /// refresh token, so a restart mid-show still signs in.
    #[tokio::test]
    async fn a_four_hour_session_stays_signed_in_on_one_sign_in() {
        let osc = FakeOsc::start("refresh-0").await;
        let store = Arc::new(MemoryKeychain::new());
        signed_in(&store, "refresh-0");
        let (time, clock) = fake_clock();
        let kept = KeptCredential::with(store.clone(), &osc.base, clock).unwrap();

        let mut last = String::new();
        for minute in 0..=4 * 60 {
            time.store(START + minute * 60, Ordering::SeqCst);
            let token = kept.access_token().await.unwrap();
            let n: u64 = token.strip_prefix("access-").unwrap().parse().unwrap();
            // Each refresh comes due when the token before has the margin left.
            let issued = START + (n - 1) * (3600 - REFRESH_MARGIN.as_secs());
            let left = issued + 3600 - (START + minute * 60);
            assert!(
                left > REFRESH_MARGIN.as_secs(),
                "minute {minute}: {token} has only {left} s left"
            );
            last = token;
        }
        assert_eq!(last, "access-5");
        assert_eq!(
            osc.presented(),
            vec![
                "refresh-0",
                "refresh-1",
                "refresh-2",
                "refresh-3",
                "refresh-4"
            ],
            "one refresh per hour, each with the refresh token the one before returned"
        );
        assert_eq!(stored_refresh(store.as_ref()).as_deref(), Some("refresh-5"));
        assert_eq!(
            kept.ends_within(SESSION_HORIZON),
            None,
            "a refreshable sign-in does not end"
        );
    }

    /// OSC answers 503 while it checks the workspace: keep using the access token
    /// while it lasts, and renew once OSC is back, without asking anyone to sign in.
    #[tokio::test]
    async fn an_unavailable_token_endpoint_is_waited_out_on_the_current_token() {
        let osc = FakeOsc::start("refresh-0").await;
        let store = Arc::new(MemoryKeychain::new());
        signed_in(&store, "refresh-0");
        let (time, clock) = fake_clock();
        let kept = KeptCredential::with(store.clone(), &osc.base, clock).unwrap();
        assert_eq!(kept.access_token().await.unwrap(), "access-1");

        osc.set_mode(Mode::Unavailable);
        time.store(START + 3600 - 120, Ordering::SeqCst);
        assert_eq!(kept.access_token().await.unwrap(), "access-1");
        assert_eq!(stored_refresh(store.as_ref()).as_deref(), Some("refresh-1"));

        time.store(START + 3600 + 1, Ordering::SeqCst);
        let err = kept.access_token().await.unwrap_err();
        assert!(format!("{err:#}").contains("HTTP 503"), "{err:#}");

        osc.set_mode(Mode::Ok);
        assert_eq!(kept.access_token().await.unwrap(), "access-2");
    }

    /// A revoked or 30-day-old refresh token: say to sign in again, and repeat
    /// nothing from the reply, which can carry the token.
    #[tokio::test]
    async fn a_refused_refresh_asks_for_a_new_sign_in_and_leaks_nothing() {
        let osc = FakeOsc::start("refresh-0").await;
        osc.set_mode(Mode::Refuse);
        let store = Arc::new(MemoryKeychain::new());
        signed_in(&store, "refresh-0");
        let (_, clock) = fake_clock();
        let kept = KeptCredential::with(store.clone(), &osc.base, clock).unwrap();
        let err = format!("{:#}", kept.access_token().await.unwrap_err());
        assert!(
            err.contains("invalid_grant") && err.contains("sign in"),
            "{err}"
        );
        assert!(!err.contains("refresh-0"), "{err}");
    }

    /// OSC rate-limits or times out the refresh: like a 503, it says nothing about
    /// the sign-in, so the access token in hand is used while it lasts.
    #[tokio::test]
    async fn a_rate_limited_or_timed_out_refresh_is_waited_out_on_the_current_token() {
        for status in ["429 Too Many Requests", "408 Request Timeout"] {
            let osc = FakeOsc::start("refresh-0").await;
            let store = Arc::new(MemoryKeychain::new());
            signed_in(&store, "refresh-0");
            let (time, clock) = fake_clock();
            let kept = KeptCredential::with(store.clone(), &osc.base, clock).unwrap();
            assert_eq!(kept.access_token().await.unwrap(), "access-1");

            osc.set_mode(Mode::Status(status));
            assert!(matches!(
                osc::refresh(&kept.http, &osc.base, "refresh-1").await,
                Err(TokenError::Unavailable(_))
            ));
            time.store(START + 3600 - 120, Ordering::SeqCst);
            assert_eq!(kept.access_token().await.unwrap(), "access-1", "{status}");

            osc.set_mode(Mode::Ok);
            assert_eq!(kept.access_token().await.unwrap(), "access-2", "{status}");
        }
    }

    /// A refused refresh while the access token in hand still has a few minutes:
    /// those minutes are used, and only then is the session over.
    #[tokio::test]
    async fn a_refused_refresh_keeps_the_current_token_until_it_expires() {
        let osc = FakeOsc::start("refresh-0").await;
        let store = Arc::new(MemoryKeychain::new());
        signed_in(&store, "refresh-0");
        let (time, clock) = fake_clock();
        let kept = KeptCredential::with(store.clone(), &osc.base, clock).unwrap();
        assert_eq!(kept.access_token().await.unwrap(), "access-1");

        osc.set_mode(Mode::Refuse);
        time.store(START + 3600 - 120, Ordering::SeqCst);
        assert_eq!(kept.access_token().await.unwrap(), "access-1");

        time.store(START + 3600, Ordering::SeqCst);
        let err = format!("{:#}", kept.access_token().await.unwrap_err());
        assert!(
            err.contains("invalid_grant") && err.contains("sign in"),
            "{err}"
        );
    }

    /// A keychain that takes the first write (the sign-in) and refuses every later
    /// one until healed: locked after sleep, or an access prompt denied.
    struct LockingStore {
        inner: MemoryKeychain,
        writes: AtomicU64,
        healed: std::sync::atomic::AtomicBool,
    }

    impl LockingStore {
        fn new() -> Self {
            Self {
                inner: MemoryKeychain::new(),
                writes: AtomicU64::new(0),
                healed: Default::default(),
            }
        }
    }

    impl SecretStore for LockingStore {
        fn get(&self, account: &str) -> Result<Option<String>> {
            self.inner.get(account)
        }
        fn set(&self, account: &str, secret: &str) -> Result<()> {
            if self.writes.fetch_add(1, Ordering::SeqCst) > 0 && !self.healed.load(Ordering::SeqCst)
            {
                bail!("the keychain is locked (OSStatus -25308)");
            }
            self.inner.set(account, secret)
        }
        fn delete(&self, account: &str) -> Result<()> {
            self.inner.delete(account)
        }
    }

    /// The keychain refuses to store each rotated refresh token. The newest is held
    /// in memory, so the session survives refresh after refresh, and it reaches the
    /// keychain as soon as the keychain takes writes again.
    #[tokio::test]
    async fn a_session_survives_refreshes_the_keychain_cannot_store() {
        let osc = FakeOsc::start("refresh-0").await;
        let store = Arc::new(LockingStore::new());
        signed_in_to(store.as_ref(), "refresh-0");
        let (time, clock) = fake_clock();
        let kept = KeptCredential::with(store.clone(), &osc.base, clock).unwrap();

        for minute in 0..=3 * 60 {
            time.store(START + minute * 60, Ordering::SeqCst);
            kept.access_token()
                .await
                .unwrap_or_else(|err| panic!("minute {minute}: {err:#}"));
        }
        assert_eq!(
            osc.presented(),
            vec!["refresh-0", "refresh-1", "refresh-2", "refresh-3"],
            "each refresh used the token the one before returned, not the keychain's"
        );
        assert_eq!(
            stored_refresh(store.as_ref()).as_deref(),
            Some("refresh-0"),
            "the keychain took none of them"
        );

        store.healed.store(true, Ordering::SeqCst);
        assert_eq!(kept.access_token().await.unwrap(), "access-4");
        assert_eq!(
            stored_refresh(store.as_ref()).as_deref(),
            Some("refresh-4"),
            "the next call writes the newest refresh token"
        );
    }

    /// While a renewed refresh token waits in memory, a new sign-in replaces the
    /// keychain entry: the keychain wins, and the one in memory is not written over it.
    #[tokio::test]
    async fn a_new_sign_in_wins_over_a_refresh_token_held_in_memory() {
        let osc = FakeOsc::start("refresh-0").await;
        let store = Arc::new(LockingStore::new());
        signed_in_to(store.as_ref(), "refresh-0");
        let (time, clock) = fake_clock();
        let kept = KeptCredential::with(store.clone(), &osc.base, clock).unwrap();
        assert_eq!(kept.access_token().await.unwrap(), "access-1");
        assert_eq!(stored_refresh(store.as_ref()).as_deref(), Some("refresh-0"));

        // The app signs in again; its refresh token is good at OSC.
        signed_in(&store.inner, "fresh-sign-in");
        osc.state.lock().unwrap().valid.push("fresh-sign-in".into());
        store.healed.store(true, Ordering::SeqCst);

        time.store(START + 3600 - 120, Ordering::SeqCst);
        assert_eq!(kept.access_token().await.unwrap(), "access-2");
        assert_eq!(osc.presented(), vec!["refresh-0", "fresh-sign-in"]);
        assert_eq!(stored_refresh(store.as_ref()).as_deref(), Some("refresh-2"));
    }

    /// The keychain as two processes share it: the first read returns what this
    /// process saw before another one refreshed, every later read the current entry.
    struct RacingStore {
        before: StdMutex<Option<String>>,
        now: MemoryKeychain,
    }

    impl SecretStore for RacingStore {
        fn get(&self, account: &str) -> Result<Option<String>> {
            match self.before.lock().unwrap().take() {
                Some(stale) => Ok(Some(stale)),
                None => self.now.get(account),
            }
        }
        fn set(&self, account: &str, secret: &str) -> Result<()> {
            self.now.set(account, secret)
        }
        fn delete(&self, account: &str) -> Result<()> {
            self.now.delete(account)
        }
    }

    /// Another process on the machine refreshed between our read of the keychain and
    /// our refresh, so ours is refused. The keychain is read again rather than
    /// asking for a new sign-in.
    #[tokio::test]
    async fn a_refresh_by_another_process_is_picked_up_from_the_keychain() {
        let osc = FakeOsc::start("refresh-0").await;
        let spent = serde_json::to_string(&StoredCredential::SignIn {
            refresh_token: "spent".into(),
            client_id: "registered-id".into(),
        })
        .unwrap();
        let store = Arc::new(RacingStore {
            before: StdMutex::new(Some(spent)),
            now: MemoryKeychain::new(),
        });
        signed_in(&store.now, "refresh-0");
        let (_, clock) = fake_clock();
        let kept = KeptCredential::with(store.clone(), &osc.base, clock).unwrap();
        assert_eq!(kept.access_token().await.unwrap(), "access-1");
        assert_eq!(osc.presented(), vec!["spent", "refresh-0"]);
        assert_eq!(stored_refresh(&store.now).as_deref(), Some("refresh-1"));
    }

    /// No refresh token: the credential ends, and a session starting now hears about
    /// it if the end falls within the session (#65 AC2's "point to the fallback").
    #[tokio::test]
    async fn a_token_that_cannot_be_renewed_says_when_it_ends() {
        let store = Arc::new(MemoryKeychain::new());
        keep(
            store.as_ref(),
            &StoredCredential::Token {
                token: "access".into(),
                expires_at: Some(START + 3600),
            },
        )
        .unwrap();
        let (time, clock) = fake_clock();
        let kept = KeptCredential::with(store.clone(), "http://127.0.0.1:9", clock).unwrap();
        assert_eq!(kept.ends_within(SESSION_HORIZON), Some(START + 3600));
        assert_eq!(kept.ends_within(Duration::from_secs(60)), None);
        assert_eq!(kept.access_token().await.unwrap(), "access");

        time.store(START + 3600, Ordering::SeqCst);
        let err = format!("{:#}", kept.access_token().await.unwrap_err());
        assert!(
            err.contains("expired") && err.contains("personal access token"),
            "{err}"
        );

        // A personal access token with no expiry is fine for any session.
        keep(
            store.as_ref(),
            &StoredCredential::from_token(" pat \n").unwrap(),
        )
        .unwrap();
        assert_eq!(kept.ends_within(SESSION_HORIZON), None);
        assert_eq!(kept.access_token().await.unwrap(), "pat");
    }

    #[test]
    fn what_a_sign_in_keeps_is_its_refresh_token_and_never_shown() {
        let credential = osc::Credential {
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            expires_at: Some(START + 3600),
            client_id: "registered-id".into(),
        };
        let kept = StoredCredential::from_sign_in(&credential);
        assert_eq!(
            kept,
            StoredCredential::SignIn {
                refresh_token: "refresh-secret".into(),
                client_id: "registered-id".into(),
            }
        );
        let json = serde_json::to_string(&kept).unwrap();
        assert!(
            !json.contains("access-secret"),
            "the access token is not stored"
        );
        assert!(!format!("{kept:?}").contains("secret"));

        let without = osc::Credential {
            refresh_token: None,
            ..credential
        };
        assert_eq!(
            StoredCredential::from_sign_in(&without).ends_at(),
            Some(START + 3600)
        );
        assert!(StoredCredential::from_token("  ").is_err());
    }

    #[test]
    fn an_unreadable_entry_is_an_error_that_shows_none_of_it() {
        let store = MemoryKeychain::new();
        store.set(ACCOUNT, "not json secret").unwrap();
        let err = format!("{:#}", load(&store).unwrap_err());
        assert!(
            err.contains("unreadable") && !err.contains("secret"),
            "{err}"
        );
        forget(&store).unwrap();
        assert_eq!(load(&store).unwrap(), None);
    }
}
