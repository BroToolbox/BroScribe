//! BroScribe's command shortcuts. GPUI handles text editing in its input context.
use std::rc::Rc;

use broscribe_platform::HostPlatform;
use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate};

use crate::menu::*;
use crate::views::library::ImportRecordings;
use crate::views::meeting::{NameTranscriptLine, PlayTranscriptLine, RemoveMeetingTag};

pub(super) const STARTUP_ERROR: &str = "BroScribeStartupError";
gpui::actions!(startup, [RetryStartup, QuitFailedStartup]);
gpui::actions!(transient, [CancelTransient]);

const TRANSIENT_EDITORS: &str =
    "MeetingView || (SetupAssistant && !Select && !List && !PopupMenu) || RecordingTitleEditor";
const CANCELABLE_SURFACE: &str = "Dialog || PopoverContent || PopupMenu || Select || MeetingView || SetupAssistant || RecordingTitleEditor || PersonEditor || PersonPicker";

pub(super) fn bind_startup_error(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", RetryStartup, Some(STARTUP_ERROR)),
        KeyBinding::new("escape", QuitFailedStartup, Some(STARTUP_ERROR)),
    ]);
}

pub(crate) struct Shortcut {
    pub group: &'static str,
    pub label: &'static str,
    pub keys: &'static str,
    pub action: Box<dyn Action>,
    context: Option<&'static str>,
}

impl Shortcut {
    pub fn binding(self, cx: &App) -> gpui::Result<KeyBinding> {
        let context = self
            .context
            .map(|context| KeyBindingContextPredicate::parse(context).map(Rc::new))
            .transpose()?;
        Ok(KeyBinding::load(
            self.keys,
            self.action,
            context,
            false,
            None,
            cx,
        )?)
    }
}

