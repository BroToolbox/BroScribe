use super::tests::{dismissed_at, named, open_review, open_review_with, with_audio};
use super::*;
use crate::views::person_picker::fixtures::{self, sid};
use broscribe_platform::PlayerState;
use gpui::{TestAppContext, WindowHandle};

fn rendered(
    window: WindowHandle<ReviewView>,
    cx: &mut TestAppContext,
) -> crate::ui::a11y_tree::Tree {
    cx.update_window(window.into(), |_, window, cx| {
        crate::ui::a11y_tree::draw_and_read(window, cx)
    })
    .unwrap()
}

fn focus_list(window: WindowHandle<ReviewView>, cx: &mut TestAppContext) {
    window
        .update(cx, |view, window, cx| view.focus_handle.focus(window, cx))
        .unwrap();
    rendered(window, cx);
}

#[cfg(test)]
#[gpui::test]
fn waiting_rows_paint_a_single_primary_with_selected_shortcuts_and_speaker_first(
    cx: &mut TestAppContext,
) {
    use gpui::base::test_support::{snapshots, texts};
    let (window, _app, _lib, _synthetic, _dir) = open_review_with(cx, with_audio);
    window
        .update(cx, |view, _, cx| {
            let mut duplicate = view.rows[1].clone();
            duplicate.entry.item.speaker_id = sid(4);
            view.rows.push(duplicate);
            cx.notify();
        })
        .unwrap();
    focus_list(window, cx);
    cx.simulate_keystrokes(window.into(), "down");
    cx.update_window(window.into(), |_, window, cx| {
        let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
        assert!(tree.all("review-keyboard-help").is_empty());
        assert_eq!(
            tree.read(tree.find("review-speaker-1")),
            ("Label", "Speaker 2")
        );
        assert_eq!(
            tree.read(tree.find("review-open-1")),
            ("Link", "Open Synthetic sync")
        );
        let row = &tree.nodes[tree.find("review-row-1")];
        assert!(
            row.name.starts_with("Speaker 2 in “Synthetic sync”,"),
            "{row:?}"
        );
        assert!(
            row.name
                .contains("Suggested José Pereira, voice similarity 0.70"),
            "{row:?}"
        );

        assert_eq!(
            tree.read(tree.find("review-voice-score-1")),
            ("Label", "Voice similarity 0.70")
        );
        assert_eq!(
            tree.read(tree.find("review-no-suggestion-0")),
            ("Label", "No suggestion")
        );
        assert_eq!(
            tree.read(tree.find("review-accept-1")),
            (
                "Button",
                "Accept José Pereira as Speaker 2 in “Synthetic sync”"
            )
        );
        assert_eq!(
            tree.nodes[tree.find("review-accept-1")].description,
            "BroScribe learns this voice from the speaker’s samples"
        );
        assert_eq!(
            tree.read(tree.find("review-dismiss-1")),
            ("Button", "Dismiss Speaker 2 from review without naming it")
        );
        let controls = snapshots(window);
        let bounds = |id: &str| {
            controls
                .iter()
                .find(|node| {
                    node.path()
                        .last()
                        .is_some_and(|part| part.to_string() == id)
                })
                .unwrap_or_else(|| panic!("missing {id}"))
                .bounds()
        };
        let play_id = controls
            .iter()
            .find(|node| {
                node.role() == Some(Role::Button)
                    && node
                        .path()
                        .iter()
                        .any(|part| part.to_string() == "review-row-1")
                    && node.label().is_some_and(|name| name.starts_with("Play "))
            })
            .unwrap()
            .bounds();
        let ids = [
            "review-accept-1",
            "review-reject-1",
            "review-name-1",
            "review-dismiss-1",
        ];
        let actions = ids.map(bounds);
        assert!(bounds("review-proposal-1").bottom() <= actions[0].top());
        assert!(play_id.left() < actions[0].left());
        assert!(
            actions
                .windows(2)
                .all(|pair| pair[0].left() < pair[1].left())
        );
        let painted = texts(window);
        let inside = |id: &str, text: &str| {
            painted.iter().any(|label| {
                label.text() == text && label.path().iter().any(|part| part.to_string() == id)
            })
        };
        assert!(inside("review-voice-score-1", "voice 0.70"));
        let hint = Kbd::format(&Keystroke::parse("r").unwrap());
        assert!(inside("review-reject-1", &hint));
        assert!(!inside("review-reject-3", &hint));
        let scale = window.scale_factor();
        let row_bounds = actions[0];
        assert!(
            window.painted_quads().iter().any(|quad| {
                quad.border_widths.bottom == px(1.).scale(scale)
                    && quad.border_widths.top == px(0.).scale(scale)
                    && quad.bounds.size.width > (row_bounds.size.width * 2.).scale(scale)
            }),
            "expected a hairline row separator"
        );
        assert_eq!(
            kit::painted_primary_actions(window, cx),
            ["review-accept-1"]
        );
    })
    .unwrap();
}

