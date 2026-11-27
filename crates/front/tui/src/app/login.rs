//! `/login`, `/logout`, and `/setup` TUI workflows: in-TUI OAuth/device-code flows, redirect URL
//! completion, probed API-key submission, credential logout, and the 3-step onboarding wizard.

use super::App;
use crate::login::{LoginAction, LoginView};
use crate::logout::{LogoutAction, LogoutView};
use crate::port::{LoginOutcome, LoginSubmission, LoginUiEvent};
use crate::setup_wizard::{SetupAction, SetupStep, SetupWizardView};
use crate::theme;
use crate::transcript::Level;
use crossterm::event::KeyEvent;

/// Blocked Google/Antigravity OAuth names that must never initiate an OAuth flow.
const BLOCKED_GOOGLE_OAUTH: &[&str] = &[
    "google",
    "antigravity",
    "google-antigravity",
    "gemini-cli",
    "gemini-oauth",
    "code-assist",
    "cloudcode",
];

fn is_blocked_google_oauth(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    BLOCKED_GOOGLE_OAUTH.iter().any(|blocked| lower == *blocked)
}

/// Detect `/login <redirect-url>` or `/login <code#state>` carrying an OAuth callback.
pub(super) fn is_oauth_redirect_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    let is_http = trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || trimmed
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"));
    if is_http
        && (trimmed.contains("code=")
            || trimmed.contains("state=")
            || trimmed.contains("error=")
            || trimmed.contains('#'))
    {
        return true;
    }
    !trimmed.contains(char::is_whitespace)
        && trimmed
            .split_once('#')
            .is_some_and(|(code, state)| !code.is_empty() && !state.is_empty())
}

impl App {
    pub(super) fn handle_login(&mut self, arg: Option<&str>) {
        let Some(raw) = arg.map(str::trim).filter(|s| !s.is_empty()) else {
            self.open_login_picker();
            return;
        };
        if is_oauth_redirect_url(raw) {
            let clean = crate::login_draw::sanitize_oauth_input(raw);
            match self.backend.submit_login_code(&clean) {
                Ok(()) => self.flash("submitted OAuth callback"),
                Err(err) => self.notice(Level::Warn, err.to_string()),
            }
            return;
        }
        if let Some((provider, key)) = raw.split_once(char::is_whitespace) {
            let key = key.trim();
            if !key.is_empty() {
                if is_blocked_google_oauth(provider) {
                    self.notice(
                        Level::Error,
                        "Google / Antigravity OAuth is blocked by ToS guard; use `/login gemini` with an official Gemini API key",
                    );
                    return;
                }
                self.submit_inline_login(LoginSubmission {
                    provider_id: provider.to_ascii_lowercase(),
                    custom_name: None,
                    api_key: key.to_owned(),
                    base_url: None,
                    model_id: None,
                });
                return;
            }
        }
        if is_blocked_google_oauth(raw) {
            self.notice(
                Level::Error,
                "Google / Antigravity OAuth is blocked by ToS guard; use `/login gemini` with an official Gemini API key",
            );
            return;
        }
        let providers = self.backend.login_providers();
        if let Some(choice) = providers
            .iter()
            .find(|p| p.id.eq_ignore_ascii_case(raw))
            .cloned()
        {
            let start_id = choice.is_oauth.then(|| choice.id.clone());
            self.login = Some(LoginView::for_provider(providers, choice));
            if let Some(id) = start_id {
                self.start_oauth_login(&id);
            }
            return;
        }
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        if ids.is_empty() {
            self.notice(Level::Warn, "no login providers available in this backend");
        } else {
            self.notice(
                Level::Warn,
                format!("unknown provider `{raw}` · available: {}", ids.join(", ")),
            );
        }
    }

    fn open_login_picker(&mut self) {
        let providers = self.backend.login_providers();
        if providers.is_empty() {
            self.notice(Level::Warn, "no login providers available in this backend");
            return;
        }
        self.login = Some(LoginView::new(providers));
    }

    fn start_oauth_login(&mut self, provider_id: &str) {
        if is_blocked_google_oauth(provider_id) {
            if let Some(login) = &mut self.login {
                login.on_event(&LoginUiEvent::Finished(Err(
                    "Google / Antigravity OAuth is blocked by ToS guard".to_owned(),
                )));
            }
            return;
        }
        if let Err(err) = self.backend.start_login(provider_id)
            && let Some(login) = &mut self.login
        {
            login.on_event(&LoginUiEvent::Finished(Err(err.to_string())));
        }
    }

