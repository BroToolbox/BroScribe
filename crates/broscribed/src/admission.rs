//! Install admission (SH23): the agent's fenced promise to the signed
//! updater that nothing records while an installation is replaced.
//!
//! - `install.acquire` is decided under the session mutex: refused while a
//!   recording is capturing, paused, in its stop grace, stopping or stopped
//!   and not saved yet (`recording_active`), and while another capture or
//!   admission holds the capture slot (`busy`). A grant persists a fence
//!   strictly greater than every earlier one before answering, so fences
//!   stay monotonic across agent restarts, and binds it to the asking
//!   connection.
//! - While an admission is held, every capture start (record.start,
//!   detection, a Record answer, setup samples, audio checks, permission
//!   prompts) is refused under the same mutex (`update_installing`).
//! - `install.confirm` re-validates the fence under the session mutex and
//!   makes the admission durable (persisted before the answer): it then
//!   outlives its connection and agent restarts, because the replacement
//!   restarts the agent, and is ended only by `install.release` (installed
//!   and verified, or rolled back). No capture can open between the
//!   replacement and that release.
//! - An admission not yet confirmed ends when its connection closes or the
//!   agent exits.
//!
//! The persisted record is `<data_dir>/run/install-admission.json` (0600).
//! A record that cannot be read fails closed, because the fences granted
//! before are unknown and no guess (a clock, a fresh count) can prove a new
//! fence greater than all of them: it is shown as a durable admission with
//! fence 0 in `DaemonState.install`, every capture start is refused, and
//! every `install.acquire|confirm|release` (fence 0 included) is refused
//! with `update_admission_damaged`. The agent never rewrites it; it holds
//! until the record is repaired (restored readable) and the agent restarts.

use std::path::{Path, PathBuf};

use broscribe_core::fsutil;
use broscribe_ipc::install_admission::Persisted;
use broscribe_ipc::{InstallHold, IpcError};

/// One client connection of the agent, for connection-bound admissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub(crate) u64);

/// The admission state, guarded by the daemon's `install` mutex; every
/// mutation is taken under the session mutex first.
pub(crate) struct Admission {
    path: PathBuf,
    last_fence: u64,
    held: Option<(InstallHold, Option<ConnectionId>)>,
    job_fence: Option<u64>,
    /// The record could not be read: nothing is granted, confirmed,
    /// released or written until it is repaired.
    damaged: bool,
}

