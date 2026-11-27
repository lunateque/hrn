//! The prompt-source data: which embedded `.md` file backs each prompt role and the shared
//! budget/clipping helpers. The numbers themselves live in the kernel (`prompt_budget`) so the
//! memory module and the `promptstats` gate read the same table; this module owns the prompt
//! text, cached from `crates/harness/prompts/*.md` via `include_str!`.

pub use hrn_kernel::prompt_budget::{
    BYTES_PER_TOKEN, Budgets, budgets, clip_at, clip_bytes, tokens,
};

/// The embedded base-prompt variants, keyed by the variant name a model family resolves to.
pub(crate) const BASE_DEFAULT: &str = include_str!("../prompts/base.md");
/// Variant `claude`: lean, judgement-over-rules (Anthropic's Claude-5 "new rules").
pub(crate) const BASE_CLAUDE: &str = include_str!("../prompts/base.claude.md");
/// Variant `gpt5`: the same law without the framing/repetition GPT-5 degrades on.
pub(crate) const BASE_GPT5: &str = include_str!("../prompts/base.gpt5.md");
/// The plan-mode block (kept byte-identical to the old `PLAN_MODE_PROMPT`).
pub(crate) const PLAN: &str = include_str!("../prompts/plan.md");
/// The hand-off block only child (`task` subagent) sessions get.
pub(crate) const SUBAGENT: &str = include_str!("../prompts/subagent.md");
/// The one-line pointer to the `bash` tool-commands help page (the `# Tool Commands` block).
pub(crate) const TOOL_COMMANDS: &str = include_str!("../prompts/tool-commands.md");
/// The standalone prompt used for model auto-titles and bare `/rename` regeneration.
pub(crate) const TITLE: &str = include_str!("../prompts/title.md");
/// The bundled `/init` one-shot prompt for creating or refreshing a concise `AGENTS.md`.
/// Expanded only on explicit `/init`; never injected into cold-turn system prompts.
pub const INIT_PROMPT: &str = include_str!("../prompts/init.md");

/// The tag wrapping every working-directory / volatile block after the cache breakpoint, so a
/// provider places its `cache_control` marker before it and the static prefix survives
/// (also the marker the Claude subscription wire already splits on).
pub(crate) const PROJECT_OPEN: &str = "<project-context>";
/// The closing tag of the block above.
pub(crate) const PROJECT_CLOSE: &str = "</project-context>";
