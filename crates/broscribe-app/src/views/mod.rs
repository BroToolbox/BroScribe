//! The views behind each [`crate::state::Route`]; the shell (`ui.rs`) mounts
//! them.

pub mod about;
pub mod agents;
pub mod delete_meeting;
pub mod export;
pub mod generate;
pub mod import_sheet;
pub mod integrations;
pub mod library;
pub mod meeting;
pub mod panel;
pub mod people;
pub mod person_picker;
pub mod providers;
pub mod review;
pub mod settings;
pub mod setup;
pub mod speaker_review;
pub mod templates;
pub mod update;

/// WRT-08: one pattern for failures, "Couldn't <do X>. <Cause>. <Fix>.",
/// with contractions, the cause from `CliError::user_text` (WRT-04) and no
/// code. One case per before → after pair of the item (and 3.4, which wins
/// where both changed a sentence).
#[cfg(test)]
mod error_copy_tests {
    use crate::generation::GenerationTarget;
    use crate::jobs::CliError;

    fn stopped() -> CliError {
        CliError::new("exit", "the CLI exited with exit status: 1")
    }

    const STOPPED: &str = "BroScribe's command-line tool stopped unexpectedly.";

    /// Every rewritten line starts with "Couldn't", carries no code and no
    /// colon before its cause.
    fn plain(line: &str) {
        assert!(line.starts_with("Couldn't "), "{line}");
        for leftover in [
            "(exit)",
            "(not_found)",
            "invalid input",
            "failed:",
            "Cannot",
            ": BroScribe",
        ] {
            assert!(!line.contains(leftover), "{line} contains {leftover}");
        }
    }

    #[test]
    fn people_download_rename_and_generation() {
        // "Cannot read people: {e}"
        let people = super::people::people_load_failure(&stopped().user_text());
        assert_eq!(
            people,
            format!("Couldn't load people. {STOPPED} Try again.")
        );
        // "Download failed: {e}": the model by its registry name, the id
        // falls back to plain wording.
        let list = super::settings::parse_models_list(&serde_json::json!({
            "models": [{"id": "silero-vad", "display_name": "Silero VAD"}],
            "profiles": [{"id": "default", "description": "Parakeet TDT 0.6B v3",
                          "asr": "parakeet-tdt-0.6b-v3"}],
        }));
        assert_eq!(
            list.model_name("parakeet-tdt-0.6b-v3"),
            "Parakeet TDT 0.6B v3"
        );
        assert_eq!(list.model_name("silero-vad"), "Silero VAD");
        let download = super::settings::download_failure(
            &list.model_name("parakeet-tdt-0.6b-v3"),
            &CliError::answered("io", "checksum mismatch for /synthetic/models/x.onnx"),
        );
        assert_eq!(
            download,
            "Couldn't download Parakeet TDT 0.6B v3. The download didn't finish. Check your \
             connection, then try again."
        );
        let crashed = super::settings::download_failure(
            "silero-vad",
            &CliError::new("io", "the CLI timed out"),
        );
        assert_eq!(
            crashed,
            format!(
                "Couldn't download silero-vad. {STOPPED} Check your connection, then try again."
            )
        );
        let crowded = super::settings::download_failure(
            "Silero VAD",
            &CliError::answered(
                "model_cache_has_other_files",
                "BroScribe found an unrecognised entry in the model folder (notes.txt). Move it \
                 out before downloading again.",
            ),
        );
        assert!(
            crowded.starts_with("Couldn't download Silero VAD."),
            "{crowded}"
        );
        assert!(!crowded.contains("Check your connection"), "{crowded}");
        // "Renaming failed: {e}"
        let rename = super::meeting::rename_failure(
            &CliError::new("invalid_input", "invalid input: the title is empty").user_text(),
        );
        assert_eq!(
            rename,
            "Couldn't rename the meeting. The title is empty. Your title is still in the field."
        );
        // "Generation failed: {e}" (3.4: "generate", not "write")
        let summary = super::generate::generation_failure(
            &GenerationTarget::Summary,
            "synthetic provider refused the request",
        );
        assert_eq!(
            summary,
            "Couldn't generate the summary. Synthetic provider refused the request."
        );
        let template = super::generate::generation_failure(
            &GenerationTarget::Template {
                id: "notes".into(),
                name: "Meeting notes".into(),
            },
            "",
        );
        assert_eq!(template, "Couldn't run Meeting notes.");
        for line in [&people, &download, &crashed, &rename, &summary, &template] {
            plain(line);
        }
    }

    #[test]
    fn export_template_test_and_playback() {
        // "Export failed: {e}"
        let export = super::export::export_failure(&stopped());
        assert_eq!(
            export,
            format!("Couldn't export. {STOPPED} Choose another folder and try again.")
        );
        // "Failed: {e}" (template save; SET-64 adds "Your text is kept.")
        let template = super::templates::save_failure(
            "Meeting notes",
            &CliError::new("spawn", "/synthetic/broscribe: not found").user_text(),
            true,
        );
        assert_eq!(
            template,
            "Couldn't save Meeting notes. BroScribe's command-line tool couldn't start. Your \
             text is kept."
        );
        // "It did not work: {e}"
        let test = super::setup::try_failure(&stopped().user_text());
        assert_eq!(test, format!("Couldn't transcribe the test. {STOPPED}"));
        // "Playback failed: {error}" (3.4: "the recording")
        let playback = super::meeting::playback_failure("Audio playback isn't available.");
        assert_eq!(
            playback,
            "Couldn't play the recording. Audio playback isn't available."
        );
        for line in [&export, &template, &test, &playback] {
            plain(line);
        }
    }

    #[test]
    fn verification_search_and_settings() {
        // "Verification failed: {e}"
        let verify = super::settings::verify_failure(&stopped().user_text());
        assert_eq!(verify, format!("Couldn't check the models. {STOPPED}"));
        // "Search failed: {error}"
        let search = super::library::search_failure(
            &CliError::new("cancelled", "the command was cancelled").user_text(),
        );
        assert_eq!(search, "Couldn't search the library. It was cancelled.");
        // SET-64: "Couldn't open that folder as a library: <reason>."
        let library = super::settings::library_open_failure(&crate::jobs::core_user_text(
            &broscribe_core::Error::io(
                "/synthetic/private-folder",
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ),
        ));
        assert_eq!(
            library,
            "Couldn't open that folder as a library. BroScribe isn't allowed to open it."
        );
        plain(&library);
        // SET-64: "The update could not continue: …"
        let update =
            super::update::refusal_text(&CliError::new("update_hash_mismatch", "bad hash"));
        assert_eq!(update, "Couldn't continue the update. Bad hash.");
        for line in [&verify, &search, &update] {
            plain(line);
        }
        // An unknown cause is left out, never "Unknown error".
        assert_eq!(
            super::library::search_failure(""),
            "Couldn't search the library."
        );
    }

    /// §4.2 Irreversible actions: refusals keep saying what did not change.
    #[test]
    fn refusals_keep_nothing_was_changed() {
        let delete = super::delete_meeting::delete_refusal(
            super::delete_meeting::DeleteStep::Delete,
            &CliError::new("job_running", "a job is working on m-00000a"),
        );
        assert!(delete.contains("nothing was deleted"), "{delete}");
        let update = super::update::refusal_text(&CliError::new("update_changed", ""));
        assert!(update.contains("nothing was"), "{update}");
        let line = super::panel::control_error(&broscribe_ipc::IpcError::new(
            "stale_session",
            "session s1 is not active",
        ));
        assert!(line.contains("nothing was changed"), "{line}");
    }
}
