use broscribe_core::ops::{IdentityCheckDecision, IdentityChecks};
use gpui::{Div, ParentElement as _, Styled as _, component::button::ButtonVariants as _};

use super::{
    Context, ReviewView, Route, SharedString, button, core_user_text, div, failure_text, kit,
    label_of, playback_of,
};
use crate::ui::{heading, kit::StatusKind};
use crate::views::people::{Clip, clip_of};

pub(super) type Snapshot = Option<std::result::Result<IdentityChecks, String>>;

impl ReviewView {
    pub(super) fn stop_identity_check_sample(&mut self, cx: &mut Context<Self>) {
        if self
            .playback
            .active()
            .is_some_and(|key| key.starts_with("identity-check/"))
        {
            self.playback.stop(&self.state, cx);
        }
    }

    pub(super) fn render_identity_checks(&self, cx: &Context<Self>) -> Option<Div> {
        let checks = match self.identity_checks.as_ref()? {
            Ok(checks) if !checks.alerts.is_empty() || checks.unreadable_meetings > 0 => checks,
            Ok(_) => return None,
            Err(error) => {
                return Some(div().px_3().child(kit::retry_line(
                    "identity-checks-error",
                    failure_text("Couldn't check confirmed identifications.", error, ""),
                    cx,
                    |this: &mut Self, _, cx| this.reload(cx),
                )));
            }
        };
        let enabled = self.saving.is_none() && !self.identity.saving;
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .child(heading("identity-checks-heading", "Confirmed names to check"))
                .children((checks.unreadable_meetings > 0).then(|| {
                    kit::status_line(
                        "identity-checks-incomplete",
                        StatusKind::Info,
                        crate::ui::format::count(
                            checks.unreadable_meetings as u64,
                            "meeting could not be checked",
                            "meetings could not be checked",
                        ),
                        cx,
                    )
                }))
                .children(checks.alerts.iter().enumerate().map(|(ix, check)| {
                    let decision = check.decision.clone();
                    let meeting = decision.meeting_id.clone();
                    let person = &decision.person_id;
                    let key = format!("identity-check/{}/{}/{}", meeting, decision.speaker_id, decision.revision);
                    let label = label_of(decision.speaker_id, check.meeting_title.as_deref());
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(heading(("identity-check-label", ix), format!("{} · {label}", check.person_name)))
                        .child(crate::ui::text_line(
                            ("identity-check-reason", ix),
                            gpui::Role::Label,
                            "This voice sounds different from the person’s other confirmed samples.",
                        ))
                        .child(
                            div().flex().flex_wrap().items_center().gap_2()
                                .children(check.occurrence_sample.as_ref().and_then(|src| clip_of(person, src))
                                    .map(|clip| self.check_sample(("identity-check-occurrence", ix), format!("{key}/occurrence").into(), "Play meeting voice", clip, cx)))
                                .children(clip_of(person, &check.reference_sample)
                                    .map(|clip| self.check_sample(("identity-check-reference", ix), format!("{key}/reference").into(), "Play comparison voice", clip, cx)))
                                .child(button(("identity-check-keep", ix), "Keep identification", enabled, cx,
                                    move |this: &mut Self, _, cx| this.keep_identification(decision.clone(), cx)).outline())
                                .child(button(("identity-check-meeting", ix), "Open meeting", enabled, cx,
                                    move |this: &mut Self, _, cx| {
                                        this.playback.stop(&this.state, cx);
                                        this.state.update(cx, |state, cx| state.navigate(Route::meeting(meeting.clone()), cx));
                                    }).link()),
                        )
                })),
        )
    }

    fn check_sample(
        &self,
        id: (&'static str, usize),
        key: SharedString,
        label: &'static str,
        clip: Clip,
        cx: &Context<Self>,
    ) -> Div {
        let playing = self.playback.is_playing(&key);
        let playing_key = key.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                button(
                    id,
                    if playing { "Stop sample" } else { label },
                    !self.identity.saving,
                    cx,
                    move |this: &mut Self, _, cx| {
                        this.cursor = Default::default();
                        let state = this.state.clone();
                        this.playback.toggle(
                            playing_key.clone(),
                            clip.clone(),
                            &state,
                            cx,
                            playback_of,
                        );
                    },
                )
                .outline(),
            )
            .children(self.playback.status_of(&key).map(|status| {
                kit::status_line(
                    SharedString::from(format!("identity-check-sample-status/{key}")),
                    StatusKind::Info,
                    status.to_owned(),
                    cx,
                )
            }))
    }

    fn keep_identification(&mut self, decision: IdentityCheckDecision, cx: &mut Context<Self>) {
        if self.saving.is_some() || self.identity.saving {
            self.wait_for_write(cx);
            return;
        }
        self.identity.saving = true;
        self.message = Some("Saving identification…".into());
        self.message_kind = StatusKind::Pending;
        self.playback.stop(&self.state, cx);
        let app = self.state.read(cx);
        let lib = app.library().clone();
        let model = app.config().profile.speaker_embedding.clone();
        let thresholds = app.config().thresholds(&model).copied();
        let task = cx.background_executor().spawn(async move {
            lib.write(|tx| {
                broscribe_core::ops::keep_identification(tx, &decision, &model, thresholds.as_ref())
            })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |this, cx| {
                this.identity.saving = false;
                match result {
                    Ok(()) => {
                        this.message = Some("Identification kept.".into());
                        this.message_kind = StatusKind::Done;
                    }
                    Err(e) => {
                        this.message = Some(failure_text(
                            "Couldn't keep the identification.",
                            &core_user_text(&e),
                            "",
                        ));
                        this.message_kind = StatusKind::Failed;
                    }
                }
                this.reload(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}
