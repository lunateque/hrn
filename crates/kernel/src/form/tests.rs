use super::*;
use serde_json::json;

fn parse(html: &str) -> Form {
    Form::parse(html).expect("form parses")
}

fn field<'a>(form: &'a Form, name: &str) -> &'a Field {
    form.fields()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

const SURVEY: &str = r#"
<html><body><p>ignored outside the form</p>
<form>
  <h2>Deploy &amp; ship</h2>
  <p>Staging looks <b>healthy</b>.</p>
  <pre>
cargo test
  ok
</pre>
  <table><tr><th>Env</th><th>State</th></tr><tr><td>web</td><td>up</td></tr></table>
  <ul><li>one</li><li>two</li></ul>
  <img src="/a/diagram.png" alt="Topology">
  <fieldset>
    <legend>Database</legend>
    <label><input type="radio" name="db" value="pg" checked> PostgreSQL</label>
    <label><input type="radio" name="db" value="my"> MySQL</label>
  </fieldset>
  <p>Extras</p>
  <label><input type="checkbox" name="ci" value="ci"> CI</label>
  <label><input type="checkbox" name="ci" value="tf" checked> Terraform</label>
  <label><input type="checkbox" name="tests" value="yes"> Run tests</label>
  <label for="who">Your name</label>
  <input id="who" name="who" required minlength="2" placeholder="Ada">
  <label>Replicas <input type="number" name="n" min="1" max="5" value="2"></label>
  <select name="region"><option value="">Choose</option><option value="eu" selected>Europe</option><option>US</option><option disabled>Moon</option></select>
  <select name="tags" multiple><option>a</option><option selected>b</option></select>
  <textarea name="notes">
line1
line2</textarea>
  <input type="hidden" name="token" value="t0">
  <input type="submit" value="Go"><button>Send</button>
  <script>var x = "<input name=evil>";</script>
</form><p>after</p></body></html>"#;

#[test]
fn parses_context_in_order() {
    let form = parse(SURVEY);
    let texts: Vec<(&TextKind, &str)> = form
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Text { kind, text } => Some((kind, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(texts[0], (&TextKind::Heading, "Deploy & ship"));
    assert_eq!(texts[1], (&TextKind::Para, "Staging looks healthy."));
    assert_eq!(texts[2], (&TextKind::Pre, "cargo test\n  ok"));
    assert_eq!(texts[3], (&TextKind::List, "• one"));
    assert!(texts.contains(&(&TextKind::Legend, "Database")));
    assert!(
        !texts
            .iter()
            .any(|(_, t)| t.contains("outside") || t.contains("after"))
    );
    assert!(form.items.contains(&Item::Image {
        alt: "Topology".into()
    }));
    let table = form
        .items
        .iter()
        .find_map(|i| match i {
            Item::Table { header, rows } => Some((*header, rows.clone())),
            _ => None,
        })
        .expect("table");
    assert_eq!(
        table,
        (
            true,
            vec![
                vec!["Env".into(), "State".into()],
                vec!["web".into(), "up".into()]
            ]
        )
    );
}

#[test]
fn groups_labels_and_questions() {
    let form = parse(SURVEY);
    let names: Vec<&str> = form.fields().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "db", "ci", "tests", "who", "n", "region", "tags", "notes", "token"
        ]
    );
    let db = field(&form, "db");
    assert_eq!(db.kind, Kind::One);
    assert_eq!(db.question, "Database");
    assert_eq!(db.options[0].label, "PostgreSQL");
    assert!(db.other, "radio groups get an Other answer");
    let ci = field(&form, "ci");
    assert_eq!(ci.kind, Kind::Many);
    assert_eq!(ci.question, "Extras");
    assert!(ci.other);
    let tests = field(&form, "tests");
    assert_eq!(tests.kind, Kind::Flag);
    assert_eq!(tests.question, "Run tests");
    assert!(!tests.other);
    let who = field(&form, "who");
    assert_eq!(who.question, "Your name");
    assert!(who.required);
    assert_eq!(field(&form, "n").question, "Replicas");
    let region = field(&form, "region");
    assert_eq!(
        region.options.len(),
        2,
        "placeholder and disabled options are dropped"
    );
    assert!(!region.other, "selects get no Other answer");
    let notes = field(&form, "notes");
    assert!(matches!(&notes.kind, Kind::Text(s) if s.multiline && s.value == "line1\nline2"));
}

#[test]
fn an_option_labelled_other_suppresses_the_automatic_one() {
    let form = parse(
        r"<form><input type=radio name=a value=1> One <input type=radio name=a value=o> Other…</form>",
    );
    let a = field(&form, "a");
    assert_eq!(a.options[1].label, "Other…");
    assert!(!a.other);
}