#[cfg(test)]
#[gpui::test]
fn review_loading_and_secondary_sections_keep_the_waiting_list_independent(
    cx: &mut TestAppContext,
) {
    use crate::ui::kit::dialog_gate::click;
    use broscribe_core::live_identity::{LiveOutcome, LiveReviewItem};

    let (window, _app, _lib, s, _dir) = open_review(cx);
    let mut initial = None;
    window
        .update(cx, |view, _, cx| {
            initial = Some(std::mem::take(&mut view.rows));
            view.loading = true;
            view.loaded = false;
            cx.notify();
        })
        .unwrap();
    let loading = rendered(window, cx);
    assert_eq!(
        loading.read(loading.find("review-status")),
        ("Status", "Reading the review queue…")
    );
    window
        .update(cx, |view, _, cx| {
            view.loading = false;
            cx.notify();
        })
        .unwrap();
    let empty = rendered(window, cx);
    assert!(empty.all("review-live-heading").is_empty());
    assert_eq!(
        empty.read(empty.find("review-empty-title")).1,
        "No speakers to review"
    );

    window
        .update(cx, |view, _, cx| {
            view.rows = initial.take().unwrap();
            let mut quality = view.rows[0].entry.clone();
            quality.item.speaker_id = sid(9);
            quality.item.talk_s = 14.;
            view.without_transcript.push(quality);
            view.live.push(LiveReviewMeeting {
                meeting_id: s.meeting.clone(),
                title: Some("Synthetic sync".into()),
                started_at: view.rows[0].entry.item.started_at,
                recording_sha256: None,
                items: vec![LiveReviewItem {
                    turn_id: "synthetic-1".into(),
                    decision: 1,
                    outcome: LiveOutcome::Unmapped,
                    person_id: None,
                    track: "t0".into(),
                    from_s: 0.,
                    to_s: 1.,
                    mapped: false,
                    supported: Vec::new(),
                    lines: Vec::new(),
                    review_key: "synthetic-review".into(),
                }],
            });
            view.message = Some("A review update.".into());
            cx.notify();
        })
        .unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(1000.), px(1000.)));
    let closed = rendered(window, cx);
    assert!(closed.all("review-quality-list").is_empty());
    assert!(closed.all("review-coverage-hint").is_empty());
    let section_order = &closed.nodes[closed.find("review")].children;
    assert!(
        section_order
            .iter()
            .position(|node| *node == closed.find("review-header"))
            < section_order
                .iter()
                .position(|node| *node == closed.find("review-activity"))
    );
    assert!(closed.parent(closed.find("review-list")).is_some());
    let body = closed.parent(closed.find("review-list")).unwrap();
    let order = &closed.nodes[body].children;
    assert!(
        order.iter().position(|i| *i == closed.find("review-list"))
            < order.iter().position(|i| *i == closed.find("review-live"))
    );
    assert!(
        order.iter().position(|i| *i == closed.find("review-live"))
            < order
                .iter()
                .position(|i| *i == closed.find("review-quality"))
    );
    assert_eq!(
        closed.read(closed.find("review-live-heading")),
        ("Heading", "Names from recordings (1)")
    );
    assert_eq!(
        closed.read(closed.find("review-quality-heading")),
        ("Heading", "Voices without a transcript (1 · 14 s)")
    );
    click(&mut visual, "review-quality-toggle", Role::Button);
    visual.update(|window, cx| {
        let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
        let waiting = tree.subtree(tree.find("review-list"));
        assert!(!waiting.contains(&tree.find("review-quality-speaker-0")));
        assert_eq!(tree.read(tree.find("review-quality-list")).0, "List");
        assert_eq!(
            tree.read(tree.find("review-quality-speaker-0")).0,
            "ListItem"
        );
        assert_eq!(tree.read(tree.find("review-live-open-0")).0, "Button");
    });
    window
        .update(cx, |view, _, cx| {
            view.rows.clear();
            cx.notify();
        })
        .unwrap();
    let quality_only = rendered(window, cx);
    assert_eq!(
        quality_only.read(quality_only.find("review-status")).1,
        "No speakers to review · 1 voice without a transcript"
    );
}

