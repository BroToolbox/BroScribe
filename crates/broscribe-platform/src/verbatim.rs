//! Plain spelling of Windows verbatim (`\\?\`) paths.
//!
//! `std::fs::canonicalize` on Windows always answers `\\?\C:\…`. Text read
//! by people and by other programs (a skill file, an agent CLI's MCP entry
//! started through Git Bash, PowerShell or Node) works better with `C:\…`,
//! but the two spellings mean different things for some names: a verbatim
//! path takes `..`, trailing dots or spaces and device names such as `CON`
//! literally and ignores `MAX_PATH`. [`plain_verbatim`] strips the prefix
//! only when the plain spelling names the same file, following the rules of
//! the `dunce` crate.

use std::path::{Path, PathBuf};

/// Paths of this many UTF-16 units or more need the verbatim form.
const MAX_PATH: usize = 260;
/// Longest file name component, in UTF-16 units.
const MAX_NAME: usize = 255;

/// `\\?\X:\rest` as `X:\rest` when that plain path means the same file;
/// every other path unchanged. Kept verbatim: UNC (`\\?\UNC\…`) and other
/// verbatim forms (`\\?\Volume{…}`, `\\?\GLOBALROOT…`), a drive part other
/// than one ASCII letter, `:` and `\`, text that is not Unicode, results of
/// 260 UTF-16 units or more, and any later part that is empty, `.`/`..`,
/// longer than 255 UTF-16 units, holds `< > : " / | ? *` or a control
/// character, ends in `.` or a space, or is a reserved device name (`CON`,
/// `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9` and the superscript
/// `¹²³` forms, ignoring case, anything from the first `.` and the spaces
/// before it).
///
/// Pure text work, the same on every OS: on Unix a canonical path starts
/// with `/`, so it comes back unchanged.
pub fn plain_verbatim(path: &Path) -> PathBuf {
    match path.to_str().and_then(plain_text) {
        Some(plain) => PathBuf::from(plain),
        None => path.to_path_buf(),
    }
}

fn plain_text(text: &str) -> Option<&str> {
    let plain = text.strip_prefix(r"\\?\")?;
    let rest = match plain.as_bytes() {
        [letter, b':', b'\\', ..] if letter.is_ascii_alphabetic() => &plain[3..],
        _ => return None,
    };
    (plain.encode_utf16().count() < MAX_PATH && rest.split('\\').all(safe_part)).then_some(plain)
}

/// A file name that a plain path reads exactly as a verbatim one does.
fn safe_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part.encode_utf16().count() <= MAX_NAME
        && !part.ends_with(['.', ' '])
        && !part
            .chars()
            .any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        && !is_reserved(part)
}

/// A DOS device name, with or without an extension (`con .txt`, `PrN.....`).
fn is_reserved(part: &str) -> bool {
    let stem = part
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    match stem.len() {
        3 => ["CON", "PRN", "AUX", "NUL"]
            .iter()
            .any(|name| stem.eq_ignore_ascii_case(name)),
        _ => {
            let mut chars = stem.chars();
            let prefix: String = chars.by_ref().take(3).collect();
            let digit = chars.next();
            chars.next().is_none()
                && (prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
                && digit.is_some_and(|d| d.is_ascii_digit() || matches!(d, '¹' | '²' | '³'))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> String {
        plain_verbatim(Path::new(text)).to_str().unwrap().to_owned()
    }

    fn kept(text: &str) {
        assert_eq!(plain(text), text, "{text:?} must stay verbatim");
    }

    #[test]
    fn a_drive_path_loses_its_prefix() {
        assert_eq!(
            plain(r"\\?\C:\Users\Synthetic\bin\broscribe.exe"),
            r"C:\Users\Synthetic\bin\broscribe.exe"
        );
        assert_eq!(plain(r"\\?\d:\a b\c.d\x"), r"d:\a b\c.d\x");
        assert_eq!(plain(r"\\?\C:\Común\ünï.exe"), r"C:\Común\ünï.exe");
        // Names that only start like a device are ordinary.
        assert_eq!(
            plain(r"\\?\C:\console\com10\lpt\auxiliary.txt\nul1"),
            r"C:\console\com10\lpt\auxiliary.txt\nul1"
        );
    }

    #[test]
    fn other_verbatim_forms_stay() {
        kept(r"\\?\UNC\server\share\dir\broscribe.exe");
        kept(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\bin\broscribe.exe");
        kept(r"\\?\GLOBALROOT\Device\HarddiskVolume1\bin\broscribe.exe");
        kept(r"\\?\C:");
        kept(r"\\?\C:/x");
        kept(r"\\?\CC:\x");
        kept(r"\\?\1:\x");
        kept(r"\\?\é:\x");
    }

    #[test]
    fn parts_a_plain_path_would_read_differently_stay() {
        for part in [
            "",
            ".",
            "..",
            "x.",
            "x ",
            "a<b",
            "a>b",
            "a:b",
            "a\"b",
            "a/b",
            "a|b",
            "a?b",
            "a*b",
            "a\u{1}b",
            "a\u{1f}b",
            "con",
            "CON",
            "con.txt",
            "con .txt",
            "PrN.....",
            "aux",
            "nul.tar.gz",
            "COM0",
            "com9",
            "LPT1.log",
            "com¹",
            "LPT³.x",
        ] {
            kept(&format!(r"\\?\C:\dir\{part}\broscribe.exe"));
        }
        kept(r"\\?\C:\");
        kept(r"\\?\C:\dir\");
        kept(r"\\?\C:\dir\\x");
    }

    #[test]
    fn long_paths_stay() {
        let long_name = "n".repeat(256);
        kept(&format!(r"\\?\C:\{long_name}"));
        // 3 (`C:\`) + 256 = 259 units: plain; one more: verbatim.
        let ok = format!(r"\\?\C:\{}\{}", "a".repeat(200), "b".repeat(55));
        assert_eq!(plain(&ok), &ok[4..]);
        assert_eq!(ok[4..].encode_utf16().count(), 259);
        kept(&format!(r"\\?\C:\{}\{}", "a".repeat(200), "b".repeat(56)));
        // Counted in UTF-16 units, not bytes: 200 `é` are 400 bytes.
        let wide = format!(r"\\?\C:\{}\{}", "é".repeat(200), "b".repeat(55));
        assert_eq!(plain(&wide), &wide[4..]);
        kept(&format!(r"\\?\C:\{}\{}", "é".repeat(200), "b".repeat(56)));
    }

    #[test]
    fn plain_and_unix_paths_are_unchanged() {
        for text in [
            r"C:\Users\Synthetic\broscribe.exe",
            r"\\server\share\broscribe.exe",
            r"\\.\pipe\broscribe",
            "/Applications/BroScribe.app/Contents/MacOS/broscribe",
            "/home/synthetic/../x/./broscribe",
            "relative/broscribe",
            "",
        ] {
            kept(text);
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_text_is_kept() {
        use std::os::unix::ffi::OsStrExt as _;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"\\\\?\\C:\\a\xff"));
        assert_eq!(plain_verbatim(path), path);
    }
}
