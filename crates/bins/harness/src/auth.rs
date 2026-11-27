//! `hrn login [provider]` (interactive API-key wizard with pre-save probe, browser/`--manual`
//! `OAuth2` PKCE, or a Claude / `ChatGPT` / Kimi Code subscription), `hrn logout [provider]` and
//! `hrn auth status`.

use crate::auth_input::{self, StdinLineReader};
use crate::cli::LoginArgs;
use crate::commands::format_utc;
use crate::config::Config;
use crate::setup;
use crate::term::{err_line, out};
use hrn_edge_http::HttpClient;
use hrn_edge_llm::{AuthMode, ProviderEntry, ProviderKind, discover_provider, standard_catalogs};
use hrn_edge_oauth::{
    DeviceAuthResponse, LoginIo, Subscription, SubscriptionStatus, TokenFileStatus,
    inspect_token_file, run_pkce_login, run_pkce_manual_login, run_subscription_login,
    subscription_accounts_status,
};
use hrn_edge_tools::StdWorkspaceFs;
use hrn_kernel::{CancelFlag, HrnError, Result, WorkspaceFs};
use hrn_tui::{LoginProviderChoice, LoginSubmission};
use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Execute `hrn login [provider] [--manual] [--no-browser]`.
pub(crate) async fn login(config_path: &Path, auth_dir: &Path, args: &LoginArgs) -> Result {
    if let Some(provider) = args.provider.as_deref() {
        setup::reject_google_oauth(provider)?;
    }
    let existing_cfg = if config_path.is_file() {
        Config::load(config_path).ok()
    } else {
        None
    };
    if let Some(provider) = args.provider.as_deref() {
        if provider.eq_ignore_ascii_case("agy") {
            return crate::auth_agy::login_agy_cli(config_path, existing_cfg.as_ref(), args).await;
        }
        if let Some(cfg) = &existing_cfg
            && cfg.oauth_logins.contains_key(provider)
        {
            return login_oauth(cfg, provider, args).await;
        }
        if let Some(kind) = Subscription::from_id(provider) {
            return login_subscription(config_path, auth_dir, kind, args).await;
        }
    }

    let choices = setup::provider_choices_in(existing_cfg.as_ref(), Some(auth_dir));
    let picked = match args.provider.as_deref() {
        Some(wanted) => choices
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case(wanted))
            .cloned()
            .ok_or_else(|| {
                let names: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
                HrnError::Config(format!(
                    "unknown provider `{wanted}` (available: {})",
                    names.join(", ")
                ))
            })?,
        None => prompt_provider(&choices)?,
    };

    if picked.is_oauth {
        if let Some(kind) = Subscription::from_id(&picked.id) {
            return login_subscription(config_path, auth_dir, kind, args).await;
        }
        let cfg = existing_cfg.ok_or_else(|| {
            HrnError::Config(format!(
                "missing config `{}` for OAuth",
                config_path.display()
            ))
        })?;
        return login_oauth(&cfg, &picked.id, args).await;
    }

    let submission = prompt_credentials(&picked)?;
    let http = HttpClient::new()?;
    let allow_plaintext_http = existing_cfg.as_ref().map_or_else(
        || setup::read_allow_plaintext_http(config_path),
        |c| c.allow_plaintext_http,
    );
    out("Validating API key...\n");
    let (cfg, outcome) = setup::probe_and_apply_login(
        &http,
        config_path,
        StdWorkspaceFs.home_dir().as_deref(),
        &submission,
        allow_plaintext_http,
    )
    .await?;
    let cache_dir = crate::app_data_dir().ok().map(|d| d.join("catalog-cache"));
    if let Some(entry) = cfg.llm.providers.get(&outcome.provider) {
        let _ = discover_provider(
            &http,
            &outcome.provider,
            entry,
            cache_dir.as_deref(),
            Duration::from_secs(2),
        )
        .await;
    }
    out(&format!("\n{}\n", outcome.summary));
    Ok(())
}

fn prompt_line(label: &str) -> Result<String> {
    auth_input::prompt_line(label, false)
}

