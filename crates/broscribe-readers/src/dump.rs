//! Privacy-preserving AX structure dumps for writing reader rules. Never
//! serialize a raw AX tree: only structure, AX roles and hashed identifiers
//! leave this module; names, captions, titles and other text never do.
use broscribe_core::schema::sha256_hex;
use broscribe_platform::{
    AppUiReader, AxIncomplete, AxLimits, AxNode, AxTree, MicUser, WindowInfo,
};
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DumpSummary {
    pub nodes: usize,
    /// What cut the snapshot short; [`AxIncomplete::names`] spells it.
    pub incomplete: AxIncomplete,
}

impl DumpSummary {
    pub fn truncated(&self) -> bool {
        self.incomplete.any()
    }
}

/// Snapshot `pid` through the supplied platform reader and return only the
/// redacted shape. Never enables Chromium accessibility or prompts for
/// permissions.
pub fn dump_snapshot(
    reader: &dyn AppUiReader,
    pid: u32,
) -> broscribe_core::Result<(String, DumpSummary)> {
    let tree = reader.snapshot(pid, AxLimits::default())?;
    Ok((redacted_tree(&tree), summary(&tree)))
}

fn summary(tree: &AxTree) -> DumpSummary {
    DumpSummary {
        nodes: tree.root.count(),
        incomplete: tree.incomplete,
    }
}

/// The process to dump for a native app identity (macOS bundle id, Linux
/// application id / binary name, Windows executable file name), compared
/// ASCII case-insensitively: a microphone user (its responsible process for
/// helpers) first, then a window owner.
pub fn find_pid(app: &str, mic_users: &[MicUser], windows: &[WindowInfo]) -> Option<u32> {
    let same = |id: Option<&str>| id.is_some_and(|id| id.eq_ignore_ascii_case(app));
    mic_users
        .iter()
        .find_map(|u| {
            if same(u.responsible_bundle_id.as_deref()) {
                u.responsible_pid
            } else if same(u.bundle_id.as_deref()) {
                Some(u.responsible_pid.unwrap_or(u.pid))
            } else {
                None
            }
        })
        .or_else(|| {
            windows
                .iter()
                .find(|w| same(w.bundle_id.as_deref()))
                .map(|w| w.pid)
        })
}

/// AX constants (`AXGroup`, `AXSecureTextField`) are structural; anything
/// else in a role slot is treated as text.
fn ax_constant(value: &str) -> Option<&str> {
    (value.starts_with("AX") && value.chars().all(|c| c.is_ascii_alphanumeric())).then_some(value)
}

