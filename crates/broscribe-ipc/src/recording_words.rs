//! Recording causes shared by the daemon's notifications and its clients.
use crate::IpcError;

pub const UNTITLED_MEETING: &str = "Untitled meeting";
pub const MIN_FREE_BYTES: u64 = 500 * 1024 * 1024;
pub const MIN_HEADROOM_SECONDS: u64 = 15 * 60;

pub const JOURNAL_LIBRARY_UNKNOWN: &str = "journal_library_unknown";
pub const JOURNAL_LIBRARY_MISMATCH: &str = "journal_library_mismatch";

/// Guidance for recording journals kept outside the open library's queue.
pub fn journal_library_issue(code: &str) -> Option<&'static str> {
    match code {
        JOURNAL_LIBRARY_UNKNOWN => Some(
            "This recording was made before BroScribe tracked its library. Save it in this library or discard the recording.",
        ),
        JOURNAL_LIBRARY_MISMATCH => {
            Some("This recording belongs to another library. Open that library to recover it.")
        }
        _ => None,
    }
}

pub fn meeting_title(title: Option<&str>) -> &str {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(UNTITLED_MEETING)
}

pub fn interrupted_stop(reason: Option<&str>) -> Option<&str> {
    let code = reason?.split(':').next()?;
    (!matches!(code, "" | "user" | "auto_stop")).then_some(code)
}

pub fn stop_reason_text(reason: &str) -> &'static str {
    match interrupted_stop(Some(reason)) {
        Some("disk_low") => {
            "The disk is almost full, so recording stopped to protect what was already recorded. Free some space to record again."
        }
        Some("disk_check_failed") => {
            "BroScribe couldn't check free disk space, so recording stopped to protect what was already recorded."
        }
        Some("capture_device_unavailable") => {
            "The microphone or audio output was disconnected. What was recorded until then is kept."
        }
        Some("journal_error") => {
            "BroScribe couldn't write the recording to disk. What was written until then is kept. Check that the library folder is still available."
        }
        Some("start_failed") => {
            "The audio devices didn't start, so nothing was recorded. Try again, or choose another microphone in Settings."
        }
        _ => "Recording stopped unexpectedly. What was recorded until then is kept.",
    }
}

/// Known final-job causes, with any filenames or pipeline messages removed.
pub fn transcription_cause(error: Option<&str>) -> Option<&'static str> {
    let error = error?.trim();
    let code = match error.split_once(':').map(|(prefix, _)| prefix) {
        Some("model missing") => "model_missing",
        Some("schema error") => "schema",
        Some("decode error") => "decode",
        Some("inference error") => "inference",
        Some("library busy") => "library_busy",
        Some("lease lost") => "lease_lost",
        Some("not found") => "not_found",
        _ => error,
    };
    Some(match code {
        "model_missing" => "A model it needs isn't installed.",
        "io" => "BroScribe couldn't read or write a file.",
        "schema" | "decode" => "Its saved data couldn't be read.",
        "inference" => "A model on this computer couldn't finish.",
        "library_busy" | "busy" => "BroScribe is busy with the library.",
        "lease_lost" => "Another BroScribe process took over this work.",
        "not_found" => "Part of the recording is missing.",
        "worker exhausted its attempts" => "Every attempt stopped unexpectedly.",
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartRecovery {
    Storage,
    Setup,
    Updates,
    Recording,
    MicrophonePrivacy,
    CopyDetails,
}

/// A notification has no setup window context, so its caller passes false.
pub fn start_refusal(
    error: &IpcError,
    setup_capturing: bool,
) -> Option<(String, Option<StartRecovery>)> {
    use StartRecovery::*;
    let (reason, recovery) = match error.code.as_str() {
        IpcError::DISK_LOW => {
            return Some((
                format!(
                    "The disk is almost full: recording needs {} MB free and room for {} minutes. Free some space, then try again.",
                    MIN_FREE_BYTES / (1024 * 1024),
                    MIN_HEADROOM_SECONDS / 60
                ),
                Some(Storage),
            ));
        }
        IpcError::BUSY if setup_capturing => (
            "The setup assistant is using the microphone for a test. Finish or cancel the test, then try again.",
            Some(Setup),
        ),
        IpcError::BUSY => (
            "BroScribe Agent is still getting ready. Try again in a moment.",
            None,
        ),
        IpcError::INSTALLING => (
            "Recording is off until you keep or roll back the BroScribe update.",
            Some(Updates),
        ),
        IpcError::ADMISSION_DAMAGED => (
            "BroScribe can't read its update record, so recording stays off until it's repaired.",
            Some(CopyDetails),
        ),
        IpcError::SHUTTING_DOWN => (
            "BroScribe Agent is restarting. Try again in a moment.",
            None,
        ),
        IpcError::MICROPHONE_PRIVACY => (
            "Allow microphone access in Settings › Privacy & security › Microphone, then try again.",
            Some(MicrophonePrivacy),
        ),
        "not_found"
        | "unsupported"
        | "platform"
        | "capture_failed"
        | "capture_device_unavailable" => (
            "The microphone or audio output didn't start. Check the devices in Recording settings, then try again.",
            Some(Recording),
        ),
        _ => return None,
    };
    Some((reason.into(), recovery))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_refusal_codes_explain_the_cause_and_offer_the_matching_recovery() {
        use StartRecovery::*;
        for (code, setup, snippet, recovery) in [
            (
                IpcError::DISK_LOW,
                false,
                "500 MB free and room for 15 minutes",
                Some(Storage),
            ),
            (
                IpcError::BUSY,
                true,
                "using the microphone for a test",
                Some(Setup),
            ),
            (IpcError::BUSY, false, "is still getting ready", None),
            (
                IpcError::INSTALLING,
                false,
                "keep or roll back",
                Some(Updates),
            ),
            (
                IpcError::ADMISSION_DAMAGED,
                false,
                "can't read its update record",
                Some(CopyDetails),
            ),
            (IpcError::SHUTTING_DOWN, false, "is restarting", None),
            (
                IpcError::MICROPHONE_PRIVACY,
                false,
                "Privacy & security › Microphone",
                Some(MicrophonePrivacy),
            ),
            (
                "not_found",
                false,
                "microphone or audio output didn't start",
                Some(Recording),
            ),
            (
                "unsupported",
                false,
                "microphone or audio output didn't start",
                Some(Recording),
            ),
            (
                "platform",
                false,
                "microphone or audio output didn't start",
                Some(Recording),
            ),
            (
                "capture_failed",
                false,
                "microphone or audio output didn't start",
                Some(Recording),
            ),
        ] {
            let (cause, action) =
                start_refusal(&IpcError::new(code, "synthetic raw message"), setup).unwrap();
            assert!(
                cause.contains(snippet) && !cause.contains("synthetic"),
                "{cause}"
            );
            assert_eq!(action, recovery);
        }
        assert!(start_refusal(&IpcError::new("unknown", "synthetic refusal"), false).is_none());
    }

    #[test]
    fn one_display_title_and_no_raw_transcription_cause() {
        assert_eq!(meeting_title(None), "Untitled meeting");
        assert_eq!(meeting_title(Some("   ")), "Untitled meeting");
        assert_eq!(meeting_title(Some(" Synthetic sync ")), "Synthetic sync");
        assert_eq!(
            transcription_cause(Some("decode error: /synthetic/private")),
            Some("Its saved data couldn't be read.")
        );
        assert!(transcription_cause(Some("/synthetic/private")).is_none());
    }
}
