//! Built-in Claude Pro/Max, ChatGPT Plus/Pro, and Kimi Code subscription logins: PKCE flows
//! raced with a pasted code (Claude/ChatGPT) or RFC 8628 device code flow (Kimi Code).
//! Credentials land in `<auth dir>/<id>.json`, one file per login.

use crate::callback::{CodeSources, await_code};
use crate::device_code::{DeviceAuthResponse, poll_device_token_raw, start_device_flow};
use crate::kimi::{
    KIMI_CLIENT_ID, KIMI_DEVICE_AUTH_URL, KIMI_TOKEN_URL, kimi_device_code_config_with_urls,
    load_or_create_kimi_device_id,
};
use crate::lock::FileLock;
use crate::manager::{TokenBody, now_ms, post_token};
use crate::pkce::{
    LOGIN_TIMEOUT, PkceChallenge, PkceLoginConfig, build_authorize_url, generate_pkce,
};
use crate::sub_store::{
    SubscriptionCredential, credential_from_reply, upsert_subscription_account,
};
use hrn_edge_http::{HttpClient, guard};
use hrn_kernel::{CancelFlag, HrnError, Result};
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;

/// A built-in subscription login.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Subscription {
    /// Claude Pro/Max through the Claude Code `OAuth` client.
    Claude,
    /// ChatGPT Plus/Pro through the Codex CLI `OAuth` client.
    Chatgpt,
    /// Kimi Code subscription through the `KimiCLI` RFC 8628 device-code client.
    KimiCode,
}

/// Wire constants of one login (omp `rules/auth/anthropic.kdl`, `openai-codex.kdl`).
pub(crate) struct Spec {
    authorize_url: &'static str,
    pub(crate) token_url: &'static str,
    pub(crate) client_id: &'static str,
    scope: &'static str,
    port: u16,
    path: &'static str,
    /// Bind a random port when the fixed one is busy; OpenAI allowlists only the exact 1455 URI.
    port_fallback: bool,
    /// Authorize parameters after the standard set.
    extra: &'static [(&'static str, &'static str)],
    /// JSON token requests (Claude) instead of form posts (ChatGPT).
    json: bool,
    pub(crate) token_timeout: Duration,
}

const CLAUDE: Spec = Spec {
    authorize_url: "https://claude.ai/oauth/authorize",
    token_url: "https://api.anthropic.com/v1/oauth/token",
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    scope: "org:create_api_key user:profile user:inference user:sessions:claude_code \
            user:mcp_servers user:file_upload",
    port: 54545,
    path: "/callback",
    port_fallback: true,
    extra: &[("code", "true")],
    json: true,
    token_timeout: Duration::from_secs(30),
};

const CHATGPT: Spec = Spec {
    authorize_url: "https://auth.openai.com/oauth/authorize",
    token_url: "https://auth.openai.com/oauth/token",
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    scope: "openid profile email offline_access api.connectors.read api.connectors.invoke",
    port: 1455,
    path: "/auth/callback",
    port_fallback: false,
    extra: &[
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "hrn"),
    ],
    json: false,
    token_timeout: Duration::from_secs(15),
};

const KIMI_CODE: Spec = Spec {
    authorize_url: KIMI_DEVICE_AUTH_URL,
    token_url: KIMI_TOKEN_URL,
    client_id: KIMI_CLIENT_ID,
    scope: "",
    port: 0,
    path: "/callback",
    port_fallback: true,
    extra: &[],
    json: false,
    token_timeout: Duration::from_secs(30),
};

impl Subscription {
    /// Every built-in subscription, in menu order.
    pub const ALL: [Self; 3] = [Self::Claude, Self::Chatgpt, Self::KimiCode];

