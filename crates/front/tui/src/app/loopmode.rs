//! `/loop [count|duration] [--while|--until '<cmd>'] [prompt]`: bounded prompt repetition with
//! optional shell condition gating (`edge/proc`), `Esc` suspension, and dynamic autocomplete state.

use super::App;
use crate::transcript::Level;
use std::time::{Duration, Instant};

/// Shell condition checked after each turn yield to gate loop continuation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoopCondition {
    /// Continue looping while the command exits with status `0`.
    While(String),
    /// Stop looping once the command exits with status `0`.
    Until(String),
}

impl LoopCondition {
    pub(crate) fn command(&self) -> &str {
        match self {
            Self::While(cmd) | Self::Until(cmd) => cmd.as_str(),
        }
    }

    pub(crate) const fn flag(&self) -> &'static str {
        match self {
            Self::While(_) => "--while",
            Self::Until(_) => "--until",
        }
    }
}

/// Active or paused `/loop` configuration and runtime counters.
#[derive(Clone, Debug)]
pub(crate) struct ActiveLoop {
    pub(crate) prompt: String,
    pub(crate) max_runs: Option<u32>,
    pub(crate) runs_started: u32,
    pub(crate) deadline: Option<Instant>,
    pub(crate) condition: Option<LoopCondition>,
    pub(crate) pending_check: Option<u64>,
}

/// Lifecycle state of `/loop`.
#[derive(Clone, Debug, Default)]
pub(crate) enum LoopState {
    #[default]
    Off,
    On(ActiveLoop),
    Paused(ActiveLoop),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedLoop {
    pub(crate) max_runs: Option<u32>,
    pub(crate) duration: Option<Duration>,
    pub(crate) condition: Option<LoopCondition>,
    pub(crate) prompt: Option<String>,
}

impl App {
    /// Live autocomplete description for `/loop` (`Loop: off | paused | on (...)`).
    pub(super) fn loop_autocomplete_help(&self) -> String {
        let now = self.now();
        match &self.loop_mode {
            LoopState::Off => {
                "Loop: off · repeat a prompt across turns (Esc pauses, /loop stops)".to_owned()
            }
            LoopState::On(active) => {
                format!(
                    "Loop: on ({}) · Esc pauses, /loop stops",
                    active.summary(now)
                )
            }
            LoopState::Paused(active) => format!(
                "Loop: paused ({}) · /loop stops, /loop resume continues",
                active.summary(now)
            ),
        }
    }