fn prompt_provider(choices: &[LoginProviderChoice]) -> Result<LoginProviderChoice> {
    out("Configure a provider for hrn:\n\n");
    for (idx, c) in choices.iter().enumerate() {
        let mark = if c.configured { "active" } else { "not set" };
        out(&format!(
            "  {}) {:<18} [{mark:<7}]  {}\n",
            idx + 1,
            c.name,
            c.hint
        ));
    }
    out("\n");
    let raw = prompt_line(&format!(
        "Select provider [1-{}] (or name): ",
        choices.len()
    ))?;
    if raw.is_empty() {
        return Err(HrnError::Cancelled);
    }
    if let Ok(num) = raw.parse::<usize>()
        && let Some(choice) = num.checked_sub(1).and_then(|i| choices.get(i))
    {
        return Ok(choice.clone());
    }
    choices
        .iter()
        .find(|c| c.id.eq_ignore_ascii_case(&raw) || c.name.eq_ignore_ascii_case(&raw))
        .cloned()
        .ok_or_else(|| HrnError::Config(format!("invalid selection `{raw}`")))
}

fn prompt_credentials(choice: &LoginProviderChoice) -> Result<LoginSubmission> {
    out(&format!(
        "\nConfiguring {} ({})\n",
        choice.name, choice.hint
    ));
    if choice.needs_custom_fields {
        let name = prompt_line("Provider name [custom]: ")?;
        let url = prompt_line(&format!("Base URL [{}]: ", choice.default_base_url))?;
        let model = prompt_line(&format!("Model ID [{}]: ", choice.default_model))?;
        let key = prompt_line("API key (leave empty for keyless local server): ")?;
        return Ok(LoginSubmission {
            provider_id: choice.id.clone(),
            custom_name: (!name.is_empty()).then_some(name),
            api_key: key,
            base_url: (!url.is_empty()).then_some(url),
            model_id: (!model.is_empty()).then_some(model),
        });
    }
    let key = prompt_line("Paste API key: ")?;
    let model = prompt_line(&format!(
        "Model override (Enter for default `{}`): ",
        choice.default_model
    ))?;
    Ok(LoginSubmission {
        provider_id: choice.id.clone(),
        custom_name: None,
        api_key: key,
        base_url: None,
        model_id: (!model.is_empty()).then_some(model),
    })
}

async fn login_oauth(cfg: &Config, provider: &str, args: &LoginArgs) -> Result {
    let Some(pkce_cfg) = cfg.oauth_logins.get(provider) else {
        return Err(HrnError::Config(format!(
            "provider `{provider}` is not configured for OAuth2 PKCE"
        )));
    };

    let http = HttpClient::new()?;
    let open_browser = !args.no_browser && !args.manual && browser_env_enabled();
    let on_url = |url: &str| {
        out(&format!(
            "Open this URL to authorize `{provider}`:\n\n  {url}\n\n",
        ));
        if open_browser && let Err(e) = try_open_browser(url) {
            err_line(&format!(
                "note: could not open browser automatically ({e}); open the URL above manually"
            ));
        }
    };

    let token = if args.manual {
        run_pkce_manual_login(&http, pkce_cfg, on_url, || {
            auth_input::prompt_line(
                "Paste the full redirect URL or code (Ctrl+V / Alt+V, or type `paste`) and press Enter:\n> ",
                true,
            )
        })
        .await?
    } else {
        out(
            "Waiting for localhost OAuth callback (Ctrl+C to cancel, or re-run with --manual)...\n",
        );
        run_pkce_login(&http, pkce_cfg, on_url).await?
    };

    let refresh_note = if token.refresh_token.is_some() {
        "with refresh_token"
    } else {
        "access_token only"
    };
    out(&format!(
        "Logged in to `{provider}` ({refresh_note}). Token saved to {}\n",
        pkce_cfg.token_file
    ));
    Ok(())
}

