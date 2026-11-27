//! `hrn usage` and `hrn usage accounts`: provider usage limits and stored account listing.

use crate::term::out;
use hrn_edge_http::HttpClient;
use hrn_edge_llm::{fetch_usage, render_usage};
use hrn_edge_oauth::{Subscription, read_subscription_accounts};
use hrn_kernel::Result;
use std::path::Path;

/// Print the limits of every stored account of every logged-in subscription in `auth_dir`.
pub(crate) async fn run(auth_dir: &Path) -> Result {
    let http = HttpClient::new()?;
    out(&render_usage(&fetch_usage(&http, auth_dir).await));
    Ok(())
}

/// Collect `<provider>/<account_id>` lines for all stored subscription accounts (offline).
pub(crate) fn account_ids(auth_dir: &Path) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    for kind in Subscription::ALL {
        let file = auth_dir.join(kind.file_name());
        if !file.is_file() {
            continue;
        }
        let accounts = read_subscription_accounts(kind, &file)?;
        for cred in accounts {
            rows.push(format!("{}/{}", kind.id(), cred.identity_key()));
        }
    }
    Ok(rows)
}

/// Print `<provider>/<account_id>` for every stored subscription account (offline, zero network).
pub(crate) fn accounts(auth_dir: &Path) -> Result {
    let rows = account_ids(auth_dir)?;
    if rows.is_empty() {
        out("No subscription accounts stored.\n");
        return Ok(());
    }
    for row in rows {
        out(&format!("{row}\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_edge_oauth::{SubscriptionCredential, upsert_subscription_account};

    #[test]
    fn lists_all_stored_accounts_offline() {
        let dir = std::env::temp_dir().join(format!("hrn-usage-accts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (kind, id) in [
            (Subscription::Claude, "acct-1"),
            (Subscription::Claude, "acct-2"),
            (Subscription::Chatgpt, "gpt-1"),
        ] {
            let file = dir.join(kind.file_name());
            let mut cred = SubscriptionCredential {
                provider: kind.id().into(),
                access: "tok".into(),
                ..SubscriptionCredential::default()
            };
            if kind == Subscription::Claude {
                cred.account_id = Some(id.into());
            } else {
                cred.chatgpt_account_id = Some(id.into());
            }
            upsert_subscription_account(kind, &file, cred).expect("seed");
        }
        let ids = account_ids(&dir).expect("ids");
        assert_eq!(ids, ["claude/acct-1", "claude/acct-2", "chatgpt/gpt-1"]);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
