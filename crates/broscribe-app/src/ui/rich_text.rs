//! Selectable text with inline marks and links that open (design §4.2,
//! INS-59): one run of a template result block.
//!
//! [`RichText`] takes part in the window's text selection as gpui-base's
//! `SelectableText` does (the same public `TextSelectionHandle`,
//! `TextSelectionRegistration` and `TextSelectionRun`, with its reading
//! order), so a selection runs across a result's blocks and Edit › Copy
//! copies the text as shown. Unlike `SelectableText` it owns its
//! `StyledText`, which gives it what that one keeps private:
//!
//! - the text's layout, so each link gets a node of its own drawn over its
//!   words: a `Role::Link` named by them, a tab stop that Enter activates,
//!   and the target of a pointer click and of assistive technology's Click
//!   action. A link that wraps gets a click area on every row it spans; its
//!   one node, focus ring and tab stop sit on its first row. The nodes are
//!   laid out as roots during this element's prepaint, so in the
//!   accessibility tree they sit inside the block that holds the text. A
//!   press and release that moved past [`CLICK_SLOP`], or a double or
//!   triple click or shift-click that selected a character of the text, is
//!   a selection, not a click, and opens nothing ([`opens_link`]);
//! - per-range font families, so code spans are drawn in the monospace font;
//! - its paint passes, so the selection sits between the text's run
//!   backgrounds (a code span's) and its glyphs.
//!
//! Enter opens a focused link on key-down. GPUI also turns Enter's and
//! Space's key-up on a focused element into a keyboard click; a link
//! ignores that one, so Enter opens it once and Space, as in a browser,
//! not at all.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::base::{TextSelection, TextSelectionHandle, TextSelectionRegistration, TextSelectionRun};
use gpui::{
    AnyElement, App, AvailableSpace, BorderStyle, Bounds, ClickEvent, Corners, Edges, Element,
    ElementId, GlobalElementId, HighlightStyle, HitboxBehavior, Hsla, InspectorElementId,
    InteractiveElement as _, IntoElement, KeyDownEvent, LayoutId, PaintQuad, Pixels, Point, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledText, Subscription,
    TextLayout, Window, div, point, px, transparent_black,
};

/// How far, in logical pixels, the pointer may move between pressing and
/// releasing on a link for that to still open it; further is a drag.
const CLICK_SLOP: f64 = 4.;

pub(crate) fn highlight_words(text: &str, words: &[String], cx: &App) -> StyledText {
    StyledText::new(text.to_owned()).with_highlights(word_highlights(text, words, cx))
}

/// A search or find hit: the find yellow with its own ink, and semibold, so
/// a hit is marked by more than its colour. Text selection stays the
/// theme's selection colour.
pub(crate) fn find_hit_style(cx: &App) -> HighlightStyle {
    let colours = super::brand::brand(cx);
    HighlightStyle {
        background_color: Some(colours.find),
        color: Some(colours.find_foreground),
        font_weight: Some(gpui::FontWeight::SEMIBOLD),
        ..Default::default()
    }
}

pub(crate) fn word_highlights(
    text: &str,
    words: &[String],
    cx: &App,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let hit = find_hit_style(cx);
    // The index snippet uses the original spellings. Fold case for repeated
    // occurrences and keep byte offsets in the original Unicode text.
    let mut folded = String::new();
    let mut offsets = Vec::new();
    for (index, ch) in text.char_indices() {
        offsets.push((folded.len(), index));
        folded.extend(ch.to_lowercase());
    }
    offsets.push((folded.len(), text.len()));
    let original = |index| {
        offsets
            .binary_search_by_key(&index, |(folded, _)| *folded)
            .ok()
            .map(|position| offsets[position].1)
    };
    let mut ranges = Vec::new();
    for word in words.iter().filter(|word| !word.is_empty()) {
        let word = word.to_lowercase();
        for (start, matched) in folded.match_indices(&word) {
            if let Some((start, end)) = original(start).zip(original(start + matched.len())) {
                ranges.push(start..end);
            }
        }
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for range in ranges {
        if let Some((last, _)) = highlights.last_mut()
            && last.end >= range.start
        {
            last.end = last.end.max(range.end);
        } else {
            highlights.push((range, hit));
        }
    }
    highlights
}

/// Opens a link's address (the caller checked it is http or https).
pub(crate) type OpenLink = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// Opens one link.
type Activate = Rc<dyn Fn(&mut Window, &mut App)>;

/// Whether the window's text selection covers at least one character of a
/// run.
type SelectsText = Rc<dyn Fn(&mut App) -> bool>;

/// One link of a [`RichText`]: the bytes of its words, the name its node is
/// read by (those words) and the address it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RichLink {
    pub range: Range<usize>,
    pub name: SharedString,
    pub url: SharedString,
}

