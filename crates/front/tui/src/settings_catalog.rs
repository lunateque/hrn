//! Curated category tabs, subsection taxonomy, and human-readable labels for `/settings`.

use crate::port::SettingItem;
use std::borrow::Cow;

/// Category tabs shown across the top of `/settings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Display,
    Interaction,
    Models,
    Session,
    Agents,
    Tools,
    More,
    All,
}

/// Ordered category tab bar rendered at the top of `/settings`.
pub(crate) const ALL_TABS: [SettingsTab; 8] = [
    SettingsTab::Display,
    SettingsTab::Interaction,
    SettingsTab::Models,
    SettingsTab::Session,
    SettingsTab::Agents,
    SettingsTab::Tools,
    SettingsTab::More,
    SettingsTab::All,
];

impl SettingsTab {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Display => "Display",
            Self::Interaction => "Interaction",
            Self::Models => "Models",
            Self::Session => "Session",
            Self::Agents => "Agents",
            Self::Tools => "Tools",
            Self::More => "More",
            Self::All => "All",
        }
    }

    pub(crate) const fn order(self) -> u8 {
        match self {
            Self::Display => 0,
            Self::Interaction => 1,
            Self::Models => 2,
            Self::Session => 3,
            Self::Agents => 4,
            Self::Tools => 5,
            Self::More => 6,
            Self::All => 7,
        }
    }

    pub(crate) fn next(self) -> Self {
        let idx = ALL_TABS.iter().position(|t| *t == self).unwrap_or(0);
        ALL_TABS
            .get((idx + 1) % ALL_TABS.len())
            .copied()
            .unwrap_or(Self::Display)
    }

    pub(crate) fn prev(self) -> Self {
        let idx = ALL_TABS.iter().position(|t| *t == self).unwrap_or(0);
        ALL_TABS
            .get((idx + ALL_TABS.len() - 1) % ALL_TABS.len())
            .copied()
            .unwrap_or(Self::Display)
    }
}

/// Resolved presentation metadata for a single [`SettingItem`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SettingMeta {
    pub(crate) tab: SettingsTab,
    pub(crate) section: Cow<'static, str>,
    pub(crate) section_order: u16,
    pub(crate) item_order: u16,
    pub(crate) label: Cow<'static, str>,
}

/// Whether `key` selects a TUI theme slot that supports live preview in the choice picker.
pub(crate) fn is_theme_key(key: &str) -> bool {
    matches!(key, "tui.theme" | "tui.theme.dark" | "tui.theme.light")
}

/// Classify `item` into its curated tab, subsection, sort priority, and human-readable label.
pub(crate) fn classify(item: &SettingItem) -> SettingMeta {
    let (tab, section, section_order) = classify_bucket(item);
    let (label, item_order) = curated_label(&item.key).map_or_else(
        || (Cow::Owned(humanize_key(&item.key)), 500),
        |(lbl, ord)| (Cow::Borrowed(lbl), ord),
    );
    SettingMeta {
        tab,
        section,
        section_order,
        item_order,
        label,
    }
}

fn classify_bucket(item: &SettingItem) -> (SettingsTab, Cow<'static, str>, u16) {
    let key = item.key.as_str();
    if matches!(
        key,
        "tui.vim_mode" | "tui.vim_mode_display" | "tui.recall_cleared_drafts"
    ) || key.starts_with("interaction.")
    {
        return (
            SettingsTab::Interaction,
            Cow::Borrowed("Input & Editing"),
            10,
        );
    }
    if key.starts_with("output.") {
        return (SettingsTab::Interaction, Cow::Borrowed("Output"), 20);
    }
    if let Some(bucket) = classify_display_key(key) {
        return bucket;
    }
    if let Some(bucket) = classify_models_key(key) {
        return bucket;
    }
    if let Some(bucket) = classify_session_or_agents_key(key) {
        return bucket;
    }
    if let Some(bucket) = classify_tools_or_more_key(key) {
        return bucket;
    }
    classify_by_schema_tab(&item.tab)
}

fn classify_display_key(key: &str) -> Option<(SettingsTab, Cow<'static, str>, u16)> {
    if !key.starts_with("tui.") {
        return None;
    }
    let (section, order) = if key == "tui.theme"
        || key.starts_with("tui.theme.")
        || matches!(key, "tui.symbol_preset" | "tui.color_blind_mode")
    {
        ("Appearance", 10)
    } else if key.starts_with("tui.statusline.") {
        ("Statusline", 20)
    } else if matches!(
        key,
        "tui.pinned_agents" | "tui.subagent_live_preview" | "tui.mouse" | "tui.hide_thinking"
    ) {
        ("Workspace", 30)
    } else if key.starts_with("tui.notify") {
        ("Notifications", 40)
    } else {
        ("Workspace", 35)
    };
    Some((SettingsTab::Display, Cow::Borrowed(section), order))
}