impl Admission {
    /// The state a restarted agent starts with: a confirmed admission is
    /// still held (no connection); an unconfirmed one ended with the agent
    /// that granted it and is cleared on disk.
    pub(crate) fn load(data_dir: &Path) -> Self {
        let path = Persisted::path(data_dir);
        let persisted = match Persisted::load(data_dir) {
            Ok(Some(p)) => p,
            Ok(None) => Persisted::default(),
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    "install admission record unreadable; holding capture until it is repaired"
                );
                return Self {
                    path,
                    last_fence: 0,
                    held: Some((
                        InstallHold {
                            fence: 0,
                            durable: true,
                        },
                        None,
                    )),
                    job_fence: None,
                    damaged: true,
                };
            }
        };
        let admission = Self {
            path,
            last_fence: persisted.last_fence,
            held: persisted
                .held
                .map(|hold| (hold, None))
                .filter(|(hold, _)| hold.durable),
            job_fence: None,
            damaged: false,
        };
        if persisted.held.is_some_and(|hold| !hold.durable) {
            // The holder's connection died with the previous agent.
            if let Err(error) = admission.persist() {
                tracing::warn!(code = error.code, "clearing an ended install admission");
            }
        }
        admission
    }

    pub(crate) fn hold(&self) -> Option<InstallHold> {
        self.held.map(|(hold, _)| hold)
    }

    /// The refusal of a capture start while an admission is held.
    pub(crate) fn refuse_capture(&self) -> Result<(), IpcError> {
        match self.held {
            Some(_) if self.damaged => Err(IpcError::new(
                IpcError::INSTALLING,
                "the update admission record is damaged; recording starts once it is repaired",
            )),
            Some(_) => Err(IpcError::new(
                IpcError::INSTALLING,
                "an update is being installed; recording can start after it",
            )),
            None => Ok(()),
        }
    }

    /// The refusal of every admission call while the record is damaged.
    fn refuse_damaged(&self) -> Result<(), IpcError> {
        if self.damaged {
            return Err(IpcError::new(
                IpcError::ADMISSION_DAMAGED,
                "the update admission record cannot be read; repair it, then restart BroScribe Agent",
            ));
        }
        Ok(())
    }

    /// Grants a new admission to `connection`. The caller checked, under the
    /// session mutex, that nothing records or holds the capture slot.
    pub(crate) fn acquire(&mut self, connection: ConnectionId) -> Result<u64, IpcError> {
        self.refuse_damaged()?;
        if self.held.is_some() {
            return Err(IpcError::new(
                IpcError::BUSY,
                "another install admission is held",
            ));
        }
        let fence = self
            .last_fence
            .checked_add(1)
            .ok_or_else(|| IpcError::new("internal", "install admission fences ran out"))?;
        let previous = (self.last_fence, self.held.take());
        self.last_fence = fence;
        self.held = Some((
            InstallHold {
                fence,
                durable: false,
            },
            Some(connection),
        ));
        if let Err(error) = self.persist() {
            // Never grant a fence that a restart could hand out again.
            (self.last_fence, self.held) = previous;
            return Err(error);
        }
        Ok(fence)
    }

    pub(crate) fn acquire_job_fence(&mut self, connection: ConnectionId) -> Result<u64, IpcError> {
        let fence = self.acquire(connection)?;
        self.job_fence = Some(fence);
        Ok(fence)
    }

    pub(crate) fn fences_jobs(&self) -> bool {
        self.hold()
            .is_some_and(|hold| self.job_fence == Some(hold.fence))
    }

    /// Re-validates `fence` and makes it durable. The caller checked, under
    /// the session mutex, that nothing records.
    pub(crate) fn confirm(&mut self, fence: u64) -> Result<(), IpcError> {
        self.refuse_damaged()?;
        if self.job_fence == Some(fence) {
            return Err(lost(fence));
        }
        let Some((hold, connection)) = self.held.filter(|(h, _)| h.fence == fence) else {
            return Err(lost(fence));
        };
        if !hold.durable {
            self.held = Some((
                InstallHold {
                    fence,
                    durable: true,
                },
                connection,
            ));
            if let Err(error) = self.persist() {
                self.held = Some((hold, connection));
                return Err(error);
            }
        }
        Ok(())
    }

    /// Ends admission `fence`, from any connection.
    pub(crate) fn release(&mut self, fence: u64) -> Result<(), IpcError> {
        self.refuse_damaged()?;
        let Some(held) = self.held.filter(|(h, _)| h.fence == fence) else {
            return Err(lost(fence));
        };
        self.held = None;
        if let Err(error) = self.persist() {
            self.held = Some(held);
            return Err(error);
        }
        Ok(())
    }

    /// `connection` closed: an admission it holds that was not confirmed
    /// ends. Returns whether one ended.
    pub(crate) fn connection_closed(&mut self, connection: ConnectionId) -> bool {
        let Some((hold, _)) = self
            .held
            .filter(|(h, c)| !h.durable && *c == Some(connection))
        else {
            return false;
        };
        self.held = None;
        if let Err(error) = self.persist() {
            // In memory it ended; on disk an unconfirmed record is cleared
            // at the next start anyway.
            tracing::warn!(
                code = %error.code,
                fence = hold.fence,
                "recording the end of an install admission"
            );
        }
        true
    }

    fn persist(&self) -> Result<(), IpcError> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| IpcError::new("internal", "install admission path has no folder"))?;
        broscribe_platform_native::ipc::ensure_private_dir(dir)
            .map_err(|e| IpcError::from(broscribe_core::Error::io_at(dir, e)))?;
        let record = Persisted {
            last_fence: self.last_fence,
            held: self.hold(),
        };
        let mut bytes = serde_json::to_vec_pretty(&record)
            .map_err(|e| IpcError::new("internal", format!("serialising: {e}")))?;
        bytes.push(b'\n');
        fsutil::write_atomic_with_mode(&self.path, &bytes, Some(0o600)).map_err(IpcError::from)
    }
}