/// The links of a [`RichText`] and how they behave.
struct Links {
    /// ID stem of the link nodes: link `i` is `(id, i)`.
    id: SharedString,
    links: Vec<RichLink>,
    /// The focus ring's colour.
    ring: Hsla,
    open: OpenLink,
}

/// A selectable run of text with highlights, monospace ranges and links.
pub(crate) struct RichText {
    id: ElementId,
    text: SharedString,
    styled: StyledText,
    order: u64,
    links: Option<Links>,
}

impl RichText {
    /// `text` as the run `id`, at `order` among the window's selectable
    /// runs.
    pub(crate) fn new(id: impl Into<ElementId>, text: impl Into<SharedString>, order: u64) -> Self {
        let text = text.into();
        Self {
            id: id.into(),
            styled: StyledText::new(text.clone()),
            text,
            order,
            links: None,
        }
    }

    /// Styles ranges of the text: sorted, without overlap, on character
    /// boundaries.
    pub(crate) fn highlights(mut self, highlights: Vec<(Range<usize>, HighlightStyle)>) -> Self {
        if !highlights.is_empty() {
            self.styled = self.styled.with_highlights(highlights);
        }
        self
    }

    /// Draws ranges in another font family: sorted, without overlap, each
    /// also a highlighted range (only whole runs change family).
    pub(crate) fn font_families(mut self, ranges: Vec<(Range<usize>, SharedString)>) -> Self {
        if !ranges.is_empty() {
            self.styled = self.styled.with_font_family_overrides(ranges);
        }
        self
    }

    /// Gives each of `links` its node (IDs `(id, i)`), opened by `open`.
    pub(crate) fn links(
        mut self,
        id: SharedString,
        links: Vec<RichLink>,
        ring: Hsla,
        open: OpenLink,
    ) -> Self {
        if !links.is_empty() {
            self.links = Some(Links {
                id,
                links,
                ring,
                open,
            });
        }
        self
    }
}

/// What a run keeps between frames: its selection participant, the text it
/// was made for and the text it last added to the window's selection.
struct Retained {
    participant: Participant,
    text: SharedString,
    _refresh: Subscription,
}

#[derive(Clone)]
pub(crate) struct Participant {
    handle: TextSelectionHandle,
    selected: Rc<RefCell<Option<String>>>,
}

pub(crate) struct Prepainted {
    links: Vec<AnyElement>,
}

