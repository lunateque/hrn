//! The `KEY=value` dotenv syntax omp keeps its provider keys in (`~/.omp/agent/.env`).

/// The value of `name` in dotenv `text`: the last assignment wins; one pair of matching quotes
/// around the value is removed.
pub(crate) fn lookup(text: &str, name: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != name {
            continue;
        }
        let value = value.trim();
        let unquoted = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(value);
        found = Some(unquoted.to_owned());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::lookup;

    #[test]
    fn reads_plain_quoted_and_exported_assignments() {
        let text = "# c\nA=1\nexport B=\"two words\"\nC='x'\nA=3\nbare\n";
        assert_eq!(lookup(text, "A").as_deref(), Some("3"));
        assert_eq!(lookup(text, "B").as_deref(), Some("two words"));
        assert_eq!(lookup(text, "C").as_deref(), Some("x"));
        assert_eq!(lookup(text, "D"), None);
    }
}