    /// Stable id: `hrn login <id>`, the credential file stem and the `auth_mode` value.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Chatgpt => "chatgpt",
            Self::KimiCode => "kimi-code",
        }
    }

    /// Human label for menus and messages.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Pro/Max",
            Self::Chatgpt => "ChatGPT Plus/Pro",
            Self::KimiCode => "Kimi Code",
        }
    }

    /// Whether this subscription authenticates via RFC 8628 device authorization flow.
    #[must_use]
    pub const fn is_device_code(self) -> bool {
        matches!(self, Self::KimiCode)
    }

    /// The subscription whose [`Self::id`] is `id`.
    pub fn from_id(id: &str) -> Option<Self> {
        let lower = id.trim();
        if lower.eq_ignore_ascii_case("kimi_code") {
            return Some(Self::KimiCode);
        }
        Self::ALL
            .into_iter()
            .find(|s| s.id().eq_ignore_ascii_case(lower))
    }

    /// Credential file name inside the auth directory.
    pub fn file_name(self) -> String {
        format!("{}.json", self.id())
    }

    pub(crate) const fn spec(self) -> &'static Spec {
        match self {
            Self::Claude => &CLAUDE,
            Self::Chatgpt => &CHATGPT,
            Self::KimiCode => &KIMI_CODE,
        }
    }
}

/// Terminal or TUI side of a subscription login.
pub struct LoginIo<'a> {
    /// Skip the loopback listener; the code then only arrives through `pasted`.
    pub manual: bool,
    /// Lines the user pastes (redirect URL, `code#state`, `?code=…&state=…`), raced with the
    /// browser redirect.
    pub pasted: Option<&'a mut UnboundedReceiver<String>>,
    /// Receives the authorize URL once the listener is ready (or verification URI for device flow).
    pub on_url: &'a (dyn Fn(&str) + Sync),
    /// Receives non-fatal notices (an ignored paste or progress update).
    pub on_notice: &'a (dyn Fn(&str) + Sync),
    /// Optional structured callback for RFC 8628 device-code flows (carries `user_code`).
    pub on_device_code: Option<&'a (dyn Fn(&DeviceAuthResponse) + Sync)>,
    /// Optional cancellation flag (`Esc` in the TUI).
    pub cancel: Option<CancelFlag>,
}

/// Log in through the browser (or RFC 8628 device flow for Kimi Code) and store the credential in
/// `file` (written atomically under the cross-process lock).
pub async fn run_subscription_login(
    http: &HttpClient,
    kind: Subscription,
    file: &Path,
    io: LoginIo<'_>,
) -> Result<SubscriptionCredential> {
    let spec = kind.spec();
    run_subscription_login_with_urls(http, kind, file, spec.authorize_url, spec.token_url, io).await
}

/// Same as [`run_subscription_login`] with explicit authorize/device-auth and token URLs (for
/// loopback stub tests).
pub async fn run_subscription_login_with_urls(
    http: &HttpClient,
    kind: Subscription,
    file: &Path,
    authorize_url_override: &str,
    token_url_override: &str,
    io: LoginIo<'_>,
) -> Result<SubscriptionCredential> {
    guard::check_token_path(&file.to_string_lossy())?;
    if kind.is_device_code() {
        return run_kimi_device_login(http, file, authorize_url_override, token_url_override, io)
            .await;
    }
    let spec = kind.spec();
    let (listener, port) =
        resolve_callback_listener(spec, io.manual, io.pasted.is_some(), io.on_notice).await?;
    let redirect_uri = format!("http://localhost:{port}{}", spec.path);
    let pkce = generate_pkce()?;
    let url = authorize_url_for(
        kind,
        authorize_url_override,
        token_url_override,
        &redirect_uri,
        &pkce,
    )?;
    (io.on_url)(&url);
    let code = tokio::time::timeout(
        LOGIN_TIMEOUT,
        await_code(CodeSources {
            listener: listener.as_ref(),
            path: spec.path,
            state: &pkce.state,
            pasted: io.pasted,
            notice: io.on_notice,
            cancel: io.cancel.as_ref(),
        }),
    )
    .await
    .map_err(|_| HrnError::Auth(format!("{} login timed out", kind.label())))??;
    drop(listener);
    let cred = exchange_code(http, kind, token_url_override, &redirect_uri, &code, &pkce).await?;
    let _lock = FileLock::acquire(file).await?;
    upsert_subscription_account(kind, file, cred.clone())?;
    Ok(cred)
}