/// The redacted tree under a header line: `truncated=false`, or
/// `truncated=true incomplete=<reason>,…` naming what cut it short.
pub fn redacted_tree(tree: &AxTree) -> String {
    let mut out = format!("truncated={}", tree.truncated());
    if tree.truncated() {
        let _ = write!(out, " incomplete={}", tree.incomplete.names().join(","));
    }
    out.push('\n');
    fn walk(node: &AxNode, depth: usize, out: &mut String) {
        let _ = write!(
            out,
            "{}{}",
            "  ".repeat(depth),
            ax_constant(&node.role).unwrap_or("<role>")
        );
        if let Some(subrole) = &node.subrole {
            let _ = write!(out, " subrole={}", ax_constant(subrole).unwrap_or("<text>"));
        }
        // Text attributes: presence and length only, never content or a hash
        // (a hash of a short name is reversible by guessing).
        for (key, value) in [
            ("title", node.title.as_deref()),
            ("value", node.value.as_deref()),
            ("description", node.description.as_deref()),
        ] {
            if let Some(value) = value {
                let _ = write!(out, " {key}[len={}]", value.chars().count());
            }
        }
        // Identifiers and classes are hashed so a rule author can compare a
        // candidate selector string without the dump revealing it.
        let ids = node
            .identifier
            .iter()
            .map(|id| ("identifier", id))
            .chain(node.classes.iter().map(|class| ("class", class)));
        for (key, value) in ids {
            let _ = write!(out, " {key}[sha256={}]", sha256_hex(value.as_bytes()));
        }
        out.push('\n');
        for child in &node.children {
            walk(child, depth + 1, out);
        }
    }
    walk(&tree.root, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> AxTree {
        AxTree {
            root: AxNode {
                title: Some("Invented private title".into()),
                description: Some("Invented display name".into()),
                identifier: Some("synthetic-dom-id".into()),
                classes: vec!["secret-class".into()],
                children: vec![AxNode {
                    value: Some("Synthetic caption words".into()),
                    subrole: Some("Invented subrole text".into()),
                    children: vec![AxNode::new("Invented role text")],
                    ..AxNode::new("AXStaticText")
                }],
                ..AxNode::new("AXWindow")
            },
            incomplete: Default::default(),
        }
    }

    #[test]
    fn dump_contains_structure_roles_and_hashed_ids_but_no_text() {
        let output = redacted_tree(&fixture());
        for text in [
            "Invented private title",
            "Invented display name",
            "Synthetic caption words",
            "Invented",
            "Synthetic",
            "synthetic-dom-id",
            "secret-class",
        ] {
            assert!(!output.contains(text), "{text} leaked");
        }
        // Not even a guessable digest of text values.
        for text in [
            "Invented private title",
            "Invented display name",
            "Synthetic caption words",
        ] {
            assert!(!output.contains(&sha256_hex(text.as_bytes())));
        }
        assert!(output.contains(&sha256_hex(b"synthetic-dom-id")));
        assert!(output.starts_with("truncated=false\nAXWindow title[len=22]"));
        assert!(output.contains("\n  AXStaticText subrole=<text> value[len=23]\n"));
        assert!(output.contains("\n    <role>\n"));
    }

    #[test]
    fn record_auto_stop_document_identity_never_reaches_dumps_or_debug() {
        let tree = AxTree {
            root: AxNode {
                document: Some(broscribe_platform::DocumentContext::new(
                    Some("https://meet.google.com/abc-defg-hij".into()),
                    Some("mac-doc-41".into()),
                    false,
                )),
                ..AxNode::new("AXWebArea")
            },
            incomplete: Default::default(),
        };
        let output = redacted_tree(&tree);
        let debug = format!("{tree:?}");
        for secret in ["meet.google.com", "abc-defg-hij", "mac-doc-41"] {
            assert!(!output.contains(secret), "{secret} in dump");
            assert!(!debug.contains(secret), "{secret} in debug output");
        }
    }

    #[test]
    fn dump_snapshot_reports_node_count_and_incomplete_reasons_through_the_reader() {
        let reader = broscribe_platform::fakes::FakeAppUiReader::default();
        reader.set_tree(4242, fixture());
        let (text, summary) = dump_snapshot(&reader, 4242).unwrap();
        assert_eq!(
            summary,
            DumpSummary {
                nodes: 3,
                incomplete: AxIncomplete::default(),
            }
        );
        assert!(!summary.truncated());
        assert!(text.starts_with("truncated=false\n"));
        assert!(!text.contains("Synthetic"));

        let mut cut = fixture();
        cut.incomplete.read_failed = true;
        cut.incomplete.clipped = true;
        reader.set_tree(4242, cut);
        let (text, summary) = dump_snapshot(&reader, 4242).unwrap();
        assert!(summary.truncated());
        assert_eq!(summary.incomplete.names(), ["read_failed", "clipped"]);
        assert!(text.starts_with("truncated=true incomplete=read_failed,clipped\nAXWindow"));
    }

    #[test]
    fn app_identity_selects_mic_user_responsible_process_then_window_owner() {
        let user = |pid, own: &str, responsible: Option<(u32, &str)>| MicUser {
            pid,
            bundle_id: Some(own.into()),
            responsible_pid: responsible.map(|r| r.0),
            responsible_bundle_id: responsible.map(|r| r.1.into()),
        };
        let users = [
            user(
                11,
                "com.google.Chrome.helper",
                Some((10, "com.google.Chrome")),
            ),
            user(20, "MS-Teams.exe", None),
        ];
        let windows = [WindowInfo {
            pid: 30,
            app: "Synthetic".into(),
            bundle_id: Some("zoom".into()),
            title: None,
        }];
        assert_eq!(find_pid("com.google.chrome", &users, &windows), Some(10));
        assert_eq!(find_pid("ms-teams.exe", &users, &windows), Some(20));
        assert_eq!(find_pid("zoom", &users, &windows), Some(30));
        assert_eq!(find_pid("slack", &users, &windows), None);
    }
}