fn classify_models_key(key: &str) -> Option<(SettingsTab, Cow<'static, str>, u16)> {
    if key.starts_with("roles.") {
        return Some((
            SettingsTab::Models,
            Cow::Borrowed("Model Roles (use /model)"),
            90,
        ));
    }
    if key.starts_with("advisor.") {
        return Some((SettingsTab::Models, Cow::Borrowed("Passive Advisor"), 30));
    }
    if matches!(
        key,
        "thinking"
            | "enabled_models"
            | "disabled_providers"
            | "llm.role_storage"
            | "llm.context_promotion"
            | "llm.extended_context"
    ) || key.starts_with("models.")
    {
        return Some((
            SettingsTab::Models,
            Cow::Borrowed("Reasoning & Behavior"),
            10,
        ));
    }
    if key.starts_with("llm.") {
        return Some((SettingsTab::Models, Cow::Borrowed("Providers & Tiers"), 20));
    }
    None
}

fn classify_session_or_agents_key(key: &str) -> Option<(SettingsTab, Cow<'static, str>, u16)> {
    if key.starts_with("session.") {
        return Some((SettingsTab::Session, Cow::Borrowed("Sessions"), 10));
    }
    if key.starts_with("compaction.") {
        return Some((SettingsTab::Session, Cow::Borrowed("Compaction"), 20));
    }
    if key.starts_with("gc.") {
        return Some((
            SettingsTab::Session,
            Cow::Borrowed("Garbage Collection"),
            30,
        ));
    }
    if key.starts_with("task.isolation.") {
        return Some((SettingsTab::Agents, Cow::Borrowed("Worktree Isolation"), 20));
    }
    if key.starts_with("task.") {
        return Some((SettingsTab::Agents, Cow::Borrowed("Subagents"), 10));
    }
    if key.starts_with("eval.") {
        return Some((SettingsTab::Agents, Cow::Borrowed("Eval Sandbox"), 30));
    }
    None
}

fn classify_tools_or_more_key(key: &str) -> Option<(SettingsTab, Cow<'static, str>, u16)> {
    if key.starts_with("tools.") || key.starts_with("mcp") || key.starts_with("hooks.") {
        return Some((SettingsTab::Tools, Cow::Borrowed("Approvals & Tools"), 10));
    }
    if key.starts_with("edit.") {
        return Some((SettingsTab::Tools, Cow::Borrowed("Editor"), 20));
    }
    if key.starts_with("lsp.") {
        return Some((SettingsTab::Tools, Cow::Borrowed("LSP"), 30));
    }
    if key.starts_with("shell.") || key.starts_with("fs.") {
        return Some((SettingsTab::Tools, Cow::Borrowed("Shell & Filesystem"), 40));
    }
    if key.starts_with("commit.") {
        return Some((SettingsTab::More, Cow::Borrowed("Commit"), 10));
    }
    if key.starts_with("browser.") {
        return Some((SettingsTab::More, Cow::Borrowed("Browser"), 20));
    }
    if key.starts_with("desktop.") {
        return Some((SettingsTab::More, Cow::Borrowed("Desktop"), 30));
    }
    if key.starts_with("share.") || key.starts_with("collab.") {
        return Some((SettingsTab::More, Cow::Borrowed("Share & Collab"), 40));
    }
    if key.starts_with("update.") || key.starts_with("web.") {
        return Some((SettingsTab::More, Cow::Borrowed("Updates & Web"), 50));
    }
    None
}

