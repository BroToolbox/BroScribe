use broscribe_updates::{UpdateStatus, pending_replacement};

#[test]
fn offline_inspection_does_not_create_state_or_require_enabled_settings() {
    let profile = tempfile::tempdir().unwrap();
    let state = profile.path().join("updates");
    assert_eq!(pending_replacement(&state).unwrap(), UpdateStatus::Idle);
    assert!(
        !state.exists(),
        "offline inspection bootstrapped updater state"
    );
}
