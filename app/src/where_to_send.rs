//! Step 1 of the window, "Where to send" (docs/specs/desktop-app.md, sections 5 and 6).
//!
//! Three ways to say which Open Live the feeds go to:
//!
//! - **Sign in with Open Source Cloud** in the browser ([`api::osc_sign_in_at`]: PKCE
//!   with a loopback redirect, through a client the library registers as "Open Live
//!   Ingest", or OSC's `cli` client when registration fails). The credential goes to
//!   the OS keychain and the settings file only gets the marker, never
//!   `open_live.api_key`. Then the workspace the person picked on the consent page and
//!   its Open Live instances are listed, and one is picked. Changing workspace is
//!   signing in again: OSC has no API listing a person's workspaces.
//! - **A self-hosted Open Live**: an address and an API key, checked with
//!   [`api::check_self_hosted`] before anything is saved.
//! - **Advanced**: a pasted personal access token, checked by listing its workspace
//!   and then kept in the keychain like a sign-in.
//!
//! Every choice is written to the settings file at once, through the library's
//! [`api::load_from`] and [`api::save_to`], so `open-live-ingest check`, `up` and
//! `status` read the same file. Each change reloads the file first and touches only
//! the `open_live` keys it owns, so it does not undo another step's change saved in
//! between. Nothing else is stored: the workspace and instance names shown are looked
//! up again from the kept credential on the next launch.

use anyhow::{anyhow, bail, Context, Result};
use open_live_ingest::api::{self, Instance, OscCredential, Workspace};
use open_live_ingest::config::{AuthMode, Config, CredentialStore};
use open_live_ingest::keychain::SecretStore;
use open_live_ingest::osc::Endpoints;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shows the Open Source Cloud sign-in page: the system browser, and the address in
/// the window for when no browser opens.
pub type Opener = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// How long a lookup of the workspace and its instances may take, so a window opened
/// with no network still shows what is saved.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Which kind of Open Live the settings send to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Nothing chosen yet.
    None,
    /// An Open Source Cloud instance.
    Osc,
    /// An Open Live at an address of its own, with an API key or none.
    SelfHosted,
}

/// What the page shows for step 1. Holds no secret: whether an API key is set, never
/// the key; the Open Source Cloud credential is in the keychain and not read here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WhereView {
    pub mode: Mode,
    /// The step is done: there is an address, and for Open Source Cloud a sign-in
    /// kept in the keychain for it.
    pub ready: bool,
    /// `open_live.url`.
    pub url: Option<String>,
    /// The instance's name, when the workspace's list has it; otherwise its host.
    pub instance: Option<String>,
    /// The Open Source Cloud workspace signed in to, once it has been looked up.
    pub workspace: Option<String>,
    /// That workspace's Open Live instances, to pick one from.
    pub instances: Vec<Instance>,
    /// An Open Source Cloud credential is kept in the keychain for this app.
    pub signed_in: bool,
    /// A self-hosted Open Live's API key is set.
    pub api_key_set: bool,
    /// "Sending to <instance> (<workspace>)", once the step is done.
    pub summary: Option<String>,
    /// Why the step is not done although something is saved, in one line.
    pub problem: Option<String>,
}

/// The step's model: the settings file it writes, the keychain the Open Source Cloud
/// credential is kept in, and where Open Source Cloud answers. The window's core owns
/// one; tests build one over fake servers and an in-memory keychain.
pub struct WhereToSend {
    settings_file: PathBuf,
    keychain: Arc<dyn SecretStore>,
    endpoints: Endpoints,
    sign_in_wait: Duration,
    open: Opener,
    /// The workspace last listed with the kept credential, in this run only.
    listed: Mutex<Option<Workspace>>,
    signing_in: AtomicBool,
}

impl WhereToSend {
    pub fn new(
        settings_file: PathBuf,
        keychain: Arc<dyn SecretStore>,
        endpoints: Endpoints,
        sign_in_wait: Duration,
        open: Opener,
    ) -> Self {
        WhereToSend {
            settings_file,
            keychain,
            endpoints,
            sign_in_wait,
            open,
            listed: Mutex::new(None),
            signing_in: AtomicBool::new(false),
        }
    }

    /// The keychain the Open Source Cloud sign-in is kept in.
    pub fn keychain(&self) -> Arc<dyn SecretStore> {
        self.keychain.clone()
    }