#[cfg(test)]
#[gpui::test]
fn waiting_keyboard_moves_active_descendant_by_pages_and_enters_name(cx: &mut TestAppContext) {
    let (window, _app, lib, synthetic, _dir) = open_review(cx);
    window
        .update(cx, |view, _, cx| {
            let mut extra = view.rows[0].clone();
            for i in 4..=12 {
                extra.entry.item.speaker_id = sid(i);
                view.rows.push(extra.clone());
            }
            cx.notify();
        })
        .unwrap();
    cx.simulate_window_resize(window.into(), gpui::size(px(960.), px(640.)));
    focus_list(window, cx);
    for (key, ix) in [("pagedown", 5), ("end", 11), ("pageup", 6), ("home", 0)] {
        cx.simulate_keystrokes(window.into(), key);
        cx.run_until_parked();
        let tree = rendered(window, cx);
        assert_eq!(tree.read(tree.find("review-list")).0, "ListBox");
        assert_eq!(
            tree.read(tree.find(&format!("review-row-{ix}"))).0,
            "ListBoxOption"
        );
        window
            .update(cx, |view, window, _| {
                assert_eq!(view.selected, ix);
                assert!(view.focus_handle.is_focused(window));
                let raw: serde_json::Value =
                    serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
                let descendant = raw["active_descendant_focus"].as_str().unwrap();
                assert_eq!(
                    raw["nodes"][descendant]["element_id"],
                    format!("NamedInteger(\"review-row\", {ix})")
                );
                let row = view.row_bounds.bounds_for_item(ix).unwrap();
                assert!(
                    row.intersects(&view.scroll.bounds()),
                    "{row:?} outside {:?}",
                    view.scroll.bounds()
                );
            })
            .unwrap();
    }
    cx.simulate_keystrokes(window.into(), "enter");
    cx.run_until_parked();
    window
        .update(cx, |view, _, _| {
            assert_eq!(view.naming.as_ref().unwrap().key.1, sid(1));
        })
        .unwrap();
    assert_eq!(named(&lib, &synthetic, 1), (Assignment::None, None));
}

