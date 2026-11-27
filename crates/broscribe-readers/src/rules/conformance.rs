//! Synthetic conformance matrix of the shipped reader rules (NA07), for the
//! platform crates' tests (feature `conformance`). Every shipped rule, on
//! every platform it claims ([`CLAIMS`]), is fed representative native
//! fields through that platform's own mapper ([`Mapper::snapshot`]), then
//! through the real extraction ([`RuleEngine`], [`ReaderRule::read_call`]):
//! roster, captions and active speaker; missing, unreadable (incomplete) and
//! unlisted-locale scenes; call state with its identity, hidden windows and
//! document scope; and every selector of the rule must match something the
//! platform mapped.
//!
//! The fixtures are synthetic and shaped after each rule's own candidate
//! anchors. They establish that a platform's mapping keeps what the rule
//! matches, not that any meeting app shows these structures or names: every
//! rule, native app identity and locale stays unverified until owner-run
//! acceptance in a real call. The third-locale strings are invented
//! negatives, never observed labels.

use crate::rules::{CallReading, ReaderRule, RuleEngine, builtin_rules, reader_for_app};
use crate::{CaptionEvent, ReaderEvent, RosterMember};
use broscribe_platform::{AxNode, AxScopePolicy, AxTree};

/// A platform whose reader maps native accessibility fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

const PLATFORMS: [Os; 3] = [Os::MacOs, Os::Linux, Os::Windows];

/// What an element is to its app, before any platform API names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A top-level window: `name` is its title, `minimized` its state.
    Window,
    /// Web content: `url`, and `text` its whole-document text.
    Document,
    /// A generic container (a `div`, a native panel).
    Group,
    /// A tree or outline list of [`Kind::Row`]s.
    Outline,
    Row,
    Button,
    /// Static text: its text is `text`.
    Text,
    Heading,
}

/// One synthetic element. Inside a [`Kind::Document`] it is web content
/// (`id` a DOM id, `classes` its class list); outside, native UI (`id` an
/// automation identifier).
#[derive(Clone, Debug)]
pub struct Element {
    pub kind: Kind,
    /// The same key is the same native element in every snapshot (windows
    /// and documents always have one); other elements are new each time.
    pub key: Option<String>,
    /// The accessible name.
    pub name: Option<String>,
    /// The name is an author label (`aria-label`), which macOS reports as
    /// `AXDescription`; a name from contents is `AXTitle` there. Other
    /// platforms expose one name.
    pub labelled: bool,
    pub text: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub url: Option<String>,
    /// A window's minimized state; `None`: the platform cannot tell.
    pub minimized: Option<bool>,
    /// Reading its name fails (the element did not answer).
    pub unreadable: bool,
    pub children: Vec<Element>,
}

impl Element {
    fn new(kind: Kind, children: Vec<Element>) -> Self {
        Self {
            kind,
            key: None,
            name: None,
            labelled: false,
            text: None,
            id: None,
            classes: Vec::new(),
            url: None,
            minimized: None,
            unreadable: false,
            children,
        }
    }

    fn window(key: &str, title: &str, children: Vec<Element>) -> Self {
        Self {
            key: Some(key.into()),
            name: Some(title.into()),
            minimized: Some(false),
            ..Self::new(Kind::Window, children)
        }
    }

    fn document(key: &str, url: Option<&str>, children: Vec<Element>) -> Self {
        Self {
            key: Some(key.into()),
            url: url.map(Into::into),
            text: Some(DOCUMENT_TEXT.into()),
            ..Self::new(Kind::Document, children)
        }
    }

    fn group(children: Vec<Element>) -> Self {
        Self::new(Kind::Group, children)
    }

