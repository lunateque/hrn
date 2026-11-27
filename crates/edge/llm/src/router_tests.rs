use super::*;
use crate::candidates::resolve_alias;

#[test]
fn thinking_loop_detector_trips_on_exact_cycle() {
    let mut det = ThinkingLoopDetector::default();
    let block = "Wait let me reconsider the same assumption carefully. ";
    assert!(!det.push_reasoning(block));
    assert!(!det.push_reasoning(block));
    assert!(det.push_reasoning(block));
}

fn cfg() -> LlmConfig {
    let preset = |id: &str| ModelPreset {
        provider: "p".into(),
        id: id.into(),
        ..ModelPreset::default()
    };
    LlmConfig {
        presets: [
            ("opus".to_owned(), preset("o")),
            ("sonnet".to_owned(), preset("s")),
        ]
        .into(),
        roles: [("plan".to_owned(), "opus".to_owned())].into(),
        ..LlmConfig::default()
    }
}

#[test]
fn unknown_model_is_an_error_not_the_alphabetically_first_preset() {
    let c = cfg();
    assert_eq!(resolve_alias(&c, Some("sonnet")).expect("preset"), "sonnet");
    assert_eq!(resolve_alias(&c, Some("plan")).expect("role"), "opus");
    assert_eq!(resolve_alias(&c, None).expect("fallback"), "opus");
    assert!(
        resolve_alias(&c, Some("sonet")).is_err(),
        "typo must not silently pick a model"
    );
}

#[test]
fn unconfigured_standard_role_falls_back_to_default() {
    let mut c = cfg();
    c.roles.insert("default".to_owned(), "sonnet".to_owned());
    assert_eq!(resolve_alias(&c, Some("fast")).expect("fast"), "sonnet");
    c.roles.insert("fast".to_owned(), "opus".to_owned());
    assert_eq!(resolve_alias(&c, Some("fast")).expect("fast"), "opus");
    assert!(resolve_alias(&c, Some("fastest")).is_err());
}
