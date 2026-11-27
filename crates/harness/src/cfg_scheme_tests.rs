use super::*;
use crate::testutil::{block_on, hook_ctx};
use hrn_kernel::BoxFuture;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct MockPort {
    views: Mutex<Vec<SettingView>>,
    saved: Mutex<Vec<(String, String)>>,
    session: Mutex<Vec<(String, String)>>,
}

fn sv(
    key: &str,
    value: serde_json::Value,
    default: Option<serde_json::Value>,
    kind: &str,
    source: &str,
    description: &str,
) -> SettingView {
    SettingView {
        key: key.into(),
        value: Some(value),
        default,
        kind: kind.into(),
        source: source.into(),
        values: Vec::new(),
        description: description.into(),
        secret: false,
    }
}

impl MockPort {
    fn sample_rows() -> Vec<SettingView> {
        let rows = [
            (
                "tui.theme",
                json!("titanium"),
                json!("titanium"),
                "string",
                "default",
                "TUI color theme.",
            ),
            (
                "tools.premium_defer",
                json!(true),
                json!(true),
                "bool",
                "default",
                "Defer rarely-used tools behind tool_search.",
            ),
            (
                "shell.git_guard",
                json!(true),
                json!(false),
                "bool",
                "global config",
                "Refuse destructive git actions in main checkout.",
            ),
            (
                "eval.tools_enabled",
                json!(false),
                json!(true),
                "bool",
                "global config",
                "Allow kernel-defined @tool functions.",
            ),
            (
                "desktop.eval_prelude",
                json!(false),
                json!(true),
                "bool",
                "global config",
                "Expose computer.* facade in eval.",
            ),
            (
                "edit.block_auto_generated",
                json!(true),
                json!(true),
                "bool",
                "default",
                "Refuse edits to auto-generated files.",
            ),
            (
                "lsp.enabled",
                json!(false),
                json!(true),
                "bool",
                "global config",
                "Enable LSP tool and hook.",
            ),
            (
                "gc.retain_days",
                json!(90),
                json!(90),
                "integer",
                "default",
                "Days before unpinned sessions become GC-eligible.",
            ),
            (
                "commit.enabled",
                json!(true),
                json!(true),
                "bool",
                "default",
                "Enable attributed /commit workflow.",
            ),
            (
                "update.repo",
                json!("lunateque/harness"),
                json!("lunateque/harness"),
                "string",
                "default",
                "GitHub owner/repo slug for hrn update.",
            ),
        ];
        rows.into_iter()
            .map(|(k, v, d, kind, src, desc)| sv(k, v, Some(d), kind, src, desc))
            .collect()
    }

    fn sample() -> Arc<Self> {
        let mut web_key = sv(
            "web.api_key",
            json!("super-secret-token"),
            None,
            "string",
            "global config",
            "Web provider API key.",
        );
        web_key.secret = true;
        let mut tier = sv(
            "llm.tier.openai",
            json!("none"),
            Some(json!("none")),
            "string",
            "default",
            "OpenAI service tier.",
        );
        tier.values = vec!["none".into(), "priority".into()];
        let mut views = Self::sample_rows();
        views.push(web_key);
        views.push(tier);
        Arc::new(Self {
            views: Mutex::new(views),
            saved: Mutex::new(Vec::new()),
            session: Mutex::new(Vec::new()),
        })
    }
}

impl SettingsPort for MockPort {
    fn list(&self) -> Vec<SettingView> {
        self.views.lock().unwrap().clone()
    }

    fn get(&self, key: &str) -> Option<SettingView> {
        self.views
            .lock()
            .unwrap()
            .iter()
            .find(|v| v.key == key)
            .cloned()
    }

    fn similar(&self, _key: &str) -> Vec<String> {
        vec!["tools.premium_defer".into()]
    }

    fn set_session(&self, key: &str, raw_value: &str) -> Result<String> {
        self.session
            .lock()
            .unwrap()
            .push((key.to_owned(), raw_value.to_owned()));
        Ok("ok".into())
    }

    fn save_global(&self, key: &str, raw_value: &str) -> Result<String> {
        self.saved
            .lock()
            .unwrap()
            .push((key.to_owned(), raw_value.to_owned()));
        Ok("ok".into())
    }

    fn confirm(
        &self,
        gate: &Arc<dyn ApprovalGate>,
        request: ApprovalRequest,
        cancel: CancelFlag,
    ) -> Result<ApprovalDecision> {
        block_on(gate.await_decision(request, cancel))
    }
}

struct CountingGate {
    decision: ApprovalDecision,
    calls: AtomicUsize,
}

