//! Speaker review pieces shared by the review queue and the meeting's
//! Speakers tab (design §4 "Review queue").
//!
//! Samples: a speaker's candidate clips come from [`ops::sample_clips`] in
//! rank order (longest first). "Play sample" plays the current clip (the
//! longest until the user steps); while a speaker has more than one clip,
//! "Previous" / "Next" step through them in rank order ("Sample 2 of 5") and
//! play the clip they reach; the line being played shows under the row so it
//! can be read while heard. Playback goes through the view's
//! [`SamplePlayback`], so starting another speaker's sample stops the current
//! one; the [`SampleCursor`] remembers which speaker's clip is current.
//!
//! "Can't tell": [`set_review_dismissed`] dismisses a speaker from review
//! (or restores it) through `ops::dismiss_review` / `ops::restore_review`,
//! which check the meeting is done and idle and the speaker fence under the
//! library lock. The view offers Undo for [`UNDO_WINDOW`] afterwards.

use crate::ui::kit::{self, StatusKind, button};
use std::time::Duration;

use crate::ui::format::{format_clock, spoken_position};
use broscribe_core::ops::{self, DismissOutcome, SampleClip, SpeakerFence};
use broscribe_core::{Library, MeetingId, Result, SpeakerId};
use gpui::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui::component::kbd::Kbd;
use gpui::component::{ActiveTheme as _, Disableable as _, Sizable as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, AnyElement, Context, Div, ElementId, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Role, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _,
    Window, div,
};

use crate::ui::Spoken as _;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::views::people::{Clip, ClipSource, LOADING, SamplePlayback};

/// How long "Dismissed — Undo" stays offered after "Can't tell" (well under
/// the retention grace, `retention::DISMISS_GRACE`).
pub const UNDO_WINDOW: Duration = Duration::from_secs(10);

// ---- samples (pure, tested) ----

/// The speaker whose samples were last played and the clip that is current
/// for them: its position in their [`ops::sample_clips`] and its range, so a
/// reload that changes the candidates can find it again
/// ([`Self::reconcile`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SampleCursor {
    current: Option<Current>,
}

#[derive(Clone, Debug, PartialEq)]
struct Current {
    key: SharedString,
    ix: usize,
    start: f64,
    end: f64,
}

impl SampleCursor {
    /// Whether `key`'s samples are the current ones.
    pub fn is_current(&self, key: &str) -> bool {
        self.current_key().is_some_and(|k| k.as_ref() == key)
    }

    /// The speaker whose samples are current, if any.
    pub fn current_key(&self) -> Option<&SharedString> {
        self.current.as_ref().map(|c| &c.key)
    }

    /// The current clip of `key` among `len` clips: the one last played or
    /// stepped to, else the longest (0).
    pub fn index(&self, key: &str, len: usize) -> usize {
        match &self.current {
            Some(c) if c.key.as_ref() == key && c.ix < len => c.ix,
            _ => 0,
        }
    }

    fn set(&mut self, key: &SharedString, ix: usize, clips: &[SampleClip]) -> Option<usize> {
        let clip = clips.get(ix)?;
        self.current = Some(Current {
            key: key.clone(),
            ix,
            start: clip.start,
            end: clip.end,
        });
        Some(ix)
    }

    /// Makes `key` current at its current clip, which "Play sample" plays;
    /// `None` without clips.
    pub fn select(&mut self, key: &SharedString, clips: &[SampleClip]) -> Option<usize> {
        self.set(key, self.index(key, clips.len()), clips)
    }

    /// Steps `key` to the next (or previous) clip in rank order, wrapping
    /// around; `None` without clips.
    pub fn step(
        &mut self,
        key: &SharedString,
        clips: &[SampleClip],
        forward: bool,
    ) -> Option<usize> {
        let len = clips.len();
        if len == 0 {
            return None;
        }
        let ix = self.index(key, len);
        let ix = if forward {
            (ix + 1) % len
        } else {
            (ix + len - 1) % len
        };
        self.set(key, ix, clips)
    }

