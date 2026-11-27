//! Embedded, structural-only reader rules. `verified` and
//! `structural_verified` are separate from owner-run in-call verification.
use crate::captions::{RawCaption, folded_name, normalize_name};
use crate::selector::Selector;
use crate::{ActiveSpeakerEvent, CaptionTracker, ReaderEvent, RosterEvent, RosterMember};
use broscribe_core::schema::sha256_hex;
use broscribe_core::{Error, Result};
use broscribe_platform::ui::url_origin;
use broscribe_platform::{AxNode, AxScopePolicy, AxTree};
use serde::Deserialize;
use std::sync::Arc;

/// Synthetic per-rule, per-platform conformance matrix for the platform
/// crates' mapper tests (test support only, never in production builds).
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;

#[derive(Clone, Debug, Deserialize)]
pub struct Field {
    pub selector: Option<Selector>,
    #[serde(default = "default_attribute")]
    pub attribute: String,
    #[serde(default)]
    pub index: usize,
    pub strip_prefix: Option<String>,
}
fn default_attribute() -> String {
    "value".into()
}
impl Field {
    fn read(&self, scope: &AxNode) -> Option<String> {
        let selected;
        let node = if let Some(selector) = &self.selector {
            selected = selector.select_within(scope);
            *selected.get(self.index)?
        } else {
            scope
        };
        let value = match self.attribute.as_str() {
            "value" => node.value.as_ref().or(node.title.as_ref()),
            "title" => node.title.as_ref(),
            "description" => node.description.as_ref(),
            "identifier" => node.identifier.as_ref(),
            _ => None,
        }?;
        let value = self.strip_prefix.as_ref().map_or(value.as_str(), |prefix| {
            value.strip_prefix(prefix).unwrap_or(value)
        });
        (!value.trim().is_empty()).then(|| value.trim().to_owned())
    }