impl ApprovalGate for CountingGate {
    fn await_decision(
        &self,
        _request: ApprovalRequest,
        _cancel: CancelFlag,
    ) -> BoxFuture<Result<ApprovalDecision>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let d = self.decision.clone();
        Box::pin(async move { Ok(d) })
    }
}

#[test]
fn read_leaf_redacts_secret() {
    let port = MockPort::sample();
    let scheme = CfgScheme::new(port);
    let ctx = hook_ctx();

    let UrlBody::Text(leaf) = scheme.read(&ctx, "web.api_key").expect("read secret") else {
        panic!("expected text");
    };
    assert!(leaf.contains("value: <redacted>"), "{leaf}");
    assert!(!leaf.contains("super-secret-token"), "{leaf}");

    let UrlBody::Text(tree) = scheme.read(&ctx, "").expect("read tree") else {
        panic!("expected text");
    };
    assert!(tree.contains("tools.premium_defer = true"), "{tree}");
    assert!(!tree.contains("super-secret-token"), "{tree}");

    let err = scheme
        .read(&ctx, "tools.premium_defr")
        .unwrap_err()
        .to_string();
    assert!(err.contains("Similar: tools.premium_defer"), "{err}");
}

#[test]
fn write_prompts_supports_session_grant_save_and_blocks_subagents() {
    let port = MockPort::sample();
    let scheme = CfgScheme::new(Arc::clone(&port) as Arc<dyn SettingsPort>);

    // Subagents cannot write.
    let mut sub_ctx = hook_ctx();
    sub_ctx.is_subagent = true;
    assert!(
        scheme
            .write(&sub_ctx, "tools.premium_defer", "false")
            .is_err()
    );

    // Headless without gate fails closed.
    let ctx = hook_ctx();
    assert!(scheme.write(&ctx, "tools.premium_defer", "false").is_err());

    // Session grant skips prompt on second write for non-security settings; `/save` persists.
    let grants = GrantCache::memory();
    let scheme = Arc::new(scheme);
    scheme.bind_grant_cache(&grants);
    let gate = Arc::new(CountingGate {
        decision: ApprovalDecision::tool_session(),
        calls: AtomicUsize::new(0),
    });
    let mut live_ctx = hook_ctx();
    live_ctx.approval = Some(Arc::clone(&gate) as Arc<dyn ApprovalGate>);
    scheme
        .write(&live_ctx, "tui.theme", "nord")
        .expect("session write");
    scheme
        .write(&live_ctx, "tui.theme/save", "solarized")
        .expect("global save");
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

    // Security-sensitive keys (`tools.*`, `shell.*`, `llm.*`, `eval.*`, `desktop.eval_prelude`)
    // ALWAYS re-prompt with Once-only scope even with a session grant.
    scheme
        .write(&live_ctx, "tools.premium_defer", "false")
        .expect("tools write");
    scheme
        .write(&live_ctx, "shell.git_guard", "false")
        .expect("shell write");
    scheme
        .write(&live_ctx, "llm.tier.openai", "priority")
        .expect("llm tier write");
    scheme
        .write(&live_ctx, "eval.tools_enabled", "true")
        .expect("eval write");
    scheme
        .write(&live_ctx, "desktop.eval_prelude", "true")
        .expect("desktop eval_prelude write");
    scheme
        .write(&live_ctx, "edit.block_auto_generated", "false")
        .expect("edit guard write");
    scheme
        .write(&live_ctx, "lsp.enabled", "true")
        .expect("lsp write");
    assert_eq!(gate.calls.load(Ordering::SeqCst), 8);
    assert!(is_security_sensitive_key("edit"));
    assert!(is_security_sensitive_key("edit.auto_repair"));
    assert!(is_security_sensitive_key("lsp"));
    assert!(is_security_sensitive_key("lsp.lazy"));
    assert!(is_security_sensitive_key("lsp.format_on_write"));
    assert!(is_security_sensitive_key("llm.accounts"));
    assert!(is_security_sensitive_key("llm.retry.usage_reserve_pct"));
    assert!(is_security_sensitive_key("llm.tier.anthropic"));
    assert!(is_security_sensitive_key("llm.extended_context"));
    assert!(is_security_sensitive_key("llm.anthropic.slow_mode"));
    assert!(is_security_sensitive_key("llm.kimi"));
    assert!(is_security_sensitive_key("llm.kimi.api_format"));
    assert!(is_security_sensitive_key("share"));
    assert!(is_security_sensitive_key("share.redact"));
    assert!(is_security_sensitive_key("share.server"));
    assert!(is_security_sensitive_key("share.store"));
    assert!(is_security_sensitive_key("collab"));
    assert!(is_security_sensitive_key("collab.relay"));

    // Clearing `GrantCache` (`/forget-approvals`) also clears `cfg://` session grants.
    grants.clear().expect("clear grants");
    scheme
        .write(&live_ctx, "tui.theme", "dark")
        .expect("re-prompted after forget");
    assert_eq!(gate.calls.load(Ordering::SeqCst), 9);
    assert_eq!(
        *port.saved.lock().unwrap(),
        vec![("tui.theme".into(), "solarized".into())]
    );
}

