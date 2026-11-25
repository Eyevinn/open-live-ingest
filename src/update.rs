//! Whether a newer release of Open Live Ingest is out (#164).
//!
//! One request to GitHub's public API for the latest release of this repository, a
//! draft or a pre-release ignored, and its tag compared with the running version. The
//! desktop app asks at startup and once a day after, unless `[app] check_for_updates`
//! is off; nothing is downloaded or installed. Every failure (no network, a GitHub
//! error, its rate limit, an answer that does not read) is an error for the caller to
//! log and otherwise ignore, and a tag that is not a plain `vMAJOR.MINOR.PATCH` is no
//! update at all.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// GitHub's REST API, where [`check`] asks unless a test points it elsewhere.
pub const GITHUB_API: &str = "https://api.github.com";

/// The repository whose releases are checked.
pub const REPOSITORY: &str = "Eyevinn/open-live-ingest";

/// How often a running app asks again.
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Short: the answer only decides whether a line is shown.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A release version, `MAJOR.MINOR.PATCH`, compared field by field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// `1.2.3` or `v1.2.3`. Anything else, a pre-release or build suffix included, is
    /// `None`: an update is only announced for a version that reads unambiguously.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let mut number = || -> Option<u64> {
            let part = parts.next()?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            part.parse().ok()
        };
        let version = Version {
            major: number()?,
            minor: number()?,
            patch: number()?,
        };
        parts.next().is_none().then_some(version)
    }

    /// The release's page on GitHub. Built from the version's numbers alone, so it
    /// holds nothing an answer from the network chose.
    pub fn release_page(&self) -> String {
        format!("https://github.com/{REPOSITORY}/releases/tag/v{self}")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The release `latest` names, when it is newer than `current`. `None` when it is
/// not, or when either does not read as a version.
pub fn newer(current: &str, latest: &str) -> Option<Version> {
    let current = Version::parse(current)?;
    let latest = Version::parse(latest)?;
    (latest > current).then_some(latest)
}

/// The fields of GitHub's release object this reads.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Asks `api` (normally [`GITHUB_API`]) for the latest release, and returns it when it
/// is newer than `current`. `Ok(None)`: no newer release, or a draft, a pre-release or
/// a tag that does not read as a version. `Err`: the question could not be answered.
pub async fn check(api: &str, current: &str) -> Result<Option<Version>> {
    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // GitHub refuses a request without a User-Agent.
        .user_agent(concat!("open-live-ingest/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;
    let url = format!(
        "{}/repos/{REPOSITORY}/releases/latest",
        api.trim_end_matches('/')
    );
    let res = http
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the latest release")?;
    let status = res.status();
    if !status.is_success() {
        let exhausted = res
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v.as_bytes() == b"0");
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || (status == reqwest::StatusCode::FORBIDDEN && exhausted)
        {
            bail!("GitHub's rate limit is reached ({status})");
        }
        bail!("GitHub answered {status} for the latest release");
    }
    let release: Release = res
        .json()
        .await
        .context("reading GitHub's latest release")?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    Ok(newer(current, &release.tag_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_read_with_or_without_a_v() {
        assert_eq!(v("0.6.0"), v("v0.6.0"));
        assert_eq!(v("v10.20.30").to_string(), "10.20.30");
    }

    #[test]
    fn anything_but_three_numbers_is_no_version() {
        for text in [
            "",
            "v",
            "0.6",
            "0.6.0.1",
            "0.6.0-rc.1",
            "0.6.0+build",
            "v0.6.x",
            "V0.6.0",
            " 0.6.0",
            "0..6",
            "-1.0.0",
            "+1.0.0",
            "latest",
            "99999999999999999999.0.0",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.6.10") > v("0.6.9"));
        assert!(v("0.6.0") == v("v0.6.0"));
    }

    #[test]
    fn only_a_newer_release_is_an_update() {
        assert_eq!(newer("0.6.0", "v0.7.0"), Some(v("0.7.0")));
        assert_eq!(newer("0.6.0", "v0.6.1"), Some(v("0.6.1")));
        assert_eq!(newer("0.6.0", "v1.0.0"), Some(v("1.0.0")));
        assert_eq!(newer("0.6.0", "v0.6.0"), None);
        assert_eq!(newer("0.7.0", "v0.6.0"), None);
        assert_eq!(newer("0.6.0", "v0.7.0-rc.1"), None);
        assert_eq!(newer("0.6.0", "nightly"), None);
        assert_eq!(newer("not-a-version", "v0.7.0"), None);
    }

    #[test]
    fn the_release_page_is_built_from_the_numbers() {
        assert_eq!(
            v("v0.7.0").release_page(),
            "https://github.com/Eyevinn/open-live-ingest/releases/tag/v0.7.0"
        );
    }

    #[test]
    fn this_build_has_a_version_that_reads() {
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_some());
    }

    /// Answers one request on a local port with `head` (status line and any extra
    /// headers) and `body`; returns the base URL and the request as it arrived.
    async fn serve_once(
        head: &'static str,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let response = format!(
                "HTTP/1.1 {head}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn a_newer_release_is_reported() {
        let (api, request) = serve_once(
            "200 OK",
            r#"{"tag_name":"v0.7.0","draft":false,"prerelease":false,"html_url":"https://example.com/elsewhere"}"#,
        )
        .await;
        assert_eq!(check(&api, "0.6.0").await.unwrap(), Some(v("0.7.0")));
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /repos/eyevinn/open-live-ingest/releases/latest "));
        assert!(request.contains("\r\nuser-agent: open-live-ingest/"));
        assert!(request.contains("\r\naccept: application/vnd.github+json"));
    }

    #[tokio::test]
    async fn the_same_release_is_no_update() {
        let (api, _) = serve_once("200 OK", r#"{"tag_name":"v0.6.0"}"#).await;
        assert_eq!(check(&api, "0.6.0").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_draft_or_a_pre_release_is_no_update() {
        let (api, _) = serve_once("200 OK", r#"{"tag_name":"v9.0.0","draft":true}"#).await;
        assert_eq!(check(&api, "0.6.0").await.unwrap(), None);
        let (api, _) = serve_once("200 OK", r#"{"tag_name":"v9.0.0","prerelease":true}"#).await;
        assert_eq!(check(&api, "0.6.0").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_tag_that_does_not_read_is_no_update() {
        let (api, _) = serve_once("200 OK", r#"{"tag_name":"release-2026-11"}"#).await;
        assert_eq!(check(&api, "0.6.0").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_non_200_is_an_error() {
        let (api, _) = serve_once("404 Not Found", r#"{"message":"Not Found"}"#).await;
        let err = check(&api, "0.6.0").await.unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
        let (api, _) = serve_once("500 Internal Server Error", "").await;
        assert!(check(&api, "0.6.0").await.is_err());
    }

    #[tokio::test]
    async fn the_rate_limit_is_an_error() {
        let (api, _) = serve_once(
            "403 Forbidden\r\nx-ratelimit-remaining: 0",
            r#"{"message":"API rate limit exceeded"}"#,
        )
        .await;
        let err = check(&api, "0.6.0").await.unwrap_err();
        assert!(format!("{err:#}").contains("rate limit"), "{err:#}");
        let (api, _) = serve_once("429 Too Many Requests", "{}").await;
        let err = check(&api, "0.6.0").await.unwrap_err();
        assert!(format!("{err:#}").contains("rate limit"), "{err:#}");
    }

    #[tokio::test]
    async fn an_answer_that_does_not_read_is_an_error() {
        let (api, _) = serve_once("200 OK", "<html>not json</html>").await;
        assert!(check(&api, "0.6.0").await.is_err());
        let (api, _) = serve_once("200 OK", r#"{"name":"no tag"}"#).await;
        assert!(check(&api, "0.6.0").await.is_err());
    }

    #[tokio::test]
    async fn a_refused_connection_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(check(&format!("http://{addr}"), "0.6.0").await.is_err());
    }
}
