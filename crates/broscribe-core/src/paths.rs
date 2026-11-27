//! Library layout: folder names and well-known file names.

use std::path::PathBuf;

use chrono::{DateTime, FixedOffset};

use crate::MeetingId;

pub const INDEX_DIR: &str = ".index";
/// Relative to the library root.
pub const INDEX_DB: &str = ".index/broscribe.sqlite";
pub const QUEUE_DIR: &str = ".queue";
pub const LOCK_FILE: &str = ".lock";
pub const PEOPLE_DIR: &str = "people";
pub const MEETING_JSON: &str = "meeting.json";
pub const MEETING_MD: &str = "meeting.md";
/// Derived Markdown copies of a meeting's template results, inside its folder.
pub const MEETING_TEMPLATES_DIR: &str = "templates";
pub const PERSON_MD: &str = "person.md";
pub const VOICEPRINTS_JSON: &str = "voiceprints.json";
pub const SLUG_MAX: usize = 48;

/// `YYYY-MM-DD-HHMM-<slug|untitled>-<6hex>`, in the meeting's own UTC offset.
pub fn meeting_dir_name(
    started_at: &DateTime<FixedOffset>,
    title: Option<&str>,
    id: &MeetingId,
) -> String {
    let slug = title.map(slugify).filter(|s| !s.is_empty());
    format!(
        "{}-{}-{}",
        started_at.format("%Y-%m-%d-%H%M"),
        slug.as_deref().unwrap_or("untitled"),
        id.short_hex()
    )
}

/// `YYYY/MM/<meeting_dir_name>`, relative to the library root.
pub fn meeting_rel_dir(
    started_at: &DateTime<FixedOffset>,
    title: Option<&str>,
    id: &MeetingId,
) -> PathBuf {
    PathBuf::from(started_at.format("%Y").to_string())
        .join(started_at.format("%m").to_string())
        .join(meeting_dir_name(started_at, title, id))
}

/// ASCII lowercase words joined by dashes, accents folded where known,
/// at most [`SLUG_MAX`] characters. May be empty.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        let mut buf = [0u8; 4];
        let folded: &str = match fold_char(c) {
            Some(s) => s,
            None => c.encode_utf8(&mut buf),
        };
        for f in folded.chars() {
            if f.is_ascii_alphanumeric() {
                if dash && !out.is_empty() {
                    out.push('-');
                }
                dash = false;
                out.push(f.to_ascii_lowercase());
            } else {
                dash = true;
            }
        }
    }
    if out.len() > SLUG_MAX {
        out.truncate(SLUG_MAX);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

/// Best-effort Latin accent folding; `None` keeps the character as is
/// (non-ASCII characters then become separators).
fn fold_char(c: char) -> Option<&'static str> {
    let s = match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' => "a",
        'ç' | 'ć' | 'č' | 'Ç' | 'Ć' | 'Č' => "c",
        'ď' | 'đ' | 'Ď' | 'Đ' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ė' | 'Ę' | 'Ě' => "e",
        'ğ' | 'Ğ' => "g",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' | 'Ì' | 'Í' | 'Î' | 'Ï' | 'Ī' | 'Į' | 'İ' => {
            "i"
        }
        'ł' | 'ľ' | 'ĺ' | 'Ł' | 'Ľ' | 'Ĺ' => "l",
        'ñ' | 'ń' | 'ň' | 'Ñ' | 'Ń' | 'Ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => "o",
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ő' => "o",
        'ŕ' | 'ř' | 'Ŕ' | 'Ř' => "r",
        'ś' | 'š' | 'ş' | 'ș' | 'Ś' | 'Š' | 'Ş' | 'Ș' => "s",
        'ť' | 'ţ' | 'ț' | 'Ť' | 'Ţ' | 'Ț' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => "u",
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ū' | 'Ů' | 'Ű' | 'Ų' => "u",
        'ý' | 'ÿ' | 'Ý' | 'Ÿ' => "y",
        'ź' | 'ż' | 'ž' | 'Ź' | 'Ż' | 'Ž' => "z",
        'ß' => "ss",
        'æ' | 'Æ' => "ae",
        'œ' | 'Œ' => "oe",
        'þ' | 'Þ' => "th",
        'ð' | 'Ð' => "d",
        _ => return None,
    };
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_folds_and_dashes() {
        assert_eq!(slugify("Roadmap review"), "roadmap-review");
        assert_eq!(
            slugify("  Reunião de Coração: Ação!  "),
            "reuniao-de-coracao-acao"
        );
        assert_eq!(slugify("Straße & Æsir"), "strasse-aesir");
        assert_eq!(slugify("会議"), "");
        assert_eq!(slugify("---"), "");
        let long = slugify(&"word ".repeat(30));
        assert!(long.len() <= SLUG_MAX && !long.ends_with('-'));
    }

    #[test]
    fn dir_name_layout() {
        let t = DateTime::parse_from_rfc3339("2026-11-13T14:05:00-03:00").unwrap();
        let id = MeetingId::parse("m-7f3a2c").unwrap();
        assert_eq!(
            meeting_dir_name(&t, Some("Roadmap review"), &id),
            "2026-11-13-1405-roadmap-review-7f3a2c"
        );
        assert_eq!(
            meeting_dir_name(&t, None, &id),
            "2026-11-13-1405-untitled-7f3a2c"
        );
        assert_eq!(
            meeting_dir_name(&t, Some("会議"), &id),
            "2026-11-13-1405-untitled-7f3a2c"
        );
        assert_eq!(
            meeting_rel_dir(&t, None, &id),
            PathBuf::from("2026/11/2026-11-13-1405-untitled-7f3a2c")
        );
    }
}