/// Log in to a Claude / `ChatGPT` / Kimi Code subscription, then ensure `config.toml` can use it.
async fn login_subscription(
    config_path: &Path,
    auth_dir: &Path,
    kind: Subscription,
    args: &LoginArgs,
) -> Result {
    let http = HttpClient::new()?;
    let file = auth_dir.join(kind.file_name());
    let open_browser = !args.no_browser && !args.manual && browser_env_enabled();
    let cancel = CancelFlag::new();
    let (mut reader, raw_armed) = StdinLineReader::spawn(cancel.clone());
    let on_url = |url: &str| {
        out(&format!(
            "Open this URL to log in to {}:\n\n  {url}\n\n",
            kind.label()
        ));
        if open_browser && let Err(e) = try_open_browser(url) {
            err_line(&format!(
                "note: could not open browser automatically ({e}); open the URL above manually"
            ));
        }
        out(if args.manual {
            "Paste the redirect URL or code (Ctrl+V / Alt+V, or type `paste`) and press Enter:\n> "
        } else {
            "Waiting for the browser (or paste redirect URL / code with Ctrl+V / Alt+V or `paste` + Enter; \
             Ctrl+C cancels)...\n> "
        });
        raw_armed.store(true, Ordering::Release);
    };
    let on_device = |resp: &DeviceAuthResponse| {
        let url = resp.display_url();
        out(&format!(
            "Open this URL to log in to {}:\n\n  {url}\n\n  User code: {}\n\n\
             Waiting for browser approval (Ctrl+C cancels)...\n",
            kind.label(),
            resp.user_code
        ));
        if open_browser && let Err(e) = try_open_browser(url) {
            err_line(&format!(
                "note: could not open browser automatically ({e}); open the URL above manually"
            ));
        }
    };
    let on_notice = |msg: &str| auth_input::write_notice(msg, &raw_armed);
    let io = LoginIo {
        manual: args.manual,
        pasted: Some(reader.rx_mut()),
        on_url: &on_url,
        on_notice: &on_notice,
        on_device_code: Some(&on_device),
        cancel: Some(cancel),
    };
    let cred = run_subscription_login(&http, kind, &file, io).await;
    drop(reader);
    let cred = cred?;
    out(&format!(
        "Logged in to {} as {}. Credentials saved to {}\n",
        kind.label(),
        cred.account_label().unwrap_or("an unnamed account"),
        file.display()
    ));
    if !config_path.exists() {
        let home = StdWorkspaceFs.home_dir();
        match seed_config_with_auth_dir(config_path, kind, home.as_deref(), auth_dir) {
            Ok(summary) => out(&format!("{summary}\n")),
            Err(e) => err_line(&format!(
                "note: `{}` was not created ({e}); add `[roles] default = \"<model>\"` by hand \
                 (`hrn models` lists the names)",
                config_path.display()
            )),
        }
    }
    Ok(())
}

/// First login without a `config.toml`: write one whose default model is `kind`'s built-in role.
pub(crate) fn seed_config_with_auth_dir(
    config_path: &Path,
    kind: Subscription,
    home: Option<&Path>,
    auth_dir: &Path,
) -> Result<String> {
    let catalogs = home.map_or_else(Vec::new, |h| standard_catalogs(h, auth_dir));
    let model = catalogs
        .iter()
        .find_map(|catalog| catalog.roles.get(kind.id()).cloned())
        .ok_or_else(|| HrnError::Config(format!("no `{}` model role is defined", kind.id())))?;
    let content = format!("[roles]\ndefault = \"{model}\"\n");
    Config::parse_with_catalogs(&content, home, catalogs)?;
    crate::toml_write::atomic_write(config_path, &content)?;
    Ok(format!(
        "Wrote {} with default model {model}; switch with /model or ctrl+p",
        config_path.display()
    ))
}

/// Execute `hrn logout [provider]`: delete stored credentials after path-confinement verification.
pub(crate) fn logout(config_path: &Path, auth_dir: &Path, provider: Option<&str>) -> Result {
    crate::logout::run_cli(config_path, auth_dir, provider)
}