#[cfg(test)]
#[gpui::test]
fn review_write_feedback_stays_on_its_row_and_unreadable_decisions_retry(cx: &mut TestAppContext) {
    use crate::ui::kit::dialog_gate::click;
    use gpui::base::test_support::snapshots;
    let (window, _app, lib, synthetic, _dir) = open_review(cx);
    focus_list(window, cx);
    window
        .update(cx, |view, _, cx| {
            view.saving = Some((view.rows[0].key(), Decision::Dismiss));
            cx.notify();
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "n");
    cx.update_window(window.into(), |_, window, cx| {
        let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("review-status")).1,
            "3 speakers to review"
        );
        assert_eq!(
            tree.read(tree.find("review-saving-0")),
            ("Status", "Saving…")
        );
        assert!(tree.all("review-saving-1").is_empty());
        assert_eq!(
            tree.read(tree.find("review-message")).1,
            "Wait for the current change to finish."
        );
        assert!(
            tree.read(tree.find("review-dismiss-0"))
                .1
                .starts_with("Dismissing…")
        );
        let controls = snapshots(window);
        for id in [
            "review-name-0",
            "review-dismiss-0",
            "review-accept-1",
            "review-reject-1",
        ] {
            let node = controls
                .iter()
                .find(|node| {
                    node.path()
                        .last()
                        .is_some_and(|part| part.to_string() == id)
                })
                .unwrap();
            assert_eq!(node.disabled(), Some(true), "{id}");
        }
    })
    .unwrap();
    window
        .update(cx, |view, _, cx| {
            view.saving = None;
            view.rows[0].fence = None;
            cx.notify();
        })
        .unwrap();
    focus_list(window, cx);
    cx.simulate_keystrokes(window.into(), "enter d");
    assert!(!dismissed_at(&lib, &synthetic, 1));
    let tree = rendered(window, cx);
    assert_eq!(
        tree.read(tree.find("review-unreadable-0")),
        ("Alert", "Can’t read this meeting")
    );
    assert_eq!(
        tree.nodes[tree.find("review-name-0")].description,
        "Can’t read this meeting"
    );
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    click(&mut visual, "review-retry-0", Role::Button);
    window
        .update(&mut visual, |view, _, _| {
            assert!(view.rows[0].fence.is_some())
        })
        .unwrap();
}

#[cfg(test)]
#[gpui::test]
fn dismissed_tab_plays_and_restores_with_focus_on_dismissed(cx: &mut TestAppContext) {
    use crate::ui::kit::dialog_gate::click;
    let (window, app, lib, synthetic, _dir) = open_review_with(cx, with_audio);
    app.update(cx, |state, _| {
        state.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
            60.,
        )));
    });
    focus_list(window, cx);
    cx.simulate_keystrokes(window.into(), "d");
    cx.run_until_parked();
    let tree = rendered(window, cx);
    assert!(
        tree.nodes
            .iter()
            .any(|node| node.role == "Tab" && node.name == "Identify (2)")
    );
    assert!(
        tree.nodes
            .iter()
            .any(|node| node.role == "Tab" && node.name == "Dismissed (1)")
    );
    // The selected tab is the tab stop. Right moves focus, then Enter selects.
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    let focused_tab = |visual: &mut gpui::VisualTestContext| {
        visual.update(|window, _| {
            gpui::base::test_support::snapshots(window)
                .into_iter()
                .filter(|node| node.role() == Some(Role::Tab))
                .find(|node| node.focused() == Some(true))
                .and_then(|node| node.label().map(str::to_owned))
        })
    };
    visual.update(|window, cx| window.blur(cx));
    crate::ui::kit::dialog_gate::draw(&mut visual);
    visual.update(|window, cx| window.focus_next(cx));
    crate::ui::kit::dialog_gate::draw(&mut visual);
    assert_eq!(focused_tab(&mut visual).as_deref(), Some("Identify (2)"));
    visual.simulate_keystrokes("right");
    crate::ui::kit::dialog_gate::draw(&mut visual);
    assert_eq!(focused_tab(&mut visual).as_deref(), Some("Dismissed (1)"));
    visual.simulate_keystrokes("enter");
    visual.simulate_event(gpui::KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    visual.run_until_parked();
    let dismissed = rendered(window, cx);
    assert!(dismissed.all("review-list").is_empty());
    assert_eq!(
        dismissed.read(dismissed.find("review-open-0")),
        ("Link", "Open Synthetic sync")
    );
    assert!(
        dismissed
            .read(dismissed.find("review-dismissed-date-0"))
            .1
            .starts_with("Dismissed ")
    );
    let play = dismissed
        .nodes
        .iter()
        .find(|node| node.name.starts_with("Play ") && node.name.contains("Speaker 1"))
        .and_then(|node| node.element_id.clone())
        .unwrap();
    click(&mut visual, &play, Role::Button);
    assert!(matches!(
        app.update(&mut visual, |state, _| state.player_mut().unwrap().poll()),
        PlayerState::Playing { .. }
    ));
    click(&mut visual, "review-restore-0", Role::Button);
    assert!(!dismissed_at(&lib, &synthetic, 1));
    visual.update(|window, cx| {
        let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("review-empty-title")).1,
            "No dismissed speakers"
        );
        assert_eq!(
            tree.read(tree.find("review-message")).1,
            "Speaker 1 in “Synthetic sync” is back in the review queue."
        );
        assert!(tree.all("review-undo").is_empty());
    });
}

