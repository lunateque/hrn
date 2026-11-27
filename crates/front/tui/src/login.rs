//! `/login`: full-window interactive provider login wizard (in-TUI OAuth/device-code flows and
//! pre-save validated API-key setup).

use crate::clipboard;
use crate::login_draw::{draw_form, draw_oauth_flow, draw_picker, sanitize_oauth_input};
use crate::port::{LoginProviderChoice, LoginSubmission, LoginUiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::{Clear, Widget};

pub(crate) enum LoginAction {
    Stay,
    Close,
    Submit(LoginSubmission),
    StartOAuth(String),
    SubmitCode(String),
    CancelOAuth,
}

pub(crate) struct FormField {
    pub(crate) label: &'static str,
    pub(crate) placeholder: String,
    pub(crate) value: String,
    pub(crate) secret: bool,
    pub(crate) required: bool,
}

enum Stage {
    PickProvider,
    Form {
        provider: LoginProviderChoice,
        fields: Vec<FormField>,
        focused: usize,
        reveal_secret: bool,
        validating: Option<String>,
        error: Option<String>,
        back_closes: bool,
    },
    OAuthFlow {
        provider: LoginProviderChoice,
        url: Option<String>,
        user_code: Option<String>,
        message: String,
        needs_code: bool,
        input: String,
        error: Option<String>,
        back_closes: bool,
    },
}

pub(crate) struct LoginView {
    providers: Vec<LoginProviderChoice>,
    selected: usize,
    stage: Stage,
}

impl LoginView {
    pub(crate) fn new(providers: Vec<LoginProviderChoice>) -> Self {
        Self {
            providers,
            selected: 0,
            stage: Stage::PickProvider,
        }
    }

    /// Open directly on `choice` (for `/login <provider>`): API-key form or OAuth flow.
    pub(crate) fn for_provider(
        providers: Vec<LoginProviderChoice>,
        choice: LoginProviderChoice,
    ) -> Self {
        let mut view = Self::new(providers);
        if choice.is_oauth {
            view.open_oauth(choice, true);
        } else {
            view.open_form(choice, true);
        }
        view
    }

    pub(crate) fn is_oauth_active(&self) -> bool {
        matches!(self.stage, Stage::OAuthFlow { .. })
    }

    pub(crate) fn set_validating(&mut self, msg: impl Into<String>) {
        if let Stage::Form {
            validating, error, ..
        } = &mut self.stage
        {
            *validating = Some(msg.into());
            *error = None;
        }
    }

    pub(crate) fn set_form_error(&mut self, msg: String) {
        if let Stage::Form {
            validating, error, ..
        } = &mut self.stage
        {
            *validating = None;
            *error = Some(msg);
        }
    }

    pub(crate) fn on_event(&mut self, event: &LoginUiEvent) {
        match event {
            LoginUiEvent::ShowUrl {
                url,
                user_code,
                message,
                needs_code,
                ..
            } => {
                if let Stage::OAuthFlow {
                    url: slot_url,
                    user_code: slot_code,
                    message: slot_msg,
                    needs_code: slot_needs,
                    error,
                    ..
                } = &mut self.stage
                {
                    *slot_url = Some(url.clone());
                    slot_code.clone_from(user_code);
                    slot_msg.clone_from(message);
                    *slot_needs = *needs_code;
                    *error = None;
                }
            }
            LoginUiEvent::Progress(msg) => match &mut self.stage {
                Stage::OAuthFlow { message, .. } => message.clone_from(msg),
                Stage::Form { validating, .. } => *validating = Some(msg.clone()),
                Stage::PickProvider => {}
            },
            LoginUiEvent::ProbeFailed(err) => self.set_form_error(err.clone()),
            LoginUiEvent::Finished(Err(err)) => match &mut self.stage {
                Stage::OAuthFlow { error, message, .. } => {
                    *error = Some(err.clone());
                    "Login failed · press Esc to go back".clone_into(message);
                }
                Stage::Form {
                    validating, error, ..
                } => {
                    *validating = None;
                    *error = Some(err.clone());
                }
                Stage::PickProvider => {}
            },
            LoginUiEvent::Finished(Ok(_)) => {}
        }
    }

    pub(crate) fn paste(&mut self, text: &str) {
        let cleaned = text.trim();
        if cleaned.is_empty() {
            return;
        }
        match &mut self.stage {
            Stage::Form {
                fields,
                focused,
                validating,
                error,
                ..
            } if validating.is_none() => {
                if let Some(field) = fields.get_mut(*focused) {
                    field.value.push_str(cleaned);
                    *error = None;
                }
            }
            Stage::OAuthFlow {
                needs_code,
                url,
                user_code,
                input,
                error,
                ..
            } if *needs_code || (url.is_some() && user_code.is_none()) => {
                let sanitized = sanitize_oauth_input(cleaned);
                if !sanitized.is_empty() {
                    input.push_str(&sanitized);
                    *error = None;
                }
            }
            Stage::PickProvider | Stage::Form { .. } | Stage::OAuthFlow { .. } => {}
        }
    }

    pub(crate) fn key_with_burst(&mut self, key: KeyEvent, burst: bool) -> LoginAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return if self.is_oauth_active() {
                LoginAction::CancelOAuth
            } else {
                LoginAction::Close
            };
        }
        match &self.stage {
            Stage::PickProvider => self.pick_key(key),
            Stage::Form { .. } => self.form_key(key, ctrl, alt),
            Stage::OAuthFlow { .. } => self.oauth_key(key, ctrl, alt, burst),
        }
    }

    fn pick_key(&mut self, key: KeyEvent) -> LoginAction {
        let count = self.providers.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return LoginAction::Close,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Enter => {
                let Some(choice) = self.providers.get(self.selected).cloned() else {
                    return LoginAction::Close;
                };
                if choice.is_oauth {
                    let id = choice.id.clone();
                    self.open_oauth(choice, false);
                    return LoginAction::StartOAuth(id);
                }
                self.open_form(choice, false);
            }
            _ => {}
        }
        LoginAction::Stay
    }

    fn open_oauth(&mut self, provider: LoginProviderChoice, back_closes: bool) {
        self.stage = Stage::OAuthFlow {
            message: format!("Starting {} login…", provider.name),
            provider,
            url: None,
            user_code: None,
            needs_code: false,
            input: String::new(),
            error: None,
            back_closes,
        };
    }

    fn open_form(&mut self, provider: LoginProviderChoice, back_closes: bool) {
        let mk = |label, placeholder: &str, value: String, secret, required| FormField {
            label,
            placeholder: placeholder.to_owned(),
            value,
            secret,
            required,
        };
        let fields = if provider.needs_custom_fields {
            vec![
                mk(
                    "Provider name",
                    "ollama / my-gateway",
                    "custom".into(),
                    false,
                    true,
                ),
                mk(
                    "Base URL",
                    "http://localhost:11434/v1",
                    provider.default_base_url.clone(),
                    false,
                    true,
                ),
                mk(
                    "Model ID",
                    "qwen2.5-coder:latest",
                    provider.default_model.clone(),
                    false,
                    true,
                ),
                mk(
                    "API key (optional)",
                    "leave empty for keyless local servers",
                    String::new(),
                    true,
                    false,
                ),
            ]
        } else {
            let (lbl, ph) = if provider.key_optional {
                ("API key (optional)", "leave empty for keyless local server")
            } else {
                ("API key", "paste your API key (alt+v / ctrl+v)")
            };
            vec![
                mk(lbl, ph, String::new(), true, !provider.key_optional),
                mk(
                    "Model override (optional)",
                    &format!("default: {}", provider.default_model),
                    String::new(),
                    false,
                    false,
                ),
            ]
        };
        self.stage = Stage::Form {
            provider,
            fields,
            focused: 0,
            reveal_secret: false,
            validating: None,
            error: None,
            back_closes,
        };
    }

    fn form_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool) -> LoginAction {
        let Stage::Form {
            provider,
            fields,
            focused,
            reveal_secret,
            validating,
            error,
            back_closes,
        } = &mut self.stage
        else {
            return LoginAction::Stay;
        };
        let closes = *back_closes;
        if validating.is_some() {
            if key.code == KeyCode::Esc {
                return if closes {
                    LoginAction::Close
                } else {
                    self.stage = Stage::PickProvider;
                    LoginAction::Stay
                };
            }
            return LoginAction::Stay;
        }
        let count = fields.len();
        match key.code {
            KeyCode::Esc => {
                return if closes {
                    LoginAction::Close
                } else {
                    self.stage = Stage::PickProvider;
                    LoginAction::Stay
                };
            }
            KeyCode::Tab | KeyCode::Down if count > 1 => {
                *focused = (*focused + 1) % count;
            }
            KeyCode::BackTab | KeyCode::Up if count > 1 => {
                *focused = (*focused + count - 1) % count;
            }
            KeyCode::Char('r' | 'R') if ctrl => *reveal_secret = !*reveal_secret,
            KeyCode::Char('v' | 'V') if (ctrl && !alt) || (alt && !ctrl) => {
                if let Some(text) = clipboard::read_text()
                    && let Some(field) = fields.get_mut(*focused)
                {
                    field.value.push_str(text.trim());
                    *error = None;
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(field) = fields.get_mut(*focused) {
                    field.value.clear();
                }
            }
            KeyCode::Char('s' | 'S') if ctrl => return build_submission(provider, fields, error),
            KeyCode::Enter => {
                if !provider.needs_custom_fields
                    && *focused == 0
                    && (provider.key_optional
                        || fields.first().is_some_and(|f| !f.value.trim().is_empty()))
                {
                    return build_submission(provider, fields, error);
                }
                if *focused + 1 < count {
                    *focused += 1;
                } else {
                    return build_submission(provider, fields, error);
                }
            }
            KeyCode::Backspace => {
                if let Some(field) = fields.get_mut(*focused) {
                    field.value.pop();
                    *error = None;
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Some(field) = fields.get_mut(*focused) {
                    field.value.push(c);
                    *error = None;
                }
            }
            _ => {}
        }
        LoginAction::Stay
    }

    fn oauth_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool, burst: bool) -> LoginAction {
        let Stage::OAuthFlow {
            needs_code,
            url,
            user_code,
            input,
            message,
            error,
            back_closes,
            ..
        } = &mut self.stage
        else {
            return LoginAction::Stay;
        };
        let closes = *back_closes;
        let can_input = *needs_code || (url.is_some() && user_code.is_none());
        match key.code {
            KeyCode::Esc => {
                if !closes {
                    self.stage = Stage::PickProvider;
                }
                return LoginAction::CancelOAuth;
            }
            KeyCode::Char('v' | 'V') if can_input && ((ctrl && !alt) || (alt && !ctrl)) => {
                if let Some(text) = clipboard::read_text() {
                    let sanitized = sanitize_oauth_input(&text);
                    if !sanitized.is_empty() {
                        input.push_str(&sanitized);
                        *error = None;
                    }
                }
            }
            KeyCode::Char('u') if can_input && ctrl => {
                input.clear();
                *error = None;
            }
            KeyCode::Backspace if can_input => {
                input.pop();
                *error = None;
            }
            KeyCode::Enter if can_input && burst => {}
            KeyCode::Enter if can_input => {
                let sanitized = sanitize_oauth_input(input);
                if sanitized.is_empty() {
                    *error = Some("paste the redirect URL or authorization code first".to_owned());
                } else {
                    input.clear();
                    *error = None;
                    "Exchanging authorization code…".clone_into(message);
                    return LoginAction::SubmitCode(sanitized);
                }
            }
            KeyCode::Char(c) if can_input && !ctrl && !alt && !c.is_whitespace() => {
                input.push(c);
                *error = None;
            }
            _ => {}
        }
        LoginAction::Stay
    }

    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        if area.width < 20 || area.height < 8 {
            return None;
        }
        Clear.render(area, buf);
        match &self.stage {
            Stage::PickProvider => {
                draw_picker(area, buf, &self.providers, self.selected);
                None
            }
            Stage::Form {
                provider,
                fields,
                focused,
                reveal_secret,
                validating,
                error,
                ..
            } => draw_form(
                area,
                buf,
                provider,
                fields,
                *focused,
                *reveal_secret,
                validating.as_deref(),
                error.as_deref(),
            ),
            Stage::OAuthFlow {
                provider,
                url,
                user_code,
                message,
                needs_code,
                input,
                error,
                ..
            } => draw_oauth_flow(
                area,
                buf,
                &provider.name,
                url.as_deref(),
                user_code.as_deref(),
                message,
                *needs_code,
                input,
                error.as_deref(),
            ),
        }
    }
}

fn build_submission(
    provider: &LoginProviderChoice,
    fields: &[FormField],
    error: &mut Option<String>,
) -> LoginAction {
    for field in fields {
        if field.required && field.value.trim().is_empty() {
            *error = Some(format!("{} is required", field.label));
            return LoginAction::Stay;
        }
    }
    let val = |idx: usize| -> String {
        fields
            .get(idx)
            .map_or_else(String::new, |f| f.value.trim().to_owned())
    };
    let non_empty = |idx: usize| -> Option<String> {
        let s = val(idx);
        (!s.is_empty()).then_some(s)
    };
    let submission = if provider.needs_custom_fields {
        LoginSubmission {
            provider_id: provider.id.clone(),
            custom_name: non_empty(0),
            base_url: non_empty(1),
            model_id: non_empty(2),
            api_key: val(3),
        }
    } else {
        LoginSubmission {
            provider_id: provider.id.clone(),
            custom_name: None,
            api_key: val(0),
            base_url: None,
            model_id: non_empty(1),
        }
    };
    LoginAction::Submit(submission)
}
