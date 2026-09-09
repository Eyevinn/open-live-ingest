//! The Open Source Cloud command line tool's login, used instead of a pasted token.
//!
//! `npx @osaas/cli login` signs in through the browser and saves the token to
//! `~/.osc/token`, mode 0600. That token is exchanged for a service token exactly like
//! a personal access token, so the gateway reads it from there and never has to hold
//! a secret in its own settings. It is read again at each exchange: a fresh login in
//! another session takes effect without restarting a running gateway.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// What an operator types to sign in, or to sign in again once the token has expired.
pub const LOGIN_COMMAND: &str = "npx @osaas/cli login";

/// The OSC CLI's own environment variable, which it prefers over its saved token.
const ENV_TOKEN: &str = "OSC_ACCESS_TOKEN";

/// Where `login` saves the production token.
pub fn token_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".osc").join("token"))
}

/// The token the OSC CLI would use itself: its environment variable first, then the
/// saved login. None when the operator has never logged in on this machine.
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
    if std::env::var_os(ENV_TOKEN).is_some_and(|v| !v.is_empty()) {
        return ENV_TOKEN.to_string();
    }
    token_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.osc/token".to_string())
}

/// Runs the OSC CLI's login on the operator's terminal. It opens a browser on this
/// machine and waits for the redirect, so it works on a laptop and not over a bare
/// SSH session; the caller falls back to a pasted token when it fails.
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
}