    /// What step 1 shows, from the settings file and what has been listed this run.
    /// Reads the file only; asks no server.
    pub fn view(&self) -> WhereView {
        match self.load() {
            Ok(settings) => self.view_of(&settings),
            Err(err) => WhereView {
                problem: Some(format!("{err:#}")),
                ..empty_view()
            },
        }
    }

    /// Whether [`refresh`](Self::refresh) has anything to look up: a sign-in kept in
    /// the keychain whose workspace has not been listed yet this run.
    pub fn needs_lookup(&self) -> bool {
        self.listed().is_none()
            && self
                .load()
                .is_ok_and(|settings| settings.open_live.uses_keychain())
    }

    /// Looks up the workspace of the kept sign-in and its instances, for the names in
    /// "Sending to <instance> (<workspace>)" on a later launch and for the picker. A
    /// failure is the view's `problem`; what is saved is still shown.
    pub async fn refresh(&self) -> WhereView {
        let settings = match self.load() {
            Ok(settings) => settings,
            Err(_) => return self.view(),
        };
        if !settings.open_live.uses_keychain() {
            return self.view_of(&settings);
        }
        let credential = OscCredential::Keychain(self.keychain.clone());
        match lookup(api::workspace_at(&self.endpoints, &credential)).await {
            Ok(workspace) => {
                self.set_listed(Some(workspace));
                self.view_of(&settings)
            }
            Err(err) => WhereView {
                problem: Some(format!(
                    "Could not look up your Open Source Cloud workspace: {err:#}"
                )),
                ..self.view_of(&settings)
            },
        }
    }

    /// Signs in to Open Source Cloud in the browser, or in again to change workspace,
    /// and lists that workspace's instances. The instance saved already is kept when it
    /// is in that workspace; a workspace with exactly one is picked for the person.
    /// Otherwise nothing is saved until [`choose_instance`](Self::choose_instance).
    pub async fn sign_in(&self) -> Result<WhereView> {
        if self.signing_in.swap(true, Ordering::SeqCst) {
            bail!(
                "A sign-in is already waiting in the browser. Finish it there, or wait a few minutes and try again."
            );
        }
        let _done = ClearOnDrop(&self.signing_in);
        // The library points these settings at the keychain; they are not saved. What
        // is saved is decided in `signed_in`, on the file as it is by then.
        let mut scratch = self.load()?;
        let open = self.open.clone();
        let workspace = api::osc_sign_in_at(
            &self.endpoints,
            self.sign_in_wait,
            &mut scratch,
            self.keychain.as_ref(),
            move |url| open(url),
        )
        .await?;
        self.signed_in(workspace)
    }

    /// Keeps a pasted personal access token in the keychain, once Open Source Cloud
    /// has listed its workspace with it: a token that does not work is not kept.
    pub async fn paste_token(&self, token: &str) -> Result<WhereView> {
        let token = token.trim();
        if token.is_empty() {
            bail!("Paste a personal access token from the Open Source Cloud console.");
        }
        let workspace = lookup(api::workspace_at(
            &self.endpoints,
            &OscCredential::Token(token.to_string()),
        ))
        .await
        .context("Open Source Cloud did not accept that token")?;
        let mut scratch = Config::default();
        api::keep_osc_token(&mut scratch, self.keychain.as_ref(), token)?;
        self.signed_in(workspace)
    }

    /// Sends to `url`, one of the instances listed for the signed-in workspace.
    pub fn choose_instance(&self, url: &str) -> Result<WhereView> {
        let listed = self
            .listed()
            .ok_or_else(|| anyhow!("Sign in with Open Source Cloud first."))?;
        if !listed.instances.iter().any(|i| i.url == url) {
            bail!(
                "That instance is not in {}. Pick one from the list, or sign in again to change workspace.",
                workspace_words(&listed)
            );
        }
        let mut settings = self.load()?;
        settings.open_live.url = Some(url.to_string());
        point_at_keychain(&mut settings);
        self.save(&settings)?;
        Ok(self.view_of(&settings))
    }

