//! Argument-parsing tests for `cli` (the module lives beside the definitions it tests).

use super::*;

#[test]
fn exec_arguments_parse() {
    let cli = Cli::try_parse_from([
        "hrn", "exec", "--model", "flash", "--think", "high", "--yes", "do it",
    ])
    .expect("parse");
    let Some(Command::Exec(args)) = cli.command else {
        unreachable!("exec subcommand");
    };
    assert_eq!(args.model.as_deref(), Some("flash"));
    assert_eq!(args.think, Some(ThinkingLevel::High));
    assert!(args.yes);
    assert_eq!(args.prompt.as_deref(), Some("do it"));
}

#[test]
fn bad_think_level_is_a_usage_error() {
    assert!(Cli::try_parse_from(["hrn", "exec", "--think", "wild", "x"]).is_err());
}

#[test]
fn bare_hrn_and_chat_open_the_tui() {
    assert!(
        Cli::try_parse_from(["hrn"])
            .expect("parse")
            .command
            .is_none()
    );
    let cli = Cli::try_parse_from(["hrn", "chat", "--model", "glm", "--resume", "sess_1"])
        .expect("parse");
    let Some(Command::Chat(args)) = cli.command else {
        unreachable!("chat subcommand");
    };
    assert_eq!(args.model.as_deref(), Some("glm"));
    assert_eq!(args.resume.as_deref(), Some("sess_1"));
}

#[test]
fn mcp_subcommands_parse() {
    assert!(Cli::try_parse_from(["hrn", "mcp", "list"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "mcp", "ping", "demo"]).is_ok());
    let cli = Cli::try_parse_from(["hrn", "mcp", "serve", "--profile", "agy"]).expect("serve");
    let Some(Command::Mcp {
        command: McpCommand::Serve { profile },
    }) = cli.command
    else {
        unreachable!("mcp serve");
    };
    assert_eq!(profile, "agy");
}

#[test]
fn daemon_worktree_and_remote_subcommands_parse() {
    assert!(Cli::try_parse_from(["hrn", "daemon"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "attach", "ssh://dev@box/sess_1"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "worktree", "add", "tui", "--base", "main"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "worktree", "list"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "worktree", "rm", "tui", "--force"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "remote", "host-key"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "remote", "hosts"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "remote", "setup", "dev@box:2222"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "remote", "doctor", "dev@box"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "remote", "forget", "box"]).is_ok());
}

#[test]
fn session_maintenance_and_no_session_parse() {
    for word in ["delete", "rename", "pin", "unpin"] {
        let mut argv = vec!["hrn", "sessions", word, "sess_1"];
        if word == "rename" {
            argv.push("a title");
        }
        let parsed = Cli::try_parse_from(argv).expect(word);
        let Some(Command::Sessions {
            command: Some(command),
        }) = parsed.command
        else {
            unreachable!("sessions subcommand");
        };
        let described = format!("{command:?}");
        assert!(described.to_lowercase().starts_with(word), "{described}");
    }
    // `delete` takes no extra words, `rename` needs the title.
    assert!(Cli::try_parse_from(["hrn", "sessions", "delete", "s", "x"]).is_err());
    assert!(Cli::try_parse_from(["hrn", "sessions", "rename", "sess_1"]).is_err());
    // `--no-session` is a top-level flag: it reaches chat and exec only.
    let cli = Cli::try_parse_from(["hrn", "--no-session", "exec", "x"]).expect("exec");
    let Some(Command::Exec(exec)) = cli.command else {
        unreachable!("exec subcommand");
    };
    assert!(
        !exec.no_session,
        "the flag is merged in dispatch, not parsed here"
    );
    assert!(cli.no_session);
    assert!(Cli::try_parse_from(["hrn", "--no-session", "stats"]).is_ok());
}