impl IntoElement for RichText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RichText {
    type RequestLayoutState = Participant;
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text = self.text.clone();
        let participant = window.with_element_state(
            global_id.expect("RichText has an element ID"),
            |retained: Option<Retained>, window| {
                let retained = match retained {
                    Some(retained) if retained.text == text => retained,
                    Some(retained) => {
                        retained
                            .participant
                            .handle
                            .set_fallback_copy_text(text.to_string(), cx);
                        Retained { text, ..retained }
                    }
                    None => {
                        let handle = TextSelectionHandle::new(text.to_string(), cx);
                        let refresh = handle.refresh_window_on_change(window, cx);
                        Retained {
                            participant: Participant {
                                handle,
                                selected: Rc::default(),
                            },
                            text,
                            _refresh: refresh,
                        }
                    }
                };
                (retained.participant.clone(), retained)
            },
        );
        let (layout_id, ()) = self
            .styled
            .request_layout(global_id, inspector_id, window, cx);
        (layout_id, participant)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        participant: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled
            .prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let registration = TextSelectionRegistration::new(hitbox, bounds)
            .with_document_order(self.order)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(&participant.handle, window, cx);
        participant.handle.register(registration, window, cx);
        // After the text's hitbox, so a link's areas are above it.
        let links = match &self.links {
            Some(links) => {
                // The run as paint projects the selection onto it, so a
                // click reads what the selection covers at that moment.
                let run =
                    TextSelectionRun::new(self.text.clone(), self.styled.layout().clone(), bounds)
                        .with_document_order(self.order);
                let handle = participant.handle.clone();
                let selects: SelectsText = Rc::new(move |cx| {
                    handle
                        .update_runs(std::slice::from_ref(&run), cx)
                        .ranges()
                        .iter()
                        .flatten()
                        .any(|range| !range.is_empty())
                });
                link_elements(
                    links,
                    self.styled.layout(),
                    &self.text,
                    &selects,
                    window,
                    cx,
                )
            }
            None => Vec::new(),
        };
        Prepainted { links }
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        participant: &mut Self::RequestLayoutState,
        prepainted: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled.layout().clone();
        let projection = participant.handle.update_runs(
            &[
                TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
                    .with_document_order(self.order),
            ],
            cx,
        );
        let ranges: Vec<Range<usize>> = projection
            .ranges()
            .iter()
            .flatten()
            .filter(|range| !range.is_empty())
            .cloned()
            .collect();
        // What this run adds to the window's selected text changed: draw
        // again, so the selection and Copy agree.
        let selected = ranges
            .first()
            .and_then(|range| self.text.get(range.clone()))
            .map(str::to_owned);
        if *participant.selected.borrow() != selected {
            *participant.selected.borrow_mut() = selected;
            window.refresh();
        }
        // The runs' backgrounds (a code span's), then the selection over
        // them, then the glyphs and their lines over both, so a selected
        // code span shows the selection and its text stays readable. The
        // two passes are what `StyledText::paint` does in one.
        layout
            .paint_background(window, cx)
            .expect("the text was laid out and prepainted");
        let color = gpui::base::Theme::global(cx).tokens.colors.selection;
        for range in ranges {
            paint_selection(&layout, range, color, window);
        }
        layout
            .paint_foreground(window, cx)
            .expect("the text was laid out and prepainted");
        for link in &mut prepainted.links {
            link.paint(window, cx);
        }
    }
}

/// The rows a range of laid-out text covers, in window coordinates: the
/// first from the range's start, the next ones from the text's left edge,
/// each to where the range leaves that row.
fn range_rows(layout: &TextLayout, text: &str, range: Range<usize>) -> Vec<Bounds<Pixels>> {
    let line_height = layout.line_height();
    let left = layout.bounds().left();
    let mut rows = Vec::new();
    // (row top, start x, end x)
    let mut row: Option<(Pixels, Pixels, Pixels)> = None;
    let end = range.end.min(text.len());
    for ix in (range.start..=end).filter(|&ix| text.is_char_boundary(ix)) {
        // At a wrap, the index is the end of the row it leaves.
        let Some(at) = layout.position_for_index(ix) else {
            continue;
        };
        row = Some(match row {
            Some((top, start, _)) if top == at.y => (top, start, at.x),
            Some((top, start, stop)) => {
                rows.push((top, start, stop));
                (at.y, left, at.x)
            }
            None => (at.y, at.x, at.x),
        });
    }
    rows.extend(row);
    rows.into_iter()
        .filter(|(_, start, stop)| stop > start)
        .map(|(top, start, stop)| {
            Bounds::from_corners(point(start, top), point(stop, top + line_height))
        })
        .collect()
}

