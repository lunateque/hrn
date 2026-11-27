//! Self-contained RFC 6749 `refresh_token` manager, RFC 7636 `OAuth2` PKCE (`S256`) login,
//! and RFC 8628 device authorization flow with per-file single-flight locking and atomic `0600`
//! persistence, plus the built-in Claude Pro/Max, `ChatGPT` Plus/Pro, and Kimi Code subscriptions.

mod callback;
mod device_code;
mod jwt;
mod kimi;
mod lock;
mod manager;
mod pkce;
pub mod pool;
mod sha256;
mod sub_refresh;
mod sub_store;
mod subscription;

pub use device_code::{
    DEVICE_CODE_GRANT_TYPE, DeviceAuthResponse, DeviceCodeConfig, poll_device_token,
    start_device_flow,
};
pub use kimi::{
    KIMI_CLI_VERSION, KIMI_CLIENT_ID, KIMI_DEVICE_AUTH_URL, KIMI_DEVICE_ID_FILE, KIMI_TOKEN_URL,
    kimi_device_code_config, kimi_device_code_config_with_urls, kimi_fingerprint_headers,
    load_or_create_kimi_device_id,
};
pub use manager::{
    OAuthConfig, OAuthManager, StoredToken, TokenFileStatus, inspect_token_file, write_bytes_atomic,
};
pub use pkce::{
    PkceChallenge, PkceLoginConfig, build_authorize_url, generate_pkce, parse_manual_callback,
    run_pkce_login, run_pkce_login_interactive, run_pkce_manual_login,
};
pub use pool::{
    AccountPolicy, AccountUsageSnapshot, CredentialPool, DEFAULT_RESERVE_PCT, USAGE_CACHE_TTL_MS,
    USAGE_FAILURE_BACKOFF_MS, USAGE_LAST_GOOD_MS, canonical_provider,
};
pub use sha256::{sha256, to_hex};
pub use sub_store::{
    SubscriptionCredential, SubscriptionStatus, SubscriptionStore, read_subscription,
    read_subscription_accounts, subscription_accounts_status, subscription_status,
    upsert_subscription_account,
};
pub use subscription::{
    LoginIo, Subscription, run_subscription_login, run_subscription_login_with_urls,
};
