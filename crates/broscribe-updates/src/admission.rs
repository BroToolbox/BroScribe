//! Install admission: the recording agent's fenced promise that nothing
//! records while an installation is replaced.
//!
//! Producer contract (the agent, over IPC; not a local flag or snapshot):
//! - `acquire` is decided under the agent's session mutex. It refuses with
//!   [`codes::RECORDING_ACTIVE`] while any recording is capturing, paused,
//!   in its stop grace or stopped but not saved yet, and with
//!   [`codes::BUSY`] while another admission is held. A grant records a
//!   fence token strictly greater than every token the agent granted
//!   before (persisted across agent restarts) and its holder connection.
//! - While an admission is held, every way to start capture (record.start,
//!   automatic detection, sample and audio-check capture) is refused under
//!   the same mutex, so no capture can begin between `confirm` and a rename.
//! - `confirm` is a round trip naming the token; it fails unless that token
//!   is still the held admission and nothing records.
//! - The admission ends when the lease is dropped (explicit release), when
//!   the holder's connection closes (holder died) or when the agent exits; a
//!   later `confirm` with the old token then fails.
//!
//! The updater confirms immediately before every irreversible step and
//! aborts on any error.

use std::sync::{Arc, Mutex};

use broscribe_core::Result;

use crate::{codes, fail};

/// Grants install admissions.
pub trait InstallAdmission: Send + Sync {
    /// Acquires exclusive admission; see the module contract.
    fn acquire(&self) -> Result<Box<dyn InstallLease>>;
}

/// One held admission. Dropping it releases the admission.
pub trait InstallLease: Send {
    /// The agent's fence token for this admission.
    fn fence(&self) -> u64;
    /// Re-validates with the agent that this admission is still held and
    /// nothing records.
    fn confirm(&self) -> Result<()>;
}

#[derive(Default)]
struct FakeAdmissionState {
    capturing: bool,
    held: Option<u64>,
    last_fence: u64,
}

/// Deterministic [`InstallAdmission`] for tests, modelling the agent's
/// fence: captures cannot start while an admission is held, and revocation
/// invalidates the lease.
#[derive(Clone, Default)]
pub struct FakeInstallAdmission {
    state: Arc<Mutex<FakeAdmissionState>>,
}

impl FakeInstallAdmission {
    /// Starts a capture as the agent would: refused while an admission is held.
    pub fn start_capture(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.held.is_some() {
            return Err(fail(
                codes::INSTALLING,
                "an update is being installed; recording starts after it",
            ));
        }
        state.capturing = true;
        Ok(())
    }

    /// The capture stopped and was saved.
    pub fn stop_capture(&self) {
        self.state.lock().unwrap().capturing = false;
    }

    /// Ends the held admission as if the agent restarted.
    pub fn revoke(&self) {
        self.state.lock().unwrap().held = None;
    }

    pub fn held(&self) -> Option<u64> {
        self.state.lock().unwrap().held
    }
}

impl InstallAdmission for FakeInstallAdmission {
    fn acquire(&self) -> Result<Box<dyn InstallLease>> {
        let mut state = self.state.lock().unwrap();
        if state.capturing {
            return Err(fail(
                codes::RECORDING_ACTIVE,
                "a recording is active; updates install after it is saved",
            ));
        }
        if state.held.is_some() {
            return Err(fail(codes::BUSY, "another install admission is held"));
        }
        state.last_fence += 1;
        state.held = Some(state.last_fence);
        Ok(Box::new(FakeLease {
            state: Arc::clone(&self.state),
            fence: state.last_fence,
        }))
    }
}

struct FakeLease {
    state: Arc<Mutex<FakeAdmissionState>>,
    fence: u64,
}

impl InstallLease for FakeLease {
    fn fence(&self) -> u64 {
        self.fence
    }

    fn confirm(&self) -> Result<()> {
        let state = self.state.lock().unwrap();
        if state.held != Some(self.fence) || state.capturing {
            return Err(fail(
                codes::ADMISSION_LOST,
                "the install admission is no longer held",
            ));
        }
        Ok(())
    }
}

impl Drop for FakeLease {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if state.held == Some(self.fence) {
            state.held = None;
        }
    }
}