#[cfg(test)]
#[gpui::test]
fn review_keyboard_undo_survives_ten_seconds_and_clears_on_leaving(cx: &mut TestAppContext) {
    let (window, app, lib, synthetic, _dir) = open_review(cx);
    focus_list(window, cx);
    cx.simulate_keystrokes(window.into(), "down d");
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(11));
    cx.run_until_parked();
    assert!(dismissed_at(&lib, &synthetic, 2));
    let tree = rendered(window, cx);
    assert!(tree.read(tree.find("review-undo")).1.contains("Speaker 2"));
    cx.simulate_keystrokes(window.into(), "secondary-z");
    cx.run_until_parked();
    assert!(!dismissed_at(&lib, &synthetic, 2));
    window
        .update(cx, |view, _, _| {
            assert_eq!(view.rows[view.selected].key().1, sid(2))
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "d");
    cx.run_until_parked();
    app.update(cx, |state, cx| state.navigate(Route::people(), cx));
    cx.run_until_parked();
    assert!(rendered(window, cx).all("review-undo").is_empty());
}

#[cfg(test)]
#[gpui::test]
fn review_menus_share_visible_commands_and_keep_restore_fenced(cx: &mut TestAppContext) {
    let (window, _, lib, synthetic, _dir) = open_review_with(cx, with_audio);
    cx.update(|cx| {
        let keys = crate::ui::keymap::key_bindings_for(broscribe_platform::HostPlatform::Linux, cx);
        cx.bind_keys(keys);
    });
    focus_list(window, cx);
    cx.simulate_keystrokes(window.into(), "down shift-f10");
    cx.run_until_parked();
    let waiting = rendered(window, cx);
    let items = |tree: &crate::ui::a11y_tree::Tree| {
        tree.nodes
            .iter()
            .filter(|node| node.role == "MenuItem")
            .map(|node| node.name.clone())
            .collect::<Vec<_>>()
    };
    let names = items(&waiting);
    for label in [
        "Accept José Pereira",
        "Reject José Pereira",
        "Name…",
        "Can't tell",
        "Play sample",
        "Open meeting",
    ] {
        assert!(names.iter().any(|name| name == label), "{label}: {names:?}");
    }
    cx.simulate_keystrokes(window.into(), "escape");
    window
        .update(cx, |view, window, _| {
            assert!(view.focus_handle.is_focused(window))
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "d");
    cx.run_until_parked();
    assert!(dismissed_at(&lib, &synthetic, 2));
    window
        .update(cx, |view, _, cx| view.set_dismissed_tab(true, cx))
        .unwrap();
    focus_list(window, cx);
    let row = window
        .update(cx, |view, _, _| view.row_bounds.bounds_for_item(0).unwrap())
        .unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_mouse_down(
        row.origin + gpui::point(px(10.), px(10.)),
        gpui::MouseButton::Right,
        gpui::Modifiers::none(),
    );
    let names = items(&rendered(window, cx));
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.iter().any(|name| name == "Restore"));
    assert!(names.iter().any(|name| name == "Open meeting"));
    // A menu surviving a refresh must keep the original fingerprint.
    window
        .update(cx, |view, _, cx| {
            view.dismissed[0].fence = None;
            cx.notify();
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "down enter");
    cx.run_until_parked();
    assert!(dismissed_at(&lib, &synthetic, 2));
    let message = rendered(window, cx);
    assert!(
        message
            .read(message.find("review-message"))
            .1
            .contains("speaker changed")
    );
}

#[cfg(test)]
#[gpui::test]
fn shell_review_link_and_edit_undo_keep_the_row_and_history(cx: &mut TestAppContext) {
    use crate::ui::kit::dialog_gate::click;
    use broscribe_platform::fakes::{FakeMicCapture, FakeSystemInfo};

    let dir = tempfile::tempdir().unwrap();
    let (state, lib, synthetic) = fixtures::state(cx, dir.path());
    lib.write(|tx| {
        let mut meeting = tx.library().load_meeting(&synthetic.meeting)?;
        meeting.title = Some("Synthetic: sync".into());
        tx.save_meeting(&meeting)?;
        Index::rebuild(tx)?;
        Ok(())
    })
    .unwrap();
    let main = cx.update(|cx| {
        crate::ui::theme::init(cx);
        crate::menu::install_app_menu(Some(&state), |_, _| {}, cx);
        crate::ui::open_main_window(
            &state,
            &crate::views::setup::SetupDeps {
                mic: Arc::new(FakeMicCapture::new(16_000)),
                system: Arc::new(FakeSystemInfo::default()),
            },
            cx,
        )
        .unwrap()
    });
    state.update(cx, |state, cx| state.navigate(Route::ReviewQueue, cx));
    cx.run_until_parked();
    let mut visual = gpui::VisualTestContext::from_window(main.into(), cx);
    visual.update(|window, _| window.activate_window());
    visual.simulate_resize(gpui::size(px(1100.), px(760.)));
    crate::ui::kit::dialog_gate::draw(&mut visual);
    visual.simulate_keystrokes("down d");
    visual.run_until_parked();
    assert!(dismissed_at(&lib, &synthetic, 2));
    visual.update(|window, cx| {
        let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("review-message")),
            ("Status", "Speaker 2 in “Synthetic: sync”: dismissed.")
        );
        let menu = cx
            .get_menus()
            .unwrap()
            .into_iter()
            .find(|menu| menu.name == "Edit")
            .unwrap();
        let action = menu
            .items
            .into_iter()
            .find_map(|item| match item {
                gpui::OwnedMenuItem::Action {
                    name,
                    action,
                    disabled: false,
                    ..
                } if name == "Undo Can’t Tell" => Some(action),
                _ => None,
            })
            .expect("the active review publishes Edit › Undo Can’t Tell");
        window.dispatch_action(action, cx);
    });
    visual.run_until_parked();
    assert!(!dismissed_at(&lib, &synthetic, 2));
    click(&mut visual, "review-open-1", Role::Link);
    assert_eq!(
        state.read_with(&visual, |state, _| state.route().clone()),
        Route::meeting(synthetic.meeting.clone())
    );
    visual.update(|window, cx| window.dispatch_action(Box::new(crate::menu::NavigateBack), cx));
    visual.run_until_parked();
    assert!(state.read_with(&visual, |state, _| *state.route() == Route::ReviewQueue));
    visual.update(|window, cx| {
        let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
        let raw: serde_json::Value =
            serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
        let selected = raw["active_descendant_focus"].as_str().unwrap();
        assert_eq!(
            raw["nodes"][selected]["element_id"],
            "NamedInteger(\"review-row\", 1)"
        );
        assert_eq!(tree.read(tree.find("review-open-1")).0, "Link");
    });
}

#[gpui::test]
fn review_identity_buttons_undo_and_redo_an_accepted_name(cx: &mut TestAppContext) {
    use crate::ui::kit::dialog_gate::click;
    let (window, _app, lib, s, _dir) = open_review(cx);
    let before = named(&lib, &s, 2);
    window
        .update(cx, |view, window, cx| {
            view.select(1, cx);
            view.accept_selected(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        named(&lib, &s, 2),
        (Assignment::Confirmed, Some(s.jose.clone()))
    );

    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    click(&mut visual, "review-identity-undo", Role::Button);
    assert_eq!(named(&lib, &s, 2), before);
    click(&mut visual, "review-identity-redo", Role::Button);
    assert_eq!(named(&lib, &s, 2), (Assignment::Confirmed, Some(s.jose)));
}
