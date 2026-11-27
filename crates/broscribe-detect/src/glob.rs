//! Case-insensitive glob matching for window title patterns.

/// Whether `text` matches `pattern` as a whole, ignoring case: `*` matches
/// any run of characters (including none), `?` exactly one.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let (mut pi, mut ti) = (0, 0);
    // Position of the last `*` and the text index it is currently covering up to.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || (p[pi] != '*' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            // Let the last `*` swallow one more character and retry.
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn matches_globs() {
        assert!(glob_match("Meet - *", "Meet - abc-defg-hij"));
        assert!(glob_match("meet - *", "MEET - x"));
        assert!(glob_match(
            "*meet.google.com*",
            "https://meet.google.com/abc"
        ));
        assert!(glob_match("*huddle*", "Huddle in #general"));
        assert!(glob_match("Meet – *", "Meet – abc"));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("Meet - *", "Inbox - Mail"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("abc", "abcd"));
        assert!(!glob_match("a*b", "aXXc"));
    }
}
