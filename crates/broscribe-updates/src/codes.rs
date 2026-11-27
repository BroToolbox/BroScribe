//! Stable error codes of the updater ([`broscribe_core::Error::Service`]).
//! A disabled or incomplete configuration is
//! [`broscribe_core::Error::NotConfigured`] instead.

/// Another process owns the update state folder.
pub const BUSY: &str = "update_busy";
/// The envelope is not signed by a pinned key, or a pinned key's signature is wrong.
pub const SIGNATURE_INVALID: &str = "update_signature_invalid";
/// The envelope or manifest is malformed or violates a limit.
pub const MANIFEST_INVALID: &str = "update_manifest_invalid";
/// The manifest expired, or claims a publication time in the future.
pub const EXPIRED: &str = "update_expired";
/// The feed serves an older manifest than one already accepted (replay or freeze).
pub const FEED_ROLLBACK: &str = "update_feed_rollback";
/// The feed has no artifact for this installation's target, or a payload is
/// built for another target or architecture.
pub const WRONG_TARGET: &str = "update_wrong_target";
/// The artifact or payload is another product or application.
pub const WRONG_APP: &str = "update_wrong_app";
/// The payload's signer is not the pinned or installed signer.
pub const SIGNER_MISMATCH: &str = "update_signer_mismatch";
/// The payload would not move strictly forward from the installation.
pub const DOWNGRADE: &str = "update_downgrade";
/// The downloaded bytes do not have the signed length.
pub const SIZE_MISMATCH: &str = "update_size_mismatch";
/// The downloaded bytes do not have the signed SHA-256.
pub const HASH_MISMATCH: &str = "update_hash_mismatch";
/// The caller cancelled.
pub const CANCELLED: &str = "update_cancelled";
/// An archive entry is unsafe: path escape, link escape, duplicate, special
/// file, encryption or an unsupported compression method.
pub const ARCHIVE_UNSAFE: &str = "update_archive_unsafe";
/// The archive's entries or sizes exceed the signed or configured bounds.
pub const ARCHIVE_TOO_LARGE: &str = "update_archive_too_large";
/// The server answered with a redirect; redirects are never followed.
pub const REDIRECT_REFUSED: &str = "update_redirect_refused";
/// The server answered with another non-success status, or the transfer failed.
pub const TRANSFER_FAILED: &str = "update_transfer_failed";
/// A journal of an earlier update is pending (confirm, roll back or recover first).
pub const PENDING: &str = "update_pending";
/// The update a caller confirmed (from, to, signer) is no longer the one
/// pending; nothing was admitted or replaced. Show the current one again.
pub const EXPECTATION_CHANGED: &str = "update_changed";
/// Another update state folder's update keeps the staging folder beside the
/// installation; confirm, roll back or recover it from there. Nothing was
/// removed.
pub const FOREIGN_STAGE: &str = "update_foreign_stage";
/// The installation changed since the update was staged.
pub const INSTALLATION_CHANGED: &str = "update_installation_changed";
/// Windows could not move the install folder because another handle is open.
pub const INSTALLATION_IN_USE: &str = "update_installation_in_use";
/// The installed result failed native verification; the previous copy was restored.
pub const VERIFICATION_FAILED: &str = "update_verification_failed";
/// Recovery could not find the previous installation; nothing was deleted.
pub const RECOVERY_FAILED: &str = "update_recovery_failed";
/// The install admission was refused or lost (the agent records, or revoked it).
/// Producers refuse admission during capture with the existing `recording_active`.
pub const ADMISSION_LOST: &str = "update_admission_lost";
/// Code an [`crate::InstallAdmission`] uses to refuse while a recording is
/// capturing or not saved yet.
pub const RECORDING_ACTIVE: &str = "recording_active";
/// Code the agent uses to refuse starting a capture while an install
/// admission is held.
pub const INSTALLING: &str = "update_installing";
/// The installation's location cannot be replaced in place (translocated,
/// temporary, network, read-only, foreign-owned or not writable); native
/// [`broscribe_platform::update::UpdatePlatform::installation`] reports it.
pub const LOCATION_UNSUPPORTED: &str = "update_location_unsupported";

/// An installation contains entries outside the release layout or its signature seal.
pub use broscribe_platform::release_layout::RELEASE_TREE_UNSAFE;

/// The installed directory survived a manual overwrite with another build.
pub const INSTALLATION_CHANGED_OUTSIDE: &str = "update_installation_changed_outside";