#[test]
fn continue_mode_stats_and_export_parse() {
    assert!(
        Cli::try_parse_from(["hrn", "-c"])
            .expect("bare")
            .continue_session
    );
    let Some(Command::Chat(chat)) = Cli::try_parse_from(["hrn", "chat", "--continue"])
        .expect("chat")
        .command
    else {
        unreachable!("chat subcommand");
    };
    assert!(chat.continue_session);
    assert!(Cli::try_parse_from(["hrn", "chat", "-c", "--resume", "s"]).is_err());
    assert!(matches!(
        Cli::try_parse_from(["hrn", "stats"])
            .expect("stats")
            .command,
        Some(Command::Stats)
    ));
    let cli = Cli::try_parse_from(["hrn", "exec", "--mode", "json", "x"]).expect("exec");
    let Some(Command::Exec(exec)) = cli.command else {
        unreachable!("exec subcommand");
    };
    assert_eq!(exec.mode, OutputMode::Json);
    let cli = Cli::try_parse_from([
        "hrn", "sessions", "export", "s1", "-o", "-", "--format", "html",
    ])
    .expect("export");
    let Some(Command::Sessions {
        command: Some(SessionsCommand::Export(export)),
    }) = cli.command
    else {
        unreachable!("sessions export");
    };
    assert_eq!(export.session, "s1");
    assert_eq!(export.format, Some(ExportFormat::Html));
    assert!(Cli::try_parse_from(["hrn", "sessions"]).is_ok());
}

#[test]
fn prompt_override_flags_parse_on_every_mode() {
    let cli = Cli::try_parse_from([
        "hrn",
        "exec",
        "--append-system-prompt",
        "end with BANANA",
        "hi",
    ])
    .expect("parse");
    let Some(Command::Exec(a)) = cli.command else {
        unreachable!("exec");
    };
    assert_eq!(
        a.sources.append_system_prompt.as_deref(),
        Some("end with BANANA")
    );
    let cli = Cli::try_parse_from(["hrn", "chat", "--system-prompt", "prompts/custom.md"])
        .expect("parse");
    let Some(Command::Chat(a)) = cli.command else {
        unreachable!("chat");
    };
    assert_eq!(
        a.sources.system_prompt.as_deref(),
        Some("prompts/custom.md")
    );
    for argv in [
        vec!["hrn", "--mode", "rpc", "--append-system-prompt", "tail"],
        vec!["hrn", "rpc-ui", "--system-prompt", "s.md"],
        vec!["hrn", "acp", "--append-system-prompt", "a"],
    ] {
        let cli = Cli::try_parse_from(&argv).expect("parse");
        let flags = match &cli.command {
            Some(Command::Rpc(a) | Command::RpcUi(a) | Command::Acp(a)) => a,
            _ => &cli.serve,
        };
        assert!(
            flags.system_prompt.is_some() || flags.append_system_prompt.is_some(),
            "{argv:?}"
        );
    }
}

#[test]
fn titles_keep_the_prompt_language() {
    assert_eq!(
        crate::exec::session_title("привет, как дела?"),
        "привет, как дела?"
    );
    let long = "как дела и почему так ".repeat(6);
    let title = crate::exec::session_title(&long);
    assert!(title.chars().count() <= 81, "{title}");
    assert!(
        title.chars().next().is_some_and(|c| !c.is_ascii()),
        "keeps RU"
    );
}

