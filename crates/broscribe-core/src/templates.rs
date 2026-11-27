//! Meeting templates (design §8.4): named instructions that turn a meeting's
//! transcript into one Markdown document (notes, an outline, a follow-up
//! e-mail draft, sales-qualification insights…).
//!
//! - A definition is a file `<data_dir>/templates/<id>.md`: TOML front
//!   matter between `+++` lines (`name`, `description`), then the Markdown
//!   body, which is the instructions.
//! - Built-ins ship in the binary; every text is BroScribe's own. A user file
//!   with a built-in's ID overrides it, and [`reset`] deletes that file.
//! - IDs are `[a-z0-9-]{1,40}`; bodies at most [`BODY_MAX_CHARS`] characters.
//! - Which templates run on their own is config (`[templates] auto`), never
//!   part of a definition, so a reset never changes it.

use std::fs;
use std::io::{ErrorKind, Read as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, fsutil};

/// Folder of user template files inside the data folder.
pub const TEMPLATES_DIR: &str = "templates";
/// Longest template ID.
pub const ID_MAX_CHARS: usize = 40;
/// Longest template body (the instructions).
pub const BODY_MAX_CHARS: usize = 8_000;
/// Longest name.
pub const NAME_MAX_CHARS: usize = 80;
/// Longest description.
pub const DESCRIPTION_MAX_CHARS: usize = 300;
/// Largest user template file read. [`save_user`] caps every field in
/// characters, so the files it writes stay far below this; a larger file
/// could never have been saved and is refused without being read whole.
const USER_FILE_MAX_BYTES: u64 = 1 << 20;

/// One template definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub description: String,
    /// The instructions (Markdown).
    pub body: String,
    /// The ID is one of the built-ins.
    pub builtin: bool,
    /// A user file overrides the built-in of this ID.
    pub overridden: bool,
}

struct Builtin {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    body: &'static str,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "meeting-notes",
        name: "Meeting notes",
        description: "Structured notes: context, discussion points, decisions and next steps.",
        body: "\
Write clear notes of this meeting for someone who took part and wants a record to come back to.

Use these sections, leaving out any that would be empty:
## Context
One or two sentences on why the meeting happened and who took part.
## Discussion
The main points discussed, grouped by subject, as short bullet points. Keep figures, names of products and dates exactly as said.
## Decisions
What was agreed, one bullet each.
## Next steps
Follow-up tasks as bullets: the task, who owns it when that is clear, and the date when one was given.

Be concise and factual. Do not add anything the transcript does not support.",
    },
    Builtin {
        id: "meeting-outline",
        name: "Meeting outline",
        description: "The meeting's flow as a timestamped outline of topics.",
        body: "\
Produce an outline of how the meeting unfolded, in time order.

- One top-level bullet per topic, starting with the time it began as [hh:mm:ss], then a short title.
- Under each topic, two to five nested bullets with the key points, questions raised and answers given.
- Use the timestamps from the transcript lines; never invent times.

Keep each bullet to one line. No introduction or conclusion.",
    },
    Builtin {
        id: "follow-up-email",
        name: "Follow-up e-mail",
        description: "A draft e-mail to the participants recapping outcomes and next steps.",
        body: "\
Draft a follow-up e-mail the user could send to the other participants after this meeting.

- Start with a subject line: `Subject: …`.
- A short friendly opening that thanks people for their time.
- A recap of the main outcomes in two to four bullets.
- The agreed next steps as bullets, each with its owner and date when known.
- A closing line inviting corrections or questions.

Write in a professional, warm tone. Keep it under 250 words. Refer to people with the placeholders given; do not sign with a name.",
    },
    Builtin {
        id: "bant",
        name: "BANT insights",
        description: "Sales qualification: Budget, Authority, Need and Timeline.",
        body: "\
Analyse this sales conversation with the BANT qualification framework.

For each of these sections, summarise what the conversation revealed, quoting short phrases where they help, and say plainly when it revealed nothing:
## Budget
Money available or discussed, pricing reactions, budget cycles.
## Authority
Who decides or influences the purchase, and how the decision gets approved.
## Need
The problem the prospect wants solved, its impact, and how urgent it is for them.
## Timeline
When they intend to decide or start, and any deadlines driving it.

End with:
## Gaps and next questions
The most important unknowns and the questions that would close them.",
    },
    Builtin {
        id: "meddic",
        name: "MEDDIC insights",
        description: "Sales qualification: Metrics, Economic buyer, Decision criteria and process, Pain, Champion.",
        body: "\
Analyse this sales conversation with the MEDDIC qualification framework.

For each section, summarise the evidence from the conversation and say plainly when there is none:
## Metrics
Measurable outcomes the prospect expects or uses to judge success.
## Economic buyer
Who controls the budget and signs off, and whether they were involved.
## Decision criteria
What the prospect will compare options on.
## Decision process
The steps, people and timing involved in reaching a decision.
## Identify pain
The business problem behind the interest and its consequences.
## Champion
Who inside the prospect's organisation supports this and why.

End with:
## Risks and next steps
Weak or missing elements and what to do about each.",
    },
];

