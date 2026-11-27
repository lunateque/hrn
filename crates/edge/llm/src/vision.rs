//! Model vision capability detection (`LlmProvider::supports_vision`).
//!
//! Explicit preset configuration (`ModelPreset::vision`, populated from `config.toml` or
//! `models.yml` `input: [text, image]`) always wins; otherwise falls back to model-family rules.

use crate::config::LlmConfig;

/// Returns `true` when the resolved model accepts inline image inputs.
pub(crate) fn supports_vision(cfg: &LlmConfig, model: Option<&str>) -> bool {
    let resolved_alias = crate::candidates::resolve_alias(cfg, model).ok();
    let preset = resolved_alias
        .as_deref()
        .and_then(|alias| cfg.presets.get(alias));
    if let Some(preset) = preset {
        if let Some(explicit) = preset.vision {
            return explicit;
        }
        return infer_vision_from_model_id(&preset.id);
    }
    model.is_some_and(infer_vision_from_model_id)
}

/// Infer vision support from an upstream model identifier when no explicit flag was set.
pub(crate) fn infer_vision_from_model_id(raw: &str) -> bool {
    let lc = raw.trim().to_ascii_lowercase();
    if lc.is_empty() {
        return false;
    }
    let tail = lc.rsplit('/').next().unwrap_or(lc.as_str());

    // Known text-only exclusions must take precedence over broader family prefixes.
    if tail.contains("gpt-oss")
        || tail.starts_with("o1-mini")
        || tail.starts_with("o3-mini")
        || tail.contains("grok-code")
        || tail.contains("kimi-k2-thinking")
        || tail.contains("codellama")
        || tail.contains("starcoder")
        || (tail.contains("deepseek") && !tail.contains("-vl"))
    {
        return false;
    }

    // Explicit vision/multimodal tags across model families.
    if tail.contains("-vision")
        || tail.contains("-vl")
        || tail.contains("vl-")
        || tail.starts_with("qvq")
        || tail.contains("llava")
        || tail.contains("pixtral")
        || tail.contains("internvl")
        || tail.contains("minicpm-v")
    {
        return true;
    }

    // Major multimodal model families.
    tail.contains("claude")
        || tail.contains("gemini")
        || tail.starts_with("gpt-4o")
        || tail.starts_with("chatgpt-4o")
        || tail.starts_with("gpt-4.1")
        || tail.starts_with("gpt-4.5")
        || tail.starts_with("gpt-5")
        || tail.starts_with("gpt-6")
        || tail.starts_with("o1")
        || tail.starts_with("o3")
        || tail.starts_with("o4")
        || tail.starts_with("grok-3")
        || tail.starts_with("grok-4")
        || tail.starts_with("kimi-k2.5")
        || tail.starts_with("kimi-k2.7")
        || tail.starts_with("kimi-for-coding")
        || tail.starts_with("llama-4")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelPreset, ProviderEntry, ProviderKind};

    fn test_cfg() -> LlmConfig {
        let mut cfg = LlmConfig::default();
        cfg.providers.insert(
            "p".into(),
            ProviderEntry {
                kind: ProviderKind::OpenaiChat,
                ..ProviderEntry::default()
            },
        );
        let add = |cfg: &mut LlmConfig, alias: &str, id: &str, vision: Option<bool>| {
            cfg.presets.insert(
                alias.into(),
                ModelPreset {
                    provider: "p".into(),
                    id: id.into(),
                    vision,
                    ..ModelPreset::default()
                },
            );
        };
        add(&mut cfg, "claude", "claude-sonnet-4-6", None);
        add(&mut cfg, "gpt4o", "gpt-4o", None);
        add(&mut cfg, "gpt5", "gpt-5.3-codex", None);
        add(&mut cfg, "o3", "o3", None);
        add(&mut cfg, "o4", "o4-mini", None);
        add(&mut cfg, "gemini", "gemini-2.5-pro", None);
        add(&mut cfg, "qwenvl", "qwen2.5-vl-72b", None);
        add(&mut cfg, "kimi", "kimi-k2.7-code", None);
        add(&mut cfg, "llama", "llama-3.3-70b-versatile", None);
        add(&mut cfg, "gptoss", "openai/gpt-oss-120b", None);
        add(&mut cfg, "qwentext", "qwen3-32b", None);
        add(&mut cfg, "grokcode", "grok-code-fast-1", None);
        add(&mut cfg, "forced_off", "gpt-5", Some(false));
        add(&mut cfg, "forced_on", "custom-text-id", Some(true));
        cfg
    }

    #[test]
    fn detects_vision_and_text_only_models_and_honors_explicit_flag() {
        let cfg = test_cfg();
        for yes in [
            "claude",
            "gpt4o",
            "gpt5",
            "o3",
            "o4",
            "gemini",
            "qwenvl",
            "kimi",
            "forced_on",
        ] {
            assert!(
                supports_vision(&cfg, Some(yes)),
                "{yes} should support vision"
            );
        }
        for no in ["llama", "gptoss", "qwentext", "grokcode", "forced_off"] {
            assert!(
                !supports_vision(&cfg, Some(no)),
                "{no} should NOT support vision"
            );
        }
    }
}