pub(crate) fn shortcuts(host: HostPlatform) -> Vec<Shortcut> {
    use crate::views::meeting::{NextMeetingTab, PreviousMeetingTab, speakers};
    use gpui::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};

    let mac = has_global_menu_bar(host);
    let main = Some("BroScribeMain");
    let outside_input = Some("BroScribeMain && !Input");
    let playback = Some("MeetingView && !Input");
    let delete_meeting = Some("BroScribeMain && !Input && !MeetingTag && !PeopleList");
    let mut shortcuts = Vec::new();
    macro_rules! group {
        ($group:literal; $(($label:literal, $action:expr, $keys:expr, $context:expr)),* $(,)?) => {
            $(if let Some(keys) = $keys {
                shortcuts.push(Shortcut {
                    group: $group, label: $label, keys, action: Box::new($action), context: $context,
                });
            })*
        };
    }
    // The tooltip reads the same binding that submits the dialog.
    group!("Import";
        ("Import", crate::views::import_sheet::SubmitImport,
            Some("secondary-enter"), Some("BroScribeImport && !List && !TimeField && !DatePicker")),
    );
    group!("Generation options";
        ("Generate summary", crate::views::generate::SubmitGeneration, Some("enter"),
            Some("BroScribeGenerationOptions && !List && !Select && !GenerationInstructions && !Button")),
        ("Generate summary", crate::views::generate::SubmitGeneration, Some("secondary-enter"),
            Some(crate::views::generate::OPTIONS_CONTEXT)),
    );
    group!("App and windows";
        ("Settings", OpenSettings, Some("secondary-,"), None),
        ("Quit BroScribe", QuitApp, Some("secondary-q"), None),
        ("Close window", CloseWindow, Some("secondary-w"), None),
        ("Minimize window", MinimizeWindow, Some("secondary-m"), None),
        ("Hide BroScribe", HideApp, mac.then_some("cmd-h"), None),
        ("Hide other apps", HideOtherApps, mac.then_some("alt-cmd-h"), None),
        ("Enter or exit full screen", ToggleFullScreen, mac.then_some("ctrl-cmd-f"), main),
        ("Show or hide the sidebar", ToggleSidebar, Some(if mac { "ctrl-cmd-s" } else { "f9" }), main),
        ("Larger text", LargerText, Some("secondary-="), None),
        ("Larger text", LargerText, Some("secondary-+"), None),
        ("Smaller text", SmallerText, Some("secondary--"), None),
        ("Standard text size", StandardTextSize, Some("secondary-0"), None),
        ("Keyboard shortcuts", KeyboardShortcuts, Some("secondary-/"), None),
        ("Main menu", ShowMainMenu, (!mac).then_some("f10"), main),
        ("Focus next area", FocusNextArea, Some("f6"), None),
        ("Focus previous area", FocusPreviousArea, Some("shift-f6"), None),
    );
    group!("Recording";
        ("Start recording", BeginRecording, Some("secondary-shift-r"), None),
        ("Pause or resume recording", PauseOrResumeRecording, Some("secondary-shift-p"), None),
        ("Add bookmark", AddBookmark, Some("secondary-d"), None),
        ("Show recording controls", ShowRecordingControls, Some(if mac { "alt-cmd-r" } else { "secondary-shift-k" }), None),
    );
    group!("Go and find";
        ("Back", NavigateBack, Some(if mac { "secondary-[" } else { "alt-left" }), outside_input),
        ("Forward", NavigateForward, Some(if mac { "secondary-]" } else { "alt-right" }), outside_input),
        ("Library", ShowLibrary, Some("secondary-1"), None),
        ("Review queue", ShowReviewQueue, Some("secondary-2"), None),
        ("People", ShowPeople, Some("secondary-3"), None),
        ("Search meetings", SearchMeetings, Some(if mac { "alt-cmd-f" } else { "secondary-e" }), None),
        ("Find in this view", FindInView, Some("secondary-f"), main),
        ("Find next", FindNext, Some("secondary-g"), main),
        ("Find previous", FindPrevious, Some("secondary-shift-g"), main),
        ("Find next", FindNext, (!mac).then_some("f3"), main),
        ("Find previous", FindPrevious, (!mac).then_some("shift-f3"), main),
        ("Import recordings", ImportRecordings, Some("secondary-o"), None),
    );
    let windows = host == HostPlatform::Windows;
    group!("Edit";
        ("Undo", Undo, Some("secondary-z"), None),
        ("Redo", Redo, (!mac && !windows).then_some("secondary-y"), None),
        ("Redo", Redo, Some("secondary-shift-z"), None),
        ("Redo", Redo, windows.then_some("secondary-y"), None),
        ("Cut", Cut, Some("secondary-x"), None),
        ("Copy", Copy, Some("secondary-c"), None),
        ("Paste", Paste, Some("secondary-v"), None),
        ("Select all", SelectAll, Some("secondary-a"), None),
    );
    group!("Meeting view";
        ("Play or pause meeting", ToggleMeetingPlaybackShortcut, Some("space"), Some("MeetingView && !Input && !MeetingControls && !SpeakerList")),
        ("Back 15 seconds", BackFifteenSecondsShortcut, Some("shift-left"), playback),
        ("Forward 15 seconds", ForwardFifteenSecondsShortcut, Some("shift-right"), playback),
        ("Next meeting tab", NextMeetingTab, Some("ctrl-tab"), Some("MeetingView")),
        ("Previous meeting tab", PreviousMeetingTab, Some("ctrl-shift-tab"), Some("MeetingView")),
        ("Rename meeting", RenameMeeting, (!mac).then_some("f2"), outside_input),
        ("Delete meeting", DeleteMeeting, Some(if mac { "secondary-backspace" } else { "delete" }), delete_meeting),
    );
    group!("Transcript";
        ("Play from the line", PlayTranscriptLine, Some("enter"), Some("Transcript")),
        ("Name the line", NameTranscriptLine, Some("n"), Some("Transcript")),
    );
    group!("Meeting tags";
        ("Remove focused tag", RemoveMeetingTag, Some("backspace"), Some("MeetingTag")),
        ("Remove focused tag", RemoveMeetingTag, Some("delete"), Some("MeetingTag")),
    );
    group!("Lists";
        ("Open row menu", super::kit::OpenRowMenu, (!mac).then_some("shift-f10"), Some("LibraryList || PeopleList || Transcript || (SpeakerList && !Input) || (ReviewQueue && !Input)")),
        ("Open row menu", super::kit::OpenRowMenu, (!mac).then_some("menu"), Some("LibraryList || PeopleList || Transcript || (SpeakerList && !Input) || (ReviewQueue && !Input)")),
    );
    use crate::views::people::{
        CancelPersonEdit, ForgetSelectedPerson, KeepEditingPerson, SavePerson,
    };
    group!("People";
        ("Forget person", ForgetSelectedPerson, Some(if mac { "secondary-backspace" } else { "delete" }), Some("PeopleList")),
        ("Save person", SavePerson, Some("secondary-enter"), Some("PersonEditor")),
        ("Cancel person edit", CancelPersonEdit, Some("escape"), Some("PersonEditor")),
        ("Keep editing person", KeepEditingPerson, Some("enter"), Some("PersonDiscard")),
    );
    group!("Editors";
        ("Cancel editing", CancelTransient, Some("escape"), Some(TRANSIENT_EDITORS)),
        // Dispatch the component action so the innermost dialog or menu
        // handles the chord before an enclosing editor sees it.
        ("Cancel editing", gpui::base::actions::Cancel, Some("secondary-."), Some(CANCELABLE_SURFACE)),
    );
    use crate::views::review;
    group!("Review queue";
        ("Accept, name or restore", review::Decide, Some("enter"), Some("ReviewQueue && !ReviewActions && !Input")),
        ("Reject", review::Reject, Some("r"), Some("ReviewQueue && !Input")),
        ("Name", review::Name, Some("n"), Some("ReviewQueue && !Input")),
        ("Can't tell", review::Dismiss, Some("d"), Some("ReviewQueue && !Input")),
        ("Play or stop sample", review::PlaySample, Some("space"), Some("ReviewQueue && !ReviewActions && !Input")),
        ("Previous sample", review::PreviousSample, Some("left"), Some("ReviewQueue && !Input")),
        ("Next sample", review::NextSample, Some("right"), Some("ReviewQueue && !Input")),
    );
    group!("Speakers";
        ("Accept, name or restore", speakers::Decide, Some("enter"), Some("SpeakerList && !SpeakerActions && !Input")),
        ("Reject", speakers::Reject, Some("r"), Some("SpeakerList && !Input")),
        ("Name or change person", speakers::Name, Some("n"), Some("SpeakerList && !Input")),
        ("Can't tell", speakers::Dismiss, Some("d"), Some("SpeakerList && !Input")),
        ("Play or stop sample", speakers::PlaySample, Some("space"), Some("SpeakerList && !SpeakerActions && !Input")),
        ("Previous sample", speakers::PreviousSample, Some("left"), Some("SpeakerList && !Input")),
        ("Next sample", speakers::NextSample, Some("right"), Some("SpeakerList && !Input")),
    );
    shortcuts
}