fn classify_by_schema_tab(tab: &str) -> (SettingsTab, Cow<'static, str>, u16) {
    match tab.to_ascii_lowercase().as_str() {
        "tui" | "display" | "statusline" | "notify" => {
            (SettingsTab::Display, Cow::Borrowed("Workspace"), 50)
        }
        "interaction" | "output" => (
            SettingsTab::Interaction,
            Cow::Borrowed("Input & Editing"),
            30,
        ),
        "models" | "core" | "advisor" | "providers" => (
            SettingsTab::Models,
            Cow::Borrowed("Reasoning & Behavior"),
            40,
        ),
        "session" | "compaction" | "gc" | "memory" | "history" => {
            (SettingsTab::Session, Cow::Borrowed("Sessions"), 40)
        }
        "task" | "eval" | "agents" | "subagents" => {
            (SettingsTab::Agents, Cow::Borrowed("Subagents"), 40)
        }
        "tools" | "edit" | "lsp" | "shell" | "fs" | "mcp" | "hooks" | "bash" => {
            (SettingsTab::Tools, Cow::Borrowed("Approvals & Tools"), 50)
        }
        other if !other.is_empty() => (SettingsTab::More, Cow::Owned(title_word(other)), 80),
        _ => (SettingsTab::More, Cow::Borrowed("Other"), 90),
    }
}

fn curated_label(key: &str) -> Option<(&'static str, u16)> {
    CURATED_LABELS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, label, order)| (*label, *order))
}

/// Smart fallback converting any dotted TOML key into a readable title-cased label.
pub(crate) fn humanize_key(key: &str) -> String {
    if let Some(role) = key.strip_prefix("roles.") {
        return format!("{} role", humanize_segment(role));
    }
    let body = key.split_once('.').map_or(key, |(_, rest)| rest);
    let words: Vec<String> = body
        .split(['.', '_', '-'])
        .filter(|w| !w.is_empty())
        .enumerate()
        .map(|(i, w)| format_word(w, i == 0))
        .collect();
    if words.is_empty() {
        key.to_owned()
    } else {
        words.join(" ")
    }
}

fn humanize_segment(seg: &str) -> String {
    seg.split(['_', '-'])
        .filter(|w| !w.is_empty())
        .enumerate()
        .map(|(i, w)| format_word(w, i == 0))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_word(word: &str, first: bool) -> String {
    match word.to_ascii_lowercase().as_str() {
        "lsp" => "LSP".to_owned(),
        "pty" => "PTY".to_owned(),
        "url" => "URL".to_owned(),
        "cdp" => "CDP".to_owned(),
        "api" => "API".to_owned(),
        "mcp" => "MCP".to_owned(),
        "ttl" => "TTL".to_owned(),
        "ast" => "AST".to_owned(),
        "vim" => "Vim".to_owned(),
        "tui" => "TUI".to_owned(),
        "ms" => "(ms)".to_owned(),
        "secs" => "(secs)".to_owned(),
        "pct" => "(%)".to_owned(),
        _ if first => title_word(word),
        lower => lower.to_owned(),
    }
}

fn title_word(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => format!(
            "{}{}",
            c.to_uppercase(),
            chars.as_str().to_ascii_lowercase()
        ),
        None => String::new(),
    }
}