fn builtin(id: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.id == id)
}

/// Whether `id` is one of the built-in templates.
pub fn is_builtin(id: &str) -> bool {
    builtin(id).is_some()
}

/// `invalid_input` unless `id` is 1–[`ID_MAX_CHARS`] characters of `a-z`,
/// `0-9` and `-`.
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > ID_MAX_CHARS
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Error::InvalidInput(format!(
            "template ID {id:?} must be 1–{ID_MAX_CHARS} characters of a-z, 0-9 and -"
        )));
    }
    Ok(())
}

/// Checks a definition's fields: a non-blank name, lengths, and a non-blank
/// body without control characters other than newlines and tabs.
fn validate_fields(name: &str, description: &str, body: &str) -> Result<()> {
    let bad = |m: String| Err(Error::InvalidInput(m));
    if name.trim().is_empty() {
        return bad("a template needs a name".into());
    }
    if name.chars().count() > NAME_MAX_CHARS || name.contains(['\n', '\r']) {
        return bad(format!(
            "a template name is one line of at most {NAME_MAX_CHARS} characters"
        ));
    }
    if description.chars().count() > DESCRIPTION_MAX_CHARS {
        return bad(format!(
            "a template description has at most {DESCRIPTION_MAX_CHARS} characters"
        ));
    }
    if body.trim().is_empty() {
        return bad("a template needs instructions".into());
    }
    if body.chars().count() > BODY_MAX_CHARS {
        return bad(format!(
            "template instructions have at most {BODY_MAX_CHARS} characters"
        ));
    }
    if [name, description, body].iter().any(|t| {
        t.chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    }) {
        return bad("a template contains control characters".into());
    }
    Ok(())
}

/// The folder of user template files.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(TEMPLATES_DIR)
}

fn file(data_dir: &Path, id: &str) -> PathBuf {
    dir(data_dir).join(format!("{id}.md"))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    name: String,
    #[serde(default)]
    description: String,
}

/// Parses a template file: `+++` TOML front matter (`name`, `description`)
/// then the body. A text without front matter is a body only (name and
/// description empty), as `templates new <id> --stdin` accepts. Returns
/// (name, description, body); fields are not validated here.
pub fn parse_file(text: &str) -> Result<(String, String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("+++\n")
        .or_else(|| text.strip_prefix("+++\r\n"))
    else {
        return Ok((String::new(), String::new(), text.trim().to_owned()));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "+++" {
            let fm: FrontMatter = toml::from_str(&rest[..offset])
                .map_err(|e| Error::InvalidInput(format!("template front matter: {e}")))?;
            let body = rest[offset + line.len()..].trim().to_owned();
            return Ok((
                fm.name.trim().to_owned(),
                fm.description.trim().to_owned(),
                body,
            ));
        }
        offset += line.len();
    }
    Err(Error::InvalidInput(
        "template front matter is not closed with +++".into(),
    ))
}

/// A template file's text.
pub fn render_file(name: &str, description: &str, body: &str) -> Result<String> {
    let fm = toml::to_string(&FrontMatter {
        name: name.to_owned(),
        description: description.to_owned(),
    })
    .map_err(|e| Error::Internal(format!("serialising template front matter: {e}")))?;
    Ok(format!("+++\n{fm}+++\n\n{}\n", body.trim()))
}

