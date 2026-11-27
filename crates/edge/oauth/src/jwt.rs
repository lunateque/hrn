//! Unverified JWT payload reading for identity claims of tokens hrn just received over TLS from
//! the issuer itself (signature checks would add nothing: the claims only label the login).

use crate::sha256::base64url_decode;
use serde_json::{Map, Value};

/// Namespace claim carrying the ChatGPT account, plan and residency.
const OPENAI_AUTH_CLAIM: &str = "https://api.openai.com/auth";
/// Namespace claim carrying the profile (`email`) in ChatGPT access tokens.
const OPENAI_PROFILE_CLAIM: &str = "https://api.openai.com/profile";

/// Decoded payload of a compact JWT (`header.payload.signature`), or `None` when malformed.
pub(crate) fn claims(token: &str) -> Option<Map<String, Value>> {
    let mut parts = token.split('.');
    let (_header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    match serde_json::from_slice(&base64url_decode(payload)?).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

fn str_at(map: &Map<String, Value>, namespace: &str, key: &str) -> Option<String> {
    map.get(namespace)?
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Identity a ChatGPT access/id token carries.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatgptIdentity {
    /// `chatgpt_account_id`, sent as the `chatgpt-account-id` header.
    pub(crate) account_id: Option<String>,
    /// Subscription plan (`plus`, `pro`, …) for usage-limit messages.
    pub(crate) plan: Option<String>,
    /// Data residency routing value; `None` when unconstrained.
    pub(crate) residency: Option<String>,
    /// Account e-mail.
    pub(crate) email: Option<String>,
    /// Access-token expiry in Unix milliseconds.
    pub(crate) expires_ms: Option<u64>,
}

/// Read the ChatGPT identity from the access token, filling the e-mail from `id_token`.
pub(crate) fn chatgpt_identity(access: &str, id_token: Option<&str>) -> ChatgptIdentity {
    let access_claims = claims(access).unwrap_or_default();
    let id_claims = id_token.and_then(claims).unwrap_or_default();
    let auth = |key: &str| {
        str_at(&access_claims, OPENAI_AUTH_CLAIM, key)
            .or_else(|| str_at(&id_claims, OPENAI_AUTH_CLAIM, key))
    };
    let residency = auth("chatgpt_data_residency")
        .or_else(|| auth("chatgpt_compute_residency"))
        .filter(|r| r != "no_constraint");
    let email = id_claims
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| str_at(&access_claims, OPENAI_PROFILE_CLAIM, "email"))
        .filter(|e| !e.trim().is_empty());
    ChatgptIdentity {
        account_id: auth("chatgpt_account_id"),
        plan: auth("chatgpt_plan_type"),
        residency,
        email,
        expires_ms: access_claims
            .get("exp")
            .and_then(Value::as_u64)
            .map(|secs| secs.saturating_mul(1000)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sha256::base64url_encode;

    /// Unsigned compact JWT carrying `payload` (the signature part is never checked).
    pub(crate) fn fake_jwt(payload: &Value) -> String {
        format!(
            "{}.{}.sig",
            base64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#),
            base64url_encode(payload.to_string().as_bytes())
        )
    }

    #[test]
    fn chatgpt_account_plan_and_residency_come_from_the_auth_claim() {
        let access = fake_jwt(&serde_json::json!({
            "exp": 1_900_000_000u64,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-123",
                "chatgpt_plan_type": "pro",
                "chatgpt_data_residency": "eu"
            },
            "https://api.openai.com/profile": {"email": "me@example.com"}
        }));
        let id = chatgpt_identity(&access, None);
        assert_eq!(id.account_id.as_deref(), Some("acc-123"));
        assert_eq!(id.plan.as_deref(), Some("pro"));
        assert_eq!(id.residency.as_deref(), Some("eu"));
        assert_eq!(id.email.as_deref(), Some("me@example.com"));
        assert_eq!(id.expires_ms, Some(1_900_000_000_000));
    }

    #[test]
    fn unconstrained_residency_is_dropped_and_id_token_email_wins() {
        let access = fake_jwt(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_compute_residency": "no_constraint"}
        }));
        let id_token = fake_jwt(&serde_json::json!({"email": "id@example.com"}));
        let id = chatgpt_identity(&access, Some(&id_token));
        assert_eq!(id.residency, None);
        assert_eq!(id.account_id, None);
        assert_eq!(id.email.as_deref(), Some("id@example.com"));
    }

    #[test]
    fn malformed_tokens_yield_no_claims() {
        assert!(claims("not-a-jwt").is_none());
        assert!(claims("a.b").is_none());
        assert!(claims("a.!!!.c").is_none());
        assert!(claims("a.b.c.d").is_none());
        assert_eq!(chatgpt_identity("opaque", None), ChatgptIdentity::default());
    }
}