    /// After a reload: `clips` are the current speaker's candidates now
    /// (`None`: the speaker is gone). Re-points the cursor to its clip
    /// wherever it moved and returns `true`; when the clip is no longer a
    /// candidate, forgets it and returns `false` (the caller stops its
    /// playback). `true` without a current speaker.
    pub fn reconcile(&mut self, clips: Option<&[SampleClip]>) -> bool {
        let Some(current) = &mut self.current else {
            return true;
        };
        match clips.and_then(|clips| {
            clips
                .iter()
                .position(|c| c.start == current.start && c.end == current.end)
        }) {
            Some(ix) => {
                current.ix = ix;
                true
            }
            None => {
                self.current = None;
                false
            }
        }
    }
}

/// "Sample 2 of 5" (no position for a single clip).
pub fn sample_position(ix: usize, len: usize) -> String {
    if len <= 1 {
        "Sample".to_owned()
    } else {
        format!("Sample {} of {len}", ix + 1)
    }
}

/// [`sample_position`] with "· longest" on the first clip of several.
pub fn sample_label(ix: usize, len: usize) -> String {
    if ix == 0 && len > 1 {
        format!("{} · longest", sample_position(ix, len))
    } else {
        sample_position(ix, len)
    }
}

/// The line shown under the row while `clip` is current:
/// `Sample 2 of 5 · 1:05 Speaker 3: text`.
pub fn sample_line_text(ix: usize, len: usize, speaker: &str, clip: &SampleClip) -> String {
    let position = sample_label(ix, len);
    let time = format_clock(clip.start);
    match clip.text.trim() {
        "" => format!("{position} · {time} {speaker}"),
        text => format!("{position} · {time} {speaker}: {text}"),
    }
}

/// "Sample 2 of 5 of Speaker 3: text", for assistive technology.
pub fn sample_aria(ix: usize, len: usize, speaker: &str, clip: &SampleClip) -> String {
    let position = sample_position(ix, len);
    let time = spoken_position(clip.start);
    match clip.text.trim() {
        "" => format!("{position} of {speaker} at {time}"),
        text => format!("{position} of {speaker} at {time}: {text}"),
    }
}

/// The playable range of `clip` in the recording `source`.
pub fn clip_of(source: &ClipSource, clip: &SampleClip) -> Clip {
    Clip {
        source: source.clone(),
        start_s: clip.start,
        end_s: clip.end,
    }
}

// ---- samples (rendering) ----

/// One speaker's samples as a row shows them.
pub struct SampleControls<'a> {
    /// The row's sample key in the view's [`SamplePlayback`].
    pub key: &'a SharedString,
    /// The speaker as the row names it ("Speaker 3", "Ana").
    pub speaker: &'a str,
    pub clips: &'a [SampleClip],
    /// The recording can be played (else "Play sample" is disabled).
    pub playable: bool,
    pub tab_stop: bool,
    pub keyboard: Option<SampleKeyboard<'a>>,
}

pub struct SampleKeyboard<'a> {
    pub focus: &'a FocusHandle,
    pub window: &'a Window,
    pub actions: [&'a dyn Action; 3],
    pub context: &'static str,
}