/// The user file of `id` as a template, if any: parsed, given its canonical
/// attributes (a file without a name keeps the built-in's, else its ID), then
/// validated exactly as [`save_user`] validates before writing, so a file
/// edited outside BroScribe is never used unless it could have been saved
/// (`invalid_input`, naming the path). At most [`USER_FILE_MAX_BYTES`] of it
/// are read.
fn read_user(data_dir: &Path, id: &str) -> Result<Option<Template>> {
    let path = file(data_dir, id);
    let invalid = |e: Error| Error::InvalidInput(format!("{}: {e}", path.display()));
    let source = match fs::File::open(&path) {
        Ok(source) => source,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io_at(&path, e)),
    };
    let mut bytes = Vec::new();
    source
        .take(USER_FILE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::io_at(&path, e))?;
    if bytes.len() as u64 > USER_FILE_MAX_BYTES {
        return Err(invalid(Error::InvalidInput(format!(
            "a template file has at most {USER_FILE_MAX_BYTES} bytes"
        ))));
    }
    let text = String::from_utf8(bytes)
        .map_err(|e| Error::io_at(&path, std::io::Error::new(ErrorKind::InvalidData, e)))?;
    let template = from_user(id, parse_file(&text).map_err(invalid)?);
    validate_fields(&template.name, &template.description, &template.body).map_err(invalid)?;
    Ok(Some(template))
}

fn from_user(id: &str, (name, description, body): (String, String, String)) -> Template {
    let b = builtin(id);
    Template {
        id: id.to_owned(),
        // A user file without a name keeps the built-in's, else its ID.
        name: if name.is_empty() {
            b.map_or_else(|| id.to_owned(), |b| b.name.to_owned())
        } else {
            name
        },
        description,
        body,
        builtin: b.is_some(),
        overridden: b.is_some(),
    }
}

fn from_builtin(b: &Builtin) -> Template {
    Template {
        id: b.id.to_owned(),
        name: b.name.to_owned(),
        description: b.description.to_owned(),
        body: b.body.to_owned(),
        builtin: true,
        overridden: false,
    }
}

/// Every template: built-ins in their fixed order (each replaced by its
/// override), then user templates by ID. Unreadable user files are skipped
/// with a warning (their names never reach the log).
pub fn list(data_dir: &Path) -> Result<Vec<Template>> {
    let mut out = Vec::new();
    for b in BUILTINS {
        out.push(match read_user(data_dir, b.id) {
            Ok(Some(user)) => user,
            Ok(None) => from_builtin(b),
            Err(e) => {
                tracing::warn!("skipping the override of template {}: {}", b.id, e.code());
                from_builtin(b)
            }
        });
    }
    let entries = match fs::read_dir(dir(data_dir)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(Error::io_at(&dir(data_dir), e)),
    };
    let mut ids: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(&dir(data_dir), e))?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".md")) else {
            continue;
        };
        if validate_id(id).is_ok() && !is_builtin(id) {
            ids.push(id.to_owned());
        }
    }
    ids.sort();
    for id in ids {
        match read_user(data_dir, &id) {
            Ok(Some(user)) => out.push(user),
            Ok(None) => {}
            Err(e) => tracing::warn!("skipping template {id}: {}", e.code()),
        }
    }
    Ok(out)
}

/// Template `id` (its override when there is one); `not_found` otherwise.
pub fn get(data_dir: &Path, id: &str) -> Result<Template> {
    validate_id(id)?;
    match read_user(data_dir, id)? {
        Some(user) => Ok(user),
        None => builtin(id)
            .map(from_builtin)
            .ok_or_else(|| Error::NotFound(format!("template {id}"))),
    }
}

/// Writes the user file of `id` (creating a template, editing one, or
/// overriding a built-in), after validating every field. Returns the
/// template as now defined.
pub fn save_user(
    data_dir: &Path,
    id: &str,
    name: &str,
    description: &str,
    body: &str,
) -> Result<Template> {
    validate_id(id)?;
    let (name, description, body) = (name.trim(), description.trim(), body.trim());
    validate_fields(name, description, body)?;
    let d = dir(data_dir);
    fs::create_dir_all(&d).map_err(|e| Error::io_at(&d, e))?;
    fsutil::write_atomic(
        &file(data_dir, id),
        render_file(name, description, body)?.as_bytes(),
    )?;
    get(data_dir, id)
}

/// Deletes user template `id`. A built-in cannot be deleted (`invalid_input`;
/// use [`reset`] for its override); `not_found` when no such file exists.
pub fn delete_user(data_dir: &Path, id: &str) -> Result<()> {
    validate_id(id)?;
    if is_builtin(id) {
        return Err(Error::InvalidInput(format!(
            "template {id} is built in and cannot be deleted; reset it to undo changes"
        )));
    }
    remove(data_dir, id)?
        .then_some(())
        .ok_or_else(|| Error::NotFound(format!("template {id}")))
}