/// Execute `hrn auth status`: configured providers (when a config loads) and subscription
/// logins, without leaking secrets.
pub(crate) fn status(config_path: &Path, auth_dir: &Path) {
    match Config::load(config_path) {
        Ok(cfg) if cfg.llm.providers.is_empty() => out("no providers configured\n"),
        Ok(cfg) => {
            out("PROVIDER\tKIND\tAUTH\tSTATUS\n");
            for (name, entry) in &cfg.llm.providers {
                let (mode, state) = describe_auth(entry);
                out(&format!(
                    "{name}\t{}\t{mode}\t{state}\n",
                    kind_label(entry.kind)
                ));
            }
        }
        Err(e) => err_line(&format!("config `{}`: {e}", config_path.display())),
    }
    out("\nLOGIN\tACCOUNT\tPLAN\tEXPIRES\tSTATUS\n");
    for kind in Subscription::ALL {
        let file = auth_dir.join(kind.file_name());
        let statuses = subscription_accounts_status(kind, &file);
        if statuses.is_empty() {
            out(&format!(
                "{}\t-\t-\t-\tnot logged in (run `hrn login {}`)\n",
                kind.id(),
                kind.id()
            ));
            continue;
        }
        for st in statuses {
            let row = match st {
                SubscriptionStatus::Missing => {
                    format!("-\t-\t-\tnot logged in (run `hrn login {}`)", kind.id())
                }
                SubscriptionStatus::LoggedIn {
                    account,
                    plan,
                    expires_ms,
                    has_refresh,
                } => format!(
                    "{}\t{}\t{}\t{}",
                    account.as_deref().unwrap_or("-"),
                    plan.as_deref().unwrap_or("-"),
                    format_utc(i64::try_from(expires_ms).unwrap_or(i64::MAX)),
                    if has_refresh {
                        "ok (refreshable)"
                    } else {
                        "ok"
                    }
                ),
                SubscriptionStatus::Invalid(msg) => format!("-\t-\t-\tinvalid ({msg})"),
            };
            out(&format!("{}\t{row}\n", kind.id()));
        }
    }
    if let Some(bin) = hrn_edge_llm::agy_acp::discover_agy_binary(None) {
        out(&format!(
            "agy\t-\t-\t-\tcli installed ({}; run `hrn doctor` for live auth status)\n",
            bin.display()
        ));
    }
}

/// Non-secret auth description reused by both `hrn auth status` and `hrn config check`.
pub(crate) fn provider_key_report(entry: &ProviderEntry) -> std::result::Result<String, String> {
    if entry.kind == ProviderKind::AgyAcp || entry.auth_mode == AuthMode::Agy {
        return match hrn_edge_llm::agy_acp::discover_agy_binary(entry.agy.binary.as_deref()) {
            Some(path) => Ok(format!("agy CLI ({})", path.display())),
            None => Err(
                "`agy` CLI not found; install Antigravity CLI >= 1.5.0 or set `binary`".to_owned(),
            ),
        };
    }
    if let Some(kind) = entry.auth_mode.subscription() {
        let file = entry.token_file.as_deref().unwrap_or_default();
        let statuses = subscription_accounts_status(kind, Path::new(file));
        if statuses.is_empty() {
            return Err(format!("not logged in; run `hrn login {}`", kind.id()));
        }
        if let [SubscriptionStatus::Invalid(msg)] = statuses.as_slice() {
            return Err(format!("login invalid: {msg}"));
        }
        let labels: Vec<String> = statuses
            .iter()
            .filter_map(|s| match s {
                SubscriptionStatus::LoggedIn { account, .. } => {
                    Some(account.clone().unwrap_or_else(|| "default".to_owned()))
                }
                SubscriptionStatus::Missing | SubscriptionStatus::Invalid(_) => None,
            })
            .collect();
        return if labels.len() > 1 {
            Ok(format!(
                "{} login ({} accounts: {})",
                kind.label(),
                labels.len(),
                labels.join(", ")
            ))
        } else {
            Ok(format!(
                "{} login{}",
                kind.label(),
                labels
                    .first()
                    .map(|a| format!(" ({a})"))
                    .unwrap_or_default()
            ))
        };
    }
    if entry.auth_mode == AuthMode::OauthJson {
        let Some(oauth) = &entry.oauth else {
            return Err("auth_mode = oauth_json but missing [providers.oauth]".to_owned());
        };
        return match inspect_token_file(Path::new(&oauth.token_file), oauth.refresh_skew_secs) {
            TokenFileStatus::Valid {
                remaining_secs,
                has_refresh,
            } => {
                let ttl = remaining_secs
                    .map_or_else(|| "no expiry".to_owned(), |s| format!("{}m left", s / 60));
                let ref_str = if has_refresh { ", refreshable" } else { "" };
                Ok(format!("oauth {} ({ttl}{ref_str})", oauth.token_file))
            }
            TokenFileStatus::Expired { has_refresh: true } => Ok(format!(
                "oauth {} (expired; will auto-refresh)",
                oauth.token_file
            )),
            TokenFileStatus::Expired { has_refresh: false } => Err(format!(
                "oauth {} is expired with no refresh_token; run `hrn login`",
                oauth.token_file
            )),
            TokenFileStatus::Missing => Err(format!(
                "oauth token `{}` missing; run `hrn login`",
                oauth.token_file
            )),
            TokenFileStatus::Invalid(msg) => Err(format!("oauth token invalid: {msg}")),
        };
    }

    let source = match (&entry.token_env, &entry.token_file) {
        (Some(env), _) => format!("env {env}"),
        (None, Some(file)) => format!("file {file}"),
        (None, None) if entry.kind == ProviderKind::OpenaiChat => {
            return Ok("none (keyless local server)".to_owned());
        }
        (None, None) => return Err("no key source".to_owned()),
    };
    match entry.resolve_static_token() {
        Ok(Some(_)) => Ok(format!("{source} (resolved)")),
        Ok(None) => Err(format!("{source} is unset or empty")),
        Err(e) => Err(format!("{source}: {e}")),
    }
}