#[test]
fn bare_inputs_take_their_label_from_trailing_text() {
    let form = parse(
        "<form><p>Pick</p><input type=radio name=a value=x> Alpha<br><input type=radio name=a value=y> Beta<br></form>",
    );
    let a = field(&form, "a");
    assert_eq!(a.options[0].label, "Alpha");
    assert_eq!(a.options[1].label, "Beta");
}

#[test]
fn defaults_and_values_follow_omp_rules() {
    let form = parse(SURVEY);
    let answers = form.defaults();
    assert_eq!(
        form.values(&answers),
        json!({
            "db": "pg",
            "ci": ["tf"],
            "who": "",
            "n": 2,
            "region": "eu",
            "tags": ["b"],
            "notes": "line1\nline2",
            "token": "t0",
        })
    );
    assert!(
        form.values(&answers).get("tests").is_none(),
        "lone unchecked is omitted"
    );
}

#[test]
fn other_text_is_the_value_or_an_extra_entry() {
    let form = parse(SURVEY);
    let mut answers = form.defaults();
    answers[0].picked.clear();
    answers[0].other = "  SQLite ".into();
    answers[1].other = "pulumi".into();
    answers[2].picked = vec![0];
    let values = form.values(&answers);
    assert_eq!(values["db"], "SQLite");
    assert_eq!(values["ci"], json!(["tf", "pulumi"]));
    assert_eq!(values["tests"], "yes");
}

#[test]
fn validation_reports_per_field() {
    let form = parse(SURVEY);
    let mut answers = form.defaults();
    let errors = form.errors(&answers);
    let at = |name: &str| form.fields().position(|f| f.name == name).expect("field");
    assert_eq!(errors[at("who")].as_deref(), Some("This is required"));
    assert_eq!(errors.iter().flatten().count(), 1);

    answers[at("who")].text = "A".into();
    answers[at("n")].text = "9".into();
    let errors = form.errors(&answers);
    assert_eq!(
        errors[at("who")].as_deref(),
        Some("Use at least 2 characters")
    );
    assert_eq!(errors[at("n")].as_deref(), Some("Must be at most 5"));
    answers[at("n")].text = "x".into();
    assert_eq!(
        form.errors(&answers)[at("n")].as_deref(),
        Some("Enter a number")
    );
    answers[at("n")].text = "2.5".into();
    assert_eq!(form.values(&answers)["n"], json!(2.5));
}

#[test]
fn pattern_format_and_range_checks() {
    let form = parse(
        r#"<form>
        <input name=sku pattern="[A-Z]{3}-\d+" title="Like ABC-12">
        <input type=email name=mail>
        <input type=url name=site>
        <input type=date name=day min="2025-01-01" max="2025-12-31">
        </form>"#,
    );
    let mut answers = form.defaults();
    assert!(
        form.errors(&answers).iter().all(Option::is_none),
        "blank optional fields pass"
    );
    answers[0].text = "abc".into();
    answers[1].text = "me@".into();
    answers[2].text = "example.com".into();
    answers[3].text = "2026-01-01".into();
    let errors = form.errors(&answers);
    assert_eq!(errors[0].as_deref(), Some("Like ABC-12"));
    assert!(errors[1].as_deref().is_some_and(|e| e.contains("email")));
    assert!(errors[2].as_deref().is_some_and(|e| e.contains("URL")));
    assert_eq!(errors[3].as_deref(), Some("Must be at most 2025-12-31"));
    answers[0].text = "ABC-12".into();
    answers[1].text = "me@site.org".into();
    answers[2].text = "https://site.org".into();
    answers[3].text = "2025-06-30".into();
    assert!(form.errors(&answers).iter().all(Option::is_none));
}

#[test]
fn required_choices_need_an_answer() {
    let form = parse(
        "<form><input type=radio name=a value=1 required> A <input type=radio name=a value=2> B</form>",
    );
    let mut answers = form.defaults();
    assert_eq!(
        form.errors(&answers)[0].as_deref(),
        Some("This is required")
    );
    answers[0].other = "mine".into();
    assert_eq!(form.errors(&answers)[0], None);
}

#[test]
fn forms_without_named_controls_or_with_clashing_names_are_rejected() {
    assert!(Form::parse("<form><p>hi</p><input></form>").is_err());
    assert!(Form::parse("<form><input name=a><textarea name=a></textarea></form>").is_err());
    let hidden = parse("<form><input type=hidden name=h value=1></form>");
    assert!(!hidden.is_interactive());
    assert_eq!(hidden.values(&hidden.defaults()), json!({"h": "1"}));
}

#[test]
fn lenient_markup_still_parses() {
    let form = parse(
        "<p>Name <input name=n value='a &amp; b'><p>Unclosed <b>tags <select name=s><option>x<option selected>y</select>",
    );
    assert_eq!(
        form.values(&form.defaults()),
        json!({"n": "a & b", "s": "y"})
    );
}