impl SampleControls<'_> {
    /// Play/Stop and stable Previous/Next buttons for speakers with multiple clips.
    pub fn buttons<V: 'static>(
        &self,
        id: impl Into<ElementId>,
        cursor: &SampleCursor,
        playback: &SamplePlayback,
        cx: &Context<V>,
        on_play: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
        on_step: impl Fn(&mut V, bool, &mut Window, &mut Context<V>) + Clone + 'static,
    ) -> Stateful<Div> {
        let speaker = self.speaker;
        let play_id = format!("sample-play-{}", self.key);
        let len = self.clips.len();
        let playing = playback.is_playing(self.key);
        let reason = if self.playable {
            format!("No clear stretch of speech to play for {speaker}")
        } else {
            "No recording to play".to_owned()
        };
        let enabled = playing || (self.playable && len > 0);
        let ix = cursor.index(self.key, len);
        let decorate = |button: Button, action: usize, tooltip: &str| {
            let button = button.small().ghost().tab_stop(self.tab_stop);
            let Some(keyboard) = self.keyboard.as_ref() else {
                return button;
            };
            let action = keyboard.actions[action];
            button
                .tooltip_with_action(tooltip.to_owned(), action, Some(keyboard.context))
                .children(
                    self.tab_stop
                        .then(|| {
                            Kbd::binding_for_action_in(action, keyboard.focus, keyboard.window)
                        })
                        .flatten(),
                )
        };
        let play = if enabled {
            let (text, aria, icon) = if playing {
                (
                    "Stop",
                    format!("Stop the voice sample of {speaker}"),
                    crate::ui::icons::STOP,
                )
            } else {
                let aria = match self.clips.get(ix) {
                    Some(clip) if len > 1 => {
                        format!("Play {}", sample_aria(ix, len, speaker, clip))
                    }
                    _ => format!("Play a voice sample of {speaker}"),
                };
                ("Play sample", aria, crate::ui::icons::PLAY)
            };
            decorate(
                button(play_id, text, true, cx, on_play)
                    .icon(icon)
                    .accessibility_label(aria),
                0,
                text,
            )
        } else {
            Button::new(play_id)
                .label("Play sample")
                .icon(crate::ui::icons::PLAY)
                .accessibility_label(reason.clone())
                .tooltip(reason.clone())
                .small()
                .ghost()
                .disabled(true)
                .tab_stop(self.tab_stop)
        };
        let mut buttons = vec![play];
        if len > 1 {
            let aria =
                |to: usize| format!("Play {}", sample_aria(to, len, speaker, &self.clips[to]));
            let back = on_step.clone();
            buttons.extend([
                decorate(
                    button(
                        format!("sample-previous-{}", self.key),
                        "Previous",
                        enabled,
                        cx,
                        move |this: &mut V, window, cx| back(this, false, window, cx),
                    )
                    .icon(gpui::assets::IconName::ChevronLeft)
                    .accessibility_label(aria((ix + len - 1) % len)),
                    1,
                    "Previous sample",
                ),
                decorate(
                    button(
                        format!("sample-next-{}", self.key),
                        "Next",
                        enabled,
                        cx,
                        move |this: &mut V, window, cx| on_step(this, true, window, cx),
                    )
                    .icon(gpui::assets::IconName::ChevronRight)
                    .accessibility_label(aria((ix + 1) % len)),
                    2,
                    "Next sample",
                ),
            ]);
        }
        div()
            .id(id)
            .role(Role::Group)
            .aria_label(format!("Voice samples of {speaker}"))
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(ButtonGroup::new(format!("sample-buttons-{}", self.key)).children(buttons))
            .when(len > 1, |group| {
                group.child(
                    crate::ui::text_line(
                        format!("sample-position-{}", self.key),
                        Role::Status,
                        format!("{} of {len}", ix + 1),
                    )
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground),
                )
            })
            .when(!self.playable, |group| {
                group.child(
                    crate::ui::text_line(
                        format!("sample-unavailable-{}", self.key),
                        Role::Label,
                        reason,
                    )
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground),
                )
            })
    }

    /// The current start or playback result belongs to this sample's row.
    pub fn status(
        &self,
        id: impl Into<ElementId>,
        playback: &SamplePlayback,
        cx: &gpui::App,
    ) -> Option<Stateful<Div>> {
        let text = playback.status_of(self.key)?;
        let (kind, words) = if text == LOADING {
            (StatusKind::Pending, "Loading…")
        } else {
            (StatusKind::Failed, text)
        };
        Some(kit::status_line(id, kind, words.to_owned(), cx))
    }

    /// The text of the current clip, muted under the row, once this speaker's
    /// samples are current.
    pub fn line<V: 'static>(
        &self,
        id: impl Into<ElementId>,
        cursor: &SampleCursor,
        cx: &Context<V>,
    ) -> Option<AnyElement> {
        let (text, aria) = self.current_line(cursor)?;
        Some(
            div()
                .id(id)
                .spoken(Role::Status, aria)
                .text_role(TextRole::Caption)
                .text_color(cx.theme().muted_foreground)
                .child(text)
                .into_any_element(),
        )
    }

    /// The text [`Self::line`] shows and its accessible label, once this
    /// speaker's samples are current.
    pub fn current_line(&self, cursor: &SampleCursor) -> Option<(String, String)> {
        if !cursor.is_current(self.key) {
            return None;
        }
        let len = self.clips.len();
        let ix = cursor.index(self.key, len);
        let clip = self.clips.get(ix)?;
        Some((
            sample_line_text(ix, len, self.speaker, clip),
            sample_aria(ix, len, self.speaker, clip),
        ))
    }
}