    /// `/loop [count|duration] [--while|--until '<cmd>'] [prompt]`.
    pub(super) fn handle_loop(&mut self, arg: Option<&str>) {
        let trimmed = arg.map(str::trim).unwrap_or_default();
        if trimmed.is_empty() {
            self.handle_bare_loop();
            return;
        }
        if trimmed.eq_ignore_ascii_case("off") || trimmed.eq_ignore_ascii_case("stop") {
            self.stop_loop("loop stopped");
            return;
        }
        if trimmed.eq_ignore_ascii_case("resume") {
            self.resume_loop();
            return;
        }
        let parsed = match parse_loop_args(trimmed) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.notice(Level::Warn, err);
                return;
            }
        };
        let prompt = match parsed.prompt.or_else(|| self.last_prompt.clone()) {
            Some(p) if !p.trim().is_empty() => p,
            _ => {
                self.notice(
                    Level::Warn,
                    "no prompt provided and no previous turn to repeat — pass a prompt: /loop 3 <prompt>",
                );
                return;
            }
        };
        self.backend.cancel_loop_condition();
        let now = self.now();
        let deadline = parsed.duration.and_then(|d| now.checked_add(d));
        let active = ActiveLoop {
            prompt: prompt.clone(),
            max_runs: parsed.max_runs,
            runs_started: 0,
            deadline,
            condition: parsed.condition,
            pending_check: None,
        };
        let summary = active.summary(now);
        self.loop_mode = LoopState::On(active);
        self.notice(Level::Info, format!("loop started ({summary})"));
        if !self.is_busy() {
            self.submit_loop_iteration();
        }
    }

    fn handle_bare_loop(&mut self) {
        match &self.loop_mode {
            LoopState::Off => {
                self.notice(
                    Level::Info,
                    "Loop: off · usage: /loop [count|duration] [--while|--until '<cmd>'] [prompt]",
                );
            }
            LoopState::On(_) | LoopState::Paused(_) => self.stop_loop("loop disabled"),
        }
    }

    fn stop_loop(&mut self, msg: &str) {
        self.backend.cancel_loop_condition();
        self.loop_mode = LoopState::Off;
        self.notice(Level::Info, msg);
    }

    fn resume_loop(&mut self) {
        if !matches!(self.loop_mode, LoopState::Paused(_)) {
            self.notice(Level::Info, "loop is not paused");
            return;
        }
        let LoopState::Paused(active) = std::mem::take(&mut self.loop_mode) else {
            return;
        };
        let summary = active.summary(self.now());
        self.loop_mode = LoopState::On(active);
        self.notice(Level::Info, format!("loop resumed ({summary})"));
        if !self.is_busy() {
            self.advance_loop_after_turn();
        }
    }

    /// Called on `Esc` before normal idle/busy escape handling. Suspends an active loop (`Paused`),
    /// cancelling any condition probe and interrupting an in-flight turn.
    pub(super) fn suspend_loop_on_esc(&mut self) -> bool {
        if !matches!(self.loop_mode, LoopState::On(_)) {
            return false;
        }
        let LoopState::On(mut active) = std::mem::take(&mut self.loop_mode) else {
            return false;
        };
        active.pending_check = None;
        self.backend.cancel_loop_condition();
        let summary = active.summary(self.now());
        self.loop_mode = LoopState::Paused(active);
        if self.is_busy() {
            self.interrupt();
        }
        self.notice(
            Level::Info,
            format!("loop paused ({summary}) · /loop to disable or /loop resume to continue"),
        );
        true
    }

    /// Suspend an active loop when a turn ends with interruption or failure.
    pub(super) fn suspend_loop_on_abort(&mut self, reason: &str) {
        if !matches!(self.loop_mode, LoopState::On(_)) {
            return;
        }
        let LoopState::On(mut active) = std::mem::take(&mut self.loop_mode) else {
            return;
        };
        active.pending_check = None;
        self.backend.cancel_loop_condition();
        self.loop_mode = LoopState::Paused(active);
        self.notice(
            Level::Warn,
            format!("loop paused after {reason} · /loop to disable or /loop resume"),
        );
    }

    /// Called after a turn finishes with `TurnOutcome::Done` (and after `/queue` is empty).
    pub(super) fn advance_loop_after_turn(&mut self) {
        let now = self.now();
        let LoopState::On(active) = &mut self.loop_mode else {
            return;
        };
        if let Some(max) = active.max_runs
            && active.runs_started >= max
        {
            let msg = format!("loop finished ({}/{max} runs)", active.runs_started);
            self.loop_mode = LoopState::Off;
            self.notice(Level::Info, msg);
            return;
        }
        if let Some(deadline) = active.deadline
            && now >= deadline
        {
            let msg = format!(
                "loop finished: duration elapsed ({} runs)",
                active.runs_started
            );
            self.loop_mode = LoopState::Off;
            self.notice(Level::Info, msg);
            return;
        }
        if let Some(cond) = active.condition.clone() {
            self.loop_check_seq = self.loop_check_seq.wrapping_add(1);
            let check_id = self.loop_check_seq;
            active.pending_check = Some(check_id);
            let cmd = cond.command().to_owned();
            if let Err(err) = self.backend.start_loop_condition(check_id, &cmd) {
                self.loop_mode = LoopState::Off;
                self.notice(Level::Error, format!("loop condition failed: {err}"));
            } else {
                self.flash(format!("loop checking {} `{cmd}`…", cond.flag()));
            }
            return;
        }
        self.submit_loop_iteration();
    }

    /// Handle an asynchronous [`crate::port::UiEvent::LoopCondition`] result.
    pub(super) fn on_loop_condition(
        &mut self,
        check_id: u64,
        exit_code: Option<i32>,
        timed_out: bool,
        error: Option<String>,
    ) {
        let cond = match &mut self.loop_mode {
            LoopState::On(active) if active.pending_check == Some(check_id) => {
                active.pending_check = None;
                active.condition.clone()
            }
            _ => return,
        };
        let Some(cond) = cond else {
            return;
        };
        if let Some(err) = error {
            self.loop_mode = LoopState::Off;
            self.notice(
                Level::Error,
                format!("loop stopped: condition error: {err}"),
            );
            return;
        }
        if timed_out {
            self.loop_mode = LoopState::Off;
            self.notice(Level::Warn, "loop stopped: condition command timed out");
            return;
        }
        let succeeded = exit_code == Some(0);
        let should_continue = match &cond {
            LoopCondition::While(_) => succeeded,
            LoopCondition::Until(_) => !succeeded,
        };
        if should_continue {
            self.submit_loop_iteration();
        } else {
            let code_label = exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
            self.loop_mode = LoopState::Off;
            self.notice(
                Level::Info,
                format!(
                    "loop finished: {} condition exited {code_label}",
                    cond.flag()
                ),
            );
        }
    }

    fn submit_loop_iteration(&mut self) {
        let prompt = match &mut self.loop_mode {
            LoopState::On(active) => {
                active.runs_started = active.runs_started.saturating_add(1);
                active.prompt.clone()
            }
            _ => return,
        };
        self.send_prompt(&prompt, None);
    }
}

