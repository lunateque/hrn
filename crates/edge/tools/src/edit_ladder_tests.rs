use super::*;

const ONE: Opts = Opts {
    replace_all: false,
    whole_lines: false,
    fuzzy_match: true,
    fuzzy_threshold: DEFAULT_FUZZY_THRESHOLD,
};
const ALL: Opts = Opts {
    replace_all: true,
    whole_lines: false,
    fuzzy_match: true,
    fuzzy_threshold: DEFAULT_FUZZY_THRESHOLD,
};

#[test]
fn exact_step_is_unique_or_error() {
    let body = "a = 1;\nb = 2;\na = 1;\n";
    let err = apply(body, "a = 1;", "a = 9;", ONE).unwrap_err();
    assert_eq!(
        err,
        LadderError::Ambiguous {
            step: Step::Exact,
            lines: vec![1, 3]
        }
    );
    let ok = apply(body, "b = 2;", "b = 3;", ONE).unwrap();
    assert_eq!((ok.step, ok.count), (Step::Exact, 1));
    assert_eq!(ok.text, "a = 1;\nb = 3;\na = 1;\n");
    let all = apply(body, "a = 1;", "a = 9;", ALL).unwrap();
    assert_eq!(
        (all.count, all.text.as_str()),
        (2, "a = 9;\nb = 2;\na = 9;\n")
    );
}

#[test]
fn ambiguous_exact_does_not_fall_through_to_looser_steps() {
    // `x` is an exact substring twice; the looser steps must never be consulted.
    let body = "x\nx\ny  \n";
    assert!(matches!(
        apply(body, "x", "z", ONE),
        Err(LadderError::Ambiguous {
            step: Step::Exact,
            ..
        })
    ));
}

#[test]
fn trailing_whitespace_step_matches_uniquely() {
    let body = "fn f() {\n    let x = 1;  \n    let y = 2;\t\n}\n";
    let old = "    let x = 1;\n    let y = 2;\n";
    let ok = apply(body, old, "    let z = 3;\n", ONE).unwrap();
    assert_eq!(ok.step, Step::TrailingWs);
    assert_eq!(ok.text, "fn f() {\n    let z = 3;\n}\n");

    let twice = "a  \nb\na\t\nb\n";
    let err = apply(twice, "a\nb\n", "c\n", ONE).unwrap_err();
    assert_eq!(
        err,
        LadderError::Ambiguous {
            step: Step::TrailingWs,
            lines: vec![1, 3]
        }
    );
}

#[test]
fn indent_step_matches_uniquely_and_reindents() {
    let body = "mod m {\n        if a {\n            go();\n        }\n}\n";
    let old = "if a {\n    go();\n}\n";
    let new = "if a {\n    go();\n    stop();\n}\n";
    let ok = apply(body, old, new, ONE).unwrap();
    assert_eq!(ok.step, Step::Indent);
    assert_eq!(
        ok.text,
        "mod m {\n        if a {\n            go();\n            stop();\n        }\n}\n"
    );

    // Two uniformly shifted copies match; the third has the wrong relative indentation.
    let blocks = "    k(\n        1\n    )\nz\n  k(\n      1\n  )\n  k(\n  1\n  )\n";
    let err = apply(blocks, "k(\n    1\n)\n", "q\n", ONE).unwrap_err();
    assert_eq!(
        err,
        LadderError::Ambiguous {
            step: Step::Indent,
            lines: vec![1, 5]
        }
    );
}

#[test]
fn miss_returns_nearest_snippets_with_line_numbers() {
    let body = "alpha beta\ngamma delta\nfn compute_total(a: i32) -> i32 {\n    a + 1\n}\n";
    let err = apply(
        body,
        "fn compute_total(a: u64) -> u64 {\n    a + 2\n}\n",
        "x",
        ONE,
    )
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("not found"), "{text}");
    assert!(
        text.contains("    3| fn compute_total(a: i32) -> i32 {"),
        "{text}"
    );
    assert!(text.matches("--- lines").count() <= 3);
}

#[test]
fn threshold_config() {
    // Two single-char typos across a ~52-char line -> ~96% similarity.
    let body =
        "fn calculate_invoice_subtotal_for_customer_account(x: i32) -> i32 {\n    x + 1\n}\n";
    let near = "fn calculate_invoice_subtotal_for_custoner_accoumt(x: i32) -> i32 {\n";

    let loose = Opts {
        replace_all: false,
        whole_lines: false,
        fuzzy_match: true,
        fuzzy_threshold: 0.95,
    };
    let ok = apply(
        body,
        near,
        "fn calculate_invoice_subtotal_for_customer_account(x: i64) -> i64 {\n",
        loose,
    )
    .expect("96%-similar anchor applies at 0.95");
    assert_eq!(ok.step, Step::Fuzzy);
    assert!(ok.text.contains("x: i64"), "{}", ok.text);

    let strict = Opts {
        fuzzy_threshold: 0.99,
        ..loose
    };
    let err = apply(body, near, "replacement\n", strict).expect_err("refused at 0.99");
    assert!(matches!(err, LadderError::NotFound { .. }), "{err:?}");

    let disabled = Opts {
        fuzzy_match: false,
        fuzzy_threshold: 0.50,
        ..loose
    };
    assert!(
        apply(body, near, "replacement\n", disabled).is_err(),
        "fuzzy_match = false must refuse non-exact"
    );
}
