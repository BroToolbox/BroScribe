//! BroScribe's signed updater (SH23).
//!
//! The compiled stable release feed and pinned Ed25519 keys are used by
//! default. The configuration can change the source or turn off checks.
//! One update runs as:
//!
//! 1. [`Updater::check`] fetches the release envelope, verifies its
//!    signature against the pinned keys, validates the manifest (channel,
//!    expiry, product, target, application, a build strictly newer than the
//!    installed one and never older than a manifest already seen) and offers
//!    the artifact for this installation's target.
//! 2. [`Updater::download`] fetches exactly the signed number of bytes,
//!    checks the SHA-256, extracts the archive under strict path, link, size
//!    and count rules into a staging folder beside the installation, then
//!    checks the staged identity and the OS-native signature
//!    ([`broscribe_platform::update::UpdatePlatform::verify`]).
//! 3. [`Updater::install`] needs the [`UpdateExpectation`] the user
//!    confirmed (from, to, signer) and an [`InstallLease`]: the recording
//!    agent's fenced admission that nothing records and nothing may start
//!    recording. Under the state and installation locks it refuses another
//!    pending update before the admission is confirmed
//!    ([`Updater::staged_expected`] runs the same check before an admission
//!    is obtained), then under a persisted journal re-verifies the staged
//!    payload, replaces the installation (atomic exchange where the volume
//!    has one), verifies the result natively and keeps the previous copy.
//! 4. [`Updater::confirm`] removes the previous copy once the new version
//!    started; [`Updater::rollback`] restores it instead (for the confirmed
//!    expectation only, see [`Updater::awaiting_expected`]).
//!
//! A process that dies mid-replacement leaves the journal; the next owner of
//! the state folder lock runs [`Updater::recover`], which restores the
//! previous installation from whatever the filesystem holds. The staging
//! folder beside the installation records which state folder owns it, so
//! updaters of other state folders for the same installation never use or
//! remove a pending update or rollback copy that is not theirs
//! ([`codes::FOREIGN_STAGE`]).
//!
//! Failures are [`broscribe_core::Error::Service`] values whose stable codes
//! are listed in [`codes`]; nothing falls back to an unsigned path.
//!
//! The publisher's side, [`sign_manifest`], signs a release manifest with
//! Ed25519 keys in the platform secret store ([`generate_release_key`]) or
//! a private seed file ([`sign_manifest_file`]) and returns an envelope
//! [`verify_envelope`] has already accepted.

mod admission;
mod archive;
pub mod codes;
mod inventory;
mod manifest;
mod settings;
mod signing;
mod state;
mod transport;
mod updater;

pub use admission::{FakeInstallAdmission, InstallAdmission, InstallLease};
pub use manifest::{
    ArchiveFormat, Artifact, ENVELOPE_FORMAT, MANIFEST_FORMAT, ReleaseManifest, SIGNING_CONTEXT,
    verify_envelope, verify_release_envelope,
};
pub use settings::{KeyId, Limits, TrustAnchors, UpdateConfig};
pub use signing::{
    ExistingKey, GeneratedReleaseKey, KEY_EXISTS as SIGNING_KEY_EXISTS,
    KEY_INVALID as SIGNING_KEY_INVALID, ReleaseKey, ReleaseKeyLabel, SignedRelease,
    generate_release_key, new_release_key_file, release_key, sign_manifest, sign_manifest_file,
};
pub use state::{
    InventoryWarning, RemovalInventory, ensure_update_directory, inspect_update_directory,
    read_update_record, removal_inventory, removal_inventory_with_warnings, remove_owned_stage,
    tighten_update_directory,
};
pub use transport::{Cancel, FakeTransport, HttpsTransport, Transport};
pub use updater::{
    CheckOutcome, InstallOutcome, RecoveryOutcome, StagedUpdate, UpdateExpectation, UpdateOffer,
    UpdateStatus, Updater, pending_replacement,
};

use broscribe_core::Error;

pub(crate) fn fail(code: &'static str, message: impl Into<String>) -> Error {
    Error::Service {
        code,
        message: message.into(),
    }
}