    /// Whether reading it may need a web document's own value: a value read
    /// of a node that may be a document (`items` selects the node when the
    /// field has no selector of its own).
    fn reads_document_value(&self, items: &Selector) -> bool {
        self.attribute == "value"
            && self
                .selector
                .as_ref()
                .unwrap_or(items)
                .may_select_document()
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct RosterRule {
    pub items: Selector,
    pub name: Field,
    pub email: Option<Field>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct CaptionRule {
    pub items: Selector,
    pub speaker: Field,
    pub text: Field,
    pub id: Option<Field>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ActiveRule {
    pub items: Selector,
    pub name: Field,
}
/// Call-state selectors (plan H): positive, structural and scoped to a
/// recognised call or post-call surface, never chat or caption text.
/// `active` matches an in-call control (any one selector); an `ended`
/// surface needs every selector of one of its groups. A post-call surface
/// counts only inside a [`CallContext`] whose call identity can be read, so
/// it can be tied to the very call seen active. `end_on_window_closed`
/// instead ties the call to the application's top-level windows that showed
/// an in-call control: once the platform verified every one of them
/// destroyed (not hidden, minimized, on another Space or merely missing
/// from a snapshot) and nothing in-call remains, the call ended. Without
/// either a rule has in-call evidence only. Absence is unknown, never an
/// end.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CallRule {
    #[serde(default)]
    pub active: Vec<Selector>,
    #[serde(default)]
    pub ended: Vec<EndedSurface>,
    #[serde(default)]
    pub context: Option<CallContext>,
    #[serde(default)]
    pub end_on_window_closed: bool,
}
/// One recognisable post-call surface: all of its selectors must match.
#[derive(Clone, Debug, Deserialize)]
pub struct EndedSurface {
    pub all: Vec<Selector>,
}
/// Which call an observation belongs to: call selectors are matched inside
/// each `scope` element (a web area). Its identity needs both the
/// platform's identity of that very element (a new document is a new
/// element) and its document URL with a listed origin and a first path
/// segment of the `key` shape (the call code). The identity stays in memory
/// as a hash; it is never logged, sent or stored. The origins also scope
/// snapshots: documents of other origins are not read.
#[derive(Clone, Debug, Deserialize)]
pub struct CallContext {
    pub scope: Selector,
    /// Accepted `scheme://host` origins, compared ASCII case-insensitively.
    pub origins: Vec<String>,
    /// Shape of the call segment: `a` an ASCII lowercase letter, `9` an
    /// ASCII digit, any other character itself.
    pub key: String,
}
impl CallContext {
    /// The canonical `origin/call-code` `url` names, or `None` for another
    /// origin or shape.
    pub fn call_code(&self, url: &str) -> Option<String> {
        let origin = url_origin(url)?;
        if !self.origins.iter().any(|o| o.eq_ignore_ascii_case(&origin)) {
            return None;
        }
        let rest = &url[url.find("://")? + 3..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let segment = rest[end..]
            .strip_prefix('/')?
            .split(['/', '?', '#'])
            .next()?;
        let shaped = segment.chars().count() == self.key.chars().count()
            && segment.chars().zip(self.key.chars()).all(|(c, p)| match p {
                'a' => c.is_ascii_lowercase(),
                '9' => c.is_ascii_digit(),
                p => c == p,
            });
        shaped.then(|| format!("{origin}/{segment}"))
    }
    /// The identity of the call shown in `scope`: its document element and
    /// call code, hashed; `None` when either cannot be read.
    pub fn identity(&self, scope: &AxNode) -> Option<String> {
        let document = scope.document.as_ref()?;
        let element = document.element()?;
        let code = self.call_code(document.url()?)?;
        Some(sha256_hex(format!("{element}\u{0}{code}").as_bytes()))
    }
}
/// What one snapshot shows about calls, before the reader incarnation's
/// own history is applied (the host's job).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallReading {
    /// A whole, untruncated snapshot read with call selectors.
    pub complete: bool,
    /// Calls with a visible in-call control, by identity (`None`: no
    /// readable identity, or a rule without a context policy).
    pub active: Vec<Option<String>>,
    /// Calls showing a whole post-call surface (and no in-call control),
    /// by identity; only identifiable ones are listed.
    pub ended: Vec<String>,
    /// `end_on_window_closed` rules only: the element token of each
    /// top-level window showing an in-call control (`None`: a window
    /// without a token, or a control outside every window).
    pub active_windows: Vec<Option<String>>,
    /// The element tokens of every top-level window in the snapshot.
    pub windows: Vec<String>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ReaderRule {
    pub id: String,
    pub bundle_ids: Vec<String>,
    pub verified: bool,
    pub structural_verified: bool,
    /// A Chromium-based browser or Electron app: while its reader runs,
    /// `AXManualAccessibility` is turned on if the app reports it off, and
    /// restored when the last reader of that process stops.
    #[serde(default)]
    pub manual_accessibility: bool,
    /// Language -> self-labels. Unknown language uses all configured labels.
    pub self_labels: std::collections::BTreeMap<String, Vec<String>>,
    pub roster: RosterRule,
    pub captions: Option<CaptionRule>,
    pub active_speaker: Option<ActiveRule>,
    /// Without it the reader reports no call state (always unknown).
    #[serde(default)]
    pub call: Option<CallRule>,
}

/// What one snapshot shows to a rule before the reader's history is
/// applied: pure, so it is computed off the UI loop
/// ([`crate::SnapshotJob::run`]); [`RuleEngine::apply`] adds the history.
#[derive(Clone, Debug, Default)]
pub struct Extracted {
    /// Normalized names and e-mails, in tree order, without repeats.
    roster: Vec<(String, Option<String>)>,
    /// The roster is whole (an untruncated snapshot).
    whole: bool,
    captions: Vec<RawCaption>,
    active: Option<String>,
}

impl ReaderRule {
    pub fn parse(source: &str) -> Result<Self> {
        toml::from_str(source).map_err(|e| Error::InvalidInput(format!("reader rule: {e}")))
    }
    pub fn labels(&self) -> Vec<String> {
        self.self_labels.values().flatten().cloned().collect()
    }
    /// A macOS bundle id, Linux application id / binary name or Windows
    /// executable file name, compared like detection rules: ASCII
    /// case-insensitive, a trailing `*` matching by prefix.
    pub fn matches_app(&self, native_id: &str) -> bool {
        self.bundle_ids
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => native_id
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix)),
                None => pattern.eq_ignore_ascii_case(native_id),
            })
    }

