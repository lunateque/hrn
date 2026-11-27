//! Kimi Code subscription transport (`/coding/v1`): `KimiCLI` fingerprint headers + persisted
//! `X-Msh-Device-Id`, `OpenAI` (`/chat/completions`) or Anthropic (`/messages`) wire selection via
//! `api_format`, and single 401 token recovery.

use crate::anthropic::{ANTHROPIC_VERSION, read_stream as read_anthropic_stream};
use crate::anthropic_wire::encode_request_for_host;
use crate::config::{ModelPreset, ProviderEntry, ProviderKind};
use crate::config_pool::KimiApiFormat;
use crate::openai_chat::{
    encode_request as encode_openai_request, read_stream as read_openai_stream,
};
use crate::sub_auth::SubscriptionAuth;
use bytes::Bytes;
use hrn_edge_http::StreamReply;
use hrn_edge_oauth::{kimi_fingerprint_headers, load_or_create_kimi_device_id};
use hrn_kernel::{CancelFlag, ChunkSink, LlmRequest, LlmResponse, Result};
use std::path::Path;
use std::time::Duration;

/// Resolve effective Kimi Code wire format (`Openai` vs `Anthropic`).
#[must_use]
pub(crate) fn effective_format(provider: &ProviderEntry, global: KimiApiFormat) -> KimiApiFormat {
    let chosen = match provider.api_format {
        KimiApiFormat::Auto => global,
        explicit => explicit,
    };
    match chosen {
        KimiApiFormat::Auto => {
            if provider.kind == ProviderKind::Anthropic {
                KimiApiFormat::Anthropic
            } else {
                KimiApiFormat::Openai
            }
        }
        explicit => explicit,
    }
}

/// Endpoint URL for one Kimi Code request given `base` and `format`.
#[must_use]
pub(crate) fn endpoint_url(base: &str, format: KimiApiFormat) -> String {
    let trimmed = base.trim_end_matches('/');
    match format {
        KimiApiFormat::Anthropic => {
            if trimmed.ends_with("/v1") {
                format!("{trimmed}/messages")
            } else {
                format!("{trimmed}/v1/messages")
            }
        }
        KimiApiFormat::Auto | KimiApiFormat::Openai => format!("{trimmed}/chat/completions"),
    }
}

/// Stream one request against the Kimi Code `/coding/v1` subscription endpoint.
#[expect(
    clippy::too_many_arguments,
    reason = "subscription transport passes auth, provider, preset, format, model_id, req, cancel, sink"
)]
pub(crate) async fn stream(
    auth: &mut SubscriptionAuth<'_>,
    provider: &ProviderEntry,
    preset: &ModelPreset,
    global_format: KimiApiFormat,
    model_id: &str,
    req: &LlmRequest,
    cancel: &CancelFlag,
    sink: &dyn ChunkSink,
) -> Result<LlmResponse> {
    let format = effective_format(provider, global_format);
    let url = endpoint_url(provider.effective_base_url(), format);
    let auth_dir = auth.file.parent().unwrap_or_else(|| Path::new("."));
    let device_id = load_or_create_kimi_device_id(auth_dir).await?;
    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(180));
    let payload = match format {
        KimiApiFormat::Anthropic => encode_request_for_host(
            model_id,
            req,
            provider.prompt_cache,
            provider.strip_temperature,
            false,
        )?,
        KimiApiFormat::Auto | KimiApiFormat::Openai => {
            encode_openai_request(model_id, req, provider.strip_temperature, preset, false)?
        }
    };
    let body = Bytes::from(serde_json::to_vec(&payload)?);

    loop {
        let mut headers = kimi_fingerprint_headers(&device_id);
        headers.push((
            "authorization".to_owned(),
            format!("Bearer {}", auth.cred.access),
        ));
        headers.push(("content-type".to_owned(), "application/json".to_owned()));
        headers.push(("accept".to_owned(), "text/event-stream".to_owned()));
        if format == KimiApiFormat::Anthropic {
            headers.push(("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()));
        }
        for (name, value) in provider.resolve_env_headers() {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(&name)) {
                headers.push((name, value));
            }
        }
        let reply = auth
            .http
            .post_stream(&url, &headers, body.clone(), timeout)
            .await?;
        let failed = match reply {
            StreamReply::Stream(mut sse) => {
                return match format {
                    KimiApiFormat::Anthropic => {
                        read_anthropic_stream(&mut sse, model_id, false, cancel, sink).await
                    }
                    KimiApiFormat::Auto | KimiApiFormat::Openai => {
                        read_openai_stream(&mut sse, model_id, cancel, sink).await
                    }
                };
            }
            StreamReply::Failed(failed) => failed,
        };
        if failed.status.as_u16() == 401 && auth.recover_unauthorized().await? {
            continue;
        }
        return Err(failed.status_error());
    }
}
