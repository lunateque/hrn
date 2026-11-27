//! Turn free text into an FTS5 `MATCH` expression.

/// Most terms one query may carry; longer prompts keep their first terms.
const MAX_TERMS: usize = 24;
/// Words too common to say anything about a memory; as OR terms they would match everything.
const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be",
    "been", "but", "by", "can", "could", "did", "do", "does", "for", "from", "get", "had", "has",
    "have", "how", "i", "if", "in", "into", "is", "it", "its", "me", "my", "no", "not", "now",
    "of", "on", "or", "our", "out", "please", "so", "some", "tell", "that", "the", "their", "them",
    "then", "there", "these", "they", "this", "to", "up", "us", "use", "was", "we", "were", "what",
    "when", "where", "which", "who", "why", "will", "with", "would", "you", "your",
];

/// `"a" OR "b" OR …` over the distinct significant words of `text` (lower-cased, quoted so FTS
/// syntax in the input is inert), or `None` when nothing significant is left.
pub(crate) fn match_expr(text: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.chars().count() < 2 {
            continue;
        }
        let word = word.to_lowercase();
        if STOPWORDS.contains(&word.as_str()) || terms.contains(&word) {
            continue;
        }
        terms.push(word);
        if terms.len() == MAX_TERMS {
            break;
        }
    }
    if terms.is_empty() {
        return None;
    }
    let quoted: Vec<String> = terms.iter().map(|t| format!("\"{t}\"")).collect();
    Some(quoted.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::match_expr;

    #[test]
    fn keeps_significant_words_and_neutralises_fts_syntax() {
        assert_eq!(
            match_expr("What is my Favourite colour? colour NEAR(\"x\")").as_deref(),
            Some("\"favourite\" OR \"colour\" OR \"near\"")
        );
        assert_eq!(match_expr("what is the a of"), None);
    }
}