    /// Sends to a self-hosted Open Live at `url` with `api_key` (blank for none), once
    /// it has answered with that key. An Open Source Cloud sign-in kept for this app is
    /// removed from the keychain: the settings use one Open Live at a time.
    /// An address typed without a scheme is taken as `https://`.
    pub async fn self_hosted(&self, url: &str, api_key: &str) -> Result<WhereView> {
        let url = with_scheme(url);
        let url = url.as_str();
        let api_key = Some(api_key.trim()).filter(|k| !k.is_empty());
        api::check_self_hosted(url, api_key)
            .await
            .map_err(|problem| anyhow!(problem.message))?;
        let mut settings = self.load()?;
        if settings.open_live.credential.is_some() || self.listed().is_some() {
            api::forget_osc_credential(&mut settings, self.keychain.as_ref())
                .context("removing the Open Source Cloud sign-in from the keychain")?;
        }
        settings.open_live.url = Some(url.to_string());
        settings.open_live.auth_mode = AuthMode::Direct;
        settings.open_live.api_key = api_key.map(str::to_string);
        settings.open_live.credential = None;
        self.save(&settings)?;
        self.set_listed(None);
        Ok(self.view_of(&settings))
    }

    /// After a sign-in or a pasted token, kept in the keychain: the workspace is
    /// listed, and the settings are saved when an instance is settled.
    fn signed_in(&self, workspace: Workspace) -> Result<WhereView> {
        let mut settings = self.load()?;
        let saved = settings
            .open_live
            .url
            .clone()
            .filter(|url| workspace.instances.iter().any(|i| &i.url == url));
        let pick = match (saved, workspace.instances.as_slice()) {
            (Some(url), _) => Some(url),
            (None, [only]) => Some(only.url.clone()),
            (None, _) => None,
        };
        self.set_listed(Some(workspace));
        if let Some(url) = pick {
            settings.open_live.url = Some(url);
            point_at_keychain(&mut settings);
            self.save(&settings)?;
        }
        Ok(self.view_of(&settings))
    }

    fn view_of(&self, settings: &Config) -> WhereView {
        let open_live = &settings.open_live;
        let listed = self.listed();
        let signed_in = open_live.uses_keychain();
        let url = open_live
            .url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string);

        // Signed in this run, but the file still names what was there before: the
        // person is picking an instance of the new workspace.
        if let (Some(listed), false) = (&listed, signed_in) {
            return WhereView {
                mode: Mode::Osc,
                signed_in: true,
                workspace: listed.id.clone(),
                instances: listed.instances.clone(),
                problem: listed.instances.is_empty().then(|| no_instances(listed)),
                ..empty_view()
            };
        }

        let mode = match (open_live.auth_mode, &url) {
            (AuthMode::Osc, _) => Mode::Osc,
            (AuthMode::Direct, Some(_)) => Mode::SelfHosted,
            (AuthMode::Direct, None) => Mode::None,
        };
        let workspace = listed.as_ref().and_then(|l| l.id.clone());
        let instances = listed
            .as_ref()
            .map(|l| l.instances.clone())
            .unwrap_or_default();
        let named = url
            .as_ref()
            .and_then(|url| instances.iter().find(|i| &i.url == url))
            .map(|i| i.name.clone());
        let instance = named.clone().or_else(|| url.as_deref().map(host));
        let mut problem = None;
        let ready = match mode {
            Mode::None => false,
            Mode::SelfHosted => true,
            Mode::Osc if !signed_in => {
                problem = Some(
                    "Sign in with Open Source Cloud to send from this app; these settings use a token kept elsewhere."
                        .to_string(),
                );
                false
            }
            Mode::Osc => match (&url, &listed) {
                (None, Some(listed)) if listed.instances.is_empty() => {
                    problem = Some(no_instances(listed));
                    false
                }
                (None, _) => false,
                (Some(_), Some(listed)) if named.is_none() => {
                    problem = Some(format!(
                        "The saved instance is not in {}. Pick one.",
                        workspace_words(listed)
                    ));
                    false
                }
                (Some(_), _) => true,
            },
        };
        let summary = match (ready, &instance, &workspace) {
            (true, Some(instance), Some(workspace)) if mode == Mode::Osc => {
                Some(format!("Sending to {instance} ({workspace})"))
            }
            (true, Some(instance), _) => Some(format!("Sending to {instance}")),
            _ => None,
        };
        WhereView {
            mode,
            ready,
            url,
            instance,
            workspace,
            instances,
            signed_in,
            api_key_set: mode == Mode::SelfHosted
                && open_live
                    .api_key
                    .as_deref()
                    .is_some_and(|k| !k.trim().is_empty()),
            summary,
            problem,
        }
    }

    fn load(&self) -> Result<Config> {
        api::load_from(&self.settings_file)
            .with_context(|| format!("reading the settings at {}", self.settings_file.display()))
    }

    fn save(&self, settings: &Config) -> Result<()> {
        api::save_to(&self.settings_file, settings).map_err(|err| anyhow!("{err}"))
    }

    fn listed(&self) -> Option<Workspace> {
        self.listed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_listed(&self, workspace: Option<Workspace>) {
        *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = workspace;
    }
}

