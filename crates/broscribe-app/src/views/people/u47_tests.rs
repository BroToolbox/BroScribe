use super::*;
use crate::ui::a11y_tree::draw_and_read;
use crate::ui::kit::dialog_gate::{click, draw};
use broscribe_core::schema::{Meeting, RecordingState, Source, VoiceprintSource};
use broscribe_platform::fakes::{FakeAudioPlayer, FakeMicCapture, FakePaths, FakeSystemInfo};
use gpui::component::Root;
use gpui::{TestAppContext, VisualTestContext, WindowHandle};

fn setup_deps() -> crate::views::setup::SetupDeps {
    crate::views::setup::SetupDeps {
        mic: Arc::new(FakeMicCapture::new(16_000)),
        system: Arc::new(FakeSystemInfo::default()),
    }
}

fn save_people(lib: &Library, people: &[&Person]) {
    lib.write(|tx| {
        let mut index = Index::open(tx.root())?;
        for &person in people {
            tx.save_person(person)?;
            index.upsert_person(tx, person)?;
        }
        Ok(())
    })
    .unwrap();
}

fn state(dir: &Path, lib: Library, cli: PathBuf, cx: &mut TestAppContext) -> Entity<AppState> {
    let paths = broscribe_platform::resolve_paths(&FakePaths::new(dir)).unwrap();
    cx.update(|cx| {
        crate::ui::theme::init(cx);
        cx.bind_keys(crate::ui::keymap::key_bindings(cx));
    });
    cx.new(|cx| {
        let mut state = AppState::new(
            crate::test_support::app_init(paths, Default::default(), lib, cli),
            cx,
        );
        state.set_local_jobs_allowed(false, cx);
        state
    })
}

fn mounted(
    state: &Entity<AppState>,
    cx: &mut TestAppContext,
) -> (Entity<PeopleView>, WindowHandle<Root>) {
    let mut people = None;
    let window = cx.add_window(|window, cx| {
        let entity = cx.new(|cx| {
            let mut view = PeopleView::new(state.clone(), window, cx);
            view.setup_deps = Some(setup_deps());
            view
        });
        people = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    (people.unwrap(), window)
}

fn select(view: &Entity<PeopleView>, person: &Person, cx: &mut VisualTestContext) {
    view.update(cx, |view, cx| view.select(person.id.clone(), cx));
    cx.run_until_parked();
    draw(cx);
}

#[gpui::test]
fn people_rows_keep_the_height_of_all_metadata_when_the_list_scrolls(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let people: Vec<_> = (1..=40)
        .map(|ix| Person::new(PersonId::random(), format!("Moonberry Person {ix:02}")))
        .collect();
    save_people(&lib, &people.iter().collect::<Vec<_>>());
    let state = state(dir.path(), lib, dir.path().join("unused"), cx);
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1120.), gpui::px(720.)));
    visual.update(|_, cx| {
        crate::ui::apply_reading_size(broscribe_core::config::ReadingSize::Largest, cx);
    });
    draw(&mut visual);
    visual.update(|window, cx| {
        let list = &people.read(cx).list_scroll;
        let first = list.bounds_for_item(0).unwrap();
        let second = list.bounds_for_item(1).unwrap();
        let last = list.bounds_for_item(39).unwrap();
        assert!(first.size.height > window.rem_size() * 2.);
        assert!(first.bottom() <= second.top());
        assert!(last.bottom() > list.bounds().bottom());
    });
}

