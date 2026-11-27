//! In-TUI `/login`, `/logout`, and `/setup` backend plumbing on [`ChatBackend`]:
//! background OAuth PKCE & RFC 8628 device-code tasks, pre-save API-key validation probes,
//! best-effort 9e catalog discovery refresh, guarded logout, and live runtime hot-reload.

use super::ChatBackend;
use crate::auth;
use crate::config::Config;
use crate::logout;
use crate::paths;
use crate::setup;
use crate::setup_wizard;
use crate::wiring::build_runtime_with_tools;
use hrn_edge_http::HttpClient;
use hrn_edge_llm::discover_provider;
use hrn_edge_oauth::{LoginIo, Subscription, run_pkce_login_interactive, run_subscription_login};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::{CancelFlag, HrnError, Result, WorkspaceFs};
use hrn_tui::{
    LoginOutcome, LoginProviderChoice, LoginSubmission, LoginUiEvent, LogoutProviderChoice,
    SetupWizardInfo, UiEvent, UiSender,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const POST_LOGIN_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// In-flight in-TUI OAuth/device-code login handle (paste channel + cancel flag).
#[derive(Default)]
pub(super) struct ActiveLogin {
    paste_tx: Option<mpsc::UnboundedSender<String>>,
    cancel: Option<CancelFlag>,
    #[cfg(test)]
    pub(crate) overrides: LoginTestOverrides,
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct LoginTestOverrides {
    pub(crate) auth_dir: Option<PathBuf>,
    pub(crate) home_dir: Option<PathBuf>,
    pub(crate) subscription_urls: std::collections::HashMap<String, (String, String)>,
}

impl ActiveLogin {
    fn reset(&mut self) -> (mpsc::UnboundedReceiver<String>, CancelFlag) {
        self.cancel();
        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancelFlag::new();
        self.paste_tx = Some(tx);
        self.cancel = Some(cancel.clone());
        (rx, cancel)
    }

    fn submit_code(&self, input: &str) -> Result {
        let Some(tx) = &self.paste_tx else {
            return Err(HrnError::Auth(
                "no active OAuth login waiting for a callback code".into(),
            ));
        };
        tx.send(input.to_owned())
            .map_err(|_| HrnError::Auth("OAuth login task is no longer waiting for input".into()))
    }

    fn cancel(&mut self) {
        if let Some(flag) = self.cancel.take() {
            flag.cancel();
        }
        self.paste_tx = None;
    }
}

impl ChatBackend {
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "uses test overrides in #[cfg(test)]")
    )]
    fn effective_home(&self) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(dir) = &self.login_state.overrides.home_dir {
            return Some(dir.clone());
        }
        StdWorkspaceFs.home_dir()
    }

    fn effective_auth_dir(&self) -> Result<PathBuf> {
        #[cfg(test)]
        if let Some(dir) = &self.login_state.overrides.auth_dir {
            return Ok(dir.clone());
        }
        paths::auth_dir(self.effective_home().as_deref())
    }

    pub(super) fn login_providers_live(&self) -> Vec<LoginProviderChoice> {
        let auth_dir = self.effective_auth_dir().ok();
        setup::provider_choices_in(Some(&self.cfg), auth_dir.as_deref())
    }

    /// Synchronously validate (probe), persist, refresh catalog, and hot-reload one API-key login.
    pub(super) fn save_login_probed(
        &mut self,
        submission: LoginSubmission,
    ) -> Result<LoginOutcome> {
        let http = HttpClient::new()?;
        let config_path = self.config_path.clone();
        let cwd = self.cwd.clone();
        let home = self.effective_home();
        let auth_dir = self.effective_auth_dir().ok();
        let cache_dir = crate::app_data_dir().ok().map(|d| d.join("catalog-cache"));
        let handle = self.handle.clone();
        let allow_plaintext_http = self.cfg.allow_plaintext_http;
        let (new_cfg, outcome) = run_async_safe(&handle, async move {
            let (cfg, out) = setup::probe_and_apply_login(
                &http,
                &config_path,
                home.as_deref(),
                &submission,
                allow_plaintext_http,
            )
            .await?;
            let mut new_cfg =
                reload_config_for(&config_path, &cwd, home.as_deref(), auth_dir.as_deref())
                    .unwrap_or(cfg);
            refresh_provider_catalog(&http, &mut new_cfg, &out.provider, cache_dir.as_deref())
                .await;
            Ok((new_cfg, out))
        })?;
        self.hot_reload_config(new_cfg)?;
        Ok(outcome)
    }

    /// Asynchronously validate (probe) and persist an API-key submission without blocking the TUI.
    pub(super) fn start_key_login_live(
        &mut self,
        submission: LoginSubmission,
    ) -> Result<Option<LoginOutcome>> {
        let allow_plaintext_http = self.cfg.allow_plaintext_http;
        let _ = setup::probe_target_for(&submission, allow_plaintext_http)?;
        let http = HttpClient::new()?;
        let config_path = self.config_path.clone();
        let cwd = self.cwd.clone();
        let home = self.effective_home();
        let auth_dir = self.effective_auth_dir().ok();
        let cache_dir = crate::app_data_dir().ok().map(|d| d.join("catalog-cache"));
        let tx = self.tx.clone();
        let reload_slot = self.login_reload.clone();

        self.handle.spawn(async move {
            let _ = tx.send(UiEvent::Login(LoginUiEvent::Progress(
                "Validating API key…".to_owned(),
            )));
            match setup::probe_and_apply_login(
                &http,
                &config_path,
                home.as_deref(),
                &submission,
                allow_plaintext_http,
            )
            .await
            {
                Ok((cfg, outcome)) => {
                    let mut new_cfg =
                        reload_config_for(&config_path, &cwd, home.as_deref(), auth_dir.as_deref())
                            .unwrap_or(cfg);
                    refresh_provider_catalog(
                        &http,
                        &mut new_cfg,
                        &outcome.provider,
                        cache_dir.as_deref(),
                    )
                    .await;
                    reload_slot.store(new_cfg);
                    let _ = tx.send(UiEvent::Login(LoginUiEvent::Finished(Ok(outcome))));
                }
                Err(err) => {
                    let _ = tx.send(UiEvent::Login(LoginUiEvent::ProbeFailed(err.to_string())));
                }
            }
        });
        Ok(None)
    }

    /// Start an in-TUI OAuth PKCE or RFC 8628 device-code login on a background task.
    pub(super) fn start_login_live(&mut self, provider: &str) -> Result {
        setup::reject_google_oauth(provider)?;
        if let Some(kind) = Subscription::from_id(provider) {
            return self.spawn_subscription_login(kind);
        }
        if let Some(pkce_cfg) = self.cfg.oauth_logins.get(provider).cloned() {
            return self.spawn_custom_pkce_login(provider.to_owned(), pkce_cfg);
        }
        Err(HrnError::Config(format!(
            "provider `{provider}` does not support OAuth login"
        )))
    }

    fn spawn_subscription_login(&mut self, kind: Subscription) -> Result {
        let http = HttpClient::new()?;
        let auth_dir = self.effective_auth_dir()?;
        let home = self.effective_home();
        let config_path = self.config_path.clone();
        let cwd = self.cwd.clone();
        let cache_dir = crate::app_data_dir().ok().map(|d| d.join("catalog-cache"));
        let tx = self.tx.clone();
        let reload_slot = self.login_reload.clone();
        #[cfg(test)]
        let url_override = self
            .login_state
            .overrides
            .subscription_urls
            .get(kind.id())
            .cloned();
        let (mut paste_rx, cancel) = self.login_state.reset();

        self.handle.spawn(async move {
            let file = auth_dir.join(kind.file_name());
            let tx_url = tx.clone();
            let on_url = move |url: &str| {
                auth::try_open_browser_if_enabled(url);
                let _ = tx_url.send(UiEvent::Login(LoginUiEvent::ShowUrl {
                    provider: kind.id().to_owned(),
                    label: kind.label().to_owned(),
                    url: url.to_owned(),
                    user_code: None,
                    message:
                        "Open this URL in your browser (or paste the redirect URL / code below):"
                            .to_owned(),
                    needs_code: true,
                }));
            };
            let tx_dev = tx.clone();
            let on_device = move |resp: &hrn_edge_oauth::DeviceAuthResponse| {
                let url = resp.display_url();
                auth::try_open_browser_if_enabled(url);
                let _ = tx_dev.send(UiEvent::Login(LoginUiEvent::ShowUrl {
                    provider: kind.id().to_owned(),
                    label: kind.label().to_owned(),
                    url: url.to_owned(),
                    user_code: Some(resp.user_code.clone()),
                    message: "Open the URL and enter the code above. Waiting for approval…"
                        .to_owned(),
                    needs_code: false,
                }));
            };
            let tx_note = tx.clone();
            let on_notice = move |msg: &str| {
                let _ = tx_note.send(UiEvent::Login(LoginUiEvent::Progress(msg.to_owned())));
            };
            let io = LoginIo {
                manual: false,
                pasted: Some(&mut paste_rx),
                on_url: &on_url,
                on_notice: &on_notice,
                on_device_code: Some(&on_device),
                cancel: Some(cancel),
            };
            #[cfg(test)]
            let res = if let Some((auth_u, tok_u)) = url_override {
                hrn_edge_oauth::run_subscription_login_with_urls(
                    &http, kind, &file, &auth_u, &tok_u, io,
                )
                .await
            } else {
                run_subscription_login(&http, kind, &file, io).await
            };
            #[cfg(not(test))]
            let res = run_subscription_login(&http, kind, &file, io).await;

            match res {
                Ok(cred) => {
                    let who = cred.account_label().unwrap_or("default").to_owned();
                    let outcome = finish_subscription_reload(
                        &http,
                        kind,
                        &who,
                        &config_path,
                        &cwd,
                        home.as_deref(),
                        &auth_dir,
                        cache_dir.as_deref(),
                        &reload_slot,
                    )
                    .await;
                    send_finish(&tx, outcome);
                }
                Err(HrnError::Cancelled) => {}
                Err(err) => {
                    let _ = tx.send(UiEvent::Login(LoginUiEvent::Finished(Err(err.to_string()))));
                }
            }
        });
        Ok(())
    }

    fn spawn_custom_pkce_login(
        &mut self,
        provider: String,
        pkce_cfg: hrn_edge_oauth::PkceLoginConfig,
    ) -> Result {
        let http = HttpClient::new()?;
        let config_path = self.config_path.clone();
        let cwd = self.cwd.clone();
        let home = self.effective_home();
        let auth_dir = self.effective_auth_dir().ok();
        let cache_dir = crate::app_data_dir().ok().map(|d| d.join("catalog-cache"));
        let tx = self.tx.clone();
        let reload_slot = self.login_reload.clone();
        let (mut paste_rx, cancel) = self.login_state.reset();

        self.handle.spawn(async move {
            let tx_url = tx.clone();
            let prov_label = provider.clone();
            let on_url = move |url: &str| {
                auth::try_open_browser_if_enabled(url);
                let _ = tx_url.send(UiEvent::Login(LoginUiEvent::ShowUrl {
                    provider: prov_label.clone(),
                    label: prov_label.clone(),
                    url: url.to_owned(),
                    user_code: None,
                    message:
                        "Open this URL in your browser (or paste the redirect URL / code below):"
                            .to_owned(),
                    needs_code: true,
                }));
            };
            let tx_note = tx.clone();
            let on_notice = move |msg: &str| {
                let _ = tx_note.send(UiEvent::Login(LoginUiEvent::Progress(msg.to_owned())));
            };
            match run_pkce_login_interactive(
                &http,
                &pkce_cfg,
                false,
                Some(&mut paste_rx),
                &on_url,
                &on_notice,
                Some(&cancel),
            )
            .await
            {
                Ok(_) => {
                    let res =
                        reload_config_for(&config_path, &cwd, home.as_deref(), auth_dir.as_deref());
                    match res {
                        Ok(mut new_cfg) => {
                            refresh_provider_catalog(
                                &http,
                                &mut new_cfg,
                                &provider,
                                cache_dir.as_deref(),
                            )
                            .await;
                            let aliases: Vec<String> = new_cfg
                                .llm
                                .presets
                                .iter()
                                .filter(|(_, p)| p.provider == provider)
                                .map(|(a, _)| a.clone())
                                .collect();
                            reload_slot.store(new_cfg);
                            let summary = format!("Logged in to `{provider}` via OAuth2 PKCE");
                            let _ =
                                tx.send(UiEvent::Login(LoginUiEvent::Finished(Ok(LoginOutcome {
                                    provider,
                                    aliases,
                                    summary,
                                }))));
                        }
                        Err(err) => {
                            let _ = tx
                                .send(UiEvent::Login(LoginUiEvent::Finished(Err(err.to_string()))));
                        }
                    }
                }
                Err(HrnError::Cancelled) => {}
                Err(err) => {
                    let _ = tx.send(UiEvent::Login(LoginUiEvent::Finished(Err(err.to_string()))));
                }
            }
        });
        Ok(())
    }

    pub(super) fn submit_login_code_live(&mut self, input: &str) -> Result {
        self.login_state.submit_code(input)
    }

    pub(super) fn cancel_login_live(&mut self) {
        self.login_state.cancel();
    }

    pub(super) fn finish_login_live(&mut self) {
        self.login_state.cancel();
        if let Some(new_cfg) = self.login_reload.take() {
            let _ = self.hot_reload_config(new_cfg);
        }
    }

    pub(super) fn logout_providers_live(&self) -> Vec<LogoutProviderChoice> {
        let Ok(auth_dir) = self.effective_auth_dir() else {
            return Vec::new();
        };
        logout::list_logout_choices(Some(&self.cfg), &self.config_path, &auth_dir)
    }

    pub(super) fn logout_provider_live(&mut self, provider: &str) -> Result<String> {
        let auth_dir = self.effective_auth_dir()?;
        let summary = logout::remove_provider_credentials(
            Some(&self.cfg),
            &self.config_path,
            &auth_dir,
            provider,
        )?;
        self.background.pool.clear_provider(provider);
        self.background.pool.clear_all_invalidated();
        let home = self.effective_home();
        let next_cfg = match reload_config_for(
            &self.config_path,
            &self.cwd,
            home.as_deref(),
            Some(&auth_dir),
        ) {
            Ok(cfg) => cfg,
            Err(_) => {
                // Keep structural config when the deleted credential was referenced by `roles.default`;
                // rebuilding the router ensures the deleted credential is immediately unauthenticated.
                self.cfg.clone()
            }
        };
        self.hot_reload_config(next_cfg)?;
        Ok(summary)
    }

    pub(super) fn setup_info_live(&self) -> SetupWizardInfo {
        let auth_dir = self.effective_auth_dir().unwrap_or_default();
        let themes_dir = crate::app_data_dir().ok().map(|d| d.join("themes"));
        setup_wizard::collect_setup_info(Some(&self.cfg), &auth_dir, themes_dir.as_deref())
    }

    pub(super) fn apply_setup_theme_live(&mut self, theme: &str) -> Result<String> {
        let themes_dir = crate::app_data_dir().ok().map(|d| d.join("themes"));
        let msg = setup_wizard::persist_theme_in(&self.config_path, theme, themes_dir.as_deref())?;
        self.cfg.appearance.theme = hrn_tui::ThemeConfig::legacy(theme);
        Ok(msg)
    }

    pub(super) fn hot_reload_config(&mut self, mut new_cfg: Config) -> Result {
        self.background.reapply_discovered(&mut new_cfg);
        self.background.pool.clear_all_invalidated();
        let new_runtime = build_runtime_with_tools(
            &new_cfg,
            Arc::clone(&self.sqlite),
            &self.cwd,
            &self.mcp_tools,
            &self.background,
        )?;
        self.runtime = new_runtime;
        self.idle.sync_runtime(&self.runtime);
        self.llm = new_cfg.llm.clone();
        self.model_names = crate::stats::names(&new_cfg.llm);
        self.cfg = new_cfg;
        Ok(())
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "post-login reload threads http, kind, label, config/home/auth/cache paths and reload slot"
)]
async fn finish_subscription_reload(
    http: &HttpClient,
    kind: Subscription,
    who: &str,
    config_path: &Path,
    cwd: &Path,
    home: Option<&Path>,
    auth_dir: &Path,
    cache_dir: Option<&Path>,
    reload_slot: &super::reload::ReloadSlot,
) -> Result<LoginOutcome> {
    if !config_path.exists() {
        let _ = auth::seed_config_with_auth_dir(config_path, kind, home, auth_dir);
    }
    let mut new_cfg = reload_config_for(config_path, cwd, home, Some(auth_dir))?;
    refresh_provider_catalog(http, &mut new_cfg, kind.id(), cache_dir).await;
    let aliases: Vec<String> = new_cfg
        .llm
        .presets
        .iter()
        .filter(|(_, p)| {
            new_cfg
                .llm
                .providers
                .get(&p.provider)
                .is_some_and(|prov| prov.auth_mode.subscription() == Some(kind))
        })
        .map(|(a, _)| a.clone())
        .collect();
    reload_slot.store(new_cfg);
    Ok(LoginOutcome {
        provider: kind.id().to_owned(),
        aliases,
        summary: format!("Logged in to {} ({who})", kind.label()),
    })
}

