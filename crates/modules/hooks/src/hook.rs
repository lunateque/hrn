//! The user-hook runner: a [`HarnessHook`] that re-discovers the user's hook sources when an
//! event arrives, runs the ones subscribed to it, and translates their JSON decisions into
//! verdicts, argument revisions, and injected context.

use crate::config::Config;
pub(crate) use crate::decision::Decision;
use crate::decision::interpret;
use crate::discover::{self, SettingsCommand};
use crate::run;
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, CancelFlag, HarnessHook, HookEvent, HookVerdict, HrnError, Result, ToolContext,
    ToolOutput,
};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One live subscription: a source (file or command) answering one event.
pub(crate) enum Source {
    /// A discovered executable: run directly with JSON on stdin.
    File(PathBuf),
    /// A configured command: run resolved like a shell would (or through the shell itself).
    Command(String),
}

pub(crate) struct Sub {
    pub label: String,
    pub source: Source,
    pub matcher: Option<String>,
    pub timeout: Option<u64>,
}

/// The last argument revision each tool name received (so the model replaying the rewritten call
/// is not blocked again by a hook that always rewrites).
type Revisions = Mutex<Vec<(String, Value)>>;

/// A [`HarnessHook`] over the user's hook sources.
#[derive(Clone)]
pub(crate) struct UserHookRunner {
    config: Arc<Config>,
    home: Option<PathBuf>,
    revisions: Arc<Revisions>,
}