#[gpui::test]
fn people_list_search_keyboard_and_self_metadata_are_mounted(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let mut me = Person::new(PersonId::random(), "");
    me.is_self = true;
    let mut an = Person::new(
        PersonId::random(),
        "Anita Synthetic With An Exceptionally Long Display Name",
    );
    an.aliases.push("June Fiction".into());
    an.emails
        .push("anita.with.a.long.address@meetings.example.invalid".into());
    let zo = Person::new(PersonId::random(), "Zora Fiction");
    save_people(&lib, &[&me, &an, &zo]);
    let state = state(dir.path(), lib, dir.path().join("unused"), cx);
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1100.), gpui::px(760.)));
    visual.update(|window, cx| people.read(cx).focus_handle.clone().focus(window, cx));
    draw(&mut visual);
    for (key, expected) in [
        ("end", &zo.id),
        ("home", &me.id),
        ("down", &an.id),
        ("up", &me.id),
        ("z o", &zo.id),
    ] {
        visual.simulate_keystrokes(key);
        visual.run_until_parked();
        people.read_with(&visual, |view, _| {
            assert_eq!(view.selected.as_ref(), Some(expected))
        });
    }
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(tree.read(tree.find("people-list")), ("ListBox", "People"));
        assert_eq!(tree.read(tree.find("people-count")), ("Label", "3 people"));
        assert_eq!(
            tree.read(tree.find("person-row-0")),
            (
                "ListBoxOption",
                "Me, you, No email, Not named in a meeting yet, No voice yet",
            )
        );
        assert_eq!(
            tree.read(tree.find("person-row-1")).1,
            format!(
                "{}, {}, Not named in a meeting yet, No voice yet",
                an.name, an.emails[0],
            )
        );
        let raw: serde_json::Value =
            serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
        assert!(raw["active_descendant_focus"].is_string());
    });
    visual.update(|window, cx| people.update(cx, |view, cx| view.focus_search(window, cx)));
    visual.simulate_keystrokes("c a r l");
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("people-none")),
            ("Status", "Nobody matches “carl”.")
        );
    });
    visual.simulate_keystrokes("escape j u n e");
    people.read_with(&visual, |view, _| {
        assert_eq!(view.visible_rows, [1]);
        assert_eq!(view.selected.as_ref(), Some(&an.id));
    });
    visual.simulate_keystrokes("escape");
    people.read_with(&visual, |view, _| {
        assert_eq!(view.visible_rows.len(), 3);
        assert_eq!(view.selected.as_ref(), Some(&an.id));
    });
}

#[gpui::test]
fn empty_and_loading_people_and_detail_show_the_correct_header(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let state = state(dir.path(), lib.clone(), dir.path().join("unused"), cx);
    let (people, window) = mounted(&state, cx);
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| {
        people.update(cx, |view, _| view.list_loading = true);
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("people-list-loading")),
            ("Status", "Loading people…")
        );
        assert!(tree.all("people-empty-title").is_empty());
    });
    visual.run_until_parked();
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("people-empty-title")).1,
            "No people yet"
        );
    });

    let mut a = Person::new(PersonId::random(), "Amira Fiction");
    a.emails.push("amira@example.invalid".into());
    let b = Person::new(PersonId::random(), "Bela Fiction");
    save_people(&lib, &[&a, &b]);
    people.update(&mut visual, |view, cx| view.reload(cx));
    visual.run_until_parked();
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-detail-empty-title")).1,
            "No person selected"
        );
        people.update(cx, |view, cx| view.select(a.id.clone(), cx));
        let tree = draw_and_read(window, cx);
        assert_eq!(tree.read(tree.find("person-name")).1, "Amira Fiction");
        assert_eq!(
            tree.read(tree.find("person-summary")).1,
            "amira@example.invalid · Not named in a meeting yet · 0 s of speech"
        );
        assert_eq!(
            tree.read(tree.find("person-detail-loading")),
            ("Status", "Loading voice samples and meetings…")
        );
        assert_eq!(tree.read(tree.find("person-edit")).0, "Button");
        people.update(cx, |view, cx| view.select(b.id.clone(), cx));
    });
    visual.run_until_parked();
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(tree.read(tree.find("person-name")).1, "Bela Fiction");
        assert!(tree.all("person-detail-loading").is_empty());
        assert_eq!(tree.read(tree.find("person-samples-none")).1, "No voice samples yet. BroScribe learns Bela Fiction’s voice when you name them in a meeting.");
        assert!(tree.find("person-forget-section") != tree.find("person-detail"));
        let detail = tree.nodes[tree.find("person-detail")].children.clone();
        let meetings = tree.find("person-meetings-heading");
        let forget = tree.find("person-forget-heading");
        assert!(detail.iter().position(|id| *id == meetings).unwrap()
            < detail.iter().position(|id| *id == forget || tree.subtree(*id).contains(&forget)).unwrap());
    });
}