fn describe_auth(entry: &ProviderEntry) -> (&'static str, String) {
    let mode = match entry.auth_mode {
        AuthMode::OauthJson => "oauth_json",
        AuthMode::Bearer => "bearer",
        AuthMode::ApiKey => "api_key",
        AuthMode::Default => "default",
        AuthMode::Claude => "claude",
        AuthMode::Chatgpt => "chatgpt",
        AuthMode::KimiCode => "kimi_code",
        AuthMode::Agy => "agy",
    };
    let state = match provider_key_report(entry) {
        Ok(ok) => ok,
        Err(err) => format!("UNRESOLVED ({err})"),
    };
    (mode, state)
}

fn kind_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenaiChat => "openai_chat",
        ProviderKind::OpenaiResponses => "openai_responses",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::AgyAcp => "agy_acp",
    }
}

/// Check `HRN_NO_BROWSER` so headless environments and automated tests never launch a browser.
fn browser_env_enabled() -> bool {
    !std::env::var("HRN_NO_BROWSER").is_ok_and(|v| {
        let lower = v.trim().to_ascii_lowercase();
        matches!(lower.as_str(), "1" | "true" | "yes")
    })
}

/// Best-effort browser open for in-TUI OAuth flows (skipped when `HRN_NO_BROWSER` is set or in
/// unit tests).
pub(crate) fn try_open_browser_if_enabled(url: &str) {
    if cfg!(test) || !browser_env_enabled() {
        return;
    }
    let _ = try_open_browser(url);
}

/// Validate a URL before handing it to the OS browser launcher (`https://` or `http://` only,
/// with no ASCII control characters, whitespace, quotes, backslashes, or angle brackets).
pub(crate) fn validate_browser_url(url: &str) -> io::Result<()> {
    let scheme_ok = url
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        || url
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"));
    let unsafe_char = url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '<' | '>'));
    if !scheme_ok || unsafe_char {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to open non-http(s) or unsafe URL in browser",
        ));
    }
    Ok(())
}

/// Launch the OS default browser without invoking a shell (avoids `&` splitting in URLs).
fn try_open_browser(url: &str) -> io::Result<()> {
    validate_browser_url(url)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let open = |flags: u32| {
            Command::new("rundll32.exe")
                .args(["url.dll,FileProtocolHandler", url])
                .creation_flags(flags)
                .spawn()
        };
        // The browser must outlive hrn, which runs inside a kill-on-close job; a parent job that
        // forbids breakaway makes the flagged spawn fail, then the browser stays in the job.
        open(hrn_edge_proc::CREATE_BREAKAWAY_FROM_JOB).or_else(|_| open(0))?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(url).spawn()?;
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(url).spawn()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_browser_url;

    #[test]
    fn browser_url_validator_rejects_non_http_and_unsafe_chars() {
        assert!(validate_browser_url("https://claude.ai/oauth/authorize?a=1&b=2").is_ok());
        assert!(validate_browser_url("http://127.0.0.1:54545/callback?code=x").is_ok());
        for bad in [
            "file:///etc/passwd",
            "cmd:calc",
            "javascript:alert(1)",
            "https://example.com/a b",
            "https://example.com/a\nb",
            "https://example.com/a\x00b",
            "https://example.com/a\"b",
        ] {
            assert!(validate_browser_url(bad).is_err(), "must reject `{bad}`");
        }
    }
}
