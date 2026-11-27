//! End-to-end tests of the sidecar binary: one JSON line in, one JSON line out, exactly as
//! the `ast` module drives it.

use hrn_edge_ast::protocol::{Data, Request, Response, Rule};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hrn-ast-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }
    fn forward_slashed(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }
    /// One request line into the built binary, one response line back.
    fn ask(request: &Request) -> Response {
        let exe = env!("CARGO_BIN_EXE_hrn-ast");
        let mut child = Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn hrn-ast");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(serde_json::to_string(request).expect("encode").as_bytes())
            .expect("write request");
        let output = child.wait_with_output().expect("wait");
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text
            .lines()
            .find(|l| !l.trim().is_empty())
            .expect("a response line");
        serde_json::from_str(line).expect("a valid response")
    }
    #[test]
    fn a_search_round_trip_returns_matches_with_bindings() {
        let dir = tree("search");
        std::fs::write(dir.join("a.rs"), "fn main() {\n    log(\"x\");\n}\n").expect("write");
        let response = ask(&Request::Search {
            cwd: forward_slashed(&dir),
            path: forward_slashed(&dir),
            lang: None,
            pattern: "log($A)".to_owned(),
            skip: 0,
            limit: 50,
        });
        let Data::Search(found) = response.into_data().expect("data") else {
            panic!("a rewrite for a search");
        };
        assert_eq!(found.total, 1);
        let first = found.matches.first().expect("one match");
        assert_eq!(first.path, "a.rs");
        assert_eq!(first.line, 2);
        assert_eq!(first.meta, vec![("A".to_owned(), "\"x\"".to_owned())]);
    }
    #[test]
    fn a_rewrite_round_trip_previews_then_writes() {
        let dir = tree("rewrite");
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn main() {\n    log(1);\n}\n").expect("write");
        let build = |write: bool| Request::Rewrite {
            cwd: forward_slashed(&dir),
            path: forward_slashed(&dir),
            lang: None,
            rules: vec![Rule {
                pat: "log($A)".to_owned(),
                out: "trace($A)".to_owned(),
            }],
            max_files: 1000,
            write,
        };
        let response = ask(&build(false));
        let Data::Rewrite(preview) = response.into_data().expect("data") else {
            panic!("a search for a rewrite");
        };
        assert_eq!(preview.total, 1);
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "fn main() {\n    log(1);\n}\n",
            "a preview never writes"
        );
        let response = ask(&build(true));
        let Data::Rewrite(applied) = response.into_data().expect("data") else {
            panic!("a search for a rewrite");
        };
        assert_eq!(applied.per_file, preview.per_file);
        assert!(
            std::fs::read_to_string(&file)
                .expect("read")
                .contains("trace(1)")
        );
    }
    #[test]
    fn a_bad_request_and_an_unknown_language_answer_with_one_error_line() {
        let dir = tree("errors");
        std::fs::write(dir.join("a.rs"), "fn main() { p(1); }\n").expect("write");
        let response = ask(&Request::Search {
            cwd: forward_slashed(&dir),
            path: forward_slashed(&dir),
            lang: Some("cobol".to_owned()),
            pattern: "p($A)".to_owned(),
            skip: 0,
            limit: 50,
        });
        let error = response.into_data().expect_err("unknown language");
        assert!(error.contains("cobol"), "{error}");
    }
}