#[gpui::test]
fn people_editor_return_escape_and_draft_discard_are_fenced(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let a = Person::new(PersonId::random(), "Amira Fiction");
    let b = Person::new(PersonId::random(), "Bela Fiction");
    save_people(&lib, &[&a, &b]);
    let reply = r#"{"undo_id":"pu-u47"}"#;
    let cli = crate::test_support::write_cli(
        &dir.path().join("cli"),
        &format!(
            r#"case "$*" in *'people edit'*) cat >/dev/null; echo '{reply}';; *) echo '{{}}';; esac"#
        ),
        &format!(
            "if($all -clike '*people edit*'){{fx_drain_stdin;fx_echo {}}}else{{fx_echo '{{}}'}}",
            crate::test_support::ps_str(reply),
        ),
    );
    let state = state(dir.path(), lib, cli, cx);
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1150.), gpui::px(1040.)));
    select(&people, &a, &mut visual);
    click(&mut visual, "person-edit", Role::Button);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-editor")),
            ("Form", "Edit person")
        );
        assert_eq!(
            tree.read(tree.find("person-emails-help")),
            ("Label", "Separate with commas")
        );
        assert!(
            kit::painted_primary_actions(window, cx)
                .iter()
                .any(|id| id == "person-save")
        );
        people.update(cx, |view, cx| {
            view.editor
                .as_ref()
                .unwrap()
                .name
                .update(cx, |input, cx| input.set_value("", window, cx));
        });
    });
    visual.simulate_keystrokes("enter");
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-name-error")),
            ("Alert", "The name can't be empty.")
        );
        people.update(cx, |view, cx| {
            view.editor
                .as_ref()
                .unwrap()
                .name
                .update(cx, |input, cx| input.set_value("Amira Draft", window, cx));
            view.request_selection(b.id.clone(), false, window, cx);
            assert_eq!(view.selected.as_ref(), Some(&a.id));
            assert!(view.discard_focus.is_focused(window));
        });
    });
    draw(&mut visual);
    visual.simulate_keystrokes("enter");
    people.read_with(&visual, |view, _| assert!(view.pending_selection.is_none()));
    visual.update(|window, cx| {
        people.update(cx, |view, cx| {
            assert!(
                view.editor
                    .as_ref()
                    .unwrap()
                    .name
                    .focus_handle(cx)
                    .is_focused(window)
            );
            assert_eq!(
                view.editor.as_ref().unwrap().name.read(cx).value().as_ref(),
                "Amira Draft"
            );
            view.request_selection(b.id.clone(), false, window, cx);
        })
    });
    click(&mut visual, "person-discard-changes", Role::Button);
    people.read_with(&visual, |view, _| {
        assert_eq!(view.selected.as_ref(), Some(&b.id));
        assert!(view.editor.is_none());
    });

    click(&mut visual, "person-edit", Role::Button);
    visual.simulate_keystrokes("escape");
    people.read_with(&visual, |view, _| assert!(view.editor.is_none()));
    click(&mut visual, "person-edit", Role::Button);
    visual.update(|window, cx| {
        people.update(cx, |view, cx| {
            view.editor
                .as_ref()
                .unwrap()
                .name
                .update(cx, |input, cx| input.set_value("Bela Revised", window, cx));
        })
    });
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    people.read_with(&visual, |view, _| {
        assert_eq!(view.message.as_deref(), Some("Saved Bela Revised."));
        assert!(view.editor.is_none());
        assert!(view.undo.is_some());
    });
    visual.update(|window, cx| people.read(cx).detail_focus.clone().focus(window, cx));
    draw(&mut visual);
    visual.simulate_keystrokes("secondary-z");
    visual.run_until_parked();
    people.read_with(&visual, |view, _| {
        assert_eq!(
            view.message.as_deref(),
            Some("Undone: the previous details are back.")
        )
    });
}

#[gpui::test]
fn people_undo_expiry_clears_only_its_saved_message(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let state = state(dir.path(), lib, dir.path().join("unused"), cx);
    let (people, _) = mounted(&state, cx);
    let saved = "Saved Amira Fiction.".to_owned();
    for newer_error in [false, true] {
        people.update(cx, |view, cx| {
            view.undo = Some(PeopleView::offer_undo(
                PersonId::random(),
                "pu-expiry".into(),
                saved.clone(),
                cx,
            ));
            view.message_kind = if newer_error {
                StatusKind::Failed
            } else {
                StatusKind::Done
            };
            view.message = Some(if newer_error {
                "The latest operation failed.".into()
            } else {
                saved.clone()
            });
        });
        cx.run_until_parked();
        cx.executor().advance_clock(UNDO_WINDOW);
        cx.run_until_parked();
        people.read_with(cx, |view, _| {
            assert!(view.undo.is_none());
            assert_eq!(
                view.message.as_deref(),
                newer_error.then_some("The latest operation failed.")
            );
        });
    }
}