/// Each link's node over its first row and a click area over each further
/// row, laid out and prepainted where its words are. `selects` tells
/// whether the selection covers a character of the text.
fn link_elements(
    links: &Links,
    layout: &TextLayout,
    text: &str,
    selects: &SelectsText,
    window: &mut Window,
    cx: &mut App,
) -> Vec<AnyElement> {
    let mut out = Vec::new();
    for (i, link) in links.links.iter().enumerate() {
        let open = links.open.clone();
        let url = link.url.clone();
        let activate: Activate = Rc::new(move |window, cx| open(&url, window, cx));
        for (k, row) in range_rows(layout, text, link.range.clone())
            .into_iter()
            .enumerate()
        {
            let on_click = activate.clone();
            let selects = selects.clone();
            let el = if k == 0 {
                let on_key = activate.clone();
                let ring = links.ring;
                let id = links.id.clone();
                div()
                    .id((links.id.clone(), i))
                    .debug_selector(move || format!("{id}-{i}"))
                    .role(Role::Link)
                    .aria_label(link.name.clone())
                    .tab_index(0)
                    .rounded(px(2.))
                    .border_1()
                    .border_color(transparent_black())
                    .focus(crate::ui::kit::focus_halo(ring))
                    .on_key_down(move |event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "enter" {
                            // A held key's repeats open nothing more.
                            if !event.is_held {
                                on_key(window, cx);
                            }
                            cx.stop_propagation();
                        }
                    })
            } else {
                let id = links.id.clone();
                div()
                    .id(SharedString::from(format!("{}-{i}-row-{k}", links.id)))
                    .debug_selector(move || format!("{id}-{i}-row-{k}"))
            };
            let mut el = el
                .w(row.size.width)
                .h(row.size.height)
                .cursor_pointer()
                .on_click(move |event: &ClickEvent, window, cx| {
                    if opens_link(event, || selects(cx)) {
                        // A click, not a selection: drop what the pointer's
                        // jitter selected between the press and the
                        // release, so Copy doesn't take it.
                        if TextSelection::has_selection(window, cx) {
                            TextSelection::clear(window, cx);
                        }
                        on_click(window, cx);
                    }
                })
                .into_any_element();
            el.prepaint_as_root(
                row.origin,
                row.size.map(AvailableSpace::Definite),
                window,
                cx,
            );
            out.push(el);
        }
    }
    out
}

/// Whether a click on a link opens it; `selects_text` tells whether the
/// window's selection covers at least one character of the text the link
/// is in.
///
/// A pointer's press and release that moved more than [`CLICK_SLOP`] is a
/// drag and opens nothing, whether or not it selected text. Within the slop
/// a single press and release is a click, though the selection layer may
/// hold two points apart (a 1 px move) or a character between them (a
/// jitter across the middle of one); the caller drops that selection. Only
/// a gesture made to select that selected a character is a selection,
/// which Copy can take: a double or triple click (a word, a line; the
/// double click's first click opened the link already) or a shift-click
/// (extending a selection). A tap reads the same way, a double tap being
/// touch's way to select a word. Assistive technology's Click is a press
/// and release at one point, one click and no modifier, so it opens the
/// link. A keyboard click (GPUI's, on Enter's or Space's key-up) opens
/// nothing: Enter already opened the link on key-down, and Space does not
/// open a link.
fn opens_link(event: &ClickEvent, selects_text: impl FnOnce() -> bool) -> bool {
    let selecting = match event {
        ClickEvent::Keyboard(_) => return false,
        ClickEvent::Mouse(click) => {
            if (click.up.position - click.down.position).magnitude() > CLICK_SLOP {
                return false;
            }
            click.down.click_count > 1 || click.down.modifiers.shift
        }
        ClickEvent::Touch(tap) => tap.tap_count > 1,
    };
    !(selecting && selects_text())
}

fn paint_selection(layout: &TextLayout, range: Range<usize>, color: Hsla, window: &mut Window) {
    let (Some(start), Some(end)) = (
        layout.position_for_index(range.start),
        layout.position_for_index(range.end),
    ) else {
        return;
    };
    for bounds in selection_rows(start, end, layout.bounds(), layout.line_height()) {
        window.paint_quad(PaintQuad {
            bounds,
            background: color.into(),
            corner_radii: Corners::default(),
            border_widths: Edges::default(),
            border_color: transparent_black(),
            border_style: BorderStyle::default(),
        });
    }
}