/// Restores built-in `id` by deleting its override (nothing to do without
/// one); `invalid_input` for a template that is not built in.
pub fn reset(data_dir: &Path, id: &str) -> Result<()> {
    validate_id(id)?;
    if !is_builtin(id) {
        return Err(Error::InvalidInput(format!(
            "template {id} is not built in; only built-in templates can be reset"
        )));
    }
    remove(data_dir, id).map(|_| ())
}

fn remove(data_dir: &Path, id: &str) -> Result<bool> {
    let path = file(data_dir, id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_at(&path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_are_listed_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let ids: Vec<String> = list(dir.path())
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            ids,
            [
                "meeting-notes",
                "meeting-outline",
                "follow-up-email",
                "bant",
                "meddic"
            ]
        );
        for t in list(dir.path()).unwrap() {
            assert!(t.builtin && !t.overridden);
            validate_fields(&t.name, &t.description, &t.body).unwrap();
        }
    }

    #[test]
    fn user_templates_override_and_reset() {
        let dir = tempfile::tempdir().unwrap();
        let t = save_user(dir.path(), "bant", "BANT, short", "", "Only budget.").unwrap();
        assert!(t.builtin && t.overridden);
        assert_eq!(get(dir.path(), "bant").unwrap().body, "Only budget.");
        assert_eq!(
            delete_user(dir.path(), "bant").unwrap_err().code(),
            "invalid_input"
        );
        reset(dir.path(), "bant").unwrap();
        let back = get(dir.path(), "bant").unwrap();
        assert!(!back.overridden && back.body.contains("Budget"));
        // Reset without an override is a no-op.
        reset(dir.path(), "bant").unwrap();

        let mine = save_user(dir.path(), "retro", "Retro", "Team retro", "List wins.").unwrap();
        assert!(!mine.builtin);
        let all = list(dir.path()).unwrap();
        assert_eq!(all.last().unwrap().id, "retro");
        assert_eq!(
            reset(dir.path(), "retro").unwrap_err().code(),
            "invalid_input"
        );
        delete_user(dir.path(), "retro").unwrap();
        assert_eq!(get(dir.path(), "retro").unwrap_err().code(), "not_found");
        assert_eq!(
            delete_user(dir.path(), "retro").unwrap_err().code(),
            "not_found"
        );
    }

    #[test]
    fn ids_and_sizes_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["", "Upper", "a b", "../x", "a_b", &"a".repeat(41)] {
            let e = save_user(dir.path(), bad, "N", "", "Body").unwrap_err();
            assert_eq!(e.code(), "invalid_input", "{bad:?}");
        }
        save_user(dir.path(), &"a".repeat(40), "N", "", "Body").unwrap();
        let long = "x".repeat(BODY_MAX_CHARS + 1);
        assert_eq!(
            save_user(dir.path(), "big", "N", "", &long)
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        save_user(dir.path(), "big", "N", "", &long[1..]).unwrap();
        assert_eq!(
            save_user(dir.path(), "blank", " ", "", "Body")
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        assert_eq!(
            save_user(dir.path(), "empty", "N", "", "  ")
                .unwrap_err()
                .code(),
            "invalid_input"
        );
    }

    #[test]
    fn files_round_trip_with_front_matter() {
        let text = render_file("Weekly \"sync\"", "Notes + actions", "# Do\nThis.").unwrap();
        assert!(text.starts_with("+++\n"));
        assert_eq!(
            parse_file(&text).unwrap(),
            (
                "Weekly \"sync\"".into(),
                "Notes + actions".into(),
                "# Do\nThis.".into()
            )
        );
        assert_eq!(
            parse_file("Just a body\n").unwrap(),
            (String::new(), String::new(), "Just a body".into())
        );
        assert_eq!(
            parse_file("+++\nname = 'x'\n").unwrap_err().code(),
            "invalid_input"
        );
        assert_eq!(
            parse_file("+++\nauto = true\nname = 'x'\n+++\nb")
                .unwrap_err()
                .code(),
            "invalid_input"
        );
    }

    #[test]
    fn an_unreadable_user_file_is_skipped_in_the_list() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(super::dir(dir.path())).unwrap();
        fs::write(file(dir.path(), "broken"), "+++\nname = \n").unwrap();
        let ids: Vec<String> = list(dir.path())
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(!ids.contains(&"broken".to_owned()));
        assert_eq!(
            get(dir.path(), "broken").unwrap_err().code(),
            "invalid_input"
        );
    }

    #[test]
    fn externally_edited_files_are_validated_on_read_as_on_write() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(super::dir(dir.path())).unwrap();
        let oversized = format!(
            "+++\nname = 'Big'\n+++\n\n{}",
            "x".repeat(BODY_MAX_CHARS + 1)
        );
        let long_name = format!(
            "+++\nname = '{}'\n+++\n\nBody",
            "n".repeat(NAME_MAX_CHARS + 1)
        );
        let cases = [
            ("bell", "+++\nname = 'Bell'\n+++\n\nRing \u{7} now."),
            ("escape", "+++\nname = \"Esc\\u001b[2J\"\n+++\n\nBody"),
            (
                "two-lines",
                "+++\nname = \"\"\"Two\nlines\"\"\"\n+++\n\nBody",
            ),
            ("no-body", "+++\nname = 'Empty'\n+++\n"),
            ("big", oversized.as_str()),
            ("long-name", long_name.as_str()),
            // An override of a built-in is held to the same rules.
            ("bant", "+++\nname = 'BANT'\n+++\n\nBad \u{0} body"),
        ];
        for (id, text) in cases {
            fs::write(file(dir.path(), id), text).unwrap();
            assert_eq!(
                get(dir.path(), id).unwrap_err().code(),
                "invalid_input",
                "{id}"
            );
        }
        let all = list(dir.path()).unwrap();
        for (id, _) in cases {
            assert!(all.iter().all(|t| t.id != id || t.builtin), "{id}");
        }
        // The broken override leaves the built-in in the list, unchanged.
        let bant = all.iter().find(|t| t.id == "bant").unwrap();
        assert!(!bant.overridden && bant.body.contains("Budget"));
        // Every listed template would pass `save_user`'s checks.
        for t in &all {
            validate_fields(&t.name, &t.description, &t.body).unwrap();
        }
    }

    #[test]
    fn a_body_only_file_keeps_its_canonical_attributes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(super::dir(dir.path())).unwrap();
        let builtin_name = get(dir.path(), "bant").unwrap().name;
        fs::write(file(dir.path(), "bant"), "My own BANT.\n").unwrap();
        let bant = get(dir.path(), "bant").unwrap();
        assert_eq!(bant.name, builtin_name);
        assert_eq!(bant.body, "My own BANT.");
        assert!(bant.builtin && bant.overridden);
        assert_eq!(list(dir.path()).unwrap()[3], bant);
        fs::write(file(dir.path(), "retro"), "List wins.").unwrap();
        let retro = get(dir.path(), "retro").unwrap();
        assert_eq!((retro.name.as_str(), retro.builtin), ("retro", false));
    }

    #[test]
    fn a_user_file_larger_than_any_saved_template_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // The largest definition `save_user` accepts, in 4-byte characters,
        // is still read back.
        let wide = |n: usize| "\u{1d11e}".repeat(n);
        let biggest = save_user(
            dir.path(),
            "biggest",
            &wide(NAME_MAX_CHARS),
            &wide(DESCRIPTION_MAX_CHARS),
            &wide(BODY_MAX_CHARS),
        )
        .unwrap();
        assert_eq!(get(dir.path(), "biggest").unwrap(), biggest);
        // Valid once trimmed, but larger than any file `save_user` writes.
        let padded = format!(
            "+++\nname = 'Padded'\n+++\n\nList wins.{}",
            "\n".repeat(USER_FILE_MAX_BYTES as usize)
        );
        for id in ["padded", "bant"] {
            fs::write(file(dir.path(), id), &padded).unwrap();
            assert_eq!(
                get(dir.path(), id).unwrap_err().code(),
                "invalid_input",
                "{id}"
            );
        }
        let all = list(dir.path()).unwrap();
        assert!(all.iter().all(|t| t.id != "padded"));
        // The refused override leaves the built-in in the list.
        let bant = all.iter().find(|t| t.id == "bant").unwrap();
        assert!(!bant.overridden && bant.body.contains("Budget"));
        assert!(all.contains(&biggest));
    }
}
