use broscribe_core::config::{Config, SpeakerGrouping, SpeakerGroupingPreset};
use gpui::component::input::{Input, InputEvent, InputState};

use super::{ChoiceGroup, SettingsView, core_user_text, error_line, kit, muted};
use gpui::component::ActiveTheme as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Styled as _, Subscription, Window, div, rems,
};

use crate::ui::format::threshold_percent;

const CONTROL: &str = "speaker-grouping";

pub(super) struct SpeakerGroupingEditor {
    model: String,
    saved: SpeakerGrouping,
    custom_open: bool,
    custom_focus: FocusHandle,
    suggest: Entity<InputState>,
    automatic: Entity<InputState>,
}

impl SpeakerGroupingEditor {
    pub(super) fn new(config: &Config, window: &mut Window, cx: &mut App) -> Self {
        let model = config.profile.speaker_embedding.clone();
        let saved = config.speaker_grouping(&model);
        let suggest = cx.new(|cx| InputState::new(window, cx).default_value(percent(saved.merge)));
        let automatic =
            cx.new(|cx| InputState::new(window, cx).default_value(percent(saved.auto_merge)));
        Self {
            model,
            saved,
            custom_open: saved.preset == SpeakerGroupingPreset::Custom,
            custom_focus: cx.focus_handle(),
            suggest,
            automatic,
        }
    }

    pub(super) fn subscribe(
        &self,
        window: &Window,
        cx: &mut Context<SettingsView>,
    ) -> [Subscription; 2] {
        [&self.suggest, &self.automatic].map(|input| {
            cx.subscribe_in(input, window, |view, _, event, _, cx| match event {
                InputEvent::PressEnter { .. } => view.save_custom_speaker_grouping(cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            })
        })
    }

    pub(super) fn sync(
        &mut self,
        model: String,
        saved: SpeakerGrouping,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) {
        if self.model == model && self.saved == saved {
            return;
        }
        self.model = model;
        self.saved = saved;
        self.reset(window, cx);
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<SettingsView>) {
        self.custom_open = self.saved.preset == SpeakerGroupingPreset::Custom;
        self.suggest.update(cx, |input, cx| {
            input.set_value(percent(self.saved.merge), window, cx)
        });
        self.automatic.update(cx, |input, cx| {
            input.set_value(percent(self.saved.auto_merge), window, cx);
        });
    }

    fn custom(&self, cx: &App) -> Result<SpeakerGrouping, String> {
        SpeakerGrouping::custom(
            parse_percent(&self.suggest.read(cx).value(), "Suggest duplicates")?,
            parse_percent(&self.automatic.read(cx).value(), "Merge automatically")?,
        )
        .map_err(|err| core_user_text(&err))
    }
}

fn percent(score: Option<f32>) -> String {
    score.map_or_else(String::new, |value| (value * 100.0).to_string())
}

fn parse_percent(input: &str, label: &str) -> Result<Option<f32>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    input
        .trim_end_matches('%')
        .trim_end()
        .replace(',', ".")
        .parse()
        .map(Some)
        .map_err(|_| format!("Enter a percentage from 0 to 100 for {label}."))
}

impl SettingsView {
    fn save_speaker_grouping(&mut self, grouping: SpeakerGrouping, cx: &mut Context<Self>) {
        let model = self.speaker_grouping.model.clone();
        self.apply_setting(
            CONTROL,
            move |config| config.set_speaker_grouping(&model, grouping),
            cx,
        );
    }

    fn save_custom_speaker_grouping(&mut self, cx: &mut Context<Self>) {
        if self.speaker_grouping.custom_open
            && let Ok(grouping) = self.speaker_grouping.custom(cx)
        {
            self.save_speaker_grouping(grouping, cx);
        }
    }

    fn cancel_custom_speaker_grouping(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving.contains(CONTROL) {
            return;
        }
        let editor = &mut self.speaker_grouping;
        editor.reset(window, cx);
        editor.custom_focus.focus(window, cx);
        self.setting_notices.remove(CONTROL);
        cx.notify();
    }