fn lost(fence: u64) -> IpcError {
    IpcError::new(
        IpcError::ADMISSION_LOST,
        format!("install admission {fence} is not held"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ConnectionId = ConnectionId(1);
    const B: ConnectionId = ConnectionId(2);

    #[test]
    fn fences_grow_across_restarts_and_only_confirmed_admissions_survive_them() {
        let tmp = tempfile::tempdir().unwrap();
        let mut admission = Admission::load(tmp.path());
        let first = admission.acquire(A).unwrap();
        assert_eq!(
            admission.acquire(B).unwrap_err().code,
            IpcError::BUSY,
            "one admission at a time"
        );
        // An unconfirmed admission ends with the agent.
        let mut restarted = Admission::load(tmp.path());
        assert!(restarted.hold().is_none());
        let second = restarted.acquire(B).unwrap();
        assert!(second > first);
        restarted.confirm(second).unwrap();
        // A confirmed one outlives the restart the replacement causes, and
        // its connection closing.
        let mut replaced = Admission::load(tmp.path());
        assert_eq!(
            replaced.hold(),
            Some(InstallHold {
                fence: second,
                durable: true
            })
        );
        assert!(!replaced.connection_closed(B));
        assert_eq!(
            replaced.refuse_capture().unwrap_err().code,
            IpcError::INSTALLING
        );
        assert_eq!(
            replaced.release(first).unwrap_err().code,
            IpcError::ADMISSION_LOST
        );
        replaced.release(second).unwrap();
        assert!(Admission::load(tmp.path()).hold().is_none());
        assert!(Admission::load(tmp.path()).acquire(A).unwrap() > second);
    }

    #[test]
    fn closing_the_holder_connection_ends_an_unconfirmed_admission_only() {
        let tmp = tempfile::tempdir().unwrap();
        let mut admission = Admission::load(tmp.path());
        let fence = admission.acquire(A).unwrap();
        assert!(!admission.connection_closed(B), "another connection");
        assert!(admission.connection_closed(A));
        assert_eq!(
            admission.confirm(fence).unwrap_err().code,
            IpcError::ADMISSION_LOST
        );
        assert!(admission.refuse_capture().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_record_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        Admission::load(tmp.path()).acquire(A).unwrap();
        let mode = std::fs::metadata(Persisted::path(tmp.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// SH23: an unreadable record fails closed until it is repaired: it
    /// shows as a held admission, refuses every capture and every admission
    /// call (releasing fence 0 included), is never rewritten, and once
    /// restored readable the count goes on above its last fence.
    #[test]
    fn an_unreadable_record_fails_closed_until_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let record = Persisted::path(tmp.path());
        let mut admission = Admission::load(tmp.path());
        let first = admission.acquire(A).unwrap();
        admission.confirm(first).unwrap();
        let healthy = std::fs::read(&record).unwrap();
        std::fs::write(&record, b"not json").unwrap();

        let mut damaged = Admission::load(tmp.path());
        assert_eq!(
            damaged.hold(),
            Some(InstallHold {
                fence: 0,
                durable: true
            })
        );
        assert_eq!(
            damaged.refuse_capture().unwrap_err().code,
            IpcError::INSTALLING
        );
        for refused in [
            damaged.release(0),
            damaged.release(first),
            damaged.confirm(0),
            damaged.acquire(B).map(drop),
        ] {
            assert_eq!(refused.unwrap_err().code, IpcError::ADMISSION_DAMAGED);
        }
        assert!(!damaged.connection_closed(B));
        assert_eq!(std::fs::read(&record).unwrap(), b"not json");

        // Repaired: the confirmed admission is back, and fences keep growing.
        std::fs::write(&record, healthy).unwrap();
        let mut repaired = Admission::load(tmp.path());
        repaired.release(first).unwrap();
        assert!(repaired.acquire(B).unwrap() > first);
    }
}