/// Curated human labels and within-subsection ordering for schema settings.
const CURATED_LABELS: &[(&str, &str, u16)] = &[
    // Display · Appearance
    ("tui.theme.dark", "Theme (dark)", 1),
    ("tui.theme.light", "Theme (light)", 2),
    ("tui.theme.current", "Theme mode", 3),
    ("tui.theme.mode", "Theme mode", 4),
    ("tui.symbol_preset", "Symbol preset", 5),
    ("tui.color_blind_mode", "Color-blind mode", 6),
    ("tui.theme", "Theme override (legacy)", 7),
    // Display · Statusline
    ("tui.statusline.preset", "Statusline preset", 1),
    ("tui.statusline.separator", "Statusline separator", 2),
    (
        "tui.statusline.session_accent",
        "Session accent highlight",
        3,
    ),
    ("tui.statusline.transparent", "Transparent background", 4),
    ("tui.statusline.show_hook_status", "Show hook status", 5),
    ("tui.statusline.left_segments", "Left segments", 6),
    ("tui.statusline.right_segments", "Right segments", 7),
    // Display · Workspace
    ("tui.pinned_agents", "Pinned subagents", 1),
    ("tui.subagent_live_preview", "Subagent live preview", 2),
    ("tui.mouse", "Mouse click-to-focus", 3),
    ("tui.hide_thinking", "Hide thinking blocks", 4),
    // Display · Notifications
    ("tui.notify", "Desktop notifications", 1),
    ("tui.notify_after_secs", "Notify after (secs)", 2),
    ("tui.notify_title", "Notification title", 3),
    // Interaction
    ("tui.vim_mode", "Vim mode", 1),
    ("tui.vim_mode_display", "Vim mode indicator", 2),
    ("tui.recall_cleared_drafts", "Recall cleared drafts", 3),
    ("interaction.double_escape_action", "Double-Esc action", 4),
    ("interaction.btw_inject", "/btw inject rail", 5),
    ("output.out_enabled", "Mid-turn out:// stream", 1),
    // Models · Reasoning & Behavior
    ("thinking", "Thinking effort", 1),
    ("llm.extended_context", "Extended context window", 2),
    ("llm.context_promotion", "Auto context promotion", 3),
    ("llm.role_storage", "Role storage layer", 4),
    ("enabled_models", "Enabled models", 5),
    ("disabled_providers", "Disabled providers", 6),
    ("models.default", "Default model", 7),
    // Models · Providers & Tiers
    ("llm.tier.openai", "OpenAI service tier", 1),
    ("llm.tier.anthropic", "Anthropic service tier", 2),
    ("llm.tier.google", "Google service tier", 3),
    ("llm.tier.subagent", "Subagent service tier", 4),
    ("llm.anthropic.slow_mode", "Anthropic slow mode", 5),
    ("llm.kimi.api_format", "Kimi API format", 6),
    ("llm.retry.usage_reserve_pct", "Quota reserve (%)", 7),
    ("llm.accounts", "Account pool policies", 8),
    // Models · Passive Advisor
    ("advisor.enabled", "Enable advisor", 1),
    ("advisor.model", "Advisor model", 2),
    ("advisor.thinking", "Advisor thinking effort", 3),
    ("advisor.max_tokens", "Advisor max tokens", 4),
    // Models · Roles (bottom section)
    ("roles.default", "Default role", 1),
    ("roles.smol", "Smol role", 2),
    ("roles.fast", "Fast role", 3),
    ("roles.slow", "Slow role", 4),
    ("roles.plan", "Plan role", 5),
    ("roles.vision", "Vision role", 6),
    ("roles.commit", "Commit role", 7),
    ("roles.task", "Task role", 8),
    ("roles.tiny", "Tiny role", 9),
    ("roles.memory", "Memory role", 10),
    ("roles.advisor", "Advisor role", 11),
    ("roles.title", "Title role", 12),
    ("roles.reviewer", "Reviewer role", 13),
    ("roles.designer", "Designer role", 14),
    ("roles.find", "Find role", 15),
    ("roles.judge", "Judge role", 16),
    // Session
    ("session.auto_resume", "Auto-resume session", 1),
    ("session.auto_title", "Auto-generate title", 2),
    (
        "session.title_refresh_on_replan",
        "Refresh title on replan",
        3,
    ),
    ("session.branch_summary", "Branch switch summary", 4),
    ("compaction.enabled", "Auto-compaction", 1),
    ("compaction.threshold_percent", "Threshold (%)", 2),
    ("compaction.threshold_tokens", "Threshold (tokens)", 3),
    ("compaction.reserve_tokens", "Output reserve tokens", 4),
    ("compaction.keep_recent_tokens", "Keep recent tokens", 5),
    ("compaction.mid_turn_enabled", "Mid-turn compaction", 6),
    ("compaction.auto_continue", "Auto-continue after compact", 7),
    (
        "compaction.async_enabled",
        "Speculative background compact",
        8,
    ),
    ("compaction.method_order", "Compaction method order", 9),
    ("compaction.model_thresholds", "Per-model thresholds", 10),
    ("compaction.idle.enabled", "Idle compaction", 11),
    (
        "compaction.idle.threshold_tokens",
        "Idle threshold (tokens)",
        12,
    ),
    ("compaction.idle.timeout_secs", "Idle timeout (secs)", 13),
    ("gc.retain_days", "Retain sessions (days)", 1),
    ("gc.retain_newest_per_cwd", "Retain newest per dir", 2),
    ("gc.retain_newest_global", "Retain newest globally", 3),
    ("gc.vacuum_reclaim_bytes", "VACUUM threshold (bytes)", 4),
    // Agents
    ("task.async", "Run subagents async", 1),
    ("task.batch", "Batched task arrays", 2),
    ("task.max_concurrency", "Max concurrency", 3),
    ("task.max_recursion_depth", "Max nesting depth", 4),
    ("task.soft_request_budget", "Soft request budget", 5),
    ("task.max_runtime_ms", "Max runtime (ms)", 6),
    ("task.agent_idle_ttl_ms", "Idle park TTL (ms)", 7),
    ("task.steps", "Multi-step chains", 8),
    ("task.enable_effort", "Per-task effort overrides", 9),
    ("task.structured", "Structured output schema", 10),
    ("task.per_call_model", "Per-call model override", 11),
    ("task.per_call_model_allow", "Allowed override models", 12),
    ("task.agent_models", "Per-agent model overrides", 13),
    ("task.disabled_agents", "Disabled subagents", 14),
    ("task.isolation.enabled", "Git worktree isolation", 1),
    ("task.isolation.merge", "Isolation return mode", 2),
    ("task.isolation.apply", "Auto-apply isolation patches", 3),
    ("eval.py", "Python interpreter", 1),
    ("eval.js", "JavaScript interpreter", 2),
    ("eval.tools_enabled", "Shareable @tool functions", 3),
    ("eval.default_timeout_secs", "Cell timeout (secs)", 4),
    // Tools
    ("tools.approval_mode", "Approval mode", 1),
    ("tools.premium_defer", "Defer rarely-used tools", 2),
    ("tools.ask_timeout_secs", "Ask form timeout (secs)", 3),
    ("tools.restricted_folders", "Restricted folders", 4),
    ("tools.approval", "Per-tool approval rules", 5),
    ("mcp_tools.budget_tokens", "MCP eager schema budget", 6),
    ("hooks.discover_claude_settings", "Discover Claude hooks", 7),
    ("edit.mode", "Edit tool mode", 1),
    ("edit.fuzzy_match", "Fuzzy matching", 2),
    ("edit.fuzzy_threshold", "Fuzzy similarity threshold", 3),
    ("edit.block_auto_generated", "Block generated files", 4),
    ("edit.enforce_seen_lines", "Require seen lines", 5),
    ("edit.recover_inline_edits", "Recover inline patches", 6),
    ("edit.auto_repair", "AST auto-repair", 7),
    ("edit.blackbox", "Syntax regression blackbox", 8),
    ("edit.model_variants", "Per-model edit modes", 9),
    ("lsp.enabled", "Enable LSP", 1),
    ("lsp.lazy", "Lazy server startup", 2),
    ("lsp.diagnostics_on_write", "Diagnostics on write", 3),
    ("lsp.diagnostics_on_edit", "Diagnostics on edit", 4),
    ("lsp.format_on_write", "Format on write", 5),
    ("lsp.diagnostics_deduplicate", "Deduplicate diagnostics", 6),
    ("lsp.request_timeout_secs", "Request timeout (secs)", 7),
    ("lsp.idle_timeout_secs", "Idle shutdown (secs)", 8),
    ("shell.auto_background_secs", "Auto-background (secs)", 1),
    ("shell.pty_default", "Run bash in PTY", 2),
    ("shell.pty_idle_ms", "PTY quiet wait (ms)", 3),
    ("shell.input_wait_ms", "Input quiet wait (ms)", 4),
    ("shell.git_guard", "Git destructive guard", 5),
    ("shell.allow_compound_commands", "Approve && chains", 6),
    ("shell.patterns", "Approval pattern rules", 7),
    ("shell.allowed_env_vars", "Allowed env prefixes", 8),
    ("shell.interceptor.enabled", "Command interceptor", 9),
    ("shell.interceptor.patterns", "Interceptor patterns", 10),
    ("fs.grep_context_before", "Grep context before", 11),
    ("fs.grep_context_after", "Grep context after", 12),
    // More
    ("commit.enabled", "Enable /commit", 1),
    ("commit.changelog", "Update CHANGELOG.md", 2),
    ("browser.headless", "Headless browser", 1),
    ("browser.relay", "Use Chrome relay", 2),
    ("browser.cdp_url", "CDP endpoint URL", 3),
    ("browser.binary", "Browser executable", 4),
    ("browser.screenshot_dir", "Screenshot directory", 5),
    ("desktop.display", "Capture display", 1),
    ("desktop.max_width", "Max capture width", 2),
    ("desktop.max_height", "Max capture height", 3),
    (
        "desktop.background_fallback",
        "Background input fallback",
        4,
    ),
    ("desktop.eval_prelude", "Expose computer.* in eval", 5),
    ("share.store", "Share backend store", 1),
    ("share.server", "Share blob server URL", 2),
    ("share.redact", "Redact secrets on share", 3),
    ("collab.relay", "Collab WebSocket relay", 4),
    ("update.channel", "Update channel", 1),
    ("update.repo", "Update GitHub repo", 2),
    ("web.api_key", "Web search API key", 3),
];