/// Installs the same bindings that the native menus, popup menu, tooltip
/// actions and the Keyboard shortcuts dialog use.
pub fn key_bindings(cx: &App) -> Vec<KeyBinding> {
    key_bindings_for(
        broscribe_platform_native::NativePlatform::host_platform(),
        cx,
    )
}

pub(crate) fn key_bindings_for(host: HostPlatform, cx: &App) -> Vec<KeyBinding> {
    shortcuts(host)
        .into_iter()
        .filter_map(|shortcut| {
            let keys = shortcut.keys;
            shortcut
                .binding(cx)
                .inspect_err(|error| tracing::warn!(%error, keys, "shortcut"))
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, OperatingSystem};
    use std::collections::HashMap;

    const PLATFORMS: [(HostPlatform, OperatingSystem); 3] = [
        (HostPlatform::MacOs, OperatingSystem::Mac),
        (HostPlatform::Windows, OperatingSystem::Windows),
        (HostPlatform::Linux, OperatingSystem::Linux),
    ];

    #[test]
    fn playback_keys_do_not_become_native_menu_equivalents() {
        let rows = shortcuts(HostPlatform::MacOs);
        let actions: &[(&dyn Action, &dyn Action, &str)] = &[
            (
                &ToggleMeetingPlayback,
                &ToggleMeetingPlaybackShortcut,
                "space",
            ),
            (
                &BackFifteenSeconds,
                &BackFifteenSecondsShortcut,
                "shift-left",
            ),
            (
                &ForwardFifteenSeconds,
                &ForwardFifteenSecondsShortcut,
                "shift-right",
            ),
        ];
        for (menu, shortcut, key) in actions {
            assert!(!rows.iter().any(|row| row.action.partial_eq(*menu)));
            assert!(rows.iter().any(|row| {
                row.action.partial_eq(*shortcut)
                    && row.keys == *key
                    && row
                        .context
                        .is_some_and(|context| context.contains("!Input"))
            }));
        }
    }

    #[test]
    fn keymap_has_no_collisions() {
        for (host, os) in PLATFORMS {
            let mut seen = HashMap::new();
            let reserved: &[&str] = if host == HostPlatform::MacOs {
                &[
                    "cmd-tab",
                    "cmd-space",
                    "alt-cmd-escape",
                    "alt-cmd-d",
                    "cmd-`",
                    "shift-cmd-3",
                    "shift-cmd-4",
                    "shift-cmd-5",
                ]
            } else {
                &[
                    "alt-tab",
                    "alt-f4",
                    "ctrl-escape",
                    "ctrl-shift-escape",
                    "ctrl-shift-u",
                    "ctrl-shift-e",
                    "ctrl-shift-s",
                ]
            };
            let reserved: Vec<_> = reserved
                .iter()
                .map(|key| Keystroke::parse_with_os(key, os).unwrap())
                .collect();
            for row in shortcuts(host) {
                if let Some(context) = row.context {
                    KeyBindingContextPredicate::parse(context).unwrap();
                }
                let key = Keystroke::parse_with_os(row.keys, os).unwrap();
                assert!(
                    !reserved.contains(&key),
                    "{host:?}: {} is reserved",
                    row.keys
                );
                if host == HostPlatform::MacOs {
                    assert!(
                        !(key.modifiers.control
                            && key
                                .key
                                .strip_prefix('f')
                                .is_some_and(|key| key.parse::<u8>().is_ok())),
                        "the OS owns Control with function keys"
                    );
                } else {
                    assert!(!key.modifiers.platform, "{host:?}: the OS owns Super");
                    assert!(
                        !(key.modifiers.control && key.modifiers.alt),
                        "{host:?}: Ctrl+Alt is AltGr"
                    );
                }
                // Compare with every other binding of this chord. The shell delete
                // context excludes tag and People list focus. The remaining pairs
                // belong to separate views, the modal import dialog or the
                // meeting's generation options.
                let previous: &mut Vec<Option<&str>> = seen.entry(key).or_default();
                for &context in previous.iter() {
                    let disjoint = matches!(
                        (context, row.context),
                        (
                            Some("BroScribeMain && !Input && !MeetingTag && !PeopleList"),
                            Some("MeetingTag" | "PeopleList")
                        ) | (Some("MeetingTag"), Some("PeopleList"))
                            | (
                                Some("BroScribeImport && !List && !TimeField && !DatePicker"),
                                Some("PersonEditor")
                            )
                            | (Some("Transcript"), Some("PersonDiscard"))
                            // The People editor replaces the meeting. Setup and recording use separate windows.
                            | (Some("PersonEditor"), Some(TRANSIENT_EDITORS))
                            // Review and People replace the entire meeting surface in the shell.
                            | (Some("Transcript" | "PersonDiscard"), Some("ReviewQueue && !ReviewActions && !Input"))
                            | (Some("Transcript"), Some("ReviewQueue && !Input"))
                            | (Some("MeetingView && !Input && !MeetingControls && !SpeakerList"),
                               Some("ReviewQueue && !ReviewActions && !Input"))
                            | (Some("ReviewQueue && !ReviewActions && !Input"),
                               Some("SpeakerList && !SpeakerActions && !Input"))
                            | (Some("ReviewQueue && !Input"), Some("SpeakerList && !Input"))
                            // Generation options are a meeting surface with their own focus.
                            | (Some("BroScribeGenerationOptions && !List && !Select && !GenerationInstructions && !Button"),
                               Some("ReviewQueue && !ReviewActions && !Input"))
                            | (
                                Some("PersonDiscard"),
                                Some("SpeakerList && !SpeakerActions && !Input")
                            )
                            | (
                                Some("Transcript"),
                                Some(
                                    "SpeakerList && !Input"
                                        | "SpeakerList && !SpeakerActions && !Input"
                                )
                            )
                            | (
                                Some("MeetingView && !Input && !MeetingControls && !SpeakerList"),
                                Some("SpeakerList && !SpeakerActions && !Input")
                            )
                            | (
                                Some("BroScribeImport && !List && !TimeField && !DatePicker"),
                                Some(crate::views::generate::OPTIONS_CONTEXT)
                            )
                            | (
                                Some(crate::views::generate::OPTIONS_CONTEXT),
                                Some("PersonEditor")
                            )
                            | (
                                Some(
                                    "BroScribeGenerationOptions && !List && !Select && !GenerationInstructions && !Button"
                                ),
                                Some(
                                    "Transcript"
                                        | "PersonDiscard"
                                        | "SpeakerList && !SpeakerActions && !Input"
                                )
                            )
                    );
                    assert!(disjoint, "{host:?}: duplicate {}", row.keys);
                }
                previous.push(row.context);
            }
        }
    }

    #[test]
    fn every_shell_command_has_its_catalog_chord() {
        for (host, os) in PLATFORMS {
            let mac = host == HostPlatform::MacOs;
            let expected: &[(&dyn Action, &str)] = &[
                (&ShowLibrary, "secondary-1"),
                (&ShowReviewQueue, "secondary-2"),
                (&ShowPeople, "secondary-3"),
                (&BeginRecording, "secondary-shift-r"),
                (&PauseOrResumeRecording, "secondary-shift-p"),
                (&AddBookmark, "secondary-d"),
                (
                    &ShowRecordingControls,
                    if mac { "alt-cmd-r" } else { "ctrl-shift-k" },
                ),
                (&SearchMeetings, if mac { "alt-cmd-f" } else { "ctrl-e" }),
                (&ToggleSidebar, if mac { "ctrl-cmd-s" } else { "f9" }),
                (&NavigateBack, if mac { "cmd-[" } else { "alt-left" }),
                (&NavigateForward, if mac { "cmd-]" } else { "alt-right" }),
                (&KeyboardShortcuts, "secondary-/"),
                (&ImportRecordings, "secondary-o"),
                (&CloseWindow, "secondary-w"),
            ];
            let rows = shortcuts(host);
            for (action, chord) in expected {
                let chord = Keystroke::parse_with_os(chord, os).unwrap();
                assert!(
                    rows.iter().any(|row| row.action.partial_eq(*action)
                        && Keystroke::parse_with_os(row.keys, os).unwrap() == chord),
                    "{host:?}: {}",
                    action.name()
                );
            }
            assert!(
                rows.iter()
                    .all(|row| !row.action.partial_eq(&StopAndSaveRecording)
                        && !row.action.partial_eq(&ExportMeeting))
            );
            assert_eq!(
                rows.iter()
                    .any(|row| row.action.partial_eq(&ShowMainMenu) && row.keys == "f10"),
                !mac
            );
            assert_eq!(rows.iter().any(|row| row.action.partial_eq(&ToggleFullScreen) && row.keys == "ctrl-cmd-f"), mac);
        }
    }
}
