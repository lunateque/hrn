//! Persistent shell sessions against the real shell (Git Bash on Windows, `bash` elsewhere).

use super::*;
use std::sync::Mutex;
use std::time::Instant;

static NO_EXPORTS: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

fn call<'a>(command: &'a str, cwd: &'a Path, secs: u64) -> ShellCall<'a> {
    ShellCall {
        command,
        cwd,
        cwd_override: None,
        exports: &NO_EXPORTS,
        timeout: Duration::from_secs(secs),
        max_output_bytes: 64 * 1024,
    }
}

async fn run(session: &mut ShellSession, command: &str, cwd: &Path) -> ShellResult {
    session
        .run(&call(command, cwd, 60), &CancelFlag::new(), &|_| {})
        .await
        .expect("run")
}

#[tokio::test]
async fn cwd_and_environment_persist_across_calls() {
    let cwd = std::env::temp_dir();
    let mut session = ShellSession::new();
    let first = run(
        &mut session,
        "export HRN_T=kept; mkdir -p hrn_sub && cd hrn_sub",
        &cwd,
    )
    .await;
    assert_eq!(first.run.exit_code, Some(0), "{:?}", first.run);
    assert!(
        first
            .cwd_changed
            .as_deref()
            .is_some_and(|c| c.ends_with("hrn_sub")),
        "{:?}",
        first.cwd_changed
    );
    let second = run(
        &mut session,
        "echo $HRN_T; basename \"$PWD\"; cd ..; rmdir hrn_sub",
        &cwd,
    )
    .await;
    assert_eq!(second.run.stdout.replace("\r\n", "\n"), "kept\nhrn_sub\n");
    assert!(second.cwd_changed.is_some());
    assert!(!second.restarted);
}

#[tokio::test]
async fn output_is_exact_and_streams_are_separate() {
    let cwd = std::env::temp_dir();
    let mut session = ShellSession::new();
    let seen = Mutex::new(String::new());
    let out = session
        .run(
            &call("printf 'no newline'; echo oops >&2; (exit 7)", &cwd, 60),
            &CancelFlag::new(),
            &|chunk| seen.lock().expect("lock").push_str(&chunk),
        )
        .await
        .expect("run");
    assert_eq!(out.run.stdout, "no newline");
    assert_eq!(out.run.stderr.trim_end(), "oops");
    assert_eq!(out.run.exit_code, Some(7));
    assert!(seen.lock().expect("lock").contains("no newline"));
}

#[tokio::test]
async fn commands_cannot_steal_the_protocol_stream() {
    let cwd = std::env::temp_dir();
    let mut session = ShellSession::new();
    let cat = run(&mut session, "cat", &cwd).await;
    assert_eq!(cat.run.exit_code, Some(0));
    let syntax = run(&mut session, "if", &cwd).await;
    assert_ne!(syntax.run.exit_code, Some(0));
    assert!(!syntax.run.stderr.is_empty());
    let after = run(&mut session, "echo still-here", &cwd).await;
    assert_eq!(after.run.stdout.trim_end(), "still-here");
    assert!(!after.restarted);
}

#[tokio::test]
async fn timeout_kills_the_command_but_not_the_shell() {
    let cwd = std::env::temp_dir();
    let mut session = ShellSession::new();
    run(&mut session, "export HRN_T=survivor", &cwd).await;
    let started = Instant::now();
    let slow = session
        .run(
            &call("sleep 60; echo not-reached", &cwd, 1),
            &CancelFlag::new(),
            &|_| {},
        )
        .await
        .expect("run");
    assert!(slow.run.timed_out, "{:?}", slow.run);
    assert_eq!(slow.run.exit_code, None);
    assert!(!slow.run.stdout.contains("not-reached"));
    assert!(started.elapsed() < Duration::from_secs(40));
    let after = run(&mut session, "echo $HRN_T", &cwd).await;
    assert_eq!(after.run.stdout.trim_end(), "survivor", "{:?}", after.run);
    assert!(!after.restarted);
}

#[tokio::test]
async fn cancellation_interrupts_and_the_session_stays_usable() {
    let cwd = std::env::temp_dir();
    let mut session = ShellSession::new();
    let cancel = CancelFlag::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        trigger.cancel();
    });
    let err = session
        .run(&call("sleep 60", &cwd, 120), &cancel, &|_| {})
        .await
        .expect_err("cancelled");
    assert!(matches!(err, HrnError::Cancelled), "{err}");
    let after = run(&mut session, "echo ok", &cwd).await;
    assert_eq!(after.run.stdout.trim_end(), "ok");
}

#[tokio::test]
async fn exiting_shell_is_reported_and_replaced() {
    let cwd = std::env::temp_dir();
    let mut session = ShellSession::new();
    let gone = run(&mut session, "echo bye; exit 3", &cwd).await;
    assert_eq!(gone.run.exit_code, Some(3), "{:?}", gone.run);
    assert!(gone.run.stdout.contains("bye"));
    assert!(!gone.run.warnings.is_empty());
    let next = run(&mut session, "echo new", &cwd).await;
    assert!(next.restarted);
    assert_eq!(next.run.stdout.trim_end(), "new");
}

#[tokio::test]
async fn large_output_is_bounded_and_does_not_lose_the_marker() {
    let cwd = std::env::temp_dir();
    let mut session = ShellSession::new();
    let big = session
        .run(
            &call("seq 1 200000; echo END-OF-OUTPUT", &cwd, 60),
            &CancelFlag::new(),
            &|_| {},
        )
        .await
        .expect("run");
    assert!(big.run.truncated);
    assert!(big.run.stdout.contains("END-OF-OUTPUT"));
    assert!(big.run.stdout.len() < 80 * 1024);
    let after = run(&mut session, "echo fine", &cwd).await;
    assert_eq!(after.run.stdout.trim_end(), "fine");
}

#[tokio::test]
async fn busy_session_is_not_acquired_twice() {
    let pool = ShellPool::new();
    let first = pool.acquire("s1").expect("first");
    assert!(pool.acquire("s1").is_none());
    assert!(pool.acquire("s2").is_some());
    drop(first);
    assert!(pool.acquire("s1").is_some());
}

#[tokio::test]
async fn timeout_abandons_nested_and_builtin_work_without_losing_the_shell() {
    let cwd = std::env::temp_dir();
    let mut session = ShellSession::new();
    run(&mut session, "export HRN_T=survivor", &cwd).await;
    for command in [
        "f() { sleep 60; echo A; }; f",
        "while true; do :; done; echo spin-not-reached",
        "sleep 60 | cat; echo pipe-not-reached",
    ] {
        let hung = session
            .run(&call(command, &cwd, 1), &CancelFlag::new(), &|_| {})
            .await
            .expect("run");
        assert!(hung.run.timed_out, "{command}: {:?}", hung.run);
        assert!(!hung.run.stdout.contains("not-reached"), "{command}");
        assert!(!hung.run.stdout.contains('A'), "{command}");
        let after = run(&mut session, "echo $HRN_T", &cwd).await;
        assert_eq!(after.run.stdout.trim_end(), "survivor", "{command}");
        assert!(!after.restarted, "{command}: shell was replaced");
    }
}