async fn run_kimi_device_login(
    http: &HttpClient,
    file: &Path,
    device_auth_url: &str,
    token_url: &str,
    io: LoginIo<'_>,
) -> Result<SubscriptionCredential> {
    let auth_dir = file.parent().unwrap_or_else(|| Path::new("."));
    let device_id = load_or_create_kimi_device_id(auth_dir).await?;
    let cfg = kimi_device_code_config_with_urls(&device_id, device_auth_url, token_url);
    let resp = start_device_flow(http, &cfg).await?;
    if let Some(cb) = io.on_device_code {
        cb(&resp);
    } else {
        (io.on_url)(resp.display_url());
        (io.on_notice)(&format!(
            "Enter code {} at {}",
            resp.user_code, resp.verification_uri
        ));
    }
    let cancel = io.cancel.unwrap_or_default();
    let raw = poll_device_token_raw(http, &cfg, &resp, &cancel).await?;
    let cred = credential_from_reply(Subscription::KimiCode, &raw, None, now_ms())?;
    let _lock = FileLock::acquire(file).await?;
    upsert_subscription_account(Subscription::KimiCode, file, cred.clone())?;
    Ok(cred)
}

async fn resolve_callback_listener(
    spec: &Spec,
    manual: bool,
    has_pasted: bool,
    on_notice: &(dyn Fn(&str) + Sync),
) -> Result<(Option<TcpListener>, u16)> {
    if manual {
        return Ok((None, spec.port));
    }
    match bind_callback(spec).await {
        Ok((listener, port)) => Ok((Some(listener), port)),
        Err(err) if !spec.port_fallback && has_pasted => {
            on_notice(&format!(
                "Callback port {} is busy ({err}); falling back to manual paste — open the URL, \
                 authorize, and paste the redirect URL or code below.",
                spec.port
            ));
            Ok((None, spec.port))
        }
        Err(err) => Err(err),
    }
}

async fn bind_callback(spec: &Spec) -> Result<(TcpListener, u16)> {
    let listener = match TcpListener::bind(("127.0.0.1", spec.port)).await {
        Ok(listener) => listener,
        Err(_) if spec.port_fallback => TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| HrnError::Io(format!("bind OAuth callback listener: {e}")))?,
        Err(e) => {
            return Err(HrnError::Io(format!(
                "port {port} is busy ({e}); the provider only accepts \
                 http://localhost:{port}{path} as redirect: close the program holding it \
                 (another Codex or omp login?) or pass --manual",
                port = spec.port,
                path = spec.path
            )));
        }
    };
    let port = listener
        .local_addr()
        .map_err(|e| HrnError::Io(format!("OAuth callback listener address: {e}")))?
        .port();
    Ok((listener, port))
}

/// The browser URL that starts a `kind` login.
#[cfg(test)]
pub(crate) fn authorize_url(
    kind: Subscription,
    redirect_uri: &str,
    pkce: &PkceChallenge,
) -> Result<String> {
    let spec = kind.spec();
    authorize_url_for(kind, spec.authorize_url, spec.token_url, redirect_uri, pkce)
}

fn authorize_url_for(
    kind: Subscription,
    authorize_url: &str,
    token_url: &str,
    redirect_uri: &str,
    pkce: &PkceChallenge,
) -> Result<String> {
    let spec = kind.spec();
    let cfg = PkceLoginConfig {
        authorize_url: authorize_url.to_owned(),
        token_url: token_url.to_owned(),
        client_id: spec.client_id.to_owned(),
        scope: spec.scope.to_owned(),
        callback_port: spec.port,
        callback_path: spec.path.to_owned(),
        redirect_uri_override: None,
        extra_params: spec
            .extra
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        json_body: spec.json,
        token_file: String::new(),
    };
    build_authorize_url(&cfg, redirect_uri, pkce)
}

/// Providers may echo `code#state`; the fragment then wins over the callback state.
fn split_code_state<'a>(code: &'a str, state: &'a str) -> (&'a str, &'a str) {
    match code.split_once('#') {
        Some((code, fragment)) if !fragment.is_empty() => (code, fragment),
        Some((code, _)) => (code, state),
        None => (code, state),
    }
}