/// The selection's rectangles from `start` to `end`: the rest of the first
/// row, every full row between and the last row up to `end`.
fn selection_rows(
    start: Point<Pixels>,
    end: Point<Pixels>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Vec<Bounds<Pixels>> {
    if start.y == end.y {
        return vec![Bounds::from_corners(
            start,
            point(end.x, end.y + line_height),
        )];
    }
    let mut rows = vec![Bounds::from_corners(
        start,
        point(bounds.right(), start.y + line_height),
    )];
    if end.y > start.y + line_height {
        rows.push(Bounds::from_corners(
            point(bounds.left(), start.y + line_height),
            point(bounds.right(), end.y),
        ));
    }
    rows.push(Bounds::from_corners(
        point(bounds.left(), end.y),
        point(end.x, end.y + line_height),
    ));
    rows
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{
        AppContext as _, Background, Context, InputEvent as _, KeyboardClickEvent, Modifiers,
        MouseButton, MouseClickEvent, MouseDownEvent, MouseUpEvent, ParentElement as _, Render,
        TestAppContext, TouchClickEvent, UnderlineStyle, VisualTestContext,
    };

    use super::*;

    const TEXT: &str = "Before the synthetic planning document for the next quarter after.";

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    /// A selection on one row is one rectangle from its start to its end,
    /// one row high.
    #[test]
    fn a_selection_on_one_row_is_one_rectangle() {
        let bounds = Bounds::from_corners(at(10., 20.), at(210., 120.));
        assert_eq!(
            selection_rows(at(30., 20.), at(90., 20.), bounds, px(16.)),
            [Bounds::from_corners(at(30., 20.), at(90., 36.))]
        );
    }

    /// Over two rows: the rest of the first row to the text's right edge,
    /// then the second row from its left edge to the end; nothing between.
    #[test]
    fn a_selection_over_two_rows_is_two_rectangles() {
        let bounds = Bounds::from_corners(at(10., 20.), at(210., 120.));
        assert_eq!(
            selection_rows(at(150., 20.), at(40., 36.), bounds, px(16.)),
            [
                Bounds::from_corners(at(150., 20.), at(210., 36.)),
                Bounds::from_corners(at(10., 36.), at(40., 52.)),
            ]
        );
    }

    /// Over more rows, every full row between is one rectangle across the
    /// text's width.
    #[test]
    fn a_selection_over_four_rows_covers_the_rows_between() {
        let bounds = Bounds::from_corners(at(10., 20.), at(210., 120.));
        assert_eq!(
            selection_rows(at(150., 20.), at(40., 68.), bounds, px(16.)),
            [
                Bounds::from_corners(at(150., 20.), at(210., 36.)),
                Bounds::from_corners(at(10., 36.), at(210., 68.)),
                Bounds::from_corners(at(10., 68.), at(40., 84.)),
            ]
        );
    }

    /// `TEXT` laid out 160 px wide; its layout kept for the test to read.
    struct Laid(Rc<RefCell<Option<TextLayout>>>);
    impl Render for Laid {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let styled = StyledText::new(TEXT);
            *self.0.borrow_mut() = Some(styled.layout().clone());
            div().w(px(160.)).child(styled)
        }
    }

    /// `range_rows` gives the rows a range covers: within one row, the
    /// range's own extent; across wraps, the first row from the range's
    /// start, each next one from the text's left edge, the last up to the
    /// range's end, each one line high. An empty range covers nothing.
    #[gpui::test]
    fn range_rows_follow_a_range_across_its_rows(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        let kept = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let kept = kept.clone();
            move |_, _| Laid(kept)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let layout = kept.borrow().clone().expect("the text was laid out");
        let pos = |ix: usize| layout.position_for_index(ix).expect("an index of the text");
        let line = layout.line_height();
        let left = layout.bounds().left();

        // "the" on the first row.
        let rows = range_rows(&layout, TEXT, 7..10);
        assert_eq!(
            rows,
            [Bounds::from_corners(
                pos(7),
                point(pos(10).x, pos(7).y + line)
            )]
        );

        // The whole text wraps onto several rows.
        let rows = range_rows(&layout, TEXT, 0..TEXT.len());
        assert!(rows.len() >= 3, "{rows:?}");
        assert_eq!(rows[0].origin, pos(0));
        for pair in rows.windows(2) {
            assert_eq!(pair[1].top(), pair[0].top() + line, "{rows:?}");
            assert_eq!(pair[1].left(), left, "{rows:?}");
        }
        assert!(rows.iter().all(|row| row.size.height == line));
        assert!(
            rows.iter()
                .all(|row| row.right() <= layout.bounds().right())
        );
        assert_eq!(rows.last().unwrap().right(), pos(TEXT.len()).x);

        // A range that starts on the second row starts there.
        let second = (0..TEXT.len())
            .find(|&ix| pos(ix).y > pos(0).y && pos(ix).x > left)
            .expect("a second row");
        let rows = range_rows(&layout, TEXT, second..second + 3);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].origin, pos(second));

        assert!(range_rows(&layout, TEXT, 7..7).is_empty());
    }

    type Opened = Rc<RefCell<Vec<String>>>;

    /// `TEXT` with "the synthetic planning document" a link, 400 px wide,
    /// with "synthetic" on a code background and every link underlined;
    /// its opens recorded.
    struct Linked(Opened);
    const LINK: Range<usize> = 7..38;
    const CODE: Range<usize> = 11..20;
    const CODE_BACKGROUND: Hsla = Hsla {
        h: 0.33,
        s: 1.,
        l: 0.5,
        a: 1.,
    };
    impl Render for Linked {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let opened = self.0.clone();
            let open: OpenLink = Rc::new(move |url, _, _| opened.borrow_mut().push(url.into()));
            let underline = HighlightStyle {
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(gpui::blue()),
                    wavy: false,
                }),
                ..HighlightStyle::default()
            };
            let code = HighlightStyle {
                background_color: Some(CODE_BACKGROUND),
                ..underline
            };
            div()
                .id("box")
                .debug_selector(|| "box".into())
                .w(px(400.))
                .child(
                    RichText::new("run", TEXT, 1)
                        .highlights(vec![
                            (LINK.start..CODE.start, underline),
                            (CODE, code),
                            (CODE.end..LINK.end, underline),
                        ])
                        .links(
                            "a".into(),
                            vec![RichLink {
                                range: LINK,
                                name: TEXT[LINK].into(),
                                url: "https://example.com/plan".into(),
                            }],
                            gpui::red(),
                            open,
                        ),
                )
        }
    }

    /// Opens `Linked` in a window, inside gpui-component's `Root` (which
    /// draws the window's text selection layer) when `root`.
    fn linked(cx: &mut TestAppContext, root: bool) -> (Opened, &mut VisualTestContext) {
        cx.update(gpui::init);
        let opened = Opened::default();
        let view = opened.clone();
        let cx = if root {
            cx.add_window_view(move |window, cx| {
                let view = cx.new(|_| Linked(view));
                gpui::component::Root::new(view, window, cx)
            })
            .1
        } else {
            cx.add_window_view(move |_, _| Linked(view)).1
        };
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (opened, cx)
    }

    /// Presses at `from`, moves to `to` and releases there.
    fn drag(cx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
    }

    /// A mouse press at (10, 10) and its release `dx`, `dy` away, the
    /// `click_count`th click, with or without shift.
    fn press(dx: f32, dy: f32, click_count: usize, shift: bool) -> ClickEvent {
        let modifiers = Modifiers {
            shift,
            ..Modifiers::default()
        };
        ClickEvent::Mouse(MouseClickEvent {
            down: MouseDownEvent {
                button: MouseButton::Left,
                position: at(10., 10.),
                modifiers,
                click_count,
                first_mouse: false,
            },
            up: MouseUpEvent {
                button: MouseButton::Left,
                position: at(10. + dx, 10. + dy),
                modifiers,
                click_count,
            },
        })
    }

    /// `opens_link`: within the click slop a single press and release opens
    /// the link whatever the selection holds; past it nothing opens; a
    /// double or triple click, a shift-click or a double tap opens it only
    /// when it selected no character; a keyboard click never does.
    #[test]
    fn what_opens_a_link() {
        let selected = || true;
        let nothing = || false;
        for (dx, dy) in [(0., 0.), (2., 1.), (0., 1.), (3., 0.), (4., 0.)] {
            assert!(opens_link(&press(dx, dy, 1, false), selected), "{dx} {dy}");
        }
        assert!(!opens_link(&press(4.1, 0., 1, false), nothing));
        assert!(!opens_link(&press(0., 5., 1, false), nothing));
        for count in [2, 3] {
            assert!(!opens_link(&press(0., 0., count, false), selected));
            assert!(opens_link(&press(0., 0., count, false), nothing));
        }
        assert!(!opens_link(&press(1., 0., 1, true), selected));
        assert!(opens_link(&press(1., 0., 1, true), nothing));
        let keyboard = ClickEvent::Keyboard(KeyboardClickEvent::default());
        assert!(!opens_link(&keyboard, nothing));
        let tap = |tap_count| {
            ClickEvent::Touch(TouchClickEvent {
                tap_count,
                ..TouchClickEvent::default()
            })
        };
        assert!(opens_link(&tap(1), selected));
        assert!(!opens_link(&tap(2), selected));
        assert!(opens_link(&tap(2), nothing));
    }

    /// Whether the window holds a text selection.
    fn has_selection(cx: &mut VisualTestContext) -> bool {
        cx.update(TextSelection::has_selection)
    }

    /// The selected text as the next frame draws it.
    fn selected_text(cx: &mut VisualTestContext) -> String {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            TextSelection::selected_text(window, cx)
        })
    }

    /// INS-59, in a window built like BroScribe's (`Root` draws the text
    /// selection layer): a press and release on a link that moved less than
    /// the click slop opens it once, though the selection layer holds two
    /// points apart, 2.2 px apart or 1 px straight down, or the jitter
    /// crosses the middle of a character, so the selection covers it; the
    /// click leaves nothing selected. A drag across its words copies them
    /// and opens nothing. A double click opens it once, on its first click;
    /// its second selects a word, which a shift-click extends, opening
    /// nothing.
    #[gpui::test]
    fn a_click_with_jitter_opens_a_link_and_a_selection_does_not(cx: &mut TestAppContext) {
        use gpui::component::input::Copy;

        const URL: &str = "https://example.com/plan";
        let (opened, cx) = linked(cx, true);
        let link = cx.debug_bounds("a-0").expect("the link is drawn");
        let center = link.center();
        let y = center.y;

        drag(cx, center, center + point(px(2.), px(1.)));
        assert_eq!(*opened.borrow(), [URL]);
        assert!(!has_selection(cx));
        drag(cx, center, center + point(px(0.), px(1.)));
        assert_eq!(*opened.borrow(), [URL; 2]);
        assert!(!has_selection(cx));

        // Presses 3 px from their releases along the link's first words:
        // some cross the middle of a character, and the frame drawn before
        // the release shows that character selected.
        let mut crossed = 0;
        for dx in 0..12 {
            let from = point(link.left() + px(2. + dx as f32), y);
            let to = from + point(px(3.), px(0.));
            cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
            let shown = selected_text(cx);
            if !shown.is_empty() {
                assert_eq!(shown.chars().count(), 1, "{shown:?}");
                crossed += 1;
            }
            cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
            cx.run_until_parked();
            assert!(!has_selection(cx));
        }
        assert!(crossed > 0, "no press crossed a character");
        assert_eq!(*opened.borrow(), [URL; 14]);

        drag(
            cx,
            point(link.left() + px(1.), y),
            point(link.right() - px(1.), y),
        );
        cx.dispatch_action(Copy);
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(TEXT[LINK].to_owned())
        );
        assert_eq!(*opened.borrow(), [URL; 14]);

        for click_count in [1, 2] {
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position: center,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position: center,
                modifiers: Modifiers::default(),
                click_count,
            });
        }
        cx.run_until_parked();
        assert_eq!(*opened.borrow(), [URL; 15]);
        let word = selected_text(cx);
        assert!(
            ["the", "synthetic", "planning", "document"].contains(&word.as_str()),
            "{word:?}"
        );

        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        cx.simulate_click(point(link.left() + px(1.), y), shift);
        cx.run_until_parked();
        assert_eq!(*opened.borrow(), [URL; 15]);
        let extended = selected_text(cx);
        assert!(
            extended.starts_with("the") && extended.ends_with(&word),
            "{extended:?}"
        );
    }

    /// Assistive technology's Click on a link opens it once, also while
    /// text is selected. GPUI's built-in handler for it
    /// (`Window::handle_a11y_action`, crate-private, and the test platform
    /// takes no accessibility adapter) dispatches a press and a release at
    /// the node's centre, one click and no modifier, which the test sends
    /// the same way.
    #[gpui::test]
    fn assistive_technologys_click_opens_a_link_once(cx: &mut TestAppContext) {
        use crate::ui::a11y_tree;

        let (opened, cx) = linked(cx, true);
        let tree = cx.update(a11y_tree::draw_and_read);
        let node = tree.find("a-0");
        assert_eq!(tree.read(node), ("Link", &TEXT[LINK]));
        assert!(
            tree.nodes[node]
                .actions
                .iter()
                .any(|action| action == "Click"),
            "{:?}",
            tree.nodes[node]
        );
        let link = cx.debug_bounds("a-0").expect("the link is drawn");
        drag(
            cx,
            point(link.left() + px(1.), link.center().y),
            point(link.right() - px(1.), link.center().y),
        );
        assert!(has_selection(cx));
        assert!(opened.borrow().is_empty(), "{:?}", opened.borrow());

        let center = link.center();
        cx.update(|window, cx| {
            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: center,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            let up = MouseUpEvent {
                button: MouseButton::Left,
                position: center,
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            window.dispatch_event(down.to_platform_input(), cx);
            window.dispatch_event(up.to_platform_input(), cx);
        });
        cx.run_until_parked();
        assert_eq!(*opened.borrow(), ["https://example.com/plan"]);
    }

    /// A press and release on a link that moved past the click slop is a
    /// drag and opens nothing, even with no text selection layer to say
    /// text was selected; one that moved less opens the link once.
    #[gpui::test]
    fn a_drag_on_a_link_opens_nothing(cx: &mut TestAppContext) {
        let (opened, cx) = linked(cx, false);
        let link = cx.debug_bounds("a-0").expect("the link is drawn");
        let y = link.center().y;
        drag(
            cx,
            point(link.left() + px(2.), y),
            point(link.right() - px(2.), y),
        );
        assert!(opened.borrow().is_empty(), "{:?}", opened.borrow());
        drag(cx, link.center(), link.center() + point(px(2.), px(1.)));
        assert_eq!(*opened.borrow(), ["https://example.com/plan"]);
    }

    /// INS-59: the selection is painted over the text's run backgrounds
    /// (a code span's) and under its glyphs and their lines. GPUI draws a
    /// primitive over the ones it overlaps that were painted before it, in
    /// `order`; the test platform keeps the scene's quads and underlines,
    /// so the order is checked there.
    #[gpui::test]
    fn a_selection_paints_between_backgrounds_and_text(cx: &mut TestAppContext) {
        let (_, cx) = linked(cx, true);
        let text = cx.debug_bounds("box").expect("the text is drawn");
        let y = text.top() + px(2.);
        drag(
            cx,
            point(text.left() + px(1.), y),
            point(text.right() - px(1.), text.bottom() - px(2.)),
        );
        let (quads, underlines, selection) = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            (
                window.painted_quads(),
                window.painted_underlines(),
                gpui::base::Theme::global(cx).tokens.colors.selection,
            )
        });
        let painted = |color: Hsla| {
            quads
                .iter()
                .filter(|quad| quad.background == Background::from(color))
                .map(|quad| (quad.bounds, quad.order))
                .collect::<Vec<_>>()
        };
        let selected = painted(selection);
        let code = painted(CODE_BACKGROUND);
        assert!(!selected.is_empty(), "the selection is painted");
        assert!(!code.is_empty(), "the code span's background is painted");
        let mut checked = 0;
        for (code, code_order) in &code {
            for (sel, sel_order) in selected.iter().filter(|(b, _)| b.intersects(code)) {
                assert!(sel_order > code_order, "{sel:?} under {code:?}");
                checked += 1;
            }
        }
        assert!(checked > 0, "the selection covers the code span");
        let mut checked = 0;
        for underline in &underlines {
            for (sel, sel_order) in selected
                .iter()
                .filter(|(b, _)| b.intersects(&underline.bounds))
            {
                assert!(underline.order > *sel_order, "{underline:?} under {sel:?}");
                checked += 1;
            }
        }
        assert!(checked > 0, "the selection covers the link's underline");
    }
}
