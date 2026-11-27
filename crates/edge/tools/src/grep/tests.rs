use super::*;
use crate::testutil::TempDir;

const STYLE: GrepStyle = GrepStyle {
    hashline: false,
    context_before: 0,
    context_after: 0,
};

fn grep(dir: &TempDir, pattern: &str, path: Option<&str>, skip: usize) -> String {
    run_grep(
        dir.path(),
        &GrepRequest {
            pattern,
            path,
            case_sensitive: true,
            gitignore: true,
            skip,
        },
        STYLE,
        &CancelFlag::new(),
    )
    .unwrap()
    .content
}

#[test]
fn respects_gitignore_without_a_git_repository_and_groups_by_directory() {
    let dir = TempDir::new("grep-ignore");
    dir.write(".gitignore", "ignored.txt\nbuild/\n");
    dir.write("kept.txt", "needle one\n");
    dir.write("ignored.txt", "needle two\n");
    dir.write("build/out.txt", "needle three\n");
    dir.write("sub/deep.rs", "needle four\n");
    let out = grep(&dir, "needle", None, 0);
    assert_eq!(
        out,
        "# kept.txt\n*1|needle one\n\n# sub/\n## deep.rs\n*1|needle four"
    );

    let mut req = GrepRequest {
        pattern: "needle",
        path: None,
        case_sensitive: true,
        gitignore: false,
        skip: 0,
    };
    let all = run_grep(dir.path(), &req, STYLE, &CancelFlag::new())
        .unwrap()
        .content;
    assert!(
        all.contains("ignored.txt") && all.contains("build/"),
        "{all}"
    );
    req.gitignore = true;
    req.case_sensitive = false;
    req.pattern = "NEEDLE";
    let folded = run_grep(dir.path(), &req, STYLE, &CancelFlag::new())
        .unwrap()
        .content;
    assert!(
        folded.contains("kept.txt") && !folded.contains("ignored.txt"),
        "{folded}"
    );
}

#[test]
fn multi_paths_exclusions_and_file_pages() {
    let dir = TempDir::new("grep-pages");
    for n in 0..25 {
        dir.write(&format!("src/f{n:02}.rs"), "hit\n");
    }
    dir.write("src/f00.test.rs", "hit\n");
    dir.write("tests/t.rs", "hit\n");
    let first = grep(&dir, "hit", Some("src;tests;!*.test.rs"), 0);
    assert!(
        first.contains("f00.rs") && !first.contains("f00.test.rs"),
        "{first}"
    );
    assert!(
        first.contains("Showing files 1-20 of 26. Use skip=20 for the next page"),
        "{first}"
    );
    let second = grep(&dir, "hit", Some("src;tests;!*.test.rs"), 20);
    assert!(
        second.contains("t.rs") && second.contains("Showing files 21-26 of 26"),
        "{second}"
    );
    let past = grep(&dir, "hit", Some("src;tests"), 90);
    assert!(
        past.starts_with("No more results (27 files total; skip=90"),
        "{past}"
    );
}

#[test]
fn selectors_cross_line_patterns_and_the_per_file_cap() {
    let dir = TempDir::new("grep-modes");
    dir.write("a.txt", "one\ntwo\nthree\ntwo\nfive\n");
    let ranged = grep(&dir, "two", Some("a.txt:3-5"), 0);
    assert_eq!(ranged, "[a.txt]\n*4|two");
    let multi = grep(&dir, "one\\ntwo", Some("a.txt"), 0);
    assert_eq!(multi, "[a.txt]\n*1|one\n*2|two");
    assert!(Scope::parse(dir.path(), Some("a.*:1-2"), true).is_err());

    dir.write("b.txt", &"x\n".repeat(30));
    dir.write("c.txt", "x\n");
    let capped = grep(&dir, "x", Some("b.txt;c.txt"), 0);
    assert!(capped.contains("More than 10 matches in b.txt"), "{capped}");
    assert_eq!(capped.matches('*').count(), 11, "{capped}");
}

#[test]
fn invalid_regex_falls_back_to_literal_with_a_note() {
    let dir = TempDir::new("grep-literal");
    dir.write("a.txt", "call foo(bar\n");
    let out = grep(&dir, "foo(bar", None, 0);
    assert!(
        out.contains("*1|call foo(bar") && out.contains("not a valid regex"),
        "{out}"
    );
}

#[test]
fn never_descends_into_dot_git() {
    let dir = TempDir::new("grep-dotgit");
    dir.write("a.txt", "needle\n");
    dir.write(".git/config", "needle\n");
    dir.write(".hidden.txt", "needle\n");
    assert_eq!(
        grep(&dir, "needle", None, 0),
        "# .hidden.txt\n*1|needle\n# a.txt\n*1|needle"
    );
}
