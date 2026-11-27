use super::*;
use crate::config::{ProviderEntry, validate_config};
use hrn_kernel::ThinkingLevel;

fn sample_cfg() -> LlmConfig {
    let preset = |provider: &str, id: &str| ModelPreset {
        provider: provider.into(),
        id: id.into(),
        ..ModelPreset::default()
    };
    LlmConfig {
        providers: [
            ("anth".to_owned(), ProviderEntry::default()),
            ("local".to_owned(), ProviderEntry::default()),
        ]
        .into(),
        presets: [
            ("opus".to_owned(), preset("anth", "claude-opus")),
            ("sonnet".to_owned(), preset("anth", "claude-sonnet")),
            ("flash".to_owned(), preset("local", "qwen-flash")),
        ]
        .into(),
        roles: [
            ("default".to_owned(), "sonnet".to_owned()),
            ("slow".to_owned(), "opus".to_owned()),
            ("smol".to_owned(), "flash".to_owned()),
        ]
        .into(),
        ..LlmConfig::default()
    }
}

#[test]
fn advisor_at_slow_high_resolves_opus_high_and_referrer_suffix_wins() {
    let mut cfg = sample_cfg();
    cfg.roles.insert("advisor".into(), "@slow:high".into());
    let resolved = expand_selector(&cfg, Some("advisor")).expect("advisor");
    assert_eq!(resolved.alias, "opus");
    assert_eq!(resolved.level, LevelSuffix::Level(ThinkingLevel::High));

    // Referrer suffix overrides referee suffix.
    cfg.roles.insert("slow".into(), "opus:medium".into());
    let resolved = expand_selector(&cfg, Some("@advisor")).expect("referrer wins");
    assert_eq!(resolved.alias, "opus");
    assert_eq!(resolved.level, LevelSuffix::Level(ThinkingLevel::High));

    // Direct call suffix overrides role suffix.
    let resolved = expand_selector(&cfg, Some("advisor:low")).expect("call suffix wins");
    assert_eq!(resolved.alias, "opus");
    assert_eq!(resolved.level, LevelSuffix::Level(ThinkingLevel::Low));
}

#[test]
fn role_cycle_errors_at_load_and_resolution() {
    let mut cfg = sample_cfg();
    cfg.roles.insert("plan".into(), "@reviewer".into());
    cfg.roles.insert("reviewer".into(), "@plan".into());

    let err = validate_config(&cfg).expect_err("must reject role cycle");
    assert!(err.to_string().contains("role cycle"), "{err}");
    assert!(expand_selector(&cfg, Some("plan")).is_err());
}

#[test]
fn tiny_unset_falls_back_to_smol_flash_and_star_to_default() {
    let cfg = sample_cfg();
    assert_eq!(resolve_alias(&cfg, Some("tiny")).expect("tiny"), "flash");
    assert_eq!(
        resolve_alias(&cfg, Some("memory")).expect("memory"),
        "flash"
    );
    assert_eq!(resolve_alias(&cfg, Some("title")).expect("title"), "flash");
    assert_eq!(resolve_alias(&cfg, Some("fast")).expect("fast"), "flash");
    assert_eq!(resolve_alias(&cfg, Some("*")).expect("star"), "sonnet");
}

#[test]
fn csv_ordered_candidates_pick_first_available_and_filters_apply() {
    let mut cfg = sample_cfg();
    cfg.disabled_providers = vec!["anth".into()];
    cfg.roles
        .insert("plan".into(), "missing, opus, flash:high".into());

    let resolved = expand_selector(&cfg, Some("plan")).expect("csv fallback");
    assert_eq!(resolved.alias, "flash");
    assert_eq!(resolved.level, LevelSuffix::Level(ThinkingLevel::High));

    let enabled: Vec<&str> = enabled_presets(&cfg).into_iter().map(|(a, _)| a).collect();
    assert_eq!(enabled, ["flash"]);
}

#[test]
fn ctrl_p_default_cycle_visits_smol_default_slow() {
    let cfg = sample_cfg();
    let cycle = effective_cycle_entries(&cfg);
    let labels: Vec<&str> = cycle.iter().map(|c| c.label.as_str()).collect();
    let aliases: Vec<&str> = cycle.iter().map(|c| c.alias.as_str()).collect();
    assert_eq!(labels, ["smol", "default", "slow"]);
    assert_eq!(aliases, ["flash", "sonnet", "opus"]);
}

#[test]
fn resolve_chain_consults_exact_role_and_provider_wildcard_in_order() {
    let mut cfg = sample_cfg();
    cfg.roles.insert("web".into(), "sonnet".into());
    cfg.fallback_chains
        .insert("default".into(), vec!["opus".into()]);
    cfg.fallback_chains
        .insert("web".into(), vec!["flash:low".into()]);
    cfg.fallback_chains
        .insert("anth/*".into(), vec!["flash".into(), "opus".into()]);
    validate_config(&cfg).expect("role and wildcard chains validate");

    // `None` maps to role `default`: primary sonnet -> default chain (opus) -> anth/* (flash, opus deduped).
    let (chain, _) = resolve_chain(&cfg, None).expect("default chain");
    let names: Vec<&str> = chain.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(names, ["sonnet", "opus", "flash"]);

    // `@web` resolves role `web` chain (`flash:low`) before provider wildcard `anth/*` (`opus`).
    let (chain, _) = resolve_chain(&cfg, Some("@web")).expect("web chain");
    let names: Vec<&str> = chain.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(names, ["sonnet", "flash", "opus"]);
    assert_eq!(chain[1].1.thinking, Some(ThinkingLevel::Low));

    // Direct preset alias skips role chains and consults exact alias -> provider wildcard.
    cfg.fallback_chains
        .insert("sonnet".into(), vec!["@smol".into()]);
    let (chain, _) = resolve_chain(&cfg, Some("sonnet")).expect("direct alias chain");
    let names: Vec<&str> = chain.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(names, ["sonnet", "flash", "opus"]);
}