impl ActiveLoop {
    pub(crate) fn summary(&self, now: Instant) -> String {
        let mut parts = Vec::new();
        if let Some(max) = self.max_runs {
            parts.push(format!("{}/{max}", self.runs_started));
        } else {
            parts.push(format!("{} runs", self.runs_started));
        }
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(now);
            parts.push(format!("{} left", crate::render::duration(left)));
        }
        if let Some(cond) = &self.condition {
            parts.push(format!("{} {}", cond.flag(), cond.command()));
        }
        parts.join(" · ")
    }
}

pub(crate) fn parse_loop_args(raw: &str) -> Result<ParsedLoop, String> {
    let tokens = split_tokens(raw)?;
    let mut max_runs: Option<u32> = None;
    let mut duration: Option<Duration> = None;
    let mut condition: Option<LoopCondition> = None;
    let mut prompt_parts: Vec<String> = Vec::new();
    let mut in_prompt = false;
    let mut i = 0usize;
    while let Some(tok) = tokens.get(i) {
        if in_prompt {
            prompt_parts.push(tok.clone());
            i += 1;
            continue;
        }
        if let Some(cmd) = tok
            .strip_prefix("--while=")
            .or_else(|| tok.strip_prefix("--until="))
        {
            let is_while = tok.starts_with("--while=");
            set_condition(&mut condition, is_while, cmd)?;
            i += 1;
            continue;
        }
        if tok == "--while" || tok == "--until" {
            let Some(cmd) = tokens.get(i + 1) else {
                return Err(format!("{tok} requires a shell command"));
            };
            set_condition(&mut condition, tok == "--while", cmd)?;
            i += 2;
            continue;
        }
        if max_runs.is_none()
            && let Some(count) = parse_count(tok)
        {
            max_runs = Some(count);
            i += 1;
            continue;
        }
        if duration.is_none()
            && let Some(dur) = parse_duration(tok)
        {
            duration = Some(dur);
            i += 1;
            continue;
        }
        if tok.starts_with("--") {
            return Err(format!(
                "unknown /loop flag `{tok}` — supported: --while '<cmd>', --until '<cmd>'"
            ));
        }
        in_prompt = true;
        prompt_parts.push(tok.clone());
        i += 1;
    }
    let prompt = (!prompt_parts.is_empty()).then(|| prompt_parts.join(" "));
    Ok(ParsedLoop {
        max_runs,
        duration,
        condition,
        prompt,
    })
}

fn set_condition(
    slot: &mut Option<LoopCondition>,
    is_while: bool,
    cmd: &str,
) -> Result<(), String> {
    if slot.is_some() {
        return Err("specify only one of --while or --until".to_owned());
    }
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Err("loop condition command cannot be empty".to_owned());
    }
    *slot = Some(if is_while {
        LoopCondition::While(trimmed.to_owned())
    } else {
        LoopCondition::Until(trimmed.to_owned())
    });
    Ok(())
}

fn parse_count(tok: &str) -> Option<u32> {
    let digits = tok.strip_suffix(['x', 'X']).unwrap_or(tok);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok().filter(|&n| n > 0)
}

fn parse_duration(tok: &str) -> Option<Duration> {
    let mut total_secs: u64 = 0;
    let mut num = String::new();
    let mut saw_unit = false;
    for c in tok.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u64 = num.parse().ok()?;
            num.clear();
            let mult = match c.to_ascii_lowercase() {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                _ => return None,
            };
            total_secs = total_secs.checked_add(n.checked_mul(mult)?)?;
            saw_unit = true;
        }
    }
    if !saw_unit || !num.is_empty() || total_secs == 0 {
        return None;
    }
    Some(Duration::from_secs(total_secs))
}

fn split_tokens(input: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\'
                && q == '"'
                && let Some(&next) = chars.peek()
            {
                cur.push(next);
                chars.next();
            } else {
                cur.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if quote.is_some() {
        return Err("unterminated quote in /loop arguments".to_owned());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}