    /// Every selector of the rule (items, fields, call state).
    pub fn selectors(&self) -> Vec<&Selector> {
        fn field(field: &Field) -> Option<&Selector> {
            field.selector.as_ref()
        }
        let mut out = vec![&self.roster.items];
        out.extend(field(&self.roster.name));
        out.extend(self.roster.email.as_ref().and_then(field));
        if let Some(c) = &self.captions {
            out.push(&c.items);
            out.extend(field(&c.speaker));
            out.extend(field(&c.text));
            out.extend(c.id.as_ref().and_then(field));
        }
        if let Some(a) = &self.active_speaker {
            out.push(&a.items);
            out.extend(field(&a.name));
        }
        if let Some(call) = &self.call {
            out.extend(&call.active);
            out.extend(call.ended.iter().flat_map(|s| &s.all));
            out.extend(call.context.as_ref().map(|c| &c.scope));
        }
        out
    }

    /// Whether any selector or field reads a web document's own value
    /// (whole-document text), which readers otherwise skip.
    pub fn reads_document_text(&self) -> bool {
        let mut fields: Vec<(&Field, &Selector)> = vec![(&self.roster.name, &self.roster.items)];
        fields.extend(self.roster.email.iter().map(|f| (f, &self.roster.items)));
        if let Some(c) = &self.captions {
            fields.extend([(&c.speaker, &c.items), (&c.text, &c.items)]);
            fields.extend(c.id.iter().map(|f| (f, &c.items)));
        }
        if let Some(a) = &self.active_speaker {
            fields.push((&a.name, &a.items));
        }
        self.selectors().iter().any(|s| s.reads_document_value())
            || fields
                .iter()
                .any(|(field, items)| field.reads_document_value(items))
    }

    /// What a reader incarnation of this rule may read: documents of the
    /// call context's origins (all documents without one), and document
    /// text only when the rule reads it.
    pub fn scope_policy(&self) -> AxScopePolicy {
        AxScopePolicy {
            origins: self
                .call
                .as_ref()
                .and_then(|call| call.context.as_ref())
                .map(|context| context.origins.clone())
                .unwrap_or_default(),
            document_text: self.reads_document_text(),
        }
    }

    /// What `tree` shows about calls. A truncated tree, or a rule without
    /// call selectors, is an incomplete, empty reading. With a context
    /// policy each scope is read on its own: an in-call control there makes
    /// that call active, else a whole post-call surface there ends that
    /// call if its identity is readable; an in-call control outside every
    /// scope still counts as an unidentified active call. Without one, only
    /// in-call controls count. Every selector stops at its first match.
    pub fn read_call(&self, tree: &AxTree) -> CallReading {
        let Some(rule) = &self.call else {
            return CallReading::default();
        };
        if tree.truncated() {
            return CallReading::default();
        }
        let mut reading = CallReading {
            complete: true,
            ..CallReading::default()
        };
        let in_call = |scope: &AxNode| rule.active.iter().any(|s| s.any(scope));
        if let Some(context) = &rule.context {
            for scope in context.scope.select(&tree.root) {
                let identity = context.identity(scope);
                if in_call(scope) {
                    if !reading.active.contains(&identity) {
                        reading.active.push(identity);
                    }
                } else if let Some(identity) = identity
                    // A minimized window's page is not current: never an end.
                    && !scope.document.as_ref().is_some_and(|d| d.hidden())
                    && rule.ended.iter().any(|surface| {
                        !surface.all.is_empty() && surface.all.iter().all(|s| s.any(scope))
                    })
                    && !reading.ended.contains(&identity)
                {
                    reading.ended.push(identity);
                }
            }
        }
        for window in tree.root.children.iter().filter(|n| n.role == "AXWindow") {
            let token = window
                .document
                .as_ref()
                .and_then(|d| d.element())
                .map(str::to_owned);
            if let Some(token) = &token {
                reading.windows.push(token.clone());
            }
            if rule.end_on_window_closed && in_call(window) {
                reading.active_windows.push(token);
            }
        }
        if reading.active.is_empty() && in_call(&tree.root) {
            reading.active.push(None);
        }
        if rule.end_on_window_closed
            && !reading.active.is_empty()
            && reading.active_windows.is_empty()
        {
            // In call, but not inside any top-level window.
            reading.active_windows.push(None);
        }
        reading
    }

