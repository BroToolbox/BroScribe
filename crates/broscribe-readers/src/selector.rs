//! A small CSS-like selector language over an [`AxNode`] tree, so reader
//! rules stay data (design §9).
//!
//! ```text
//! AXWindow > AXGroup[subrole="AXLandmarkRegion"] AXStaticText[value^="Talking"]
//! ```
//!
//! - A step is a role (`AXGroup`) or `*`, followed by any number of
//!   attribute tests `[attr op "value"]` and class tests `.name`.
//! - Attributes: `role`, `subrole`, `title`, `value`, `description`,
//!   `identifier`, `class`. Operators: `=` equals, `^=` starts with, `$=`
//!   ends with, `*=` contains, `~=` one whitespace-separated word equals;
//!   `[attr]` alone means "present and non-empty". Every comparison ignores
//!   case.
//! - Steps are joined by whitespace (descendant) or `>` (child).
//!
//! Rules use structural anchors (roles, subroles, identifiers, landmark
//! descriptions) rather than meeting content.

use std::fmt;

use broscribe_core::{Error, Result};
use broscribe_platform::AxNode;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attr {
    Role,
    Subrole,
    Title,
    Value,
    Description,
    Identifier,
    Class,
}

impl Attr {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "role" => Self::Role,
            "subrole" => Self::Subrole,
            "title" => Self::Title,
            "value" => Self::Value,
            "description" => Self::Description,
            "identifier" | "id" => Self::Identifier,
            "class" => Self::Class,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Role => "role",
            Self::Subrole => "subrole",
            Self::Title => "title",
            Self::Value => "value",
            Self::Description => "description",
            Self::Identifier => "identifier",
            Self::Class => "class",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Present,
    Equals,
    Prefix,
    Suffix,
    Contains,
    Word,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Test {
    attr: Attr,
    op: Op,
    /// Lowercased.
    value: String,
}

impl Test {
    fn check(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        match self.op {
            Op::Present => !text.trim().is_empty(),
            Op::Equals => text == self.value,
            Op::Prefix => text.starts_with(&self.value),
            Op::Suffix => text.ends_with(&self.value),
            Op::Contains => text.contains(&self.value),
            Op::Word => text.split_whitespace().any(|w| w == self.value),
        }
    }