// ---- "Can't tell" ----

/// Dismisses `speaker` (any id of its group) from review without naming it
/// (`dismissed`), or restores it to the queue; refused when the meeting is
/// not done and idle or its speakers changed since `fence` was read.
pub fn set_review_dismissed(
    lib: &Library,
    meeting_id: &MeetingId,
    speaker: SpeakerId,
    fence: &SpeakerFence,
    dismissed: bool,
) -> Result<DismissOutcome> {
    ops::with_index(lib, |tx, index| {
        if dismissed {
            ops::dismiss_review(tx, index, meeting_id, speaker, fence)
        } else {
            ops::restore_review(tx, index, meeting_id, speaker, fence)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(start: f64, text: &str) -> SampleClip {
        SampleClip {
            start,
            end: start + 2.0,
            text: text.into(),
        }
    }

    #[test]
    fn the_cursor_steps_through_one_speakers_clips() {
        let (a, b): (SharedString, SharedString) = ("a".into(), "b".into());
        let three: Vec<SampleClip> = (0..3).map(|n| clip(f64::from(n) * 10.0, "x")).collect();
        let four: Vec<SampleClip> = (0..4).map(|n| clip(f64::from(n) * 10.0, "y")).collect();
        let mut cursor = SampleCursor::default();
        assert_eq!(cursor.select(&a, &[]), None);
        assert!(!cursor.is_current(&a));
        assert_eq!(cursor.select(&a, &three), Some(0));
        assert_eq!(cursor.step(&a, &three, true), Some(1));
        assert_eq!(cursor.step(&a, &three, true), Some(2));
        assert_eq!(cursor.step(&a, &three, true), Some(0), "wraps around");
        assert_eq!(cursor.step(&a, &three, false), Some(2));
        // Play again keeps the clip stepped to.
        assert_eq!(cursor.select(&a, &three), Some(2));
        // Another speaker starts at their longest clip.
        assert_eq!(cursor.select(&b, &four), Some(0));
        assert!(cursor.is_current(&b) && !cursor.is_current(&a));
        assert_eq!(cursor.current_key(), Some(&b));
        assert_eq!(cursor.index(&a, 3), 0);
    }

    #[test]
    fn a_reload_follows_the_current_clip_or_forgets_it() {
        let key: SharedString = "a".into();
        let clips: Vec<SampleClip> = (0..4).map(|n| clip(f64::from(n) * 10.0, "x")).collect();
        let mut cursor = SampleCursor::default();
        assert!(cursor.reconcile(None), "nothing current: nothing to do");
        cursor.select(&key, &clips);
        assert_eq!(cursor.step(&key, &clips, true), Some(1));
        // Moved: an earlier candidate left; the cursor follows its clip.
        let moved = vec![clips[1].clone(), clips[2].clone(), clips[3].clone()];
        assert!(cursor.reconcile(Some(&moved)));
        assert_eq!(cursor.index(&key, moved.len()), 0);
        assert_eq!(cursor.step(&key, &moved, true), Some(1));
        assert_eq!(moved[1].start, 20.0);
        // Removed: the cursor forgets it; the next play starts at the longest.
        let removed = vec![clips[0].clone(), clips[3].clone()];
        assert!(!cursor.reconcile(Some(&removed)));
        assert!(!cursor.is_current(&key));
        assert_eq!(cursor.index(&key, removed.len()), 0);
        // The speaker is gone.
        cursor.select(&key, &removed);
        assert!(!cursor.reconcile(None));
        assert_eq!(cursor.current_key(), None);
    }

    #[test]
    fn labels_name_the_position_and_the_line() {
        assert_eq!(sample_label(0, 5), "Sample 1 of 5 · longest");
        assert_eq!(sample_label(1, 5), "Sample 2 of 5");
        assert_eq!(sample_label(0, 1), "Sample");
        let c = clip(65.0, " Shall we start? ");
        assert_eq!(
            sample_line_text(1, 5, "Speaker 3", &c),
            "Sample 2 of 5 · 1:05 Speaker 3: Shall we start?"
        );
        assert_eq!(
            sample_aria(1, 5, "Speaker 3", &c),
            "Sample 2 of 5 of Speaker 3 at 1 minute 5 seconds: Shall we start?"
        );
    }
}