    /// The roster, captions and active speaker `tree` shows (pure).
    pub fn extract(&self, tree: &AxTree) -> Extracted {
        let mut roster = Vec::new();
        for item in self.roster.items.select(&tree.root) {
            if let Some(name) = self.roster.name.read(item) {
                let entry = (
                    normalize_name(&name),
                    self.roster
                        .email
                        .as_ref()
                        .and_then(|field| field.read(item)),
                );
                if !roster.contains(&entry) {
                    roster.push(entry);
                }
            }
        }
        let captions = self
            .captions
            .as_ref()
            .map(|rule| {
                rule.items
                    .select(&tree.root)
                    .into_iter()
                    .filter_map(|item| {
                        Some(RawCaption {
                            id: rule.id.as_ref().and_then(|id| id.read(item)),
                            speaker_name: rule.speaker.read(item)?,
                            text: rule.text.read(item)?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let active = self.active_speaker.as_ref().and_then(|rule| {
            rule.items
                .select(&tree.root)
                .into_iter()
                .find_map(|item| rule.name.read(item))
                .map(|n| normalize_name(&n))
        });
        Extracted {
            roster,
            whole: !tree.truncated(),
            captions,
            active,
        }
    }
}

/// The reader for a resolved native app identity; `None` when no AX/UIA/AT-SPI
/// rule covers it, so the session records without a reader.
pub fn reader_for_app<'a>(rules: &'a [ReaderRule], native_id: &str) -> Option<&'a ReaderRule> {
    rules.iter().find(|rule| rule.matches_app(native_id))
}

pub fn builtin_rules() -> Result<Vec<ReaderRule>> {
    [
        include_str!("../readers/teams.toml"),
        include_str!("../readers/zoom.toml"),
        include_str!("../readers/meet-chrome.toml"),
        include_str!("../readers/meet-edge.toml"),
        include_str!("../readers/meet-safari.toml"),
        include_str!("../readers/meet-brave.toml"),
        include_str!("../readers/meet-arc.toml"),
        include_str!("../readers/meet-vivaldi.toml"),
        include_str!("../readers/slack.toml"),
        include_str!("../readers/discord.toml"),
        include_str!("../readers/webex.toml"),
    ]
    .into_iter()
    .map(ReaderRule::parse)
    .collect()
}

/// One reader incarnation's rule and history: caption revisions, the last
/// roster and active speaker, and the folded self-labels (computed once
/// per alias change).
pub struct RuleEngine {
    pub rule: Arc<ReaderRule>,
    captions: CaptionTracker,
    roster: Option<Vec<RosterMember>>,
    active: Option<String>,
    folded_self: Vec<String>,
}
impl RuleEngine {
    pub fn new(rule: ReaderRule) -> Self {
        Self::shared(Arc::new(rule))
    }
    /// An engine over a rule shared with snapshot jobs.
    pub fn shared(rule: Arc<ReaderRule>) -> Self {
        let mut engine = Self {
            rule,
            captions: CaptionTracker::default(),
            roster: None,
            active: None,
            folded_self: Vec::new(),
        };
        engine.set_self_aliases(Vec::new());
        engine
    }
    pub fn set_self_aliases(&mut self, aliases: Vec<String>) {
        let mut folded: Vec<String> = self
            .rule
            .labels()
            .iter()
            .chain(&aliases)
            .map(|label| folded_name(label))
            .filter(|label| !label.is_empty())
            .collect();
        folded.sort();
        folded.dedup();
        self.folded_self = folded;
    }
    /// [`ReaderRule::read_call`] of this engine's rule.
    pub fn read_call(&self, tree: &AxTree) -> CallReading {
        self.rule.read_call(tree)
    }
    pub fn read(&mut self, tree: &AxTree, observed_s: f64) -> Vec<ReaderEvent> {
        let extracted = self.rule.extract(tree);
        self.apply(extracted, observed_s)
    }
    /// Applies what one snapshot showed ([`ReaderRule::extract`]) to this
    /// reader's history: changed roster, new caption revisions, a changed
    /// active speaker.
    pub fn apply(&mut self, extracted: Extracted, observed_s: f64) -> Vec<ReaderEvent> {
        let mut events = Vec::new();
        let members: Vec<RosterMember> = extracted
            .roster
            .into_iter()
            .map(|(name, email)| RosterMember {
                is_self: self.folded_self.contains(&folded_name(&name)),
                name,
                email,
            })
            .collect();
        // A truncated tree must not erase participants that fell beyond the cap.
        if extracted.whole && self.roster.as_ref() != Some(&members) {
            events.push(ReaderEvent::Roster(RosterEvent {
                observed_s,
                members: members.clone(),
            }));
            self.roster = Some(members);
        }
        if self.rule.captions.is_some() {
            events.extend(
                self.captions
                    .update(extracted.captions, observed_s, &self.folded_self)
                    .into_iter()
                    .map(ReaderEvent::Caption),
            );
        }
        if self.rule.active_speaker.is_some() && extracted.active != self.active {
            if let Some(name) = &extracted.active {
                events.push(ReaderEvent::ActiveSpeaker(ActiveSpeakerEvent {
                    observed_s,
                    speaker_name: name.clone(),
                }));
            }
            self.active = extracted.active;
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(role: &str, id: &str, value: &str, children: Vec<AxNode>) -> AxNode {
        AxNode {
            identifier: Some(id.into()),
            value: Some(value.into()),
            children,
            ..AxNode::new(role)
        }
    }
    #[test]
    fn all_embedded_rules_are_unverified_and_have_language_labels() {
        let rules = builtin_rules().unwrap();
        assert_eq!(rules.len(), 11);
        for r in rules {
            assert!(!r.verified && !r.structural_verified);
            assert!(r.self_labels.contains_key("pt"));
        }
    }
    #[test]
    fn synthetic_teams_tree_emits_roster_caption_and_deduplicates() {
        let rule = builtin_rules().unwrap().remove(0);
        let mut engine = RuleEngine::new(rule);
        let tree = AxTree {
            incomplete: Default::default(),
            root: node(
                "AXApplication",
                "",
                "",
                vec![
                    node(
                        "AXGroup",
                        "roster",
                        "",
                        vec![node(
                            "AXGroup",
                            "participant-1",
                            "",
                            vec![node(
                                "AXStaticText",
                                "display-name",
                                "Dária Solís (Guest)",
                                vec![],
                            )],
                        )],
                    ),
                    node(
                        "AXGroup",
                        "captions",
                        "",
                        vec![node(
                            "AXGroup",
                            "caption-1",
                            "",
                            vec![
                                node("AXStaticText", "speaker", "Você", vec![]),
                                node("AXStaticText", "text", "hello invented meeting", vec![]),
                            ],
                        )],
                    ),
                ],
            ),
        };
        let events = engine.read(&tree, 3.0);
        assert!(matches!(&events[0], ReaderEvent::Roster(r) if r.members[0].name == "Dária Solís"));
        assert!(
            matches!(&events[1], ReaderEvent::Caption(c) if c.is_self && c.text == "hello invented meeting")
        );
        assert!(engine.read(&tree, 4.0).is_empty());
    }

    fn rule(id: &str) -> ReaderRule {
        builtin_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap()
    }

    fn tree(children: Vec<AxNode>, truncated: bool) -> AxTree {
        AxTree {
            incomplete: broscribe_platform::AxIncomplete {
                read_failed: truncated,
                ..Default::default()
            },
            root: AxNode {
                children: vec![AxNode {
                    children,
                    ..AxNode::new("AXWindow")
                }],
                ..AxNode::new("AXApplication")
            },
        }
    }

    const CALL_URL: &str = "https://meet.google.com/abc-defg-hij?authuser=0";

    /// A web area of document element `element` at `url`.
    fn web(element: Option<&str>, url: Option<&str>, children: Vec<AxNode>) -> AxNode {
        AxNode {
            children,
            document: Some(broscribe_platform::DocumentContext::new(
                url.map(Into::into),
                element.map(Into::into),
                false,
            )),
            ..AxNode::new("AXWebArea")
        }
    }

    fn leave(label: &str) -> AxNode {
        AxNode {
            description: Some(label.into()),
            ..AxNode::new("AXButton")
        }
    }

    /// The Portuguese post-call page, heading text as a child.
    fn left_pt() -> Vec<AxNode> {
        vec![
            AxNode {
                children: vec![AxNode {
                    value: Some("Você saiu da reunião".into()),
                    ..AxNode::new("AXStaticText")
                }],
                ..AxNode::new("AXHeading")
            },
            AxNode {
                title: Some("Participar novamente".into()),
                ..AxNode::new("AXButton")
            },
        ]
    }

    #[test]
    fn record_auto_stop_meet_readings_carry_the_call_identity() {
        for id in ["meet-chrome", "meet-edge", "meet-safari"] {
            let engine = RuleEngine::new(rule(id));
            let context = rule(id).call.unwrap().context.unwrap();
            for label in ["Leave call", "Sair da chamada"] {
                let reading = engine.read_call(&tree(
                    vec![web(Some("doc-1"), Some(CALL_URL), vec![leave(label)])],
                    false,
                ));
                assert!(reading.complete);
                assert_eq!(reading.active.len(), 1, "{id} {label}");
                assert!(reading.active[0].is_some());
            }
            let identity = |element: &str, url: &str| {
                context.identity(&web(Some(element), Some(url), Vec::new()))
            };
            // Same element and call code (query and fragment ignored).
            assert_eq!(
                identity("doc-1", CALL_URL),
                identity("doc-1", "https://MEET.google.com/abc-defg-hij#x")
            );
            // Another document of the same code, another code, another
            // origin or shape, or a missing part: not this call.
            assert_ne!(identity("doc-2", CALL_URL), identity("doc-1", CALL_URL));
            assert_ne!(
                identity("doc-1", "https://meet.google.com/xyz-wxyz-xyz"),
                identity("doc-1", CALL_URL)
            );
            assert_eq!(identity("doc-1", "https://example.com/abc-defg-hij"), None);
            assert_eq!(identity("doc-1", "https://meet.google.com/landing"), None);
            assert_eq!(
                context.identity(&web(None, Some(CALL_URL), Vec::new())),
                None
            );
            assert_eq!(
                context.identity(&web(Some("doc-1"), None, Vec::new())),
                None
            );

            let ended = engine.read_call(&tree(
                vec![web(Some("doc-1"), Some(CALL_URL), left_pt())],
                false,
            ));
            assert!(ended.active.is_empty());
            assert_eq!(ended.ended, [identity("doc-1", CALL_URL).unwrap()]);
            // A post-call page without a readable identity is no evidence.
            let anonymous =
                engine.read_call(&tree(vec![web(Some("doc-1"), None, left_pt())], false));
            assert!(anonymous.ended.is_empty());
            // Half a surface (heading without its Rejoin button) is no end.
            let mut half = left_pt();
            half.pop();
            assert!(
                engine
                    .read_call(&tree(vec![web(Some("doc-1"), Some(CALL_URL), half)], false))
                    .ended
                    .is_empty()
            );
            // A call control in that document wins over its post-call page
            // (e.g. a chat rendering heading and button during the call).
            let mut both = left_pt();
            both.push(leave("Leave call"));
            let reading =
                engine.read_call(&tree(vec![web(Some("doc-1"), Some(CALL_URL), both)], false));
            assert!(reading.ended.is_empty() && reading.active.len() == 1);
            // The same strings outside web content do not count.
            let outside = engine.read_call(&tree(vec![leave("Leave call")], false));
            assert!(outside.active.is_empty() && outside.ended.is_empty());
            // Truncated: incomplete and empty.
            let cut = engine.read_call(&tree(
                vec![web(Some("doc-1"), Some(CALL_URL), left_pt())],
                true,
            ));
            assert_eq!(cut, CallReading::default());
            // A minimized window: its post-call page proves nothing, its
            // call controls still veto.
            let hidden = |children: Vec<AxNode>| AxNode {
                children,
                document: Some(broscribe_platform::DocumentContext::new(
                    Some(CALL_URL.into()),
                    Some("doc-1".into()),
                    true,
                )),
                ..AxNode::new("AXWebArea")
            };
            assert!(
                engine
                    .read_call(&tree(vec![hidden(left_pt())], false))
                    .ended
                    .is_empty()
            );
            assert_eq!(
                engine
                    .read_call(&tree(vec![hidden(vec![leave("Leave call")])], false))
                    .active
                    .len(),
                1
            );
        }
    }

    #[test]
    fn record_auto_stop_native_rules_have_in_call_evidence_only() {
        let teams = RuleEngine::new(rule("teams"));
        // As observed in a real Teams call: AXButton, description "Leave",
        // DOM id `hangup-button`.
        let hangup = AxNode {
            identifier: Some("hangup-button".into()),
            description: Some("Leave".into()),
            ..AxNode::new("AXButton")
        };
        let reading = teams.read_call(&tree(vec![hangup.clone()], false));
        assert_eq!((reading.complete, reading.active), (true, vec![None]));
        assert_eq!(
            teams.read_call(&tree(vec![hangup], true)),
            CallReading::default(),
            "truncated"
        );
        let zoom = RuleEngine::new(rule("zoom"));
        let meeting = AxTree {
            incomplete: Default::default(),
            root: AxNode {
                children: vec![AxNode {
                    title: Some("Zoom Meeting".into()),
                    children: vec![AxNode {
                        title: Some("Leave".into()),
                        ..AxNode::new("AXButton")
                    }],
                    ..AxNode::new("AXWindow")
                }],
                ..AxNode::new("AXApplication")
            },
        };
        assert_eq!(zoom.read_call(&meeting).active, vec![None]);
        // Neither defines a post-call surface or a call identity.
        for id in ["teams", "zoom"] {
            let r = rule(id);
            assert!(
                r.call
                    .as_ref()
                    .is_some_and(|c| c.ended.is_empty() && c.context.is_none()),
                "{id}"
            );
        }
        // Readers without call selectors never claim anything.
        for id in ["slack", "discord", "webex"] {
            assert_eq!(
                RuleEngine::new(rule(id)).read_call(&tree(Vec::new(), false)),
                CallReading::default()
            );
        }
        // Every built-in post-call surface is bound to a call identity.
        for r in builtin_rules().unwrap() {
            if let Some(call) = &r.call {
                assert!(call.ended.is_empty() || call.context.is_some(), "{}", r.id);
            }
        }
    }

    /// A top-level window with element token `token`.
    fn window(token: Option<&str>, children: Vec<AxNode>) -> AxNode {
        AxNode {
            children,
            document: token
                .map(|t| broscribe_platform::DocumentContext::new(None, Some(t.into()), false)),
            ..AxNode::new("AXWindow")
        }
    }

    fn app(windows: Vec<AxNode>) -> AxTree {
        AxTree {
            root: AxNode {
                children: windows,
                ..AxNode::new("AXApplication")
            },
            incomplete: Default::default(),
        }
    }

    fn hangup() -> AxNode {
        AxNode {
            identifier: Some("hangup-button".into()),
            description: Some("Leave".into()),
            ..AxNode::new("AXButton")
        }
    }

    #[test]
    fn record_auto_stop_teams_reading_names_the_windows_hosting_the_call() {
        let teams = rule("teams");
        assert!(teams.call.as_ref().unwrap().end_on_window_closed);
        let reading = teams.read_call(&app(vec![
            window(Some("win-main"), vec![AxNode::new("AXGroup")]),
            window(Some("win-call"), vec![AxNode::new("AXGroup"), hangup()]),
        ]));
        assert!(reading.complete);
        assert_eq!(reading.active, [None]);
        assert_eq!(reading.active_windows, [Some("win-call".to_owned())]);
        assert_eq!(reading.windows, ["win-main", "win-call"]);
        // A window without a token cannot be tracked.
        let anonymous = teams.read_call(&app(vec![window(None, vec![hangup()])]));
        assert_eq!(anonymous.active_windows, [None]);
        assert!(anonymous.windows.is_empty());
        // A control outside every window is untrackable too.
        let mut loose = app(vec![window(Some("win-main"), Vec::new())]);
        loose.root.children.push(hangup());
        assert_eq!(teams.read_call(&loose).active_windows, [None]);
        // Nothing in call: windows are listed, nothing is active.
        let quiet = teams.read_call(&app(vec![window(Some("win-main"), Vec::new())]));
        assert!(quiet.active.is_empty() && quiet.active_windows.is_empty());
        assert_eq!(quiet.windows, ["win-main"]);
        // A cut snapshot proves nothing, windows included.
        let mut cut = app(vec![window(Some("win-call"), vec![hangup()])]);
        cut.incomplete.time_budget = true;
        assert_eq!(teams.read_call(&cut), CallReading::default());
        // Rules without the policy never list hosting windows.
        let zoom = rule("zoom");
        let reading = zoom.read_call(&app(vec![AxNode {
            title: Some("Zoom Meeting".into()),
            ..window(
                Some("zoom-win"),
                vec![AxNode {
                    title: Some("Leave".into()),
                    ..AxNode::new("AXButton")
                }],
            )
        }]));
        assert_eq!(reading.active, [None]);
        assert!(reading.active_windows.is_empty());
    }

    #[test]
    fn shipped_rules_use_the_cross_platform_vocabulary_and_scope_documents() {
        let mapped = [
            "role",
            "subrole",
            "title",
            "value",
            "description",
            "identifier",
            "class",
        ];
        for r in builtin_rules().unwrap() {
            for selector in r.selectors() {
                for role in selector.roles() {
                    assert!(
                        broscribe_platform::AX_ROLES.contains(&role),
                        "{}: {role} in {selector}",
                        r.id
                    );
                }
                for attribute in selector.attributes() {
                    // Subroles are macOS-only: no shipped rule may need one.
                    assert!(
                        mapped.contains(&attribute) && attribute != "subrole",
                        "{}: {attribute} in {selector}",
                        r.id
                    );
                }
            }
            // No shipped rule reads whole-document text.
            assert!(!r.reads_document_text(), "{}", r.id);
            let policy = r.scope_policy();
            if r.id.starts_with("meet-") {
                assert_eq!(policy.origins, ["https://meet.google.com"], "{}", r.id);
            } else {
                assert!(policy.origins.is_empty(), "{}", r.id);
            }
        }
        // Selecting documents as items reads no document text while every
        // field reads a descendant; a field reading the item's own value
        // (no selector) or a document value test does.
        let mut r = rule("slack");
        r.roster.items = Selector::parse("AXWebArea").unwrap();
        assert!(!r.reads_document_text());
        r.roster.name.selector = None;
        assert!(r.reads_document_text());
        let mut r = rule("slack");
        r.roster.items = Selector::parse(r#"AXWebArea[value*="x"] AXGroup"#).unwrap();
        assert!(r.reads_document_text());
    }

    #[test]
    fn brave_arc_and_vivaldi_are_covered_but_unverified() {
        let rules = builtin_rules().unwrap();
        for (native, id) in [
            ("com.brave.Browser", "meet-brave"),
            ("brave-browser", "meet-brave"),
            ("brave.exe", "meet-brave"),
            ("company.thebrowser.Browser", "meet-arc"),
            ("Arc.exe", "meet-arc"),
            ("com.vivaldi.Vivaldi", "meet-vivaldi"),
            ("vivaldi-stable", "meet-vivaldi"),
            ("vivaldi.exe", "meet-vivaldi"),
        ] {
            let r = reader_for_app(&rules, native).unwrap_or_else(|| panic!("{native}"));
            assert_eq!(r.id, id);
            assert!(!r.verified && !r.structural_verified);
            assert!(r.manual_accessibility);
            // Same call evidence as Chrome's rule.
            let chrome = rule("meet-chrome");
            assert_eq!(
                format!("{:?}", r.call.as_ref().unwrap().active),
                format!("{:?}", chrome.call.as_ref().unwrap().active)
            );
            assert!(r.call.as_ref().unwrap().context.is_some());
        }
    }

    #[test]
    fn engine_folds_self_labels_once_and_extraction_is_pure() {
        let r = rule("teams");
        let mut engine = RuleEngine::new(r.clone());
        engine.set_self_aliases(vec!["  ÁLEX   Example ".into()]);
        let caption = |speaker: &str| {
            node(
                "AXGroup",
                "captions",
                "",
                vec![node(
                    "AXGroup",
                    "caption-1",
                    "",
                    vec![
                        node("AXStaticText", "speaker", speaker, vec![]),
                        node("AXStaticText", "text", "synthetic meeting words", vec![]),
                    ],
                )],
            )
        };
        let tree = AxTree {
            root: node("AXApplication", "", "", vec![caption("alex example")]),
            incomplete: Default::default(),
        };
        let extracted = r.extract(&tree);
        let again = r.extract(&tree);
        assert_eq!(format!("{extracted:?}"), format!("{again:?}"));
        let events = engine.apply(extracted, 2.0);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReaderEvent::Caption(c) if c.is_self))
        );
        // Applying the same extraction again changes nothing.
        assert!(engine.apply(again, 3.0).is_empty());
    }
}