    fn text(text: &str) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::new(Kind::Text, Vec::new())
        }
    }

    /// A button named by its label (`labelled`) or its contents.
    fn button(name: &str, labelled: bool) -> Self {
        Self {
            name: Some(name.into()),
            labelled,
            ..Self::new(Kind::Button, Vec::new())
        }
    }

    fn heading(name: Option<&str>, children: Vec<Element>) -> Self {
        Self {
            name: name.map(Into::into),
            ..Self::new(Kind::Heading, children)
        }
    }

    fn id(self, id: &str) -> Self {
        Self {
            id: Some(id.into()),
            ..self
        }
    }

    fn classes(self, classes: &[&str]) -> Self {
        Self {
            classes: classes.iter().map(|c| (*c).to_owned()).collect(),
            ..self
        }
    }

    fn labelled(self, name: &str) -> Self {
        Self {
            name: Some(name.into()),
            labelled: true,
            ..self
        }
    }

    fn minimized(self, minimized: Option<bool>) -> Self {
        Self { minimized, ..self }
    }

    fn unreadable(self) -> Self {
        Self {
            unreadable: true,
            ..self
        }
    }

    fn with(self, children: Vec<Element>) -> Self {
        Self { children, ..self }
    }
}

/// One platform reader's mapping, driven by synthetic native fields.
pub trait Mapper {
    fn os(&self) -> Os;

    /// The snapshot this platform's reader makes of an application showing
    /// `windows` (top-level, in order) under `policy`: each element turned
    /// into the platform's native fields, then through its own mapping.
    fn snapshot(&self, windows: &[Element], policy: &AxScopePolicy) -> AxTree;
}

/// The native app identity each shipped rule claims per platform (macOS,
/// Linux, Windows), one representative taken from the rule's own
/// `bundle_ids`; `None`: the rule claims none there. Unverified identities,
/// like the rules.
const CLAIMS: &[(&str, [Option<&str>; 3])] = &[
    (
        "teams",
        [
            Some("com.microsoft.teams2"),
            Some("teams-for-linux"),
            Some("ms-teams.exe"),
        ],
    ),
    (
        "zoom",
        [Some("us.zoom.xos"), Some("zoom"), Some("zoom.exe")],
    ),
    (
        "meet-chrome",
        [
            Some("com.google.Chrome"),
            Some("google-chrome"),
            Some("chrome.exe"),
        ],
    ),
    (
        "meet-edge",
        [
            Some("com.microsoft.edgemac"),
            Some("microsoft-edge"),
            Some("msedge.exe"),
        ],
    ),
    ("meet-safari", [Some("com.apple.Safari"), None, None]),
    (
        "meet-brave",
        [
            Some("com.brave.Browser"),
            Some("brave-browser"),
            Some("brave.exe"),
        ],
    ),
    (
        "meet-arc",
        [Some("company.thebrowser.Browser"), None, Some("arc.exe")],
    ),
    (
        "meet-vivaldi",
        [
            Some("com.vivaldi.Vivaldi"),
            Some("vivaldi-stable"),
            Some("vivaldi.exe"),
        ],
    ),
    (
        "slack",
        [
            Some("com.tinyspeck.slackmacgap"),
            Some("slack"),
            Some("slack.exe"),
        ],
    ),
    (
        "discord",
        [
            Some("com.hnc.Discord"),
            Some("discord"),
            Some("discord.exe"),
        ],
    ),
    (
        "webex",
        [
            Some("Cisco-Systems.Spark"),
            Some("webex"),
            Some("webex.exe"),
        ],
    ),
];

fn claim(rule: &str, os: Os) -> Option<&'static str> {
    CLAIMS
        .iter()
        .find(|(id, _)| *id == rule)
        .and_then(|(_, natives)| natives[os as usize])
}