#[test]
fn person_conflict_copy_names_the_fields_and_keeps_the_draft() {
    assert_eq!(
        external_change_notice(&[PersonField::Emails]).as_deref(),
        Some(
            "Emails changed elsewhere while you were editing. Your changes to other fields will still save."
        )
    );
    assert_eq!(
        conflict_message(&[PersonField::Emails]),
        "Not saved: emails also changed elsewhere. Your text is still here. Cancel to see the current emails, then edit again."
    );
}

fn meeting_for(lib: &Library, person: &Person, n: u32) -> Meeting {
    use broscribe_core::schema::Participant;
    let date = DateTime::parse_from_rfc3339("2026-11-20T08:00:00Z").unwrap()
        + chrono::Duration::hours(i64::from(n));
    let mut meeting = Meeting::new(MeetingId::random(), Source::Import, date);
    meeting.title = Some(format!("Synthetic working session {n}"));
    meeting.duration_s = 420.;
    meeting.participants.push(Participant {
        name: Some(person.name.clone()),
        person_id: Some(person.id.clone()),
        ..Participant::default()
    });
    lib.write(|tx| {
        tx.create_meeting_dir(&meeting)?;
        tx.save_meeting(&meeting)?;
        Index::open(tx.root())?.upsert_meeting(tx, &meeting)
    })
    .unwrap();
    meeting
}

#[gpui::test]
fn person_meetings_open_with_keyboard_and_full_library_filter(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let person = Person::new(PersonId::random(), "Amira Fiction");
    save_people(&lib, &[&person]);
    let meetings: Vec<_> = (0..12).map(|i| meeting_for(&lib, &person, i)).collect();
    let state = state(dir.path(), lib, dir.path().join("unused"), cx);
    state.update(cx, |state, cx| {
        state.navigate(
            Route::People {
                person: Some(person.id.clone()),
            },
            cx,
        )
    });
    let mut window = None;
    cx.update(|cx| crate::ui::show_main_window(&mut window, &state, &setup_deps(), cx));
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.unwrap().into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1360.)));
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(tree.read(tree.find("person-meeting-0")).0, "ListItem");
        assert!(
            tree.read(tree.find("person-meeting-0"))
                .1
                .starts_with("Open Synthetic working session 11")
        );
        assert!(tree.all("person-meeting-10").is_empty());
        assert_eq!(
            tree.read(tree.find("person-meetings-all")),
            ("Link", "Show all 12 meetings in Library")
        );
    });
    let mut traversed = Vec::new();
    for _ in 0..40 {
        let current = visual.update(|window, cx| {
            draw_and_read(window, cx);
            let raw: serde_json::Value =
                serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
            raw["gpui_focus"]
                .as_str()
                .and_then(|id| raw["nodes"][id]["element_id"].as_str())
                .unwrap_or("")
                .to_owned()
        });
        traversed.push(current);
        if traversed.last().unwrap().contains("person-meeting") {
            break;
        }
        visual.update(|window, cx| window.focus_next(cx));
    }
    assert!(
        traversed
            .last()
            .is_some_and(|id| id.contains("person-meeting")),
        "Tab did not reach a meeting. Stops were {}",
        traversed.join(", ")
    );
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    state.read_with(&visual, |state, _| {
        assert_eq!(state.route(), &Route::meeting(meetings[11].id.clone()))
    });
    visual.simulate_keystrokes(
        if broscribe_platform_native::NativePlatform::host_platform()
            == broscribe_platform::HostPlatform::MacOs
        {
            "secondary-["
        } else {
            "alt-left"
        },
    );
    visual.run_until_parked();
    click(&mut visual, "person-meetings-all", Role::Link);
    state.read_with(&visual, |state, _| {
        assert_eq!(
            state.route(),
            &Route::LibraryPerson {
                person: person.id.clone()
            }
        );
        assert_eq!(
            state.search().query().person.as_deref(),
            Some(person.id.as_str())
        );
        assert_eq!(state.search().results().len(), 12);
    });
}