    fn submit_inline_login(&mut self, submission: LoginSubmission) {
        match self.backend.start_key_login(submission) {
            Ok(Some(outcome)) => self.apply_login_outcome(outcome),
            Ok(None) => self.flash("validating API key…"),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    fn submit_modal_login(&mut self, submission: LoginSubmission) {
        match self.backend.start_key_login(submission) {
            Ok(Some(outcome)) => {
                self.login = None;
                self.apply_login_outcome(outcome);
            }
            Ok(None) => {
                if let Some(login) = &mut self.login {
                    login.set_validating("Validating API key…");
                }
            }
            Err(err) => {
                // Keep the modal open on key entry with the sanitized error so the user can retry.
                if let Some(login) = &mut self.login {
                    login.set_form_error(err.to_string());
                } else {
                    self.notice(Level::Error, err.to_string());
                }
            }
        }
    }

    fn apply_login_outcome(&mut self, outcome: LoginOutcome) {
        let aliases = self.backend.model_aliases();
        if let Some(first) = outcome.aliases.first()
            && !aliases.iter().any(|a| a == &self.settings.model)
        {
            self.settings.model.clone_from(first);
            self.context_window = self.backend.context_window(&self.settings.model);
            self.backend.apply_settings(&self.settings);
        }
        self.flash(format!("logged in · {}", outcome.provider));
        self.notice(Level::Info, outcome.summary);
        if let Some(setup) = &mut self.setup {
            setup.refresh_and_advance(self.backend.setup_info());
        }
    }

    pub(super) fn on_login_event(&mut self, event: LoginUiEvent) {
        match event {
            LoginUiEvent::Finished(Ok(outcome)) => {
                self.backend.finish_login();
                self.login = None;
                self.apply_login_outcome(outcome);
            }
            LoginUiEvent::ProbeFailed(err) => {
                if let Some(login) = &mut self.login {
                    login.set_form_error(err);
                } else {
                    self.notice(Level::Error, err);
                }
            }
            LoginUiEvent::Finished(Err(err)) => {
                if let Some(login) = &mut self.login {
                    login.on_event(&LoginUiEvent::Finished(Err(err)));
                } else {
                    self.notice(Level::Error, err);
                }
            }
            other => {
                if let Some(login) = &mut self.login {
                    login.on_event(&other);
                }
            }
        }
    }

    pub(super) fn login_key(&mut self, key: KeyEvent, burst: bool) -> bool {
        let Some(login) = &mut self.login else {
            return false;
        };
        match login.key_with_burst(key, burst) {
            LoginAction::Stay => {}
            LoginAction::Close => {
                self.login = None;
                if let Some(setup) = &mut self.setup {
                    setup.refresh_info(self.backend.setup_info());
                }
            }
            LoginAction::StartOAuth(provider) => self.start_oauth_login(&provider),
            LoginAction::SubmitCode(code) => {
                if let Err(err) = self.backend.submit_login_code(&code)
                    && let Some(login) = &mut self.login
                {
                    login.on_event(&LoginUiEvent::Finished(Err(err.to_string())));
                }
            }
            LoginAction::CancelOAuth => {
                self.backend.cancel_login();
                if self.login.as_ref().is_some_and(LoginView::is_oauth_active) {
                    self.login = None;
                }
                self.flash("login cancelled");
            }
            LoginAction::Submit(sub) => self.submit_modal_login(sub),
        }
        true
    }

    pub(super) fn handle_logout(&mut self, arg: Option<&str>) {
        let choices = self.backend.logout_providers();
        if choices.is_empty() {
            self.notice(Level::Info, "no stored provider credentials to log out of");
            return;
        }
        if let Some(target) = arg.map(str::trim).filter(|s| !s.is_empty()) {
            let Some(choice) = choices
                .into_iter()
                .find(|c| c.id.eq_ignore_ascii_case(target))
            else {
                self.notice(
                    Level::Warn,
                    format!("no stored credentials found for `{target}`"),
                );
                return;
            };
            self.logout = Some(LogoutView::confirm_single(choice));
            return;
        }
        self.logout = Some(LogoutView::new(choices));
    }

    pub(super) fn logout_key(&mut self, key: KeyEvent) -> bool {
        let Some(view) = &mut self.logout else {
            return false;
        };
        match view.key(key) {
            LogoutAction::Stay => {}
            LogoutAction::Close => self.logout = None,
            LogoutAction::Confirm(provider) => {
                self.logout = None;
                match self.backend.logout_provider(&provider) {
                    Ok(summary) => {
                        self.context_window = self.backend.context_window(&self.settings.model);
                        self.flash(format!("logged out · {provider}"));
                        self.notice(Level::Info, summary);
                    }
                    Err(err) => self.notice(Level::Error, err.to_string()),
                }
            }
        }
        true
    }

    pub(super) fn handle_setup(&mut self) {
        let info = self.backend.setup_info();
        self.setup = Some(SetupWizardView::new(info));
    }

    pub(super) fn setup_key(&mut self, key: KeyEvent) -> bool {
        let Some(view) = &mut self.setup else {
            return false;
        };
        match view.key(key) {
            SetupAction::Stay => {}
            SetupAction::Close => self.setup = None,
            SetupAction::OpenLogin => self.open_login_picker(),
            SetupAction::SetDefaultRole(alias) => match self.backend.assign_role("default", &alias)
            {
                Ok(report) => {
                    self.backend.pull_goal(&mut self.settings);
                    self.context_window = self.backend.context_window(&self.settings.model);
                    self.notice(Level::Info, report);
                    if let Some(setup) = &mut self.setup {
                        setup.refresh_info(self.backend.setup_info());
                        setup.advance_to(SetupStep::Theme);
                    }
                }
                Err(err) => {
                    if let Some(setup) = &mut self.setup {
                        setup.set_error(err.to_string());
                    }
                }
            },
            SetupAction::ApplyTheme(theme_name) => {
                if let Some(problem) = theme::install_or_default(Some(&theme_name)) {
                    if let Some(setup) = &mut self.setup {
                        setup.set_error(problem);
                    }
                    return true;
                }
                self.flow.replay = true;
                match self.backend.apply_setup_theme(&theme_name) {
                    Ok(msg) => {
                        self.setup = None;
                        self.flash(format!("setup complete · theme {theme_name}"));
                        self.notice(Level::Info, msg);
                    }
                    Err(err) => {
                        if let Some(setup) = &mut self.setup {
                            setup.set_error(err.to_string());
                        }
                    }
                }
            }
            SetupAction::Finish => {
                self.setup = None;
                self.flash("setup complete");
            }
        }
        true
    }
}
