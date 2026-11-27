//! Table and wire tests: the embedded language data must survive the filter and a request must
//! round-trip through the wire types.

use crate::protocol::{Data, Request, Response, Rule};
use crate::table;

#[test]
fn the_table_describes_its_languages() {
    let specs = table::specs();
    assert!(!specs.is_empty(), "languages.json must not be empty");
    let rust = specs.iter().find(|s| s.name == "rust").expect("rust entry");
    assert!(rust.aliases.iter().any(|a| a == "rs"), "{rust:?}");
    assert!(rust.extensions.iter().any(|e| e == "rs"), "{rust:?}");
    assert_eq!(rust.expando_char(), 'µ');
    let json = specs.iter().find(|s| s.name == "json").expect("json entry");
    assert_eq!(json.expando_char(), '$');
    assert!(table::key_of("PY").is_some());
    assert_eq!(table::key_of("nolang"), None);
    assert_eq!(
        table::key_for_path(std::path::Path::new("x.rs")),
        Some("rust")
    );
    assert_eq!(table::key_for_path(std::path::Path::new("x.unknown")), None);
    assert!(table::aliases().contains("rust"));
}

#[test]
fn a_request_and_a_response_survive_the_wire() {
    let request = Request::Rewrite {
        cwd: "C:/work".to_owned(),
        path: "src/**/*.rs".to_owned(),
        lang: Some("rust".to_owned()),
        rules: vec![Rule {
            pat: "old($A)".to_owned(),
            out: "new($A)".to_owned(),
        }],
        max_files: 10,
        write: true,
    };
    let text = serde_json::to_string(&request).expect("encode");
    let back: Request = serde_json::from_str(&text).expect("decode");
    assert_eq!(request, back);
    let response = Response::err("nope");
    assert!(response.into_data().is_err());
    let response = Response::ok(Data::Search(crate::protocol::SearchData {
        matches: Vec::new(),
        total: 0,
        files_searched: 0,
        limit_reached: false,
        parse_errors: Vec::new(),
    }));
    assert!(matches!(
        response.into_data().expect("data"),
        Data::Search(_)
    ));
}