#[gpui::test]
fn forgetting_keeps_voice_focus_and_selects_the_next_or_previous_person(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let mut me = Person::new(PersonId::random(), "Zara Fiction");
    me.is_self = true;
    let a = Person::new(PersonId::random(), "Amira Fiction");
    let b = Person::new(PersonId::random(), "Bela Fiction");
    save_people(&lib, &[&me, &a, &b]);
    let state = state(dir.path(), lib.clone(), dir.path().join("unused"), cx);
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1150.), gpui::px(1040.)));
    select(&people, &me, &mut visual);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-record-voice")).1,
            "Record your voice"
        );
    });
    click(&mut visual, "person-forget-voice", Role::Button);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        let dialog = tree
            .nodes
            .iter()
            .find(|node| node.role == "AlertDialog")
            .unwrap();
        assert_eq!(dialog.name, "Forget your voice?");
        assert!(
            dialog
                .description
                .contains("Me on your lines: the microphone track still marks them")
        );
    });
    click(&mut visual, "forget-confirm-yes", Role::Button);
    visual.update(|window, cx| {
        assert!(people.read(cx).forget_voice_focus.is_focused(window));
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-record-after-forget")).1,
            "Record your voice"
        );
        assert!(tree.all("person-undo-edit").is_empty());
    });
    click(&mut visual, "person-forget", Role::Button);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        let dialog = tree
            .nodes
            .iter()
            .find(|node| node.role == "AlertDialog")
            .unwrap();
        assert_eq!(dialog.name, "Forget yourself?");
        assert!(
            dialog
                .description
                .contains("The meetings, their transcripts and recordings")
        );
    });
    click(&mut visual, "forget-confirm-yes", Role::Button);
    visual.update(|window, cx| {
        let view = people.read(cx);
        assert_eq!(view.selected.as_ref(), Some(&a.id));
        assert!(view.focus_handle.is_focused(window));
        assert!(view.record_after_forget);
    });
    assert!(lib.load_person(&me.id).is_err());
    select(&people, &b, &mut visual);
    visual.update(|window, cx| people.read(cx).focus_handle.clone().focus(window, cx));
    draw(&mut visual);
    visual.simulate_keystrokes(
        if broscribe_platform_native::NativePlatform::host_platform()
            == broscribe_platform::HostPlatform::MacOs
        {
            "secondary-backspace"
        } else {
            "delete"
        },
    );
    draw(&mut visual);
    assert!(
        lib.load_person(&b.id).is_ok(),
        "The shortcut only opens the alert"
    );
    click(&mut visual, "forget-confirm-yes", Role::Button);
    visual.update(|window, cx| {
        let view = people.read(cx);
        assert_eq!(view.selected.as_ref(), Some(&a.id));
        assert!(view.focus_handle.is_focused(window));
    });
}

#[gpui::test]
fn durable_forget_recheck_shows_links_and_dismisses(cx: &mut TestAppContext) {
    use broscribe_core::learning::{AffectedMeeting, LearningReport, ReidentifyInput};
    use broscribe_core::queue::{JOB_JSON, JobState, Queue};
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let person = Person::new(PersonId::random(), "Amira Fiction");
    save_people(&lib, &[&person]);
    let meeting = meeting_for(&lib, &person, 1);
    let state = state(dir.path(), lib.clone(), dir.path().join("unused"), cx);
    let model = state.read_with(cx, |state, _| {
        state.config().profile.speaker_embedding.clone()
    });
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1150.), gpui::px(1050.)));
    for kind in [ForgetKind::Voice, ForgetKind::Person] {
        let model = if kind == ForgetKind::Voice {
            "synthetic-previous-model"
        } else {
            model.as_str()
        };
        select(&people, &person, &mut visual);
        lib.write(|tx| broscribe_core::learning::request(tx, model, None))
            .unwrap();
        let forgotten = forget(&lib, dir.path(), &person.id, kind).unwrap();
        let request = forgotten
            .outcome
            .learning
            .iter()
            .find(|r| r.job_id == broscribe_core::learning::job_id(model))
            .unwrap()
            .clone();
        people.update(&mut visual, |view, cx| {
            view.finish_forget(&person.id, &person.name, kind, Ok(forgotten), cx)
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("people-recheck")),
                ("Group", "Speaker recheck")
            );
            assert_eq!(tree.read(tree.find("people-recheck-line-0")).0, "Label");
        });

        let queue = Queue::open(lib.root()).unwrap();
        let mut job = queue.get(&request.job_id).unwrap();
        let mut input = ReidentifyInput::of(&job).unwrap();
        input.processed = request.generation;
        input.report = Some(LearningReport {
            generation: input.processed,
            newly_named: 1,
            meetings: vec![AffectedMeeting {
                meeting_id: meeting.id.clone(),
                source: false,
                newly_named: 1,
                replaced: 0,
                demoted: 0,
                suggested: 0,
                cleared: 0,
                changes: Vec::new(),
            }],
            ..Default::default()
        });
        job.state = JobState::Done;
        job.input = input.to_value().unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &queue.job_dir(&request.job_id).join(JOB_JSON),
            &job,
        )
        .unwrap();
        state.update(&mut visual, |state, cx| state.refresh_recheck(cx));
        visual.run_until_parked();
        click(&mut visual, "people-recheck-open-0", Role::Link);
        state.read_with(&visual, |state, _| {
            assert_eq!(state.route(), &Route::meeting(meeting.id.clone()))
        });
        click(&mut visual, "people-recheck-hide", Role::Button);
        visual.update(|window, cx| {
            assert!(draw_and_read(window, cx).all("people-recheck").is_empty())
        });
        state.update(&mut visual, |state, cx| state.navigate(Route::people(), cx));
    }
}

