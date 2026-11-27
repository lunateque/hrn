//! End-to-end Wave 10f acceptance and security tests:
//! `tui_login_oauth_flow_scripted`, `api_key_probe_failure_writes_nothing`,
//! `logout_removes_and_reloads`, `logout_respects_token_path_guard`,
//! `oauth_hint_removed_and_google_oauth_still_blocked`,
//! `kimi_config_providers_table_and_project_layer_blocked`, and
//! `tui_setup_wizard_completes_and_persists_role_and_theme`.

use super::*;
use crate::cli::ChatArgs;
use crate::config_layers::{LayerInput, load_layers};
use crate::resume::StartupSession;
use crate::setup;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_edge_http::{HttpClient, guard};
use hrn_edge_llm::KimiApiFormat;
use hrn_edge_oauth::{Subscription, SubscriptionCredential, upsert_subscription_account};
use hrn_tui::{App, LoginSubmission, UiReceiver};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

fn temp_root(tag: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hrn-10f-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn drive(
    app: &mut App,
    rx: &UiReceiver,
    limit: Duration,
    mut done: impl FnMut(&mut App) -> bool,
) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        app.pump(rx, Duration::from_millis(25));
        if done(app) {
            return true;
        }
    }
    false
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let frame = app.paint(width, height);
    let buffer = &frame.live;
    let history = frame.scrollback.iter().map(ToString::to_string);
    let rows = (0..buffer.area.height).map(|y| {
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        row.trim_end().to_owned()
    });
    history.chain(rows).collect::<Vec<_>>().join("\n")
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_len {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn spawn_http_stub(
    status: &'static str,
    body: String,
    expected_requests: usize,
) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..expected_requests {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            seen.push(read_http_request(&mut stream).await);
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
        seen
    });
    (addr, handle)
}

const SEED_CONFIG: &str = "[[providers]]\n\
name = \"seed\"\n\
kind = \"openai_chat\"\n\
base_url = \"http://127.0.0.1:9/v1\"\n\
[models.seed]\n\
provider = \"seed\"\n\
id = \"seed-model\"\n\
context_window = 8192\n\
[roles]\n\
default = \"seed\"\n";

fn open_test_app(
    runtime: &tokio::runtime::Runtime,
    root: &Path,
    cfg_toml: &str,
    configure_backend: impl FnOnce(&mut ChatBackend),
) -> (App, UiReceiver, hrn_edge_mcp::McpManager, PathBuf, PathBuf) {
    let home = root.join("home");
    let harness_dir = home.join(".harness");
    let auth_dir = harness_dir.join("auth");
    let work = root.join("work");
    std::fs::create_dir_all(&auth_dir).expect("auth_dir");
    std::fs::create_dir_all(&work).expect("work");
    let cfg_path = harness_dir.join("config.toml");
    std::fs::write(&cfg_path, cfg_toml).expect("write config");
    let catalogs = hrn_edge_llm::standard_catalogs(&home, &auth_dir);
    let cfg = Config::parse_with_catalogs(cfg_toml, Some(&home), catalogs).expect("parse cfg");
    let (tx, rx) = hrn_tui::channel();
    let db = root.join("sessions.db");
    let (mut backend, mcp_manager, _warnings) = runtime
        .block_on(ChatBackend::open(
            &cfg,
            cfg_path.clone(),
            &db,
            work,
            &ChatArgs::default(),
            tx,
            runtime.handle().clone(),
            None,
        ))
        .expect("open backend");
    backend.login_state.overrides.home_dir = Some(home);
    backend.login_state.overrides.auth_dir = Some(auth_dir.clone());
    configure_backend(&mut backend);
    let options = backend
        .options_for_startup(&StartupSession::Fresh)
        .expect("options");
    (
        App::new(Box::new(backend), options),
        rx,
        mcp_manager,
        cfg_path,
        auth_dir,
    )
}

fn extract_state_from_screen(rendered: &str) -> Option<String> {
    let idx = rendered.find("state=")?;
    let tail = &rendered[idx + "state=".len()..];
    let state: String = tail
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!state.is_empty()).then_some(state)
}

