//! The application a microphone helper belongs to. Chromium (Chrome, Edge,
//! Electron apps) captures audio in a windowless utility process, and WebView2
//! hosts run theirs under `msedgewebview2.exe`; UI Automation needs the
//! process owning the meeting's windows, so detection walks up from such a
//! helper to it.
use std::collections::HashMap;

/// Longest parent chain followed.
const MAX_DEPTH: usize = 32;
/// Runtimes whose child processes serve another application's windows.
const EMBEDDED_RUNTIMES: [&str; 1] = ["msedgewebview2.exe"];

/// One entry of a process snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Process {
    pub parent: u32,
    /// Executable file name (the Windows detection identity).
    pub exe: String,
}

/// The process owning `pid`'s windows and its executable name, when it
/// differs from `pid`. The walk only climbs from a helper to a parent of the
/// same executable, or out of an embedded runtime, so a windowless app is
/// never attributed to the shell that started it. `parent_valid(child,
/// parent)` rejects a reused parent ID (a parent created after its child).
pub(crate) fn responsible_process(
    pid: u32,
    processes: &HashMap<u32, Process>,
    owns_windows: impl Fn(u32) -> bool,
    parent_valid: impl Fn(u32, u32) -> bool,
) -> Option<(u32, String)> {
    if owns_windows(pid) {
        return None;
    }
    let mut child = pid;
    for _ in 0..MAX_DEPTH {
        let current = processes.get(&child)?;
        let parent = current.parent;
        if parent == 0 || parent == child || !parent_valid(child, parent) {
            return None;
        }
        let up = processes.get(&parent)?;
        let same_app = up.exe.eq_ignore_ascii_case(&current.exe)
            || EMBEDDED_RUNTIMES
                .iter()
                .any(|runtime| current.exe.eq_ignore_ascii_case(runtime));
        if !same_app {
            return None;
        }
        if owns_windows(parent) {
            return Some((parent, up.exe.clone()));
        }
        child = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> HashMap<u32, Process> {
        let p = |parent, exe: &str| Process {
            parent,
            exe: exe.into(),
        };
        HashMap::from([
            (4, p(0, "System")),
            (900, p(4, "explorer.exe")),
            (1000, p(900, "chrome.exe")),
            (1001, p(1000, "chrome.exe")),
            (2000, p(900, "ms-teams.exe")),
            (2001, p(2000, "msedgewebview2.exe")),
            (2002, p(2001, "msedgewebview2.exe")),
            (3000, p(900, "tray-recorder.exe")),
            (4001, p(4000, "chrome.exe")),
        ])
    }

    #[test]
    fn helpers_resolve_to_the_process_owning_windows() {
        let tree = tree();
        let windows = |pid| matches!(pid, 900 | 1000 | 2000);
        let valid = |_, _| true;
        let resolve = |pid| responsible_process(pid, &tree, windows, valid);
        assert_eq!(resolve(1001), Some((1000, "chrome.exe".into())));
        assert_eq!(resolve(2002), Some((2000, "ms-teams.exe".into())));
        // The browser itself, a windowless native app, an orphan helper.
        assert_eq!(resolve(1000), None);
        assert_eq!(resolve(3000), None);
        assert_eq!(resolve(4001), None);
        // A reused parent ID is not followed.
        assert_eq!(
            responsible_process(1001, &tree, windows, |_, parent| parent != 1000),
            None
        );
    }
}