#[gpui::test]
fn sample_control_plays_the_pending_cache_once_and_stop_cancels(cx: &mut TestAppContext) {
    use broscribe_core::queue::{JOB_JSON, JobState, Queue};
    use broscribe_core::schema::{Handling, Recording, Voiceprints};
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let person = Person::new(PersonId::random(), "Amira Fiction");
    save_people(&lib, &[&person]);
    let mut meeting = meeting_for(&lib, &person, 1);
    let audio = dir.path().join("synthetic-audio.custom");
    std::fs::write(&audio, b"synthetic").unwrap();
    let sha = "a".repeat(64);
    meeting.recording = Some(Recording {
        state: RecordingState::Available,
        handling: Handling::Reference,
        path: audio,
        original_path: None,
        sha256: sha.clone(),
        bytes: 9,
        source_sha256: None,
        sources: Vec::new(),
        retention_override: None,
        delete_after: None,
    });
    lib.write(|tx| {
        tx.save_meeting(&meeting)?;
        let mut voice = Voiceprints::empty(person.id.clone());
        voice.embeddings.push(broscribe_core::schema::Voiceprint {
            id: "synthetic-sample".into(),
            model: "synthetic-model".into(),
            dim: 1,
            vector_b64: "AACAPw==".into(),
            strength: Strength::Weak,
            source: VoiceprintSource {
                kind: VoiceprintKind::Confirmation,
                meeting_id: Some(meeting.id.clone()),
                sha256: Some(sha.clone()),
                start: 3.,
                end: 5.,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            created_at: meeting.started_at,
        });
        tx.save_voiceprints(&voice)
    })
    .unwrap();

    let state = state(dir.path(), lib.clone(), dir.path().join("unused"), cx);
    let player = FakeAudioPlayer::new(420.);
    state.update(cx, |state, _| state.set_player(Box::new(player)));
    let cache = state.read_with(cx, |state, _| {
        playback_cache_path(&state.paths().cache_dir, &sha)
    });
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1150.), gpui::px(1050.)));
    select(&people, &person, &mut visual);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-samples-heading")).1,
            "Voice samples (1)"
        );
        assert!(
            tree.read(tree.find("sample-row-0"))
                .1
                .contains("Unconfirmed")
        );
        assert!(tree.read(tree.find("sample-row-0")).1.contains("2 seconds"));
    });
    click(&mut visual, "sample-play-0", Role::Button);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("sample-status-0")),
            ("Status", PREPARING_CACHE)
        );
        assert!(
            tree.read(tree.find("sample-play-0"))
                .1
                .starts_with("Stop sample:")
        );
    });
    click(&mut visual, "sample-play-0", Role::Button);
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, b"synthetic wav").unwrap();
    visual.executor().advance_clock(PLAYBACK_TICK);
    visual.run_until_parked();
    people.read_with(&visual, |view, _| assert!(view.playback.active().is_none()));
    std::fs::remove_file(&cache).unwrap();
    click(&mut visual, "sample-play-0", Role::Button);
    let queue = Queue::open(lib.root()).unwrap();
    let job = queue
        .jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.kind == "playback-cache")
        .unwrap();
    let mut failed = job.clone();
    failed.state = JobState::Failed;
    failed.error = Some("decode error: /synthetic/private/voice-sample.m4a".into());
    broscribe_core::fsutil::write_json_atomic(&queue.job_dir(&job.id).join(JOB_JSON), &failed)
        .unwrap();
    visual.executor().advance_clock(PLAYBACK_TICK);
    visual.run_until_parked();
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("sample-status-0")),
            (
                "Alert",
                "Couldn't prepare the recording for playback. BroScribe couldn't read its library data."
            )
        );
    });
    click(&mut visual, "sample-retry-0", Role::Button);
    std::fs::write(&cache, b"synthetic wav").unwrap();
    visual.executor().advance_clock(PLAYBACK_TICK);
    visual.run_until_parked();
    people.read_with(&visual, |view, _| {
        assert!(view.playback.is_playing("sample-synthetic-sample"))
    });
}