/// Every shipped rule is listed, in order, and each claimed identity
/// resolves to that very rule.
fn assert_claims(rules: &[ReaderRule]) {
    let listed: Vec<&str> = CLAIMS.iter().map(|(id, _)| *id).collect();
    let shipped: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        listed, shipped,
        "every shipped rule needs its platform claims and conformance fixtures"
    );
    for (rule, (id, natives)) in rules.iter().zip(CLAIMS) {
        for (os, native) in PLATFORMS.into_iter().zip(natives) {
            match native {
                Some(native) => assert_eq!(
                    reader_for_app(rules, native).map(|r| r.id.as_str()),
                    Some(*id),
                    "{os:?}: {native}"
                ),
                // No Windows executable name: nothing to claim there.
                None if os == Os::Windows => assert!(
                    rule.bundle_ids
                        .iter()
                        .all(|b| !b.to_ascii_lowercase().ends_with(".exe")),
                    "{id} names a Windows executable"
                ),
                None => {}
            }
        }
    }
}

/// Runs the whole matrix on `mapper`'s platform; panics on the first
/// mismatch, naming the platform, the rule and the scene.
pub fn check(mapper: &dyn Mapper) {
    let os = mapper.os();
    let rules = builtin_rules().expect("shipped rules parse");
    assert_claims(&rules);
    let mut claimed = 0;
    for rule in &rules {
        if claim(&rule.id, os).is_none() {
            continue;
        }
        Matrix {
            mapper,
            os,
            rule,
            spec: spec(&rule.id),
            policy: rule.scope_policy(),
            trees: Vec::new(),
        }
        .run();
        claimed += 1;
    }
    assert!(claimed > 0, "{os:?} claims no shipped rule");
}

const DOCUMENT_TEXT: &str = "Whole private document text";
const APP_URL: &str = "app://synthetic/";
const CALL_URL: &str = "https://meet.google.com/abc-defg-hij?authuser=0";
const OTHER_CODE_URL: &str = "https://meet.google.com/xyz-wxyz-xyz";
const OTHER_ORIGIN_URL: &str = "https://other.example/abc-defg-hij";
const GUEST: &str = "Dária Solís (Guest)";
const EMAIL: &str = "daria@example.invalid";
const SELF_NAME: &str = "Você";
const SPEAKER: &str = "Ana Example";
const FIRST_WORDS: &str = "first synthetic words";
const CORRECTED_WORDS: &str = "corrected phrase entirely";
const SECOND_WORDS: &str = "second synthetic words";

/// What surrounds an app's meeting UI.
#[derive(Clone, Copy)]
enum Shell {
    /// Electron: a window whose web document (no origin filter) holds it.
    Electron,
    /// Native UI in a window of this title.
    Native(&'static str),
    /// A browser window whose call document holds it.
    Browser,
}

impl Shell {
    fn window(self, key: &str, children: Vec<Element>) -> Element {
        let (title, url) = match self {
            Shell::Native(title) => return Element::window(key, title, children),
            Shell::Electron => ("Synthetic app", APP_URL),
            Shell::Browser => ("Synthetic browser", CALL_URL),
        };
        let document = Element::document(&format!("{key}-doc"), Some(url), children);
        Element::window(key, title, vec![document])
    }

