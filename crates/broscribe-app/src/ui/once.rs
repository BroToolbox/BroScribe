//! Durable, per-tip claims shared by windows and launches.

use std::fs::{self, OpenOptions};
use std::path::Path;

/// Claim a tip before showing it. Exclusive creation also arbitrates two
/// windows or launches reaching the same tip at once.
pub(crate) fn claim(data_dir: &Path, id: &str) -> bool {
    debug_assert!(
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    );
    let state = data_dir.join("state");
    if fs::create_dir_all(&state).is_err() {
        return false;
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(state.join(format!("{id}-shown")))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tip_is_claimed_once_and_does_not_consume_other_tips() {
        let temp = tempfile::tempdir().unwrap();
        assert!(claim(temp.path(), "tray-tip"));
        assert!(!claim(temp.path(), "tray-tip"));
        assert!(claim(temp.path(), "no-tray"));
        assert!(temp.path().join("state/tray-tip-shown").is_file());
    }
}
