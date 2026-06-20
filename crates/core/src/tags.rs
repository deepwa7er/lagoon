//! Inline `#tag` parsing.
//!
//! Tags live in the thought text itself — the single source of truth. The store
//! mirrors them into the `tags`/`thought_tags` tables for autocomplete and
//! filtering, the same way it mirrors text into the FTS index. Clients render
//! the `#tag` tokens inline; this is the canonical extractor the store uses to
//! keep the mirror tables in step.

/// Extract the distinct `#tag` names from `text`.
///
/// A tag is a `#` that starts the text or follows whitespace, then a run of
/// Unicode letters/digits, `_`, or `-` that begins with a letter, digit, or `_`
/// (so a bare `#`, a `#-lead`, and a mid-word `a#b` are not tags), with any
/// trailing `-` trimmed. Names are de-duplicated case-insensitively, keeping the
/// casing and order of the first occurrence: `"#Idea then #idea"` → `["Idea"]`.
#[must_use]
pub fn parse_tags(text: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut seen_lower = Vec::new();
    let mut prev: Option<char> = None;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        // A tag begins at a `#` on a word boundary: the start of the text, or
        // after any non-word character — so `(#work)` and `#a, #b` tag, while
        // `foo#bar` (after a letter) and `##heading` (after a `#`) do not.
        let at_boundary = prev.is_none_or(|p| !(p.is_alphanumeric() || p == '_' || p == '#'));
        if c != '#' || !at_boundary {
            prev = Some(c);
            continue;
        }
        // Consume the tag body.
        let mut body = String::new();
        while let Some(&n) = chars.peek() {
            if n.is_alphanumeric() || n == '_' || n == '-' {
                body.push(n);
                chars.next();
            } else {
                break;
            }
        }
        let name = body.trim_end_matches('-');
        let valid = name
            .chars()
            .next()
            .is_some_and(|f| f.is_alphanumeric() || f == '_');
        if valid {
            let lower = name.to_lowercase();
            if !seen_lower.contains(&lower) {
                seen_lower.push(lower);
                tags.push(name.to_owned());
            }
        }
        // The last char actually consumed was the body's last (or the `#`
        // itself when the body was empty) — so `##x` and `## heading` don't tag.
        prev = body.chars().last().or(Some('#'));
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::parse_tags;

    #[test]
    fn extracts_simple_tags() {
        assert_eq!(parse_tags("ship the #idea today #soon"), vec!["idea", "soon"]);
    }

    #[test]
    fn tag_at_start_of_text() {
        assert_eq!(parse_tags("#todo buy milk"), vec!["todo"]);
    }

    #[test]
    fn no_tag_mid_word() {
        // A `#` not on a word boundary is not a tag (URLs, `a#b`).
        assert!(parse_tags("see issue gh#42 or foo#bar").is_empty());
    }

    #[test]
    fn double_hash_is_not_a_tag() {
        assert!(parse_tags("## heading").is_empty());
        assert!(parse_tags("##notag").is_empty());
    }

    #[test]
    fn bare_hash_and_lead_hyphen_rejected() {
        assert!(parse_tags("a # b").is_empty());
        assert!(parse_tags("#- #-foo").is_empty());
    }

    #[test]
    fn allows_hyphen_underscore_digits_unicode() {
        assert_eq!(
            parse_tags("#my-tag #snake_case #2026 #思考"),
            vec!["my-tag", "snake_case", "2026", "思考"],
        );
    }

    #[test]
    fn trailing_hyphen_trimmed() {
        assert_eq!(parse_tags("#tag- end"), vec!["tag"]);
    }

    #[test]
    fn dedupes_case_insensitively_keeping_first_casing() {
        assert_eq!(parse_tags("#Idea and again #idea and #IDEA"), vec!["Idea"]);
    }

    #[test]
    fn punctuation_terminates_a_tag() {
        assert_eq!(parse_tags("done (#work), next #home."), vec!["work", "home"]);
    }
}