impl UserHookRunner {
    pub(crate) fn new(config: Arc<Config>, home: Option<PathBuf>) -> Self {
        Self {
            config,
            home,
            revisions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Everything subscribed to `event`, in execution order: layer files, config commands,
    /// `--hook` paths, settings files. Re-read per event, so editing a `hooks/` directory takes
    /// effect immediately.
    pub(crate) fn subs(&self, cwd: &Path, event: &str) -> Vec<Sub> {
        let mut subs: Vec<Sub> = Vec::new();
        for found in discover::discover_hook_executables(self.home.as_deref(), cwd) {
            subs.push(Sub {
                label: format!("{} ({})", found.path.display(), found.layer),
                source: Source::File(found.path),
                matcher: None,
                timeout: None,
            });
        }
        for command in self.config.events.get(event).into_iter().flatten() {
            subs.push(Sub {
                label: command.clone(),
                source: Source::Command(command.clone()),
                matcher: None,
                timeout: None,
            });
        }
        for path in &self.config.extra {
            subs.push(Sub {
                label: path.to_string_lossy().into_owned(),
                source: Source::File(path.clone()),
                matcher: None,
                timeout: None,
            });
        }
        if self.config.discover_claude_settings {
            for entry in discover::discover_claude_settings(self.home.as_deref(), cwd) {
                let SettingsCommand {
                    event: name,
                    matcher,
                    command,
                    timeout_secs,
                } = entry;
                if name != event {
                    continue;
                }
                subs.push(Sub {
                    label: command.clone(),
                    source: Source::Command(command),
                    matcher,
                    timeout: timeout_secs,
                });
            }
        }
        subs
    }

    /// `(event, subscriber count)` for every event this runner serves (`hooks://`).
    pub(crate) fn inventory(&self, cwd: &Path) -> Vec<(String, usize)> {
        crate::config::EVENTS
            .iter()
            .map(|event| (event.to_string(), self.subs(cwd, event).len()))
            .collect()
    }

    /// Run one subscription with `payload`; its stdout decision (or exit code) becomes a
    /// [`Decision`]. A spawn failure is an `Err` the caller renders per event.
    async fn one(
        &self,
        sub: &Sub,
        cwd: &Path,
        payload: &Value,
        cancel: &CancelFlag,
    ) -> Result<Decision> {
        let timeout = Duration::from_secs(sub.timeout.unwrap_or(self.config.timeout_secs).max(1));
        let stdin = payload.to_string();
        let result = match &sub.source {
            Source::File(path) => {
                run::run_json(path, &[], cwd, &stdin, timeout, Some(cancel)).await?
            }
            Source::Command(command) => {
                let (program, args) = split_command(command, cwd);
                run::run_json(&program, &args, cwd, &stdin, timeout, Some(cancel)).await?
            }
        };
        if result.timed_out {
            return Ok(Decision::Block(format!(
                "timed out after {}s",
                timeout.as_secs()
            )));
        }
        let stdout = if result.stdout.len() > self.config.max_output_bytes {
            let cut = result
                .stdout
                .floor_char_boundary(self.config.max_output_bytes);
            result.stdout[..cut].to_owned()
        } else {
            result.stdout
        };
        Ok(interpret(result.code, &stdout, &result.stderr))
    }

    /// The `event` subscriptions whose matcher accepts `tool_name`.
    fn tool_subs(&self, cwd: &Path, event: &str, tool_name: &str) -> Vec<Sub> {
        self.subs(cwd, event)
            .into_iter()
            .filter(|sub| matcher_allows(sub, tool_name))
            .collect()
    }

    /// Whether `tool_name`'s arguments are a revision a hook already accepted.
    fn is_revised(&self, tool_name: &str, args: &Value) -> bool {
        let guard = self
            .revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .iter()
            .any(|(name, value)| name == tool_name && value == args)
    }

    fn remember_revision(&self, tool_name: &str, args: &Value) {
        let mut guard = self
            .revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|(name, _)| name != tool_name);
        guard.push((tool_name.to_owned(), args.clone()));
    }
}

/// Does a subscription's matcher (Claude-style tool-name pattern, `|`-separated) allow
/// `tool_name`?
fn matcher_allows(sub: &Sub, tool_name: &str) -> bool {
    let Some(pattern) = sub.matcher.as_deref() else {
        return true;
    };
    if pattern.is_empty() || pattern == "*" || pattern == ".*" {
        return true;
    }
    // The kernel has no regex crate; Claude matchers are tool-name lists in practice.
    pattern
        .split('|')
        .any(|part| part.eq_ignore_ascii_case(tool_name) || tool_name.contains(part))
}

/// Split a command string into argv tokens respecting single and double quotes. Returns `None`
/// on an unclosed quote so the caller delegates to the shell.
fn split_quoted_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                in_word = true;
            }
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                in_word = true;
            }
            None if ch.is_ascii_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            Some(_) | None => {
                current.push(ch);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

/// Split a command into program + arguments, resolving the program the way the `bash` tool does
/// (`PATHEXT` on Windows). A command with shell syntax, or whose program does not resolve, runs
/// through the same POSIX shell instead.
fn split_command(command: &str, cwd: &Path) -> (PathBuf, Vec<String>) {
    let through_shell = || match hrn_edge_proc::resolve_shell() {
        Ok(shell) => (PathBuf::from(&shell.program), shell.command_args(command)),
        Err(_) => (
            PathBuf::from("sh"),
            vec!["-c".to_owned(), command.to_owned()],
        ),
    };
    if command.contains(['|', '&', ';', '$', '`', '>', '<']) {
        return through_shell();
    }
    let Some(words) = split_quoted_words(command) else {
        return through_shell();
    };
    let mut parts = words.into_iter();
    let Some(program) = parts.next() else {
        return through_shell();
    };
    let resolved = hrn_edge_proc::resolve_program(&program, cwd);
    if !resolved.exists() {
        return through_shell();
    }
    (resolved, parts.collect())
}

/// The shared stdin envelope: event, session and workspace.
fn envelope(event: &str, session_id: &str, cwd: &Path) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": session_id,
        "cwd": cwd.to_string_lossy(),
    })
}

fn with(mut payload: Value, key: &str, value: Value) -> Value {
    if let Some(map) = payload.as_object_mut() {
        map.insert(key.to_owned(), value);
    }
    payload
}