#[gpui::test]
fn people_rows_keep_one_height_and_use_a_cached_photo_or_their_hue_disc(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let a = Person::new(
        PersonId::random(),
        "Amira Fiction with a very long synthetic name",
    );
    let b = Person::new(PersonId::random(), "Bela Fiction");
    save_people(&lib, &[&a, &b]);
    let state = state(dir.path(), lib, dir.path().join("unused"), cx);
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(800.), gpui::px(760.)));
    let inspect = |visual: &mut VisualTestContext| {
        visual.update(|window, cx| {
            let tree = draw_and_read(window, cx);
            assert_eq!(tree.nodes[tree.find("people-list")].children.len(), 2);
            let view = people.read(cx);
            let first = view.list_scroll.bounds_for_item(0).unwrap().size.height;
            let second = view.list_scroll.bounds_for_item(1).unwrap().size.height;
            assert_eq!(first, second, "An elongated name must not wrap");
            let size = gpui::px(32.).scale(window.scale_factor());
            // Each person's disc is drawn in their stored hue: the first two
            // people in a library take hues 1 and 2.
            let brand = crate::ui::brand::brand(cx);
            let fills: [gpui::Background; 2] =
                [brand.person_hue(1).into(), brand.person_hue(2).into()];
            window
                .painted_quads()
                .iter()
                .filter(|quad| {
                    fills.contains(&quad.background)
                        && quad.bounds.size.width == size
                        && quad.bounds.size.height == size
                })
                .count()
        })
    };
    assert_eq!(inspect(&mut visual), 2);
    // One transparent synthetic PNG is enough to exercise the actual photo branch.
    let image: [u8; 67] = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    people.update(&mut visual, |view, cx| {
        view.photos.insert(
            a.id.clone(),
            Arc::new(Image::from_bytes(ImageFormat::Png, image.to_vec())),
        );
        cx.notify();
    });
    assert_eq!(inspect(&mut visual), 1);
}

#[gpui::test]
fn record_your_voice_opens_setup_at_the_voice_step(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let mut person = Person::new(PersonId::random(), "Amira Fiction");
    person.is_self = true;
    save_people(&lib, &[&person]);
    let state = state(dir.path(), lib, dir.path().join("unused"), cx);
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    select(&people, &person, &mut visual);
    click(&mut visual, "person-record-voice", Role::Button);
    visual.update(|_, cx| {
        let assistant = crate::ui::setup_window::show(&state, &setup_deps(), cx).unwrap();
        assert_eq!(assistant.read(cx).step(), crate::views::setup::Step::Voice);
    });
}