    pub(super) fn render_speaker_grouping(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = &self.speaker_grouping;
        let busy = self.saving.contains(CONTROL);
        let selected = if editor.custom_open {
            SpeakerGroupingPreset::Custom
        } else {
            editor.saved.preset
        };
        let mut choices = ChoiceGroup::new(kit::stack(), CONTROL, "Speaker grouping");
        for (id, label, grouping) in [
            (
                "speaker-grouping-standard",
                "Standard",
                SpeakerGrouping::STANDARD,
            ),
            (
                "speaker-grouping-conservative",
                "Conservative",
                SpeakerGrouping::CONSERVATIVE,
            ),
        ] {
            let suggestion = threshold_percent(grouping.merge.expect("preset suggestion"));
            let automatic = threshold_percent(grouping.auto_merge.expect("preset automatic merge"));
            choices = choices.option(kit::radio_option(
                id,
                format!("{label} · suggest at {suggestion}, merge at {automatic}"),
                selected == grouping.preset,
                !busy,
                cx,
                move |view, window, cx| {
                    if view.speaker_grouping.saved == grouping {
                        view.speaker_grouping.reset(window, cx);
                    }
                    view.save_speaker_grouping(grouping, cx);
                },
            ));
        }
        choices = choices.option(
            kit::radio_option(
                "speaker-grouping-custom",
                "Custom",
                selected == SpeakerGroupingPreset::Custom,
                !busy,
                cx,
                |view, _, cx| {
                    view.speaker_grouping.custom_open = true;
                    view.setting_notices.remove(CONTROL);
                    cx.notify();
                },
            )
            .track_focus(&editor.custom_focus),
        );

        kit::field_heading(
            "settings-speaker-grouping-title",
            "Speaker grouping",
            Some((
                "settings-speaker-grouping-note".into(),
                "Controls when two similar voices in the same meeting are suggested as duplicates or merged after processing.".into(),
            )),
            |_| choices,
            cx,
        )
        .when(editor.custom_open, |section| {
            let custom = editor.custom(cx);
            let fields = [
                ("speaker-grouping-suggest-label", "Suggest duplicates (%)", &editor.suggest),
                ("speaker-grouping-auto-label", "Merge automatically (%)", &editor.automatic),
            ];
            section.child(
                kit::stack()
                    .id("speaker-grouping-editor")
                    .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                            view.cancel_custom_speaker_grouping(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .p_3()
                    .child(muted(
                        "speaker-grouping-custom-help",
                        "Lower percentages include more pairs. Enter a value from 0 to 100, or leave a field blank to turn off that action.",
                        cx,
                    ))
                    .children(fields.into_iter().map(|(id, label, input)| {
                        kit::field(
                            id,
                            label,
                            None,
                            |label| div().w_full().max_w(rems(10.)).child(Input::new(input).aria_label(label).disabled(busy)),
                            cx,
                        )
                    }))
                    .child(div().flex().flex_wrap().gap(kit::CONTROL_GAP)
                        .child(kit::button(
                            "speaker-grouping-save",
                            "Save thresholds",
                            !busy && custom.is_ok(),
                            cx,
                            |view: &mut SettingsView, _, cx| view.save_custom_speaker_grouping(cx),
                        ))
                        .child(kit::button(
                            "speaker-grouping-cancel",
                            "Cancel edits",
                            !busy,
                            cx,
                            SettingsView::cancel_custom_speaker_grouping,
                        ))
                    )
                    .when_some(custom.err(), |panel, error| {
                        panel.child(error_line("speaker-grouping-validation", error, cx))
                    }),
            )
        })
        .children(self.setting_notice(CONTROL, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{settings_root_window_with, synthetic_cli};
    use super::*;
    use crate::state::SettingsPage;
    use gpui::{Focusable as _, TestAppContext};

    #[gpui::test]
    fn custom_draft_escape_restores_settings_and_radio_focus(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut initial = Config::default();
        let model = initial.profile.speaker_embedding.clone();
        initial.set_speaker_grouping(&model, SpeakerGrouping::STANDARD);
        let cli = synthetic_cli(dir.path());
        let (state, view, visual) = settings_root_window_with(cx, dir.path(), initial, cli);
        visual.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_page(SettingsPage::Transcription, window, cx);
                view.speaker_grouping.custom_open = true;
                view.speaker_grouping.suggest.update(cx, |input, cx| {
                    input.set_value("65", window, cx);
                });
                cx.notify();
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let input = &view.read(cx).speaker_grouping.suggest;
            input.focus_handle(cx).focus(window, cx);
        });
        visual.simulate_keystrokes("escape");
        visual.run_until_parked();
        visual.update(|window, cx| {
            let editor = &view.read(cx).speaker_grouping;
            assert!(!editor.custom_open);
            assert!(editor.custom_focus.is_focused(window));
            assert_eq!(&*editor.suggest.read(cx).value(), "50");
            let data_dir = &state.read(cx).paths().data_dir;
            assert_eq!(
                Config::load(data_dir).unwrap().speaker_grouping(&model),
                SpeakerGrouping::STANDARD
            );
        });
    }
}