impl HarnessHook for UserHookRunner {
    fn before_tool(
        &self,
        ctx: &ToolContext,
        tool_name: &str,
        arguments: &Value,
    ) -> BoxFuture<Result<HookVerdict>> {
        let runner = self.clone();
        let cwd = ctx.cwd.clone();
        let session_id = ctx.session_id.clone();
        let cancel = ctx.cancel.clone();
        let tool_name = tool_name.to_owned();
        let arguments = arguments.clone();
        Box::pin(async move {
            let subs = runner.tool_subs(&cwd, "pre_tool", &tool_name);
            if subs.is_empty() {
                return Ok(HookVerdict::Continue);
            }
            // A call carrying the arguments a hook just produced runs without re-asking.
            let revised = runner.is_revised(&tool_name, &arguments);
            let mut args = arguments.clone();
            let mut contexts: Vec<String> = Vec::new();
            for sub in subs {
                let payload = with(
                    with(
                        envelope("pre_tool", &session_id, &cwd),
                        "tool_name",
                        Value::String(tool_name.clone()),
                    ),
                    "tool_input",
                    args.clone(),
                );
                let decision = match runner.one(&sub, &cwd, &payload, &cancel).await {
                    Ok(decision) => decision,
                    Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
                    Err(err) => {
                        contexts.push(format!("hook `{}` failed: {err}", sub.label));
                        continue;
                    }
                };
                match decision {
                    Decision::Block(reason) if !revised => {
                        return Ok(HookVerdict::ShortCircuit(ToolOutput::err(format!(
                            "blocked by user hook `{}`: {reason}",
                            sub.label
                        ))));
                    }
                    Decision::Block(reason) => contexts.push(format!(
                        "user hook `{}` also blocked the rewritten call: {reason}",
                        sub.label
                    )),
                    Decision::Proceed {
                        arguments: Some(next),
                        context,
                    } => {
                        if next != args {
                            runner.remember_revision(&tool_name, &next);
                            args = next;
                        }
                        if let Some(text) = context {
                            contexts.push(text);
                        }
                    }
                    Decision::Proceed { context, .. } => {
                        if let Some(text) = context {
                            contexts.push(text);
                        }
                    }
                }
            }
            let context = (!contexts.is_empty()).then(|| contexts.join("\n\n"));
            if args != arguments {
                Ok(HookVerdict::Revise {
                    arguments: Some(args),
                    context,
                })
            } else if let Some(context) = context {
                Ok(HookVerdict::Revise {
                    arguments: None,
                    context: Some(context),
                })
            } else {
                Ok(HookVerdict::Continue)
            }
        })
    }

    fn after_tool(
        &self,
        ctx: &ToolContext,
        tool_name: &str,
        arguments: &Value,
        output: ToolOutput,
    ) -> BoxFuture<Result<ToolOutput>> {
        let runner = self.clone();
        let subs = runner.tool_subs(&ctx.cwd, "post_tool", tool_name);
        let session_id = ctx.session_id.clone();
        let cwd = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();
        let tool_name = tool_name.to_owned();
        let arguments = arguments.clone();
        Box::pin(async move {
            if subs.is_empty() {
                return Ok(output);
            }
            let mut output = output;
            for sub in subs {
                let payload = with(
                    with(
                        with(
                            envelope("post_tool", &session_id, &cwd),
                            "tool_name",
                            Value::String(tool_name.clone()),
                        ),
                        "tool_input",
                        arguments.clone(),
                    ),
                    "tool_output",
                    json!({"content": output.content, "is_error": output.is_error}),
                );
                match runner.one(&sub, &cwd, &payload, &cancel).await {
                    Ok(Decision::Block(reason)) => {
                        output.content = format!(
                            "post-tool hook `{}` rejected the result: {reason}",
                            sub.label
                        );
                        output.is_error = true;
                    }
                    Ok(Decision::Proceed { context, .. }) => {
                        if let Some(text) = context.filter(|t| !t.trim().is_empty()) {
                            output.content.push_str("\n\n<hook-context>\n");
                            output.content.push_str(text.trim());
                            output.content.push_str("\n</hook-context>");
                        }
                    }
                    Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
                    Err(err) => {
                        let _ = write!(
                            output.content,
                            "\n\n[user hook `{}` failed: {err}]",
                            sub.label
                        );
                    }
                }
            }
            Ok(output)
        })
    }