    fn matches(&self, node: &AxNode) -> bool {
        let one = |v: Option<&String>| v.is_some_and(|s| self.check(s));
        match self.attr {
            Attr::Role => self.check(&node.role),
            Attr::Subrole => one(node.subrole.as_ref()),
            Attr::Title => one(node.title.as_ref()),
            Attr::Value => one(node.value.as_ref()),
            Attr::Description => one(node.description.as_ref()),
            Attr::Identifier => one(node.identifier.as_ref()),
            Attr::Class => node.classes.iter().any(|c| self.check(c)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Combinator {
    Descendant,
    Child,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Step {
    /// How this step relates to the previous one (ignored on the first).
    combinator: Combinator,
    /// `None` for `*`.
    role: Option<String>,
    tests: Vec<Test>,
}

impl Step {
    fn matches(&self, node: &AxNode) -> bool {
        self.role.as_deref().is_none_or(|r| r == node.role)
            && self.tests.iter().all(|t| t.matches(node))
    }
}

/// A parsed selector; deserializes from its text form.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Selector {
    source: String,
    steps: Vec<Step>,
}

impl fmt::Debug for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Selector({:?})", self.source)
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl TryFrom<String> for Selector {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        Self::parse(&s)
    }
}

impl From<Selector> for String {
    fn from(s: Selector) -> String {
        s.source
    }
}

fn invalid(source: &str, why: &str) -> Error {
    Error::InvalidInput(format!("selector {source:?}: {why}"))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

impl Selector {
    pub fn parse(source: &str) -> Result<Self> {
        let chars: Vec<char> = source.chars().collect();
        let mut i = 0;
        let mut steps: Vec<Step> = Vec::new();
        let mut pending = Combinator::Descendant;
        loop {
            let mut saw_space = false;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
                saw_space = true;
            }
            if i >= chars.len() {
                break;
            }
            if chars[i] == '>' {
                if steps.is_empty() {
                    return Err(invalid(source, "starts with '>'"));
                }
                pending = Combinator::Child;
                i += 1;
                continue;
            }
            if !steps.is_empty() && !saw_space && pending == Combinator::Descendant {
                return Err(invalid(source, "steps must be separated"));
            }
            let mut step = Step {
                combinator: pending,
                role: None,
                tests: Vec::new(),
            };
            pending = Combinator::Descendant;
            // Role or `*`.
            if chars[i] == '*' {
                i += 1;
            } else if is_name_char(chars[i]) {
                let start = i;
                while i < chars.len() && is_name_char(chars[i]) {
                    i += 1;
                }
                step.role = Some(chars[start..i].iter().collect());
            } else if chars[i] != '[' && chars[i] != '.' {
                return Err(invalid(source, &format!("unexpected {:?}", chars[i])));
            }
            // Attribute and class tests.
            while i < chars.len() && (chars[i] == '[' || chars[i] == '.') {
                if chars[i] == '.' {
                    i += 1;
                    let start = i;
                    while i < chars.len() && is_name_char(chars[i]) {
                        i += 1;
                    }
                    if start == i {
                        return Err(invalid(source, "empty class name"));
                    }
                    step.tests.push(Test {
                        attr: Attr::Class,
                        op: Op::Equals,
                        value: chars[start..i].iter().collect::<String>().to_lowercase(),
                    });
                    continue;
                }
                i += 1;
                let start = i;
                while i < chars.len() && is_name_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                let attr = Attr::parse(&name)
                    .ok_or_else(|| invalid(source, &format!("unknown attribute {name:?}")))?;
                if chars.get(i) == Some(&']') {
                    i += 1;
                    step.tests.push(Test {
                        attr,
                        op: Op::Present,
                        value: String::new(),
                    });
                    continue;
                }
                let op = match (chars.get(i), chars.get(i + 1)) {
                    (Some('='), _) => {
                        i += 1;
                        Op::Equals
                    }
                    (Some(c), Some('=')) => {
                        let op = match c {
                            '^' => Op::Prefix,
                            '$' => Op::Suffix,
                            '*' => Op::Contains,
                            '~' => Op::Word,
                            _ => return Err(invalid(source, &format!("unknown operator {c}="))),
                        };
                        i += 2;
                        op
                    }
                    _ => return Err(invalid(source, "expected an operator or ']'")),
                };
                let quote = match chars.get(i) {
                    Some(q @ ('"' | '\'')) => *q,
                    _ => return Err(invalid(source, "attribute values are quoted")),
                };
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(invalid(source, "unterminated string"));
                }
                let value: String = chars[start..i].iter().collect::<String>().to_lowercase();
                i += 1;
                if chars.get(i) != Some(&']') {
                    return Err(invalid(source, "expected ']'"));
                }
                i += 1;
                step.tests.push(Test { attr, op, value });
            }
            steps.push(step);
        }
        if steps.is_empty() {
            return Err(invalid(source, "empty"));
        }
        if pending == Combinator::Child {
            return Err(invalid(source, "ends with '>'"));
        }
        Ok(Self {
            source: source.to_owned(),
            steps,
        })
    }

    /// Whether the last node of `path` matches, given its ancestors (path
    /// from the outermost considered ancestor to the node).
    fn matches_path(&self, path: &[&AxNode]) -> bool {
        match path.len() {
            0 => false,
            n => self.match_from(self.steps.len() - 1, path, n - 1),
        }
    }

    fn match_from(&self, si: usize, path: &[&AxNode], pi: usize) -> bool {
        let step = &self.steps[si];
        if !step.matches(path[pi]) {
            return false;
        }
        if si == 0 {
            return true;
        }
        match step.combinator {
            Combinator::Child => pi > 0 && self.match_from(si - 1, path, pi - 1),
            Combinator::Descendant => (0..pi).rev().any(|a| self.match_from(si - 1, path, a)),
        }
    }

    /// Every node of the tree under `root` (itself included) that matches,
    /// in document order.
    pub fn select<'a>(&self, root: &'a AxNode) -> Vec<&'a AxNode> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        self.walk(root, &mut path, &mut out);
        out
    }

