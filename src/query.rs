use std::collections::BTreeSet;

/// Normalization is shared by candidate generation and final verification.
pub fn normalize(value: &str) -> String {
    value.chars().flat_map(char::to_lowercase).collect()
}

/// Parse whitespace-separated literal terms and double-quoted phrases.
pub fn parse(value: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in value.chars() {
        match c {
            '"' => {
                if !current.is_empty() {
                    terms.push(normalize(&current));
                    current.clear();
                }
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    terms.push(normalize(&current));
                    current.clear();
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        terms.push(normalize(&current));
    }
    terms.retain(|term| !term.is_empty());
    terms
}

fn token(chars: &[char]) -> String {
    let mut result = String::from("g");
    for c in chars {
        use std::fmt::Write;
        let _ = write!(result, "{:x}x", *c as u32);
    }
    result
}

/// Encode distinct 1-, 2-, and 3-codepoint n-grams as FTS-safe ASCII tokens.
pub fn grams(value: &str) -> String {
    let normalized = normalize(value);
    let chars: Vec<char> = normalized.chars().collect();
    let mut result = BTreeSet::new();
    for size in 1..=3.min(chars.len()) {
        for window in chars.windows(size) {
            result.insert(token(window));
        }
    }
    result.into_iter().collect::<Vec<_>>().join(" ")
}

pub fn term_grams(term: &str) -> Vec<String> {
    let chars: Vec<char> = term.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let size = 3.min(chars.len());
    chars
        .windows(size)
        .map(token)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Both,
    Name,
    Content,
}

fn fts_term_expression(term: &str, scope: Scope) -> String {
    let tokens = term_grams(term).join(" AND ");
    match scope {
        Scope::Both => format!("(name_grams:({tokens}) OR body_grams:({tokens}))"),
        Scope::Name => format!("name_grams:({tokens})"),
        Scope::Content => format!("body_grams:({tokens})"),
    }
}

pub fn fts_expression(terms: &[String], scope: Scope) -> String {
    terms
        .iter()
        .map(|term| fts_term_expression(term, scope))
        .collect::<Vec<_>>()
        .join(" AND ")
}

pub fn fts_any_expression(terms: &[String], scope: Scope) -> String {
    terms
        .iter()
        .map(|term| fts_term_expression(term, scope))
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub fn matched_terms(terms: &[String], name: &str, body: &str, scope: Scope) -> usize {
    let name = normalize(name);
    let body = normalize(body);
    terms
        .iter()
        .filter(|term| match scope {
            Scope::Both => name.contains(term.as_str()) || body.contains(term.as_str()),
            Scope::Name => name.contains(term.as_str()),
            Scope::Content => body.contains(term.as_str()),
        })
        .count()
}

pub fn matches(terms: &[String], name: &str, body: &str, scope: Scope) -> bool {
    matched_terms(terms, name, body, scope) == terms.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_phrases_and_normalizes() {
        assert_eq!(parse("  Foo \"一 二\" C++  "), vec!["foo", "一 二", "c++"]);
    }

    #[test]
    fn every_literal_produces_candidate_tokens() {
        let text = "Aa电脑 + 中😀文";
        let tokens: BTreeSet<_> = grams(text).split_whitespace().map(str::to_owned).collect();
        for part in ["a", "电脑", "+", "中😀文", "电脑 +"] {
            for token in term_grams(&normalize(part)) {
                assert!(tokens.contains(&token), "{part}: {token}");
            }
        }
    }

    #[test]
    fn matches_across_fields_but_never_parent_path() {
        let terms = parse("报告 GPU");
        assert!(matches(&terms, "报告.txt", "GPU: 8", Scope::Both));
        assert!(!matches(&terms, "报告.txt", "GPU: 8", Scope::Name));
        assert!(!matches(&parse("假的"), "报告.txt", "GPU: 8", Scope::Both));
    }
}