#[gpui::test]
fn directory_popover_names_the_account_and_uses_add_email(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let person = Person::new(PersonId::random(), "Amira Fiction");
    save_people(&lib, &[&person]);
    let status = r#"{"integrations":[]}"#;
    let cli = crate::test_support::write_cli(
        &dir.path().join("cli"),
        &format!("printf '%s' '{status}'"),
        &format!("fx_print {}", crate::test_support::ps_str(status)),
    );
    let state = state(dir.path(), lib, cli, cx);
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1100.), gpui::px(900.)));
    select(&people, &person, &mut visual);
    click(&mut visual, "person-directory", Role::Button);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-directory-none")).1,
            "Connect a Microsoft 365 or Google account to search your work directory."
        );
        assert_eq!(
            tree.read(tree.find("person-directory-integrations")),
            ("Link", "Connect an account…")
        );
        let view = people.read(cx);
        assert_eq!(view.directory_input.read(cx).value().as_ref(), person.name);
        assert!(view.directory.search.is_none());
    });
    let account: broscribe_integrations::AccountDirectorySource = serde_json::from_value(serde_json::json!({
        "account":{"integration":"microsoft-graph","account_id":"11111111111111111111111111111111"},
        "generation":4,
    })).unwrap();
    people.update(&mut visual, |view, cx| {
        view.photo_accounts.choices.push(PhotoAccountChoice {
            source: PhotoSource::Microsoft(account.account.clone()),
            label: "Synthetic Work Account".into(),
            available: true,
        });
        view.directory.search = Some(DirectorySearch {
            results: vec![DirectoryMatch {
                integration: IntegrationId::MicrosoftGraph,
                generation: 4,
                source: Some(account),
                name: Some("Amira Directory".into()),
                email: Some("directory@example.invalid".into()),
            }],
            errors: vec![],
        });
        cx.notify();
    });
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-directory-result-0")),
            (
                "ListItem",
                "Amira Directory · directory@example.invalid · Synthetic Work Account",
            )
        );
        assert_eq!(
            tree.read(tree.find("person-directory-link-0")),
            ("Button", "Add email")
        );
        people
            .read(cx)
            .directory_input
            .focus_handle(cx)
            .focus(window, cx);
    });
    draw(&mut visual);
    visual.simulate_keystrokes("escape");
    draw(&mut visual);
    visual.update(|window, cx| {
        let view = people.read(cx);
        assert!(!view.directory.open);
        assert!(view.directory_focus.is_focused(window));
    });
}

#[cfg(test)]
#[gpui::test]
fn people_row_menu_uses_the_selected_person_and_opens_the_shared_alert(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let first = Person::new(PersonId::random(), "Ada Fiction");
    let second = Person::new(PersonId::random(), "Uma Fiction");
    save_people(&lib, &[&first, &second]);
    let app = state(dir.path(), lib.clone(), dir.path().join("unused"), cx);
    cx.update(|cx| {
        let keys =
            crate::ui::keymap::key_bindings_for(broscribe_platform::HostPlatform::Windows, cx);
        cx.bind_keys(keys);
    });
    let (people, window) = mounted(&app, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| people.read(cx).focus_handle.clone().focus(window, cx));
    draw(&mut visual);
    visual.simulate_keystrokes("end menu");
    draw(&mut visual);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        for label in ["Edit", "Forget voice", "Forget person"] {
            assert!(
                tree.nodes
                    .iter()
                    .any(|node| node.role == "MenuItem" && node.name == label)
            );
        }
    });
    visual.simulate_keystrokes("down down down enter");
    visual.run_until_parked();
    draw(&mut visual);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.role == "AlertDialog" && node.name.contains(&second.name)),
            "{tree:?}"
        );
        assert_eq!(people.read(cx).selected.as_ref(), Some(&second.id));
    });
    assert!(lib.load_person(&second.id).is_ok());
    assert!(lib.load_person(&first.id).is_ok());
}

#[gpui::test]
fn add_person_uses_the_shared_editor_even_with_an_empty_library(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("library")).unwrap();
    let state = state(dir.path(), lib, dir.path().join("missing-cli"), cx);
    let (people, window) = mounted(&state, cx);
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(gpui::px(1150.), gpui::px(1040.)));
    click(&mut visual, "people-add", Role::Button);
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert_eq!(
            tree.read(tree.find("person-editor")),
            ("Form", "Add person")
        );
        assert!(tree.all("person-aliases-label").is_empty());
        tree.find("person-emails-label");
        tree.find("person-notes-label");
    });
    click(&mut visual, "person-save", Role::Button);
    visual.update(|window, cx| {
        draw_and_read(window, cx).find("person-name-error");
    });
    people.read_with(&visual, |view, cx| {
        assert_eq!(
            view.editor.as_ref().unwrap().form(cx),
            PersonForm::default()
        )
    });
    visual.simulate_keystrokes("escape");
    visual.update(|window, cx| {
        let tree = draw_and_read(window, cx);
        assert!(tree.all("person-editor").is_empty());
        tree.find("people-add");
    });
}