    /// Matching descendants of `scope` (not `scope` itself), with
    /// ancestors counted from `scope`'s children: selectors relative to a
    /// matched item.
    pub fn select_within<'a>(&self, scope: &'a AxNode) -> Vec<&'a AxNode> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        for child in &scope.children {
            self.walk(child, &mut path, &mut out);
        }
        out
    }

    /// Whether any node of the tree under `root` (itself included)
    /// matches; stops at the first match.
    pub fn any(&self, root: &AxNode) -> bool {
        let mut path = Vec::new();
        self.find(root, &mut path)
    }

    fn find<'a>(&self, node: &'a AxNode, path: &mut Vec<&'a AxNode>) -> bool {
        path.push(node);
        let found =
            self.matches_path(path) || node.children.iter().any(|child| self.find(child, path));
        path.pop();
        found
    }

    /// The roles this selector names (`*` steps name none).
    pub fn roles(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|step| step.role.as_deref())
    }

    /// The attributes this selector tests, by their selector name.
    pub fn attributes(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.steps
            .iter()
            .flat_map(|step| step.tests.iter().map(|test| test.attr.name()))
    }

    /// Whether the last step may select a web document (`*` or
    /// `AXWebArea`).
    pub(crate) fn may_select_document(&self) -> bool {
        self.steps
            .last()
            .is_some_and(|step| step.role.as_deref().is_none_or(|r| r == "AXWebArea"))
    }

    /// Whether a step that may match a web document tests its value: the
    /// rule then needs whole-document text.
    pub(crate) fn reads_document_value(&self) -> bool {
        self.steps.iter().any(|step| {
            step.role.as_deref().is_none_or(|r| r == "AXWebArea")
                && step.tests.iter().any(|test| test.attr == Attr::Value)
        })
    }

    fn walk<'a>(&self, node: &'a AxNode, path: &mut Vec<&'a AxNode>, out: &mut Vec<&'a AxNode>) {
        path.push(node);
        if self.matches_path(path) {
            out.push(node);
        }
        for child in &node.children {
            self.walk(child, path, out);
        }
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(role: &str) -> AxNode {
        AxNode::new(role)
    }

    fn with(mut node: AxNode, children: Vec<AxNode>) -> AxNode {
        node.children = children;
        node
    }

    fn text(value: &str) -> AxNode {
        AxNode {
            value: Some(value.into()),
            ..n("AXStaticText")
        }
    }

    /// App → window → [toolbar → button, region "Captions" → group → 2 texts].
    fn fixture() -> AxNode {
        let region = AxNode {
            subrole: Some("AXLandmarkRegion".into()),
            description: Some("Captions".into()),
            classes: vec!["caption-pane".into(), "wide".into()],
            ..n("AXGroup")
        };
        with(
            n("AXApplication"),
            vec![with(
                n("AXWindow"),
                vec![
                    with(
                        n("AXToolbar"),
                        vec![AxNode {
                            title: Some("Leave".into()),
                            ..n("AXButton")
                        }],
                    ),
                    with(
                        region,
                        vec![with(
                            n("AXGroup"),
                            vec![text("Alice Example"), text("hello")],
                        )],
                    ),
                ],
            )],
        )
    }

    fn values(nodes: &[&AxNode]) -> Vec<String> {
        nodes
            .iter()
            .map(|n| {
                n.value
                    .clone()
                    .or(n.title.clone())
                    .unwrap_or(n.role.clone())
            })
            .collect()
    }

    #[test]
    fn descendant_and_child() {
        let tree = fixture();
        let s = Selector::parse("AXWindow AXStaticText").unwrap();
        assert_eq!(values(&s.select(&tree)), ["Alice Example", "hello"]);
        let s = Selector::parse("AXWindow > AXStaticText").unwrap();
        assert!(s.select(&tree).is_empty());
        let s = Selector::parse("AXWindow > AXToolbar > AXButton").unwrap();
        assert_eq!(values(&s.select(&tree)), ["Leave"]);
        let s = Selector::parse("AXWindow>AXToolbar>*").unwrap();
        assert_eq!(values(&s.select(&tree)), ["Leave"]);
    }

    #[test]
    fn attribute_operators_ignore_case() {
        let tree = fixture();
        for (sel, want) in [
            (r#"AXGroup[subrole="axlandmarkregion"] AXStaticText"#, 2),
            (r#"AXGroup[description^="cap"] > AXGroup"#, 1),
            (r#"AXGroup[description$="IONS"]"#, 1),
            (r#"AXStaticText[value*="example"]"#, 1),
            (r#"AXStaticText[value~="alice"]"#, 1),
            (r#"AXStaticText[value~="ali"]"#, 0),
            (r#"AXButton[title]"#, 1),
            (r#"AXButton[description]"#, 0),
            (r#"AXGroup.caption-pane AXStaticText"#, 2),
            (r#"*[class="wide"]"#, 1),
            (r#"*.missing"#, 0),
            (r#"[role='AXButton']"#, 1),
        ] {
            let s = Selector::parse(sel).unwrap();
            assert_eq!(s.select(&tree).len(), want, "{sel}");
        }
    }

    #[test]
    fn select_within_is_relative() {
        let tree = fixture();
        let groups = Selector::parse(r#"AXGroup[subrole="AXLandmarkRegion"] > AXGroup"#)
            .unwrap()
            .select(&tree);
        assert_eq!(groups.len(), 1);
        let first = Selector::parse("AXStaticText").unwrap();
        assert_eq!(
            values(&first.select_within(groups[0])),
            ["Alice Example", "hello"]
        );
        // The scope's own ancestors are not part of the relative path.
        let anchored = Selector::parse("AXWindow AXStaticText").unwrap();
        assert!(anchored.select_within(groups[0]).is_empty());
    }

    #[test]
    fn any_agrees_with_select_and_introspection_names_the_vocabulary() {
        let tree = fixture();
        for sel in [
            "AXWindow AXStaticText",
            "AXWindow > AXStaticText",
            r#"AXGroup.caption-pane AXStaticText[value~="alice"]"#,
            r#"AXButton[title="Leave"]"#,
            "AXWebArea",
        ] {
            let s = Selector::parse(sel).unwrap();
            assert_eq!(s.any(&tree), !s.select(&tree).is_empty(), "{sel}");
        }
        let s = Selector::parse(r#"AXWebArea[value*="x"] > *.c AXButton[id="b"]"#).unwrap();
        assert_eq!(s.roles().collect::<Vec<_>>(), ["AXWebArea", "AXButton"]);
        assert_eq!(
            s.attributes().collect::<Vec<_>>(),
            ["value", "class", "identifier"]
        );
        assert!(s.reads_document_value() && !s.may_select_document());
        assert!(
            !Selector::parse(r#"AXStaticText[value="x"]"#)
                .unwrap()
                .reads_document_value()
        );
        assert!(Selector::parse("*").unwrap().may_select_document());
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "> AXGroup",
            "AXGroup >",
            "AXGroup[foo=\"x\"]",
            "AXGroup[title=x]",
            "AXGroup[title=\"x\"",
            "AXGroup[title%=\"x\"]",
            "AXGroup.",
            "AXGroup)",
        ] {
            assert!(Selector::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn round_trips_through_serde() {
        let s: Selector = serde_json::from_str(r#""AXWindow > AXGroup[title^=\"a\"]""#).unwrap();
        assert_eq!(s.to_string(), r#"AXWindow > AXGroup[title^="a"]"#);
        assert!(serde_json::from_str::<Selector>(r#""AXGroup[""#).is_err());
    }
}