    fn scene(self, children: Vec<Element>) -> Vec<Element> {
        vec![self.window("win-main", children)]
    }
}

/// A browser window `key` whose document is at `url`.
fn tab(key: &str, url: Option<&str>, minimized: Option<bool>, children: Vec<Element>) -> Element {
    Element::window(
        key,
        "Synthetic browser",
        vec![Element::document(&format!("{key}-doc"), url, children)],
    )
    .minimized(minimized)
}

/// One rule's synthetic app.
struct Spec {
    shell: Shell,
    /// The roster container; `item` makes its participant `n`.
    roster: Element,
    item: fn(usize) -> Element,
    /// The captions container; `caption` makes its line `n`.
    captions: Element,
    caption: fn(usize) -> Element,
    /// More meeting UI (the active-speaker label).
    extra: Vec<Element>,
    /// Controls that each show the call (every listed locale and control).
    in_call: Vec<Element>,
    /// Post-call surfaces, each a list of elements of one page.
    post_call: Vec<Vec<Element>>,
    /// Whole scenes whose controls or surfaces no selector lists.
    foreign: Vec<Vec<Element>>,
}

fn participant(n: usize) -> Element {
    Element::group(Vec::new()).id(&format!("participant-{n}"))
}

fn caption_line(n: usize) -> Element {
    Element::group(Vec::new()).id(&format!("caption-{n}"))
}

fn row(_: usize) -> Element {
    Element::new(Kind::Row, Vec::new())
}

fn closed_caption(n: usize) -> Element {
    Element::group(Vec::new()).id(&format!("cc-{n}"))
}

fn electron(roster: Element, captions: Element) -> Spec {
    Spec {
        shell: Shell::Electron,
        roster,
        item: participant,
        captions,
        caption: caption_line,
        extra: Vec::new(),
        in_call: Vec::new(),
        post_call: Vec::new(),
        foreign: Vec::new(),
    }
}

fn native(title: &'static str, captions: &str) -> Spec {
    Spec {
        shell: Shell::Native(title),
        roster: Element::new(Kind::Outline, Vec::new()).id("participants"),
        item: row,
        captions: Element::group(Vec::new()).id(captions),
        caption: closed_caption,
        extra: Vec::new(),
        in_call: Vec::new(),
        post_call: Vec::new(),
        foreign: Vec::new(),
    }
}

/// A post-call page: its heading (named, or `nested` as a static text
/// child) and its rejoin button.
fn left(heading: &str, rejoin: &str, nested: bool) -> Vec<Element> {
    let heading = if nested {
        Element::heading(None, vec![Element::text(heading)])
    } else {
        Element::heading(Some(heading), Vec::new())
    };
    vec![heading, Element::button(rejoin, false)]
}

fn meet() -> Spec {
    Spec {
        shell: Shell::Browser,
        roster: Element::group(Vec::new()).labelled("Participants"),
        item: participant,
        captions: Element::group(Vec::new()).labelled("Captions"),
        caption: caption_line,
        extra: Vec::new(),
        in_call: vec![
            Element::button("Leave call", true),
            Element::button("Sair da chamada", true),
        ],
        post_call: vec![
            left("You left the meeting", "Rejoin", false),
            left("You left the meeting", "Rejoin", true),
            left("Você saiu da reunião", "Participar novamente", false),
            left("Você saiu da reunião", "Participar novamente", true),
        ],
        // A locale no rule lists (invented strings): unknown.
        foreign: vec![
            vec![tab(
                "win-1",
                Some(CALL_URL),
                Some(false),
                vec![Element::button("Salir de la llamada", true)],
            )],
            vec![tab(
                "win-1",
                Some(CALL_URL),
                Some(false),
                left("Saliste de la reunión", "Volver a unirte", false),
            )],
        ],
    }
}

/// The synthetic app of shipped rule `rule`; a new rule needs one here.
fn spec(rule: &str) -> Spec {
    match rule {
        "teams" => Spec {
            in_call: vec![Element::button("Leave", true).id("hangup-button")],
            // Its label without the matched DOM id.
            foreign: vec![Shell::Electron.scene(vec![Element::button("Leave", true)])],
            ..electron(
                Element::group(Vec::new()).id("roster"),
                Element::group(Vec::new()).id("captions"),
            )
        },
        "slack" => electron(
            Element::group(Vec::new()).id("huddle-participants"),
            Element::group(Vec::new()).id("huddle-captions"),
        ),
        "discord" => electron(
            Element::group(Vec::new()).classes(&["voiceParticipants", "panel"]),
            Element::group(Vec::new()).id("captions"),
        ),
        "zoom" => {
            let controls = vec![
                Element::button("Leave", false),
                Element::button("End", false),
            ];
            Spec {
                extra: vec![Element::text(&format!("Talking: {SPEAKER}"))],
                // The same controls in a window of another title.
                foreign: vec![vec![Element::window(
                    "win-home",
                    "Synthetic home",
                    controls.clone(),
                )]],
                in_call: controls,
                ..native("Zoom Meeting", "closed-caption")
            }
        }
        "webex" => native("Synthetic meeting", "closed-captions"),
        "meet-chrome" | "meet-edge" | "meet-safari" | "meet-brave" | "meet-arc"
        | "meet-vivaldi" => meet(),
        other => panic!("no conformance fixture for shipped rule {other}"),
    }
}

fn rosters(events: &[ReaderEvent]) -> Vec<&[RosterMember]> {
    events
        .iter()
        .filter_map(|e| match e {
            ReaderEvent::Roster(r) => Some(r.members.as_slice()),
            _ => None,
        })
        .collect()
}

fn captions(events: &[ReaderEvent]) -> Vec<&CaptionEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            ReaderEvent::Caption(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn speakers(events: &[ReaderEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            ReaderEvent::ActiveSpeaker(a) => Some(a.speaker_name.as_str()),
            _ => None,
        })
        .collect()
}

fn documents<'a>(node: &'a AxNode, out: &mut Vec<&'a AxNode>) {
    if node.role == "AXWebArea" {
        out.push(node);
    }
    for child in &node.children {
        documents(child, out);
    }
}