#[test]
fn prompt_text_resolves_files_and_literals() {
    let dir = std::env::temp_dir().join(format!("hrn-prompt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("p.md");
    std::fs::write(&file, "from file\r\nline two\n").expect("write");
    let path = file.to_str().expect("path");
    assert_eq!(
        crate::config::resolve_prompt_text(path),
        "from file\nline two"
    );
    assert_eq!(
        crate::config::resolve_prompt_text("literal text"),
        "literal text"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repeatable_config_profile_and_config_subcommands_parse() {
    let cli = Cli::try_parse_from([
        "hrn",
        "--profile",
        "work",
        "--config",
        "a.toml",
        "--config",
        "b.toml",
        "config",
        "get",
        "tui.theme",
        "--json",
    ])
    .expect("parse");
    assert_eq!(cli.profile.as_deref(), Some("work"));
    assert_eq!(
        cli.config,
        [PathBuf::from("a.toml"), PathBuf::from("b.toml")]
    );
    assert!(matches!(
        cli.command,
        Some(Command::Config {
            command: Some(ConfigCommand::Get { json: true, .. })
        })
    ));
    assert!(Cli::try_parse_from(["hrn", "config"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "config", "list", "--scope", "global"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "config", "set", "advisor.enabled", "true"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "config", "reset", "advisor.enabled"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "config", "path"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "config", "check"]).is_ok());
}

#[test]
fn restore_conflicts_with_no_session() {
    assert!(Cli::try_parse_from(["hrn", "chat", "--restore", "--no-session"]).is_err());
    assert!(Cli::try_parse_from(["hrn", "chat", "--restore", "s1", "--no-session"]).is_err());
    assert!(Cli::try_parse_from(["hrn", "chat", "--restore", "--goal", "ship"]).is_err());
    assert!(Cli::try_parse_from(["hrn", "chat", "--restore", "--continue"]).is_err());
    assert!(Cli::try_parse_from(["hrn", "chat", "--restore", "--resume", "s1"]).is_err());

    let pre = Cli::try_parse_from(["hrn", "--no-session", "chat", "--restore"]).expect("pre");
    assert!(crate::apply_no_session(pre.command, pre.no_session).is_err());

    let bare = Cli::try_parse_from(["hrn", "chat", "--restore"]).expect("bare restore");
    let Some(Command::Chat(chat)) = bare.command else {
        unreachable!("chat");
    };
    assert_eq!(chat.restore.as_deref(), Some(""));

    let named = Cli::try_parse_from(["hrn", "chat", "--restore", "sess_9"]).expect("named restore");
    let Some(Command::Chat(chat)) = named.command else {
        unreachable!("chat");
    };
    assert_eq!(chat.restore.as_deref(), Some("sess_9"));
}

#[test]
fn approval_mode_flags_parse() {
    let cases = [
        (
            vec!["hrn", "--approval-mode", "always-ask"],
            Some(PermissionMode::Default),
        ),
        (
            vec!["hrn", "--approval-mode", "always-ask-cached"],
            Some(PermissionMode::AskCached),
        ),
        (
            vec!["hrn", "--approval-mode", "write"],
            Some(PermissionMode::AutoEdit),
        ),
        (
            vec!["hrn", "--approval-mode", "yolo"],
            Some(PermissionMode::Yolo),
        ),
        (vec!["hrn", "--auto-approve"], Some(PermissionMode::Yolo)),
        (vec!["hrn", "--yolo"], Some(PermissionMode::Yolo)),
        (vec!["hrn"], None),
    ];
    for (argv, expected) in cases {
        let cli = Cli::try_parse_from(&argv).expect("parse");
        assert_eq!(cli.approval_mode_override(), expected, "{argv:?}");
    }
}

#[test]
fn service_tier_usage_accounts_and_dry_balance_parse() {
    let chat = Cli::try_parse_from(["hrn", "chat", "--service-tier", "priority"]).expect("chat");
    let Some(Command::Chat(args)) = chat.command else {
        unreachable!("chat");
    };
    assert_eq!(args.service_tier, Some(hrn_kernel::Tier::Priority));

    let exec =
        Cli::try_parse_from(["hrn", "exec", "--service-tier", "flex", "hello"]).expect("exec");
    let Some(Command::Exec(args)) = exec.command else {
        unreachable!("exec");
    };
    assert_eq!(args.service_tier, Some(hrn_kernel::Tier::Flex));

    let rpc = Cli::try_parse_from(["hrn", "rpc", "--service-tier", "ultrafast"]).expect("rpc");
    let Some(Command::Rpc(args)) = rpc.command else {
        unreachable!("rpc");
    };
    assert_eq!(args.service_tier, Some(hrn_kernel::Tier::Ultrafast));

    assert!(matches!(
        Cli::try_parse_from(["hrn", "usage", "accounts"])
            .expect("usage accounts")
            .command,
        Some(Command::Usage {
            command: Some(UsageCommand::Accounts)
        })
    ));

    let dry = Cli::try_parse_from(["hrn", "dry-balance", "claude", "--sessions", "1000"])
        .expect("dry-balance");
    let Some(Command::DryBalance(args)) = dry.command else {
        unreachable!("dry-balance");
    };
    assert_eq!(args.provider.as_deref(), Some("claude"));
    assert_eq!(args.sessions, 1000);
}

#[test]
fn setup_logout_and_kimi_login_parse() {
    assert!(matches!(
        Cli::try_parse_from(["hrn", "setup"])
            .expect("setup")
            .command,
        Some(Command::Setup)
    ));
    let bare_logout = Cli::try_parse_from(["hrn", "logout"]).expect("logout");
    assert!(matches!(
        bare_logout.command,
        Some(Command::Logout { provider: None })
    ));
    let targeted_logout =
        Cli::try_parse_from(["hrn", "logout", "kimi-code"]).expect("logout kimi-code");
    let Some(Command::Logout { provider }) = targeted_logout.command else {
        unreachable!("logout");
    };
    assert_eq!(provider.as_deref(), Some("kimi-code"));
    let kimi_login = Cli::try_parse_from(["hrn", "login", "kimi-code"]).expect("login kimi-code");
    let Some(Command::Login(args)) = kimi_login.command else {
        unreachable!("login");
    };
    assert_eq!(args.provider.as_deref(), Some("kimi-code"));

    let share = Cli::try_parse_from(["hrn", "share", "sess_1", "--no-redact"]).expect("share");
    let Some(Command::Share(s_args)) = share.command else {
        unreachable!("share");
    };
    assert_eq!(s_args.session.as_deref(), Some("sess_1"));
    assert!(s_args.no_redact);

    let join = Cli::try_parse_from([
        "hrn",
        "join",
        "http://127.0.0.1:8787/r/room1234#key",
        "--name",
        "alice",
    ])
    .expect("join");
    let Some(Command::Join(j_args)) = join.command else {
        unreachable!("join");
    };
    assert_eq!(j_args.link, "http://127.0.0.1:8787/r/room1234#key");
    assert_eq!(j_args.name.as_deref(), Some("alice"));
}

#[test]
fn no_lsp_flag_parses_across_all_modes() {
    let chat = Cli::try_parse_from(["hrn", "chat", "--no-lsp"]).expect("chat");
    let Some(Command::Chat(args)) = chat.command else {
        unreachable!("chat");
    };
    assert!(args.sources.no_lsp);

    let exec = Cli::try_parse_from(["hrn", "exec", "--no-lsp", "hi"]).expect("exec");
    let Some(Command::Exec(args)) = exec.command else {
        unreachable!("exec");
    };
    assert!(args.sources.no_lsp);

    for argv in [
        vec!["hrn", "rpc", "--no-lsp"],
        vec!["hrn", "rpc-ui", "--no-lsp"],
        vec!["hrn", "acp", "--no-lsp"],
        vec!["hrn", "--mode", "rpc", "--no-lsp"],
    ] {
        let cli = Cli::try_parse_from(&argv).expect("serve");
        let serve = match &cli.command {
            Some(Command::Rpc(a) | Command::RpcUi(a) | Command::Acp(a)) => a,
            _ => &cli.serve,
        };
        assert!(serve.no_lsp, "{argv:?}");
    }
}

#[test]
fn session_extras_cli_flags_and_conflicts_parse() {
    // `hrn gc` and `hrn gc --apply`
    let gc_dry = Cli::try_parse_from(["hrn", "gc"]).expect("gc dry");
    assert!(matches!(
        gc_dry.command,
        Some(Command::Gc(GcArgs { apply: false }))
    ));
    let gc_apply = Cli::try_parse_from(["hrn", "gc", "--apply"]).expect("gc apply");
    assert!(matches!(
        gc_apply.command,
        Some(Command::Gc(GcArgs { apply: true }))
    ));

    // `hrn sessions import claude|codex`
    assert!(Cli::try_parse_from(["hrn", "sessions", "import", "claude"]).is_ok());
    assert!(Cli::try_parse_from(["hrn", "sessions", "import", "codex", "--latest"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "hrn", "sessions", "import", "claude", "--id", "abc", "--dir", "/tmp/c"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "hrn", "sessions", "import", "claude", "--latest", "--id", "abc"
        ])
        .is_err()
    );

    // `hrn chat --from-claude` / `--from-codex` and conflict matrix
    let from_c = Cli::try_parse_from(["hrn", "chat", "--from-claude"]).expect("from-claude");
    let Some(Command::Chat(chat)) = from_c.command else {
        unreachable!("chat");
    };
    assert_eq!(chat.from_claude.as_deref(), Some("latest"));

    for bad in [
        vec!["hrn", "chat", "--from-claude", "--from-codex"],
        vec!["hrn", "chat", "--from-claude", "--resume", "s1"],
        vec!["hrn", "chat", "--from-claude", "--continue"],
        vec!["hrn", "chat", "--from-claude", "--restore"],
        vec!["hrn", "chat", "--from-claude", "--no-session"],
        vec!["hrn", "chat", "--from-codex", "--resume", "s1"],
        vec!["hrn", "chat", "--from-codex", "--continue"],
        vec!["hrn", "chat", "--from-codex", "--no-session"],
    ] {
        assert!(
            Cli::try_parse_from(&bad).is_err(),
            "expected conflict: {bad:?}"
        );
    }

    // `--no-title` on chat, exec, rpc
    let c = Cli::try_parse_from(["hrn", "chat", "--no-title"]).expect("chat no-title");
    assert!(matches!(
        c.command,
        Some(Command::Chat(ChatArgs { no_title: true, .. }))
    ));
    let e = Cli::try_parse_from(["hrn", "exec", "--no-title", "hi"]).expect("exec no-title");
    assert!(matches!(
        e.command,
        Some(Command::Exec(ExecArgs { no_title: true, .. }))
    ));
    let r = Cli::try_parse_from(["hrn", "rpc", "--no-title"]).expect("rpc no-title");
    assert!(matches!(
        r.command,
        Some(Command::Rpc(ServeArgs { no_title: true, .. }))
    ));
}

#[test]
fn cli_completions_parse() {
    for (arg, expected) in [
        ("bash", CompletionShell::Bash),
        ("zsh", CompletionShell::Zsh),
        ("fish", CompletionShell::Fish),
        ("powershell", CompletionShell::Powershell),
    ] {
        let cli = Cli::try_parse_from(["hrn", "completions", arg]).expect("completions parse");
        assert!(
            matches!(cli.command, Some(Command::Completions { shell }) if shell == expected),
            "expected {expected:?} for `{arg}`"
        );
        let mut buf = Vec::new();
        crate::completions::write_completions(expected, &mut buf);
        let script = String::from_utf8(buf).expect("utf-8 completion script");
        assert!(
            script.contains("hrn"),
            "completion script for `{arg}` should mention `hrn`"
        );
    }
    assert!(Cli::try_parse_from(["hrn", "completions", "csh"]).is_err());
}

#[test]
fn commit_git_and_update_cli_parse() {
    let commit = Cli::try_parse_from([
        "hrn",
        "commit",
        "--yes",
        "--dry-run",
        "--changelog",
        "--session",
        "sess_1",
    ])
    .expect("commit parse");
    assert!(matches!(
        commit.command,
        Some(Command::Commit(CommitArgs {
            yes: true,
            dry_run: true,
            changelog: true,
            session: Some(ref s),
        })) if s == "sess_1"
    ));

    let git_bare = Cli::try_parse_from(["hrn", "git"]).expect("git bare");
    assert!(matches!(
        git_bare.command,
        Some(Command::Git(GitArgs { revision: None }))
    ));
    let git_rev = Cli::try_parse_from(["hrn", "git", "HEAD~1"]).expect("git rev");
    assert!(matches!(
        git_rev.command,
        Some(Command::Git(GitArgs {
            revision: Some(ref r)
        })) if r == "HEAD~1"
    ));

    let upd_check = Cli::try_parse_from(["hrn", "update", "--check", "--canary"]).expect("update");
    assert!(matches!(
        upd_check.command,
        Some(Command::Update(UpdateArgs {
            check: true,
            canary: true,
            stable: false,
        }))
    ));
    assert!(
        Cli::try_parse_from(["hrn", "update", "--canary", "--stable"]).is_err(),
        "--canary and --stable must conflict"
    );
}
