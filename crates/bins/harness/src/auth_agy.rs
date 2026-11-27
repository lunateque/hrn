//! `hrn login agy [--method oauth-personal|oauth-business]` handler.
//!
//! 100% ToS-compliant: delegates authentication exclusively to the official `agy` binary via ACP
//! `authenticate` (`agy acp --stdio`) and stores zero tokens in `hrn`.

use crate::cli::LoginArgs;
use crate::config::Config;
use crate::term::{err_line, out};
use hrn_edge_llm::agy_acp::{DEFAULT_DISCOVER_TIMEOUT, discover_agy_binary, discover_agy_models};
use hrn_edge_llm::{
    AgyAuthMethod, ProviderKind, build_agy_catalog, default_agy_provider, login_agy,
    save_cached_catalog,
};
use hrn_kernel::{HrnError, Result};
use std::path::Path;
use std::time::Duration;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Execute `hrn login agy [--method oauth-personal|oauth-business]`.
pub(crate) async fn login_agy_cli(
    config_path: &Path,
    existing_cfg: Option<&Config>,
    args: &LoginArgs,
) -> Result {
    let method = match args.method.as_deref() {
        None => AgyAuthMethod::OauthPersonal,
        Some(raw) => AgyAuthMethod::parse(raw).ok_or_else(|| {
            HrnError::Config(format!(
                "unknown --method `{raw}` for `hrn login agy` (expected `oauth-personal` or `oauth-business`)"
            ))
        })?,
    };

    let explicit_binary = existing_cfg.and_then(|cfg| {
        cfg.llm
            .providers
            .values()
            .find(|p| p.kind == ProviderKind::AgyAcp)
            .and_then(|p| p.agy.binary.as_deref())
    });

    let binary = discover_agy_binary(explicit_binary).ok_or_else(|| {
        HrnError::Config(
            "`agy` CLI not found on PATH; install Antigravity CLI >= 1.5.0 first (or set \
             `binary` under `[[providers]]` / `HRN_AGY_BIN`)"
                .to_owned(),
        )
    })?;

    let cwd = std::env::current_dir()?;
    out(&format!(
        "Authenticating via official `agy` CLI ({}) using `{}`...\n",
        binary.display(),
        method.as_wire_str()
    ));
    let summary = login_agy(&binary, &cwd, method, LOGIN_TIMEOUT).await?;
    out(&format!(
        "{summary}\nAntigravity CLI manages its own credentials; `hrn` stores no tokens.\n"
    ));

    match discover_agy_models(&binary, &cwd, DEFAULT_DISCOVER_TIMEOUT).await {
        Ok(discovered) if !discovered.models.is_empty() => {
            let (prov_name, prov_entry) = existing_cfg
                .and_then(|cfg| {
                    cfg.llm
                        .providers
                        .iter()
                        .find(|(_, p)| p.kind == ProviderKind::AgyAcp)
                        .map(|(n, p)| (n.as_str(), p.clone()))
                })
                .unwrap_or_else(|| ("agy", default_agy_provider()));
            if let Some(cat) = build_agy_catalog(prov_name, &prov_entry, &discovered)
                && let Ok(data_dir) = crate::app_data_dir()
            {
                save_cached_catalog(&data_dir.join("catalog-cache"), prov_name, &cat);
            }
            out("\nAvailable Antigravity models:\n");
            for m in &discovered.models {
                let marker = if discovered.current_model.as_deref() == Some(m.value.as_str()) {
                    " (default)"
                } else {
                    ""
                };
                out(&format!("  - {}{marker}\n", m.value));
            }
        }
        Ok(_) => {}
        Err(err) => {
            err_line(&format!(
                "note: could not list Antigravity models immediately ({err}); run `hrn doctor` to check"
            ));
        }
    }

    out(&format!(
        "\n`agy` is auto-discovered automatically — run `hrn --model agy` (or pick `agy/...` via `/model`).\n\
         To make it your default model, set `default = \"agy\"` under `[roles]` in {}.\n",
        config_path.display()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::logout::remove_provider_credentials;
    use hrn_edge_llm::{AgyToolMode, AuthMode, ProviderKind};
    use std::path::Path;

    #[test]
    fn config_lowers_agy_acp_provider_and_logout_hint() {
        let valid_toml = r#"
[[providers]]
name = "agy"
kind = "agy_acp"
tool_mode = "hrn"
handoff_threshold_tokens = 16000
handoff_model = "smol"
turn_timeout_ms = 300000
stall_timeout_ms = 60000
tool_stall_timeout_ms = 180000
add_dirs = ["extra"]

[models.agy-sonnet]
provider = "agy"
id = "claude-sonnet-4-6"
context_window = 200000
thinking = "medium"

[roles]
default = "agy-sonnet"
"#;
        let cfg = Config::parse_with_home(valid_toml, None).expect("valid agy_acp config");
        let prov = cfg.llm.providers.get("agy").expect("agy provider");
        assert_eq!(prov.kind, ProviderKind::AgyAcp);
        assert_eq!(prov.auth_mode, AuthMode::Agy);
        assert_eq!(prov.agy.tool_mode, AgyToolMode::Hrn);
        assert_eq!(prov.agy.handoff_threshold_tokens, 16_000);
        assert_eq!(prov.agy.handoff_model, "smol");
        assert_eq!(prov.agy.add_dirs, vec!["extra"]);

        // `hrn logout agy` never touches disk and explains delegation cleanly.
        let msg = remove_provider_credentials(
            Some(&cfg),
            Path::new("config.toml"),
            Path::new("auth"),
            "agy",
        )
        .expect("logout agy message");
        assert!(msg.contains("agy /logout"), "unexpected message: {msg}");
    }

    #[test]
    fn config_rejects_agy_acp_misconfigurations() {
        // API key, base_url, or price on agy_acp must be rejected.
        for bad in [
            r#"
[[providers]]
name = "agy"
kind = "agy_acp"
api_key_env = "AGY_KEY"
[models.m]
provider = "agy"
id = "claude-sonnet-4-6"
context_window = 200000
[roles]
default = "m"
"#,
            r#"
[[providers]]
name = "agy"
kind = "agy_acp"
base_url = "https://cloudcode-pa.googleapis.com"
[models.m]
provider = "agy"
id = "claude-sonnet-4-6"
context_window = 200000
[roles]
default = "m"
"#,
            r#"
[[providers]]
name = "agy"
kind = "agy_acp"
[models.m]
provider = "agy"
id = "claude-sonnet-4-6"
context_window = 200000
price = { input = 3.0, output = 15.0 }
[roles]
default = "m"
"#,
            r#"
[[providers]]
name = "openai"
kind = "openai_chat"
tool_mode = "hrn"
[models.m]
provider = "openai"
id = "gpt-4o"
context_window = 128000
[roles]
default = "m"
"#,
            r#"
[[providers]]
name = "openai"
kind = "openai_chat"
auth_mode = "agy"
[models.m]
provider = "openai"
id = "gpt-4o"
context_window = 128000
[roles]
default = "m"
"#,
        ] {
            let err = Config::parse_with_home(bad, None).expect_err("must reject invalid config");
            let msg = err.to_string();
            assert!(
                !msg.contains("context_window"),
                "failed for wrong reason ({msg}) on:\n{bad}"
            );
        }
    }
}