fn send_finish(tx: &UiSender, outcome: Result<LoginOutcome>) {
    let payload = outcome.map_err(|e| e.to_string());
    let _ = tx.send(UiEvent::Login(LoginUiEvent::Finished(payload)));
}

fn reload_config_for(
    config_path: &Path,
    cwd: &Path,
    home: Option<&Path>,
    auth_dir: Option<&Path>,
) -> Result<Config> {
    let project_path = paths::project_config_path(cwd);
    let input = crate::config_layers::LayerInput {
        global_path: Some(config_path.to_path_buf()),
        project_path: project_path.is_file().then_some(project_path),
        overlays: Vec::new(),
        cwd: cwd.to_path_buf(),
        home: home.map(Path::to_path_buf),
        env: std::collections::BTreeMap::new(),
        runtime: toml::Table::new(),
    };
    let catalogs = match (home, auth_dir) {
        (Some(h), Some(ad)) => hrn_edge_llm::standard_catalogs(h, ad),
        _ => Vec::new(),
    };
    let (cfg, _) = Config::load_layered_with_catalogs(&input, catalogs)?;
    Ok(cfg)
}

async fn refresh_provider_catalog(
    http: &HttpClient,
    cfg: &mut Config,
    provider_name: &str,
    cache_dir: Option<&Path>,
) {
    let Some(entry) = cfg.llm.providers.get(provider_name).cloned() else {
        return;
    };
    if let Some(cat) = discover_provider(
        http,
        provider_name,
        &entry,
        cache_dir,
        POST_LOGIN_DISCOVERY_TIMEOUT,
    )
    .await
    {
        cfg.llm.absorb(cat);
    }
}

/// Execute an async future on `handle` safely whether the caller is on a blocking thread or
/// inside a tokio worker thread (never panics with "Cannot start a runtime from within a runtime").
fn run_async_safe<F, T>(handle: &tokio::runtime::Handle, fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send,
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| handle.block_on(fut))
                .join()
                .map_err(|_| HrnError::Internal("login probe helper thread panicked".into()))?
        })
    } else {
        handle.block_on(fut)
    }
}