    fn on_event(&self, event: &HookEvent) -> BoxFuture<Result<Option<String>>> {
        let (name, session_id, cwd, extra) = match event {
            HookEvent::SessionStart { session_id, cwd } => (
                "session_start",
                session_id.clone(),
                cwd.clone(),
                json!({"source": "startup"}),
            ),
            HookEvent::UserPrompt {
                session_id,
                cwd,
                prompt,
            } => (
                "user_prompt",
                session_id.clone(),
                cwd.clone(),
                json!({"prompt": prompt}),
            ),
            HookEvent::TurnEnd {
                session_id,
                cwd,
                final_text,
                error,
            } => (
                "turn_end",
                session_id.clone(),
                cwd.clone(),
                json!({"final_text": final_text, "error": error}),
            ),
        };
        let runner = self.clone();
        Box::pin(async move {
            let mut contexts: Vec<String> = Vec::new();
            for sub in runner.subs(&cwd, name) {
                let payload = with(envelope(name, &session_id, &cwd), "event", extra.clone());
                match runner.one(&sub, &cwd, &payload, &CancelFlag::new()).await {
                    Ok(Decision::Block(reason)) => {
                        // omp's default for these events is to stop the turn.
                        return Err(HrnError::Tool(format!(
                            "user hook `{}` stopped the turn: {reason}",
                            sub.label
                        )));
                    }
                    Ok(Decision::Proceed { context, .. }) => {
                        if let Some(text) = context {
                            contexts.push(text);
                        }
                    }
                    Err(HrnError::Cancelled) => return Err(HrnError::Cancelled),
                    Err(err) => contexts.push(format!("hook `{}` failed: {err}", sub.label)),
                }
            }
            Ok(if contexts.is_empty() {
                None
            } else {
                Some(contexts.join("\n\n"))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omp_and_claude_shapes_become_the_same_decisions() {
        // omp: blocked + reason.
        match interpret(Some(0), r#"{"blocked":"true","reason":"no rm -rf"}"#, "") {
            Decision::Block(reason) => assert_eq!(reason, "no rm -rf"),
            Decision::Proceed { .. } => panic!("expected a block"),
        }
        // Claude: permissionDecision allow + updatedInput rewrite.
        let decision = interpret(
            Some(0),
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":{"command":"ls"},"additionalContext":"prefer ls"}}"#,
            "",
        );
        match decision {
            Decision::Block(_) => panic!("updatedInput accompanies a decision, not a block"),
            Decision::Proceed { arguments, context } => {
                assert_eq!(arguments, Some(json!({"command": "ls"})));
                assert_eq!(context.as_deref(), Some("prefer ls"));
            }
        }
        // Claude's deny blocks the call.
        assert!(matches!(
            interpret(
                Some(0),
                r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"nope"}}"#,
                "",
            ),
            Decision::Block(reason) if reason == "nope"
        ));
        // stdout `decision: "block"` blocks with stderr as the reason.
        assert!(matches!(
            interpret(Some(0), r#"{"decision":"block"}"#, "stderr says why"),
            Decision::Block(reason) if reason == "stderr says why"
        ));
        // Exit code 2 is Claude's block signal.
        assert!(matches!(
            interpret(Some(2), "", "refused"),
            Decision::Block(reason) if reason == "refused"
        ));
        // Plain text / empty stdout proceeds: a chatty hook must not stall the session.
        assert!(matches!(
            interpret(Some(0), "hello", ""),
            Decision::Proceed {
                arguments: None,
                context: None
            }
        ));
        // Claude's exit-1: a failed non-blocking hook's stderr still reaches the model.
        match interpret(Some(1), "", "traceback here") {
            Decision::Proceed {
                context: Some(text),
                ..
            } => assert!(text.contains("traceback here"), "{text}"),
            Decision::Proceed { .. } => panic!("exit-1 stderr must not be swallowed"),
            Decision::Block(_) => panic!("exit 1 is not a block"),
        }
        // `args` (the hrn shape) rewrites too.
        assert!(matches!(
            interpret(Some(0), r#"{"args":{"command":"echo ok"}}"#, ""),
            Decision::Proceed { arguments: Some(next), .. } if next == json!({"command":"echo ok"})
        ));
    }

    #[test]
    fn matchers_filter_by_tool_name() {
        let sub = |matcher: Option<&str>| Sub {
            label: "x".into(),
            source: Source::Command("x".into()),
            matcher: matcher.map(str::to_owned),
            timeout: None,
        };
        assert!(matcher_allows(&sub(None), "bash"));
        assert!(matcher_allows(&sub(Some("*")), "bash"));
        assert!(matcher_allows(&sub(Some("Bash|write")), "bash"));
        assert!(!matcher_allows(&sub(Some("write")), "bash"));
    }

    #[test]
    fn split_quoted_words_unquotes_windows_hook_paths() {
        let cmd =
            r#"powershell -NoProfile -File "C:\Users\alice\.claude\hooks\my script.ps1" session"#;
        let words = split_quoted_words(cmd).expect("balanced quotes");
        assert_eq!(
            words,
            vec![
                "powershell",
                "-NoProfile",
                "-File",
                r"C:\Users\alice\.claude\hooks\my script.ps1",
                "session",
            ]
        );
        assert!(split_quoted_words(r#"powershell -File "unclosed"#).is_none());
    }
}
