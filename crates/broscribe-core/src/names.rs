//! Person-name policy shared by caption matching, forget tombstones and every
//! writer of a person's name or alias: one validation, one normalisation.

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::{Error, Result};

/// Longest person name or alias accepted, in characters.
pub const NAME_MAX_CHARS: usize = 200;

/// Characters a displayed name never carries: C0/C1 controls, line and
/// paragraph separators, and bidirectional embedding, override and isolate
/// controls (they could reorder the text around a name in Markdown, the
/// terminal or the app).
pub fn is_name_control(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}' | '\u{2029}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// A person name or alias as the user (or a reader) supplied it, trimmed:
/// non-empty, one line without [`is_name_control`] characters, at most
/// [`NAME_MAX_CHARS`] characters. Returns the trimmed slice of `name`
/// (callers own it only when storing); `invalid_input` otherwise. The name
/// is never rewritten silently.
pub fn validate_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::InvalidInput("empty name".into()));
    }
    if name.chars().any(is_name_control) {
        return Err(Error::InvalidInput(
            "a name is one line of text without control characters".into(),
        ));
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(Error::InvalidInput(format!(
            "a name has at most {NAME_MAX_CHARS} characters"
        )));
    }
    Ok(name)
}

/// Strips trailing `(Guest)`/`(External)` markers, reorders "Last, First",
/// treats [`is_name_control`] characters as spaces and collapses whitespace;
/// case and accents are kept for display.
pub fn normalize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if is_name_control(c) { ' ' } else { c })
        .collect();
    let mut name = cleaned.trim().to_owned();
    loop {
        let lower = name.to_lowercase();
        let suffix = ["(guest)", "(external)"]
            .into_iter()
            .find(|s| lower.ends_with(s));
        let Some(suffix) = suffix else { break };
        name.truncate(name.len() - suffix.len());
        name = name.trim().to_owned();
    }
    if let Some((last, first)) = name.split_once(',') {
        name = format!("{} {}", first.trim(), last.trim());
    }
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`normalize_name`] without accents, lowercased: the key two spellings of
/// one name compare equal under.
pub fn folded_name(name: &str) -> String {
    fold_search_text(&normalize_name(name))
}

/// Fold case and accents in free text without changing punctuation or spacing.
pub fn fold_search_text(text: &str) -> String {
    text.chars().flat_map(fold_character).collect()
}

fn fold_character(ch: char) -> impl Iterator<Item = char> {
    std::iter::once(ch)
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
}

/// Match a folded query and return byte ranges in the original text.
pub fn folded_match_ranges(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut folded = String::new();
    let mut offsets: Vec<(usize, usize)> = Vec::new();
    for (start, ch) in text.char_indices() {
        let end = start + ch.len_utf8();
        let previous_len = folded.len();
        folded.extend(fold_character(ch));
        let added = folded.len() - previous_len;
        if added == 0 {
            for (_, previous_end) in offsets.iter_mut().rev() {
                if *previous_end != start {
                    break;
                }
                *previous_end = end;
            }
        } else {
            offsets.extend(std::iter::repeat_n((start, end), added));
        }
    }
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for (start, matched) in folded.match_indices(query) {
        let range = offsets[start].0..offsets[start + matched.len() - 1].1;
        if let Some(last) = ranges.last_mut()
            && range.start < last.end
        {
            last.end = last.end.max(range.end);
        } else {
            ranges.push(range);
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fold_and_strip_without_partial_matching() {
        assert_eq!(
            normalize_name("  Solís, Dária (Guest) (External) "),
            "Dária Solís"
        );
        assert_eq!(folded_name("Solís, Dária (External)"), "daria solis");
        assert_ne!(folded_name("Dária"), folded_name("Dária Solís"));
    }

    #[test]
    fn controls_never_survive_normalisation() {
        assert_eq!(
            normalize_name("Ana\u{0}\u{1b}[31mExample"),
            "Ana [31mExample"
        );
        assert_eq!(
            normalize_name("Ana\u{202E}Example\nSmith"),
            "Ana Example Smith"
        );
        assert_eq!(folded_name("Ána\u{2066}Example"), "ana example");
    }

    #[test]
    fn validation_refuses_controls_empty_and_long_names_without_rewriting() {
        assert_eq!(validate_name("  Ana Example ").unwrap(), "Ana Example");
        for bad in [
            "",
            "   ",
            "Ana\nExample",
            "Ana\u{7}",
            "Ana\u{202E}elpmaxE",
            "Ana\u{2028}Example",
        ] {
            let err = validate_name(bad).unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{bad:?}");
        }
        assert!(validate_name(&"a".repeat(NAME_MAX_CHARS)).is_ok());
        assert!(validate_name(&"a".repeat(NAME_MAX_CHARS + 1)).is_err());
        // Accents, apostrophes and Markdown-significant characters are names'
        // business; escaping them is the renderer's (`render`, `export`).
        assert_eq!(validate_name("D'Ária *Solís*").unwrap(), "D'Ária *Solís*");
    }
}
