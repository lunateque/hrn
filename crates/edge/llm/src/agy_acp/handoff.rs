//! Two-tier DAG branch recovery: direct `<restored_branch_context>` below threshold and
//! Smart Handoff via a cheap `handoff_model` above threshold.

use hrn_kernel::{CancelFlag, Msg, Result, Role, ScopedFuture};
use std::fmt::Write as _;

/// Maximum character budget preserved per individual tool output in direct transcript bootstrap.
pub const MAX_BOOTSTRAP_TOOL_OUTPUT_CHARS: usize = 3_072;
/// Number of trailing raw history turns preserved verbatim after a Smart Handoff checkpoint.
pub const SMART_HANDOFF_TAIL_MSGS: usize = 4;

/// Which branch-bootstrap strategy was selected for a new ACP session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootstrapStrategy {
    /// Fresh session with no prior history (only the user's new prompt).
    Fresh,
    /// History fits under `handoff_threshold_tokens`; serialized directly into `<restored_branch_context>`.
    DirectTranscript,
    /// History exceeded `handoff_threshold_tokens`; condensed via `handoff_model` into a compact checkpoint.
    SmartHandoff,
}

/// Pluggable summarizer port implemented by `LlmRouter` (calling `handoff_model`, e.g. `smol`)
/// with a recursion guard so Smart Handoff never invokes itself recursively.
pub trait HandoffSummarizer: Send + Sync {
    /// Generate a dense 3,000-5,000 token handoff checkpoint from `messages` using `model`.
    fn summarize<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [Msg],
        cancel: &'a CancelFlag,
    ) -> ScopedFuture<'a, Result<String>>;
}

/// Conservative token estimate for a slice of canonical messages (`~4 chars/token` + framing).
pub fn estimate_messages_tokens(messages: &[Msg]) -> u64 {
    let mut total: u64 = 0;
    for msg in messages {
        let text_chars = msg.content.chars().count() as u64;
        let tool_chars: u64 = msg
            .tool_calls
            .iter()
            .map(|tc| (tc.name.len() + tc.arguments.len()) as u64)
            .sum();
        let img_tokens = (msg.images.len() as u64).saturating_mul(256);
        total = total
            .saturating_add((text_chars.saturating_add(tool_chars)) / 4)
            .saturating_add(img_tokens)
            .saturating_add(12);
    }
    total
}

/// Split `messages` into `(prior_history, trailing_user_msgs)` at the last non-user boundary.
pub fn split_history_and_new_turn(messages: &[Msg]) -> (&[Msg], &[Msg]) {
    let mut cut = messages.len();
    while cut > 0 {
        if let Some(msg) = messages.get(cut - 1)
            && msg.role == Role::User
        {
            cut -= 1;
        } else {
            break;
        }
    }
    let history = messages.get(..cut).unwrap_or_default();
    let tail = messages.get(cut..).unwrap_or_default();
    (history, tail)
}

/// Build the initial prompt text for a new ACP session created on Case #4 (rewind / `/fork` /
/// cross-provider switch).
pub async fn prepare_bootstrap_prompt(
    messages: &[Msg],
    handoff_threshold_tokens: u64,
    handoff_model: &str,
    summarizer: Option<&dyn HandoffSummarizer>,
    cancel: &CancelFlag,
) -> Result<(String, BootstrapStrategy)> {
    let (history, new_turn) = split_history_and_new_turn(messages);
    let non_system_history: Vec<&Msg> = history.iter().filter(|m| m.role != Role::System).collect();

    let prompt_tail = render_user_delta(new_turn);
    if non_system_history.is_empty() {
        let sys_prefix = render_system_preamble(history);
        let combined = join_sections(&sys_prefix, &prompt_tail);
        return Ok((combined, BootstrapStrategy::Fresh));
    }

    let est_tokens = estimate_messages_tokens(history);
    if est_tokens > handoff_threshold_tokens
        && let Some(sum) = summarizer
    {
        let handoff_req = build_smart_handoff_messages(history);
        if let Ok(checkpoint) = sum.summarize(handoff_model, &handoff_req, cancel).await
            && !checkpoint.trim().is_empty()
        {
            let tail_start = history.len().saturating_sub(SMART_HANDOFF_TAIL_MSGS);
            let raw_tail = history.get(tail_start..).unwrap_or_default();
            let restored = render_smart_handoff_block(checkpoint.trim(), raw_tail);
            return Ok((
                join_sections(&restored, &prompt_tail),
                BootstrapStrategy::SmartHandoff,
            ));
        }
    }

    let restored = build_restored_branch_context(history);
    Ok((
        join_sections(&restored, &prompt_tail),
        BootstrapStrategy::DirectTranscript,
    ))
}

