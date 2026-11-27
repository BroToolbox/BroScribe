//! Percent-encoding for URLs built by hand (authorization URLs) and
//! decoding of loopback callback queries (RFC 3986, form `+` as space).

/// Encodes everything except RFC 3986 unreserved characters.
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` escapes and `+` (as space); `None` for malformed escapes
/// or invalid UTF-8.
pub fn pct_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                let hex = std::str::from_utf8(hex).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `a=1&b=2` → pairs; pairs that fail to decode are dropped.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((pct_decode(k)?, pct_decode(v)?))
        })
        .collect()
}

/// Pairs → `a=1&b=2`, encoded.
pub fn build_query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = "a b/c?d=é&+";
        assert_eq!(pct_decode(&pct_encode(s)).as_deref(), Some(s));
        assert_eq!(pct_encode("A-z_0.~"), "A-z_0.~");
        assert_eq!(pct_decode("a+b%2B").as_deref(), Some("a b+"));
        assert_eq!(pct_decode("%G1"), None);
        assert_eq!(pct_decode("%4"), None);
        assert_eq!(
            parse_query("code=x%20y&state=s&empty&bad=%zz"),
            vec![
                ("code".to_string(), "x y".to_string()),
                ("state".to_string(), "s".to_string()),
                ("empty".to_string(), String::new()),
            ]
        );
        assert_eq!(
            build_query(&[("a", "1 2"), ("b", "x/y")]),
            "a=1%202&b=x%2Fy"
        );
    }
}