#[test]
fn tui_login_oauth_flow_scripted() {
    let runtime = tokio::runtime::Runtime::new().expect("rt");
    let root = temp_root("oauth-scripted");
    let token_reply = serde_json::json!({
        "access_token": "sk-ant-oat01-scripted-tok",
        "refresh_token": "rt-scripted-tok",
        "expires_in": 3600,
        "account": { "uuid": "u-scripted", "email_address": "scripted@test.local" }
    })
    .to_string();
    let (addr, server) = runtime.block_on(spawn_http_stub("200 OK", token_reply, 1));
    let auth_url = format!("http://{addr}/authorize");
    let token_url = format!("http://{addr}/token");

    let (mut app, rx, mcp, _cfg_path, auth_dir) =
        open_test_app(&runtime, &root, SEED_CONFIG, |backend| {
            backend
                .login_state
                .overrides
                .subscription_urls
                .insert("claude".to_owned(), (auth_url.clone(), token_url.clone()));
        });

    app.paste("/login claude");
    app.press(key(KeyCode::Enter));

    let mut shown_url = String::new();
    let got_url = drive(&mut app, &rx, Duration::from_secs(5), |a| {
        let s = screen(a, 420, 36);
        if s.contains(&auth_url) && s.contains("state=") {
            shown_url = s;
            true
        } else {
            false
        }
    });
    assert!(got_url, "OAuth authorize URL was not shown:\n{shown_url}");

    let state = extract_state_from_screen(&shown_url).expect("state param in URL");
    app.paste(&format!(
        "http://localhost:54545/callback?code=auth-code-123&state={state}"
    ));
    app.press(key(KeyCode::Enter));

    let finished = drive(&mut app, &rx, Duration::from_secs(5), |a| {
        let s = screen(a, 140, 36);
        s.contains("Logged in to Claude Pro/Max") || s.contains("logged in · claude")
    });
    assert!(
        finished,
        "OAuth exchange did not finish:\n{}",
        screen(&mut app, 140, 36)
    );

    let cred_file = auth_dir.join("claude.json");
    let raw = std::fs::read_to_string(&cred_file).expect("read claude.json");
    assert!(
        raw.contains("sk-ant-oat01-scripted-tok") && raw.contains("scripted@test.local"),
        "{raw}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&cred_file)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let requests = runtime.block_on(server).expect("stub join");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("auth-code-123") && requests[0].contains("code_verifier"));

    app.paste("/auth");
    app.press(key(KeyCode::Enter));
    let auth_view = screen(&mut app, 140, 36);
    assert!(
        auth_view.contains("anthropic") && auth_view.contains("scripted@test.local"),
        "router did not reload Claude subscription:\n{auth_view}"
    );
    runtime.block_on(mcp.shutdown());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn api_key_probe_failure_writes_nothing() {
    let runtime = tokio::runtime::Runtime::new().expect("rt");
    let root = temp_root("probe-fail");
    let (addr, server) = runtime.block_on(spawn_http_stub(
        "401 Unauthorized",
        r#"{"error":{"message":"invalid api key"}}"#.to_owned(),
        2,
    ));
    let base_url = format!("http://{addr}/v1");
    let bad_secret = "sk-bad-secret-must-never-leak-999";

    // 1. Fresh directory probe via `setup::probe_and_apply_login` writes ZERO files on 401.
    let fresh = root.join("fresh");
    std::fs::create_dir_all(&fresh).expect("fresh");
    let http = HttpClient::new().expect("http");
    let err = runtime
        .block_on(setup::probe_and_apply_login(
            &http,
            &fresh.join("config.toml"),
            Some(&fresh),
            &LoginSubmission {
                provider_id: "custom".to_owned(),
                custom_name: Some("badprov".to_owned()),
                api_key: bad_secret.to_owned(),
                base_url: Some(base_url.clone()),
                model_id: Some("bad-model".to_owned()),
            },
            false,
        ))
        .expect_err("401 probe must fail")
        .to_string();
    assert!(err.contains("401") && !err.contains(bad_secret), "{err}");
    assert_eq!(std::fs::read_dir(&fresh).expect("ls").count(), 0);

    // 2. Full TUI `/login custom` keeps modal open with sanitized error & zero disk writes.
    let (mut app, rx, mcp, cfg_path, _auth_dir) =
        open_test_app(&runtime, &root, SEED_CONFIG, |_| {});
    let before_cfg = std::fs::read_to_string(&cfg_path).expect("before cfg");
    let keys_dir = cfg_path.parent().expect("parent").join("keys");
    assert!(!keys_dir.exists());

    app.paste("/login custom");
    app.press(key(KeyCode::Enter));
    for (idx, val) in ["badprov", base_url.as_str(), "bad-model", bad_secret]
        .iter()
        .enumerate()
    {
        app.press(ctrl('u'));
        app.paste(val);
        app.press(key(if idx < 3 {
            KeyCode::Tab
        } else {
            KeyCode::Enter
        }));
    }

    let mut failed_screen = String::new();
    let saw_err = drive(&mut app, &rx, Duration::from_secs(5), |a| {
        let s = screen(a, 140, 36);
        if s.contains("401") {
            failed_screen = s;
            true
        } else {
            false
        }
    });
    assert!(
        saw_err,
        "probe failure error not displayed:\n{failed_screen}"
    );
    assert!(
        failed_screen.contains("API key"),
        "modal must stay open:\n{failed_screen}"
    );
    assert!(
        !failed_screen.contains(bad_secret),
        "secret leaked:\n{failed_screen}"
    );
    assert!(!keys_dir.exists(), "keys dir must not be created");
    assert_eq!(
        std::fs::read_to_string(&cfg_path).expect("after"),
        before_cfg
    );

    assert_eq!(runtime.block_on(server).expect("server").len(), 2);
    runtime.block_on(mcp.shutdown());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn logout_removes_and_reloads() {
    let runtime = tokio::runtime::Runtime::new().expect("rt");
    let root = temp_root("logout-reload");
    let harness_dir = root.join("home").join(".harness");
    let keys_dir = harness_dir.join("keys");
    let auth_dir = harness_dir.join("auth");
    std::fs::create_dir_all(&keys_dir).expect("keys_dir");
    std::fs::create_dir_all(&auth_dir).expect("auth_dir");

    let key_file = keys_dir.join("myprov.key");
    std::fs::write(&key_file, "sk-live-secret-123\n").expect("write key");
    let claude_file = auth_dir.join(Subscription::Claude.file_name());
    upsert_subscription_account(
        Subscription::Claude,
        &claude_file,
        SubscriptionCredential {
            provider: "claude".to_owned(),
            access: "sk-ant-oat01-live".to_owned(),
            refresh: "rt-live".to_owned(),
            expires: u64::MAX / 2,
            email: Some("me@example.com".to_owned()),
            ..SubscriptionCredential::default()
        },
    )
    .expect("seed claude");

    let key_norm = key_file.to_string_lossy().replace('\\', "/");
    let cfg_toml = format!(
        "[[providers]]\nname = \"myprov\"\nkind = \"openai_chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key_file = \"{key_norm}\"\n\
         [models.myprov]\nprovider = \"myprov\"\nid = \"m1\"\ncontext_window = 8192\n\
         [roles]\ndefault = \"myprov\"\n"
    );
    let (mut app, _rx, mcp, _cfg_path, _ad) = open_test_app(&runtime, &root, &cfg_toml, |_| {});

    app.paste("/auth");
    app.press(key(KeyCode::Enter));
    let before = screen(&mut app, 140, 36);
    assert!(
        before.contains("myprov") && before.contains("ok") && before.contains("me@example.com"),
        "{before}"
    );

    app.paste("/logout myprov");
    app.press(key(KeyCode::Enter));
    let confirm = screen(&mut app, 140, 36);
    assert!(
        confirm.contains("Logout · Confirm") && confirm.contains("Remove stored"),
        "{confirm}"
    );
    app.press(key(KeyCode::Char('y')));
    assert!(!key_file.exists(), "myprov.key must be deleted");

    app.paste("/auth");
    app.press(key(KeyCode::Enter));
    assert!(screen(&mut app, 140, 36).contains("missing"));

    app.paste("/logout claude");
    app.press(key(KeyCode::Enter));
    app.press(key(KeyCode::Char('y')));
    assert!(!claude_file.exists(), "claude.json must be deleted");

    app.paste("/logout claude");
    app.press(key(KeyCode::Enter));
    let empty = screen(&mut app, 140, 36);
    assert!(empty.contains("no stored"), "{empty}");
    runtime.block_on(mcp.shutdown());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn oauth_hint_removed_and_google_oauth_still_blocked() {
    let choices = setup::provider_choices_in(None, None);
    let ids: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
    for expected in [
        "claude",
        "chatgpt",
        "kimi-code",
        "anthropic",
        "openai",
        "gemini",
        "openrouter",
        "ollama",
        "groq",
        "moonshot",
        "xai",
    ] {
        assert!(ids.contains(&expected), "missing `{expected}` in {ids:?}");
    }
    for choice in &choices {
        assert!(
            !choice.hint.contains("run `hrn login`"),
            "{}: {}",
            choice.id,
            choice.hint
        );
    }
    assert!(
        !choices
            .iter()
            .find(|c| c.id == "gemini")
            .expect("gemini")
            .is_oauth
    );
    for blocked in [
        "google",
        "antigravity",
        "google-antigravity",
        "gemini-cli",
        "gemini-oauth",
        "code-assist",
        "cloudcode",
    ] {
        assert!(setup::reject_google_oauth(blocked).is_err());
    }
    assert!(
        guard::check_oauth_client_id(
            "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
        )
        .is_err()
    );
    assert!(
        guard::check_oauth_client_id(
            "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
        )
        .is_err()
    );
    assert!(!include_str!("../../../front/tui/src/login.rs").contains("OAuthHint"));
}

#[test]
fn kimi_config_providers_table_and_project_layer_blocked() {
    let root = temp_root("kimi-cfg");
    let home = root.join("home");
    let auth_dir = home.join(".harness").join("auth");
    std::fs::create_dir_all(&auth_dir).expect("auth_dir");
    std::fs::write(auth_dir.join(Subscription::KimiCode.file_name()), "{}").expect("seed kimi");

    let catalogs = hrn_edge_llm::standard_catalogs(&home, &auth_dir);
    let cfg_providers = Config::parse_with_catalogs(
        "[providers.kimi]\napi_format = \"anthropic\"\n[roles]\ndefault = \"kimi-code/kimi-for-coding\"\n",
        Some(&home),
        catalogs.clone(),
    )
    .expect("[providers.kimi]");
    assert_eq!(cfg_providers.llm.kimi.api_format, KimiApiFormat::Anthropic);

    let cfg_llm = Config::parse_with_catalogs(
        "[llm.kimi]\napi_format = \"openai\"\n[roles]\ndefault = \"kimi-code/kimi-for-coding\"\n",
        Some(&home),
        catalogs,
    )
    .expect("[llm.kimi]");
    assert_eq!(cfg_llm.llm.kimi.api_format, KimiApiFormat::Openai);

    let global_path = root.join("global.toml");
    let project_path = root.join("project.toml");
    std::fs::write(
        &global_path,
        format!("{SEED_CONFIG}\n[llm.kimi]\napi_format = \"openai\"\n"),
    )
    .expect("global");
    std::fs::write(
        &project_path,
        "[llm.kimi]\napi_format = \"anthropic\"\n[[providers]]\nname = \"evil\"\nkind = \"openai_chat\"\nbase_url = \"https://evil.example/v1\"\n",
    )
    .expect("project");
    let merged = load_layers(&LayerInput {
        global_path: Some(global_path),
        project_path: Some(project_path),
        overlays: Vec::new(),
        cwd: root.clone(),
        home: Some(home),
        env: BTreeMap::new(),
        runtime: toml::Table::new(),
    })
    .expect("load layers");
    let cfg = Config::from_merged(&merged, None).expect("from merged");
    assert_eq!(cfg.llm.kimi.api_format, KimiApiFormat::Openai);
    assert!(!cfg.llm.providers.contains_key("evil"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tui_setup_wizard_completes_and_persists_role_and_theme() {
    let runtime = tokio::runtime::Runtime::new().expect("rt");
    let root = temp_root("setup-tui");
    let (mut app, _rx, mcp, cfg_path, _auth_dir) =
        open_test_app(&runtime, &root, SEED_CONFIG, |_| {});

    app.paste("/setup");
    app.press(key(KeyCode::Enter));
    assert!(screen(&mut app, 120, 30).contains("Setup · Step 1/3"));
    app.press(key(KeyCode::Char('s')));
    assert!(screen(&mut app, 120, 30).contains("Setup · Step 2/3"));
    app.press(key(KeyCode::Enter));
    assert!(screen(&mut app, 120, 30).contains("Setup · Step 3/3"));
    app.press(key(KeyCode::Down));
    app.press(key(KeyCode::Enter));

    let saved = std::fs::read_to_string(&cfg_path).expect("read cfg");
    assert!(
        saved.contains("theme = \"titanium-vivid\"") && saved.contains("default = \"seed\""),
        "{saved}"
    );
    runtime.block_on(mcp.shutdown());
    let _ = std::fs::remove_dir_all(&root);
}