/// Build the compact `<restored_branch_context>` block used when history is under threshold.
pub fn build_restored_branch_context(history: &[Msg]) -> String {
    let mut out = String::new();
    out.push_str("<restored_branch_context>\n");
    for msg in history {
        match msg.role {
            Role::System => {
                if !msg.content.trim().is_empty() {
                    let _ = writeln!(out, "[system]\n{}\n", msg.content.trim());
                }
            }
            Role::User => {
                let _ = writeln!(out, "[user]\n{}\n", msg.content.trim());
            }
            Role::Assistant => {
                if !msg.content.trim().is_empty() {
                    let _ = writeln!(out, "[assistant]\n{}\n", msg.content.trim());
                }
                for tc in &msg.tool_calls {
                    let args = truncate_chars(&tc.arguments, MAX_BOOTSTRAP_TOOL_OUTPUT_CHARS);
                    let _ = writeln!(out, "[assistant tool_call {}({})]: {args}", tc.name, tc.id);
                }
            }
            Role::Tool => {
                let name = msg.tool_name.as_deref().unwrap_or("tool");
                let err_tag = if msg.is_error { " (error)" } else { "" };
                let body = truncate_chars(msg.content.trim(), MAX_BOOTSTRAP_TOOL_OUTPUT_CHARS);
                let _ = writeln!(out, "[tool_result {name}{err_tag}]\n{body}\n");
            }
        }
    }
    out.push_str("</restored_branch_context>");
    out
}

/// Build the prompt messages sent to `handoff_model` (`smol` / `agy-flash`) to generate a dense
/// 3,000-5,000 token Smart Handoff checkpoint.
pub fn build_smart_handoff_messages(history: &[Msg]) -> Vec<Msg> {
    let system = Msg::system(
        "You are a precision technical handoff summarizer. Condense the conversation branch into a \
         dense 3,000-5,000 token checkpoint that allows a fresh model session to continue seamlessly \
         without re-reading the entire transcript. Include exact sections:\n\
         1. Original goal and hard constraints\n\
         2. Modified files, symbols, and key architectural decisions\n\
         3. Latest verification results, test outputs, and unresolved errors\n\
         4. Exact unfinished next step",
    );
    let transcript = build_restored_branch_context(history);
    let user = Msg::user(format!(
        "Produce the Smart Handoff checkpoint for this branch:\n\n{transcript}"
    ));
    vec![system, user]
}

fn render_smart_handoff_block(checkpoint: &str, raw_tail: &[Msg]) -> String {
    let mut out = String::new();
    out.push_str("<restored_branch_context mode=\"smart_handoff\">\n");
    out.push_str("## Smart Handoff Checkpoint\n");
    out.push_str(checkpoint);
    out.push_str("\n\n");
    if !raw_tail.is_empty() {
        out.push_str("## Most Recent Raw Turns\n");
        for msg in raw_tail {
            if msg.role == Role::System {
                continue;
            }
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let body = truncate_chars(msg.content.trim(), MAX_BOOTSTRAP_TOOL_OUTPUT_CHARS);
            let _ = writeln!(out, "[{role}]\n{body}\n");
        }
    }
    out.push_str("</restored_branch_context>");
    out
}

/// Render delta messages (typically newly added `User` turns) into prompt text.
pub fn render_user_delta(delta: &[Msg]) -> String {
    let mut parts = Vec::new();
    for msg in delta {
        let trimmed = msg.content.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_owned());
        }
    }
    parts.join("\n\n")
}

fn render_system_preamble(history: &[Msg]) -> String {
    let mut parts = Vec::new();
    for msg in history {
        if msg.role == Role::System && !msg.content.trim().is_empty() {
            parts.push(msg.content.trim().to_owned());
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "<system_instructions>\n{}\n</system_instructions>",
        parts.join("\n\n")
    )
}

fn join_sections(prefix: &str, tail: &str) -> String {
    match (prefix.trim().is_empty(), tail.trim().is_empty()) {
        (true, true) => String::new(),
        (true, false) => tail.to_owned(),
        (false, true) => prefix.to_owned(),
        (false, false) => format!("{prefix}\n\n{tail}"),
    }
}

/// Truncate `text` at a UTF-8 char boundary to at most `max_chars` characters, appending an
/// explicit omission marker when truncated.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => {
            let omitted = text.chars().count().saturating_sub(max_chars);
            let head = text.get(..byte_idx).unwrap_or_default();
            format!("{head}\n...[truncated {omitted} chars]...")
        }
        None => text.to_owned(),
    }
}