#[test]
fn gc_keys_require_once_approval_even_with_session_grant() {
    struct CapturingGate {
        choices: Mutex<Vec<Vec<ApprovalGrantScope>>>,
    }

    impl ApprovalGate for CapturingGate {
        fn await_decision(
            &self,
            request: ApprovalRequest,
            _cancel: CancelFlag,
        ) -> BoxFuture<Result<ApprovalDecision>> {
            self.choices.lock().unwrap().push(request.choices);
            Box::pin(async { Ok(ApprovalDecision::tool_session()) })
        }
    }

    let port = MockPort::sample();
    let scheme = CfgScheme::new(Arc::clone(&port) as Arc<dyn SettingsPort>);
    let gate = Arc::new(CapturingGate {
        choices: Mutex::new(Vec::new()),
    });
    let mut ctx = hook_ctx();
    ctx.approval = Some(Arc::clone(&gate) as Arc<dyn ApprovalGate>);

    // Grant ToolSession on a non-sensitive key first.
    scheme
        .write(&ctx, "tui.theme", "nord")
        .expect("theme write");
    assert_eq!(gate.choices.lock().unwrap().len(), 1);

    // Writing `gc.*` (session or `/save`) MUST still prompt every time with Once-only choices.
    scheme
        .write(&ctx, "gc.retain_days", "0")
        .expect("gc session write");
    scheme
        .write(&ctx, "gc.retain_days/save", "0")
        .expect("gc global save");

    let recorded = gate.choices.lock().unwrap();
    assert_eq!(recorded.len(), 3, "every gc.* write must re-prompt");
    assert_eq!(recorded[1], vec![ApprovalGrantScope::Once]);
    assert_eq!(recorded[2], vec![ApprovalGrantScope::Once]);

    assert!(is_security_sensitive_key("gc"));
    assert!(is_security_sensitive_key("gc.retain_days"));
    assert!(is_security_sensitive_key("gc.retain_newest_per_cwd"));
    assert!(is_security_sensitive_key("gc.retain_newest_global"));
    assert!(is_security_sensitive_key("gc.vacuum_reclaim_bytes"));
    assert!(is_security_sensitive_key("commit"));
    assert!(is_security_sensitive_key("commit.enabled"));
    assert!(is_security_sensitive_key("commit.changelog"));
    assert!(is_security_sensitive_key("update"));
    assert!(is_security_sensitive_key("update.repo"));
    assert!(is_security_sensitive_key("update.channel"));
}

#[test]
fn commit_and_update_keys_require_once_approval_even_with_session_grant() {
    struct CapturingGate {
        choices: Mutex<Vec<Vec<ApprovalGrantScope>>>,
    }

    impl ApprovalGate for CapturingGate {
        fn await_decision(
            &self,
            request: ApprovalRequest,
            _cancel: CancelFlag,
        ) -> BoxFuture<Result<ApprovalDecision>> {
            self.choices.lock().unwrap().push(request.choices);
            Box::pin(async { Ok(ApprovalDecision::tool_session()) })
        }
    }

    let port = MockPort::sample();
    let scheme = CfgScheme::new(Arc::clone(&port) as Arc<dyn SettingsPort>);
    let gate = Arc::new(CapturingGate {
        choices: Mutex::new(Vec::new()),
    });
    let mut ctx = hook_ctx();
    ctx.approval = Some(Arc::clone(&gate) as Arc<dyn ApprovalGate>);

    scheme
        .write(&ctx, "tui.theme", "nord")
        .expect("theme write");
    scheme
        .write(&ctx, "commit.enabled", "true")
        .expect("commit write");
    scheme
        .write(&ctx, "update.repo/save", "attacker/evil")
        .expect("update write");

    let recorded = gate.choices.lock().unwrap();
    assert_eq!(recorded.len(), 3, "commit.* and update.* must re-prompt");
    assert_eq!(recorded[1], vec![ApprovalGrantScope::Once]);
    assert_eq!(recorded[2], vec![ApprovalGrantScope::Once]);
}