async fn exchange_code(
    http: &HttpClient,
    kind: Subscription,
    token_url: &str,
    redirect_uri: &str,
    code: &str,
    pkce: &PkceChallenge,
) -> Result<SubscriptionCredential> {
    let spec = kind.spec();
    let (code, state) = split_code_state(code, &pkce.state);
    let body = if spec.json {
        TokenBody::Json(json!({
            "grant_type": "authorization_code",
            "client_id": spec.client_id,
            "code": code,
            "redirect_uri": redirect_uri,
            "code_verifier": pkce.verifier,
            "state": state,
        }))
    } else {
        TokenBody::Form(vec![
            ("grant_type", "authorization_code".to_owned()),
            ("client_id", spec.client_id.to_owned()),
            ("code", code.to_owned()),
            ("redirect_uri", redirect_uri.to_owned()),
            ("code_verifier", pkce.verifier.clone()),
        ])
    };
    let what = format!("{} login", kind.label());
    let reply = post_token(http, token_url, body, &[], spec.token_timeout, &what).await?;
    credential_from_reply(kind, &reply, None, now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::{base64url_encode, sha256};
    use std::collections::HashMap;

    fn query(url: &str) -> HashMap<String, String> {
        let (_, q) = url.split_once('?').expect("query");
        form_urlencoded::parse(q.as_bytes()).into_owned().collect()
    }

    #[test]
    fn authorize_urls_carry_the_subscription_client_and_s256_pkce() {
        let pkce = generate_pkce().expect("pkce");
        let url = authorize_url(
            Subscription::Claude,
            "http://localhost:54545/callback",
            &pkce,
        )
        .expect("claude url");
        assert!(
            url.starts_with("https://claude.ai/oauth/authorize?client_id="),
            "{url}"
        );
        let q = query(&url);
        assert_eq!(q["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(
            q["scope"],
            "org:create_api_key user:profile user:inference user:sessions:claude_code \
             user:mcp_servers user:file_upload"
        );
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            q["code_challenge"],
            base64url_encode(&sha256(pkce.verifier.as_bytes()))
        );
        assert_eq!(q["state"], pkce.state);
        assert_eq!(q["code"], "true");

        let url = authorize_url(
            Subscription::Chatgpt,
            "http://localhost:1455/auth/callback",
            &pkce,
        )
        .expect("chatgpt url");
        assert!(
            url.starts_with("https://auth.openai.com/oauth/authorize?"),
            "{url}"
        );
        let q = query(&url);
        assert_eq!(q["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(q["redirect_uri"], "http://localhost:1455/auth/callback");
        assert_eq!(
            q["scope"],
            "openid profile email offline_access api.connectors.read api.connectors.invoke"
        );
        assert_eq!(q["id_token_add_organizations"], "true");
        assert_eq!(q["codex_cli_simplified_flow"], "true");
        assert_eq!(q["originator"], "hrn");
        assert_eq!(q["code_challenge_method"], "S256");
    }

    #[test]
    fn code_fragment_wins_over_the_callback_state() {
        assert_eq!(split_code_state("abc#s2", "s1"), ("abc", "s2"));
        assert_eq!(split_code_state("abc#", "s1"), ("abc", "s1"));
        assert_eq!(split_code_state("abc", "s1"), ("abc", "s1"));
    }

    #[tokio::test]
    async fn busy_fixed_port_falls_back_to_manual_paste_when_paste_channel_available() {
        let blocker = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let busy_port = blocker.local_addr().expect("addr").port();
        let spec = Spec {
            port: busy_port,
            port_fallback: false,
            ..CHATGPT
        };
        let notices = std::sync::Mutex::new(Vec::<String>::new());
        let on_notice = |msg: &str| {
            notices.lock().expect("lock").push(msg.to_owned());
        };
        let (listener, port) = resolve_callback_listener(&spec, false, true, &on_notice)
            .await
            .expect("falls back to manual paste");
        assert!(listener.is_none());
        assert_eq!(port, busy_port);
        let captured = notices.lock().expect("lock").clone();
        assert_eq!(captured.len(), 1);
        assert!(
            captured[0].contains("falling back to manual paste"),
            "{captured:?}"
        );

        let err = resolve_callback_listener(&spec, false, false, &on_notice)
            .await
            .expect_err("without paste channel must error on busy fixed port");
        assert!(err.to_string().contains("is busy"), "{err}");
        drop(blocker);
    }
}