struct Matrix<'a> {
    mapper: &'a dyn Mapper,
    os: Os,
    rule: &'a ReaderRule,
    spec: Spec,
    policy: AxScopePolicy,
    /// Every tree the platform mapped (selector coverage).
    trees: Vec<AxTree>,
}

impl Matrix<'_> {
    fn run(mut self) {
        self.content();
        self.call();
        self.coverage();
    }

    fn at(&self, scene: &str) -> String {
        format!("{:?} {}: {scene}", self.os, self.rule.id)
    }

    fn snapshot(&mut self, windows: Vec<Element>) -> AxTree {
        let tree = self.mapper.snapshot(&windows, &self.policy);
        self.trees.push(tree.clone());
        tree
    }

    fn read_call(&mut self, windows: Vec<Element>) -> CallReading {
        let tree = self.snapshot(windows);
        self.rule.read_call(&tree)
    }

    /// The meeting UI: two participants (a guest with an e-mail, then the
    /// user), two caption lines (the user's second) and the extras.
    fn ui(&self, unreadable: bool, first_words: &str) -> Vec<Element> {
        let spec = &self.spec;
        let mut user = Element::text(SELF_NAME);
        if unreadable {
            user = user.unreadable();
        }
        let roster = spec.roster.clone().with(vec![
            (spec.item)(1).with(vec![Element::text(GUEST), Element::text(EMAIL).id("email")]),
            (spec.item)(2).with(vec![user]),
        ]);
        let captions = spec.captions.clone().with(vec![
            (spec.caption)(1).with(vec![Element::text(SPEAKER), Element::text(first_words)]),
            (spec.caption)(2).with(vec![Element::text("You"), Element::text(SECOND_WORDS)]),
        ]);
        let mut ui = vec![roster, captions];
        ui.extend(spec.extra.iter().cloned());
        ui
    }

    fn content(&mut self) {
        let shell = self.spec.shell;
        let tree = self.snapshot(shell.scene(self.ui(false, FIRST_WORDS)));
        assert!(!tree.truncated(), "{}", self.at("whole meeting UI"));
        let mut engine = RuleEngine::new(self.rule.clone());
        let events = engine.read(&tree, 10.0);
        let expected = [
            RosterMember {
                name: "Dária Solís".into(),
                email: Some(EMAIL.into()),
                is_self: false,
            },
            RosterMember {
                name: SELF_NAME.into(),
                email: None,
                is_self: true,
            },
        ];
        assert_eq!(rosters(&events), [&expected[..]], "{}", self.at("roster"));
        let lines = captions(&events);
        let seen: Vec<_> = lines
            .iter()
            .map(|c| {
                (
                    c.speaker_name.as_str(),
                    c.text.as_str(),
                    c.is_self,
                    c.revision,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (SPEAKER, FIRST_WORDS, false, 1),
                ("You", SECOND_WORDS, true, 1)
            ],
            "{}",
            self.at("captions")
        );
        let first_id = lines[0].caption_id.clone();
        let speaking: &[&str] = if self.rule.active_speaker.is_some() {
            &[SPEAKER]
        } else {
            &[]
        };
        assert_eq!(speakers(&events), speaking, "{}", self.at("active speaker"));
        // Nothing in call: unknown, never an end.
        let reading = self.rule.read_call(&tree);
        assert_eq!(
            (reading.complete, reading.active.len(), reading.ended.len()),
            (self.rule.call.is_some(), 0, 0),
            "{}",
            self.at("meeting UI without a call control")
        );

        // A replaced line keeps its caption through the item's own
        // identifier: a new revision, not a new caption.
        let tree = self.snapshot(shell.scene(self.ui(false, CORRECTED_WORDS)));
        let events = engine.read(&tree, 11.0);
        let revised: Vec<_> = captions(&events)
            .iter()
            .map(|c| (c.caption_id.as_str(), c.text.as_str(), c.revision))
            .collect();
        assert_eq!(
            revised,
            [(first_id.as_str(), CORRECTED_WORDS, 2)],
            "{}",
            self.at("caption identifier")
        );
        assert!(
            rosters(&events).is_empty(),
            "{}",
            self.at("unchanged roster")
        );

        // Panels missing from a whole snapshot: an empty roster, nothing else.
        let tree = self.snapshot(shell.scene(Vec::new()));
        assert!(!tree.truncated(), "{}", self.at("missing panels"));
        let events = RuleEngine::new(self.rule.clone()).read(&tree, 1.0);
        assert!(
            matches!(rosters(&events).as_slice(), [members] if members.is_empty())
                && events.len() == 1,
            "{}",
            self.at("missing panels")
        );

        // An unreadable element: the roster is never published from a cut
        // snapshot, the captions still are, and the call state is unknown.
        let tree = self.snapshot(shell.scene(self.ui(true, FIRST_WORDS)));
        assert!(tree.truncated(), "{}", self.at("unreadable element"));
        let events = RuleEngine::new(self.rule.clone()).read(&tree, 1.0);
        assert!(rosters(&events).is_empty(), "{}", self.at("cut roster"));
        assert_eq!(captions(&events).len(), 2, "{}", self.at("cut captions"));
        assert_eq!(
            self.rule.read_call(&tree),
            CallReading::default(),
            "{}",
            self.at("cut call state")
        );
    }

    fn call(&mut self) {
        let Some(call) = self.rule.call.clone() else {
            assert!(
                self.spec.in_call.is_empty() && self.spec.foreign.is_empty(),
                "{}",
                self.at("call fixtures for a rule without call state")
            );
            return;
        };
        let in_call = self.spec.in_call.clone();
        assert!(!in_call.is_empty(), "{}", self.at("no in-call fixture"));
        let shell = self.spec.shell;
        if call.context.is_some() {
            self.identified(&in_call);
        } else {
            for control in &in_call {
                let reading = self.read_call(shell.scene(vec![control.clone()]));
                assert!(reading.complete, "{}", self.at("in call"));
                assert_eq!(reading.active, [None], "{}", self.at("in call"));
                assert!(reading.ended.is_empty(), "{}", self.at("in call"));
                let hosts: Vec<Option<String>> = if call.end_on_window_closed {
                    vec![reading.windows.first().cloned()]
                } else {
                    Vec::new()
                };
                assert_eq!(
                    reading.active_windows,
                    hosts,
                    "{}",
                    self.at("hosting window")
                );
            }
        }
        if call.end_on_window_closed {
            self.windows(&in_call[0]);
        }
        for scene in self.spec.foreign.clone() {
            let reading = self.read_call(scene);
            assert!(
                reading.active.is_empty()
                    && reading.ended.is_empty()
                    && reading.active_windows.is_empty(),
                "{}",
                self.at("unlisted control or surface")
            );
        }
        let mut cut = self.ui(true, FIRST_WORDS);
        cut.push(in_call[0].clone());
        let tree = self.snapshot(shell.scene(cut));
        assert!(tree.truncated(), "{}", self.at("unreadable in call"));
        assert_eq!(
            self.rule.read_call(&tree),
            CallReading::default(),
            "{}",
            self.at("unreadable in call")
        );
    }

    /// Rules with a call context: each call is its document and code.
    fn identified(&mut self, in_call: &[Element]) {
        let page =
            |url: Option<&str>, minimized, children| vec![tab("win-1", url, minimized, children)];
        let mut call = None;
        for control in in_call {
            let reading = self.read_call(page(Some(CALL_URL), Some(false), vec![control.clone()]));
            assert!(
                reading.complete && reading.ended.is_empty(),
                "{}",
                self.at("in call")
            );
            let [Some(identity)] = reading.active.as_slice() else {
                panic!("{}: {:?}", self.at("identified call"), reading.active);
            };
            // Every listed control shows the same call of one document.
            assert_eq!(
                call.get_or_insert_with(|| identity.clone()),
                identity,
                "{}",
                self.at("call identity")
            );
        }
        let call = call.expect("in-call fixtures");
        let surfaces = self.spec.post_call.clone();
        let ends = self.rule.call.as_ref().is_some_and(|c| !c.ended.is_empty());
        assert_eq!(
            !surfaces.is_empty(),
            ends,
            "{}",
            self.at("post-call fixtures")
        );
        for surface in &surfaces {
            let reading = self.read_call(page(Some(CALL_URL), Some(false), surface.clone()));
            assert!(reading.active.is_empty(), "{}", self.at("post call"));
            assert_eq!(
                reading.ended.as_slice(),
                std::slice::from_ref(&call),
                "{}",
                self.at("post call")
            );
            // An in-call control in that document wins over the surface.
            let mut both = surface.clone();
            both.push(in_call[0].clone());
            let reading = self.read_call(page(Some(CALL_URL), Some(false), both));
            assert!(
                reading.ended.is_empty() && reading.active.len() == 1,
                "{}",
                self.at("call control and surface")
            );
            // Half a surface, a minimized or unknown window, or a document
            // without its URL is no end.
            for missing in 0..surface.len() {
                let mut half = surface.clone();
                half.remove(missing);
                let reading = self.read_call(page(Some(CALL_URL), Some(false), half));
                assert!(reading.ended.is_empty(), "{}", self.at("half a surface"));
            }
            for minimized in [Some(true), None] {
                let reading = self.read_call(page(Some(CALL_URL), minimized, surface.clone()));
                assert!(reading.ended.is_empty(), "{}", self.at("hidden window"));
            }
            let reading = self.read_call(page(None, Some(false), surface.clone()));
            assert!(
                reading.ended.is_empty(),
                "{}",
                self.at("document without URL")
            );
        }
        if let Some(surface) = surfaces.first() {
            // Another document element, or another call code: another call.
            let other = Element::window(
                "win-1",
                "Synthetic browser",
                vec![Element::document(
                    "win-1-new",
                    Some(CALL_URL),
                    surface.clone(),
                )],
            );
            let reading = self.read_call(vec![other]);
            assert!(
                matches!(reading.ended.as_slice(), [id] if *id != call),
                "{}",
                self.at("new document")
            );
            let reading = self.read_call(page(Some(OTHER_CODE_URL), Some(false), surface.clone()));
            assert!(
                matches!(reading.ended.as_slice(), [id] if *id != call),
                "{}",
                self.at("other call code")
            );
        }
        // A hidden window's call control still shows the call; a document
        // without its URL shows an unidentified one.
        for minimized in [Some(true), None] {
            let reading = self.read_call(page(Some(CALL_URL), minimized, vec![in_call[0].clone()]));
            assert_eq!(reading.active.len(), 1, "{}", self.at("hidden in call"));
        }
        let reading = self.read_call(page(None, Some(false), vec![in_call[0].clone()]));
        assert_eq!(reading.active, [None], "{}", self.at("in call without URL"));

        // Another origin: identity only, nothing inside is read.
        let mut private = self.ui(false, FIRST_WORDS);
        private.push(in_call[0].clone());
        let tree = self.snapshot(page(Some(OTHER_ORIGIN_URL), Some(false), private));
        assert!(
            !tree.truncated(),
            "{}",
            self.at("a policy skip is not a cut")
        );
        let mut docs = Vec::new();
        documents(&tree.root, &mut docs);
        assert!(
            matches!(docs.as_slice(), [doc] if doc.children.is_empty()
                && doc.document.as_ref().is_some_and(|d| d.element().is_some())),
            "{}",
            self.at("other-origin document")
        );
        let reading = self.rule.read_call(&tree);
        assert!(
            reading.active.is_empty() && reading.ended.is_empty(),
            "{}",
            self.at("other-origin call state")
        );
        let events = RuleEngine::new(self.rule.clone()).read(&tree, 1.0);
        assert!(
            captions(&events).is_empty()
                && matches!(rosters(&events).as_slice(), [members] if members.is_empty()),
            "{}",
            self.at("other-origin meeting UI")
        );
    }

    /// `end_on_window_closed` rules: the windows hosting the call by their
    /// platform tokens, stable across snapshots.
    fn windows(&mut self, control: &Element) {
        let shell = self.spec.shell;
        let main = shell.window("win-main", self.ui(false, FIRST_WORDS));
        let host = shell.window("win-call", vec![control.clone()]);
        let reading = self.read_call(vec![main.clone(), host]);
        assert!(reading.complete, "{}", self.at("call window"));
        assert_eq!(reading.active, [None], "{}", self.at("call window"));
        let [main_token, host_token] = reading.windows.as_slice() else {
            panic!("{}: {:?}", self.at("window tokens"), reading.windows);
        };
        assert_ne!(main_token, host_token, "{}", self.at("window tokens"));
        assert_eq!(
            reading.active_windows,
            [Some(host_token.clone())],
            "{}",
            self.at("hosting window")
        );
        let quiet = self.read_call(vec![main]);
        assert!(
            quiet.active.is_empty() && quiet.active_windows.is_empty(),
            "{}",
            self.at("no call window")
        );
        assert_eq!(
            quiet.windows.as_slice(),
            std::slice::from_ref(main_token),
            "{}",
            self.at("stable token")
        );
    }

    /// Every selector of the rule matched something the platform mapped,
    /// every post-call surface whole within one call scope, and no
    /// document text was read unless the rule reads it.
    fn coverage(&self) {
        for selector in self.rule.selectors() {
            assert!(
                self.trees.iter().any(|tree| selector.any(&tree.root)),
                "{}",
                self.at(&format!("nothing mapped matches `{selector}`"))
            );
        }
        if let Some(call) = &self.rule.call {
            assert!(
                call.ended.is_empty() || call.context.is_some(),
                "{}",
                self.at("post-call surface without a call context")
            );
            if let Some(context) = &call.context {
                for surface in &call.ended {
                    assert!(
                        self.trees.iter().any(|tree| {
                            context
                                .scope
                                .select(&tree.root)
                                .into_iter()
                                .any(|scope| surface.all.iter().all(|s| s.any(scope)))
                        }),
                        "{}",
                        self.at(&format!("no mapped page shows surface {:?}", surface.all))
                    );
                }
            }
        }
        if !self.policy.document_text {
            for tree in &self.trees {
                let mut docs = Vec::new();
                documents(&tree.root, &mut docs);
                assert!(
                    docs.iter().all(|doc| doc.value.is_none()),
                    "{}",
                    self.at("document text read")
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_rule_has_platform_claims_resolving_to_it_and_fixtures() {
        let rules = builtin_rules().unwrap();
        assert_claims(&rules);
        for rule in &rules {
            // Panics for a rule without fixtures.
            let spec = spec(&rule.id);
            assert_eq!(spec.in_call.is_empty(), rule.call.is_none(), "{}", rule.id);
        }
    }
}