/// The settings point at the Open Source Cloud credential in the keychain, with no
/// token of their own (#65 AC3), as `api::keep_osc_sign_in` leaves them.
fn point_at_keychain(settings: &mut Config) {
    settings.open_live.auth_mode = AuthMode::Osc;
    settings.open_live.credential = Some(CredentialStore::Keychain);
    settings.open_live.api_key = None;
}

async fn lookup(work: impl std::future::Future<Output = Result<Workspace>>) -> Result<Workspace> {
    tokio::time::timeout(LOOKUP_TIMEOUT, work)
        .await
        .map_err(|_| anyhow!("Open Source Cloud did not answer within {LOOKUP_TIMEOUT:?}"))?
}

fn workspace_words(listed: &Workspace) -> String {
    match &listed.id {
        Some(id) => format!("workspace {id}"),
        None => "this workspace".to_string(),
    }
}

fn no_instances(listed: &Workspace) -> String {
    format!(
        "There is no Open Live instance in {}. Create one in Open Source Cloud, or sign in again to pick another workspace.",
        workspace_words(listed)
    )
}

/// `url`, trimmed, with `https://` in front when it names no scheme.
fn with_scheme(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() || url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

/// The host of an address, for naming an Open Live that has no listed name.
fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_string()
}

fn empty_view() -> WhereView {
    WhereView {
        mode: Mode::None,
        ready: false,
        url: None,
        instance: None,
        workspace: None,
        instances: Vec::new(),
        signed_in: false,
        api_key_set: false,
        summary: None,
        problem: None,
    }
}

struct ClearOnDrop<'a>(&'a AtomicBool);

impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, FakeOsc};
    use open_live_ingest::credential::{self, StoredCredential};
    use open_live_ingest::keychain::MemoryKeychain;

    struct Fixture {
        osc: FakeOsc,
        keychain: Arc<MemoryKeychain>,
        dir: testing::Scratch,
        model: WhereToSend,
    }

    impl Fixture {
        fn new(name: &str, workspace: &[(&str, &str)], outcome: &'static str) -> Self {
            let osc = FakeOsc::start(workspace);
            let keychain = Arc::new(MemoryKeychain::new());
            let dir = testing::Scratch::new(name);
            let model = WhereToSend::new(
                dir.settings(),
                keychain.clone(),
                osc.endpoints(),
                Duration::from_secs(10),
                testing::browser(outcome),
            );
            Fixture {
                osc,
                keychain,
                dir,
                model,
            }
        }

        fn file(&self) -> String {
            std::fs::read_to_string(self.dir.settings()).unwrap_or_default()
        }

        fn saved(&self) -> Config {
            api::load_from(&self.dir.settings()).unwrap()
        }

        fn kept(&self) -> Option<StoredCredential> {
            credential::load(self.keychain.as_ref()).unwrap()
        }
    }

    const VENUE: (&str, &str) = ("venue", "https://venue.example.osaas.io");
    const REHEARSAL: (&str, &str) = ("rehearsal", "https://rehearsal.example.osaas.io");

    #[test]
    fn nothing_saved_shows_nothing_chosen() {
        let f = Fixture::new("fresh", &[VENUE], testing::SIGNED_IN);
        assert_eq!(f.model.view(), empty_view());
        assert!(!f.model.needs_lookup());
    }

    /// The whole OSC path against a fake Open Source Cloud: the browser sign-in through
    /// the client the library registers, the credential in the keychain and only the
    /// marker in the file, the workspace and its one instance picked and saved, and
    /// the CLI's `check` accepting the file.
    #[tokio::test(flavor = "multi_thread")]
    async fn signing_in_to_a_workspace_with_one_instance_sends_there() {
        let f = Fixture::new("osc-one", &[VENUE], testing::SIGNED_IN);
        let view = f.model.sign_in().await.unwrap();

        assert_eq!(view.mode, Mode::Osc);
        assert!(view.ready && view.signed_in);
        assert_eq!(view.url.as_deref(), Some(VENUE.1));
        assert_eq!(view.workspace.as_deref(), Some(testing::WORKSPACE));
        assert_eq!(
            view.summary.as_deref(),
            Some("Sending to venue (venue-ws)"),
            "{view:?}"
        );
        assert_eq!(view.problem, None);
        assert_eq!(
            f.osc.registered_names(),
            vec!["Open Live Ingest".to_string()],
            "the consent page names the app"
        );
        assert_eq!(
            f.kept(),
            Some(StoredCredential::SignIn {
                refresh_token: testing::REFRESH.into(),
                client_id: testing::REGISTERED_ID.into(),
            })
        );

        let file = f.file();
        assert!(file.contains("credential = \"keychain\""), "{file}");
        assert!(file.contains("auth_mode = \"osc\""), "{file}");
        assert!(file.contains(VENUE.1), "{file}");
        for secret in [testing::ACCESS, testing::REFRESH, "api_key"] {
            assert!(!file.contains(secret), "{secret} in the file:\n{file}");
        }
        testing::cli_check(&f.dir);
    }

    /// Without a client of its own the sign-in goes through OSC's `cli` client (spec
    /// §15 Q15), and works the same.
    #[tokio::test(flavor = "multi_thread")]
    async fn when_registration_fails_the_sign_in_falls_back_to_the_cli_client() {
        let f = Fixture::new("osc-fallback", &[VENUE], testing::SIGNED_IN);
        f.osc.refuse_registration();
        let view = f.model.sign_in().await.unwrap();
        assert!(view.ready, "{view:?}");
        let ids = f.osc.token_client_ids();
        assert!(
            !ids.is_empty() && ids.iter().all(|id| id == "cli"),
            "{ids:?}"
        );
        assert!(matches!(
            f.kept(),
            Some(StoredCredential::SignIn { client_id, .. }) if client_id == "cli"
        ));
    }

    /// Two instances: nothing is saved until one is picked; a pick outside the list
    /// is refused; the pick is saved and passes the CLI's `check`.
    #[tokio::test(flavor = "multi_thread")]
    async fn with_several_instances_one_is_picked_and_saved() {
        let f = Fixture::new("osc-pick", &[VENUE, REHEARSAL], testing::SIGNED_IN);
        let view = f.model.sign_in().await.unwrap();
        assert!(!view.ready);
        assert_eq!(view.summary, None);
        assert_eq!(view.instances.len(), 2);
        assert_eq!(view.workspace.as_deref(), Some(testing::WORKSPACE));
        assert_eq!(f.file(), "", "nothing saved before an instance is picked");

        let err = f
            .model
            .choose_instance("https://elsewhere.example.com")
            .unwrap_err();
        assert!(
            err.to_string().contains("not in workspace venue-ws"),
            "{err}"
        );
        assert_eq!(f.file(), "");

        let view = f.model.choose_instance(REHEARSAL.1).unwrap();
        assert!(view.ready);
        assert_eq!(
            view.summary.as_deref(),
            Some("Sending to rehearsal (venue-ws)")
        );
        assert_eq!(f.saved().open_live.url.as_deref(), Some(REHEARSAL.1));
        assert!(f.saved().open_live.uses_keychain());
        testing::cli_check(&f.dir);
    }

    /// A later launch: the file and the keychain are all there is. The view shows what
    /// is saved at once, then the lookup names the instance and the workspace.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_later_launch_shows_sending_to_instance_and_workspace() {
        let f = Fixture::new("osc-later", &[VENUE, REHEARSAL], testing::SIGNED_IN);
        f.model.sign_in().await.unwrap();
        f.model.choose_instance(VENUE.1).unwrap();

        let later = WhereToSend::new(
            f.dir.settings(),
            f.keychain.clone(),
            f.osc.endpoints(),
            Duration::from_secs(10),
            testing::browser(testing::SIGNED_IN),
        );
        let before = later.view();
        assert!(before.ready);
        assert_eq!(
            before.summary.as_deref(),
            Some("Sending to venue.example.osaas.io")
        );
        assert!(later.needs_lookup());

        let after = later.refresh().await;
        assert_eq!(
            after.summary.as_deref(),
            Some("Sending to venue (venue-ws)")
        );
        assert_eq!(after.instances.len(), 2);
        assert!(!later.needs_lookup());
        assert!(
            matches!(f.kept(), Some(StoredCredential::SignIn { refresh_token, .. }) if refresh_token == testing::RENEWED),
            "the lookup renewed the kept sign-in"
        );
    }

    /// A lookup that fails says so and still shows what is saved.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_lookup_keeps_showing_the_saved_choice() {
        let f = Fixture::new("osc-offline", &[VENUE], testing::SIGNED_IN);
        f.model.sign_in().await.unwrap();
        f.osc.refuse_refresh();
        let later = WhereToSend::new(
            f.dir.settings(),
            f.keychain.clone(),
            f.osc.endpoints(),
            Duration::from_secs(10),
            testing::browser(testing::SIGNED_IN),
        );
        let view = later.refresh().await;
        assert!(view.ready);
        assert_eq!(view.url.as_deref(), Some(VENUE.1));
        let problem = view.problem.unwrap();
        assert!(
            problem.starts_with("Could not look up your Open Source Cloud workspace"),
            "{problem}"
        );
    }

    /// Changing workspace is signing in again. A saved instance that is not in the new
    /// workspace is not kept as ready; the person picks one of the new list.
    #[tokio::test(flavor = "multi_thread")]
    async fn signing_in_again_to_another_workspace_asks_for_an_instance() {
        let f = Fixture::new("osc-change", &[VENUE], testing::SIGNED_IN);
        f.model.sign_in().await.unwrap();
        f.osc
            .set_instances(&[("main", "https://main.example.osaas.io"), REHEARSAL]);

        let view = f.model.sign_in().await.unwrap();
        assert!(!view.ready, "{view:?}");
        assert_eq!(view.summary, None);
        assert_eq!(
            view.problem.as_deref(),
            Some("The saved instance is not in workspace venue-ws. Pick one.")
        );
        let view = f.model.choose_instance(REHEARSAL.1).unwrap();
        assert!(view.ready);
        assert_eq!(f.saved().open_live.url.as_deref(), Some(REHEARSAL.1));
    }

    /// A workspace without Open Live says so, and saves nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_workspace_without_open_live_says_so() {
        let f = Fixture::new("osc-empty", &[], testing::SIGNED_IN);
        let view = f.model.sign_in().await.unwrap();
        assert!(!view.ready);
        assert!(
            view.problem.as_deref().is_some_and(
                |p| p.starts_with("There is no Open Live instance in workspace venue-ws")
            ),
            "{view:?}"
        );
        assert_eq!(f.file(), "");
    }

    /// Declining on the consent page keeps nothing, and a second sign-in started
    /// while one waits in the browser is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_declined_sign_in_keeps_nothing() {
        let f = Fixture::new("osc-declined", &[VENUE], testing::DECLINED);
        let err = f.model.sign_in().await.unwrap_err();
        assert!(format!("{err:#}").contains("access_denied"), "{err:#}");
        assert_eq!(f.kept(), None);
        assert_eq!(f.file(), "");
        assert_eq!(f.model.view(), empty_view());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_one_sign_in_waits_at_a_time() {
        let f = Fixture::new("osc-twice", &[VENUE], testing::NEVER);
        let model = Arc::new(f.model);
        let first = tokio::spawn({
            let model = model.clone();
            async move { model.sign_in().await }
        });
        // The first is waiting for the browser once the page has been opened.
        for _ in 0..100 {
            if model.signing_in.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let err = model.sign_in().await.unwrap_err();
        assert!(err.to_string().contains("already waiting"), "{err}");
        first.abort();
    }

    /// Advanced: a pasted personal access token is checked with Open Source Cloud,
    /// then kept in the keychain, never in the file.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pasted_token_is_checked_and_kept_in_the_keychain() {
        let f = Fixture::new("osc-pat", &[VENUE], testing::SIGNED_IN);
        let err = f.model.paste_token("not-a-token").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("did not accept that token"),
            "{err:#}"
        );
        assert_eq!(f.kept(), None, "a token that does not work is not kept");

        let view = f
            .model
            .paste_token(&format!("  {}\n", testing::ACCESS))
            .await
            .unwrap();
        assert_eq!(view.summary.as_deref(), Some("Sending to venue (venue-ws)"));
        assert_eq!(
            f.kept(),
            Some(StoredCredential::Token {
                token: testing::ACCESS.into(),
                expires_at: None,
            })
        );
        let file = f.file();
        assert!(!file.contains(testing::ACCESS), "{file}");
        assert!(file.contains("credential = \"keychain\""), "{file}");
        testing::cli_check(&f.dir);
    }

    /// Self-hosted: the address and key are checked against the fake Open Live before
    /// anything is saved; then the file has them, and the CLI's `check` accepts it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_self_hosted_open_live_is_checked_then_saved() {
        let f = Fixture::new("self-hosted", &[VENUE], testing::SIGNED_IN);
        let open_live = testing::fake_open_live("the-key");

        let err = f
            .model
            .self_hosted(&open_live, "wrong-key")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("could not reach Open Live"),
            "{err}"
        );
        assert_eq!(f.file(), "", "nothing saved before the check passes");

        let err = f
            .model
            .self_hosted("https://venue.example.osaas.io", "k")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Open Source Cloud sign-in"),
            "{err}"
        );

        let view = f
            .model
            .self_hosted(&format!(" {open_live} "), " the-key ")
            .await
            .unwrap();
        assert_eq!(view.mode, Mode::SelfHosted);
        assert!(view.ready && view.api_key_set && !view.signed_in);
        assert_eq!(
            view.summary,
            Some(format!("Sending to {}", host(&open_live)))
        );
        let saved = f.saved();
        assert_eq!(saved.open_live.url.as_deref(), Some(open_live.as_str()));
        assert_eq!(saved.open_live.auth_mode, AuthMode::Direct);
        assert_eq!(saved.open_live.api_key.as_deref(), Some("the-key"));
        testing::cli_check(&f.dir);
    }

    /// Going from Open Source Cloud to self-hosted removes the sign-in from the keychain
    /// and the marker from the file, which `check` would otherwise refuse.
    #[tokio::test(flavor = "multi_thread")]
    async fn switching_to_self_hosted_forgets_the_sign_in() {
        let f = Fixture::new("osc-to-self", &[VENUE], testing::SIGNED_IN);
        f.model.sign_in().await.unwrap();
        assert!(f.kept().is_some());

        let open_live = testing::fake_open_live("");
        let view = f.model.self_hosted(&open_live, "").await.unwrap();
        assert!(view.ready && !view.api_key_set);
        assert_eq!(view.workspace, None);
        assert_eq!(f.kept(), None);
        let saved = f.saved();
        assert_eq!(saved.open_live.credential, None);
        assert_eq!(saved.open_live.api_key, None);
        testing::cli_check(&f.dir);
    }

    /// Each change reloads the file and touches only `open_live`, so what another step
    /// saved in between is kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_keeps_the_rest_of_the_file() {
        let f = Fixture::new("keeps-rest", &[VENUE, REHEARSAL], testing::SIGNED_IN);
        f.model.sign_in().await.unwrap();
        let mut other = Config::default();
        other.gateway.name = "Stage door".into();
        other.video.bitrate_kbps = 4321;
        other.open_live.register = false;
        other.uplink.host = Some("cloud.example.com".into());
        other.uplink.port_range = Some("47110-47129".into());
        api::save_to(&f.dir.settings(), &other).unwrap();

        f.model.choose_instance(VENUE.1).unwrap();
        let saved = f.saved();
        assert_eq!(saved.gateway.name, "Stage door");
        assert_eq!(saved.video.bitrate_kbps, 4321);
        assert_eq!(saved.open_live.url.as_deref(), Some(VENUE.1));
    }

    #[test]
    fn an_address_without_a_scheme_is_https() {
        assert_eq!(
            with_scheme(" open-live.example.com "),
            "https://open-live.example.com"
        );
        assert_eq!(
            with_scheme("http://192.0.2.4:8000"),
            "http://192.0.2.4:8000"
        );
        assert_eq!(with_scheme("  "), "");
    }

    #[test]
    fn a_host_names_an_address_without_a_listed_name() {
        assert_eq!(
            host("https://open-live.example.com/"),
            "open-live.example.com"
        );
        assert_eq!(host("http://192.0.2.4:8000"), "192.0.2.4:8000");
        assert_eq!(host("https://user:pw@example.com/x?y"), "example.com");
        assert_eq!(host("example.com"), "example.com");
    }
}
