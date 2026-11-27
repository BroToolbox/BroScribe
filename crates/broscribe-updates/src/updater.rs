//! The update workflow: check, download and stage, install under a fenced
//! admission, confirm or roll back, and recovery after interruption.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use broscribe_core::config::UpdateSettings;
use broscribe_core::{Error, Result};
use broscribe_platform::update::{
    Installation, PayloadIdentity, SignerIdentity, UpdatePlatform, VerifyRequest,
};
use broscribe_platform::{plain_verbatim, same_path};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::{digest_of, hex_digest, release_version};
use crate::state::{
    ExclusiveLock, InventoryWarning, JOURNAL_SCHEMA, Journal, Phase, StageClaim, StateDir,
    check_cleanup_slots, cleanup_release_slots, lock_exclusive, remove_stage,
    remove_unjournaled_stage, validate_swap_tree,
};
use crate::{
    Artifact, Cancel, InstallLease, Limits, ReleaseManifest, Transport, UpdateConfig, archive,
    codes, fail, inventory, verify_envelope,
};

/// A release newer than the installation, for this installation's target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateOffer {
    pub manifest: ReleaseManifest,
    pub artifact: Artifact,
    pub installation: Installation,
}

impl UpdateOffer {
    /// The identity the staged payload must have.
    pub fn expected(&self) -> PayloadIdentity {
        PayloadIdentity {
            product: self.manifest.product.clone(),
            app_id: self.artifact.app_id.clone(),
            version: self.manifest.version.clone(),
            build: self.manifest.build,
            target: self.artifact.target.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Nothing newer: the feed's release is the installed one or older.
    UpToDate {
        installed: PayloadIdentity,
        latest_version: String,
        latest_build: u64,
    },
    /// The verified feed has no currently usable release for this target.
    NoVerifiedUpdate { installed: PayloadIdentity },
    /// Keep routine no-update results compact; allocate only a verified offer.
    Available(Box<UpdateOffer>),
}

/// A downloaded, extracted and natively verified payload, journaled as staged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedUpdate {
    pub from: PayloadIdentity,
    pub to: PayloadIdentity,
    pub payload: PathBuf,
    pub signer: SignerIdentity,
}

/// The replacement a user confirmed: the pending update from `from` to
/// `to` (whose `target` is the confirmed target) signed by `signer`, the
/// staged payload's natively verified signer. [`Updater::install`] and
/// [`Updater::rollback`] act only while exactly this update is pending,
/// checked under the state and installation locks before anything is
/// admitted or replaced; otherwise they refuse with
/// [`codes::EXPECTATION_CHANGED`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateExpectation {
    pub from: PayloadIdentity,
    pub to: PayloadIdentity,
    pub signer: SignerIdentity,
}

impl From<&StagedUpdate> for UpdateExpectation {
    fn from(staged: &StagedUpdate) -> Self {
        Self {
            from: staged.from.clone(),
            to: staged.to.clone(),
            signer: staged.signer.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOutcome {
    pub from: PayloadIdentity,
    pub to: PayloadIdentity,
    /// The replaced installation, kept until [`Updater::confirm`].
    pub previous: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// No journal; leftovers of an interrupted download were removed.
    Clean,
    /// A staged update that never started installing was removed.
    DiscardedStaged,
    /// An interrupted replacement or rollback was undone: `restored` is
    /// installed again and the staging folder is gone.
    RolledBack { restored: PayloadIdentity },
    /// The update is installed and verified; call [`Updater::confirm`] once
    /// it runs, or [`Updater::rollback`].
    AwaitingConfirmation {
        from: PayloadIdentity,
        to: PayloadIdentity,
    },
}

/// Pending updates carry the identities and the journaled signer an
/// [`UpdateExpectation`] names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    Idle,
    Staged {
        from: PayloadIdentity,
        to: PayloadIdentity,
        signer: SignerIdentity,
    },
    /// A replacement was interrupted; [`Updater::recover`] must run first.
    Interrupted {
        from: PayloadIdentity,
        to: PayloadIdentity,
    },
    AwaitingConfirmation {
        from: PayloadIdentity,
        to: PayloadIdentity,
        signer: SignerIdentity,
    },
}

/// Observes pending replacement without config, state creation, locks or repair.
/// The caller must fence concurrent updater execution before relying on it.
pub fn pending_replacement(state_dir: &Path) -> Result<UpdateStatus> {
    journal_status(StateDir::read_journal(state_dir)?)
}

fn journal_status(journal: Option<Journal>) -> Result<UpdateStatus> {
    if let Some(journal) = &journal {
        check_layout(journal)?;
    }
    Ok(match journal {
        None => UpdateStatus::Idle,
        Some(j) => match j.phase {
            Phase::Staged => UpdateStatus::Staged {
                from: j.from,
                to: j.to,
                signer: j.signer,
            },
            Phase::Committed => UpdateStatus::AwaitingConfirmation {
                from: j.from,
                to: j.to,
                signer: j.signer,
            },
            Phase::Swapping | Phase::Swapped | Phase::RollingBack => UpdateStatus::Interrupted {
                from: j.from,
                to: j.to,
            },
        },
    })
}

/// The updater. Holds the state folder's exclusive lock for its lifetime;
/// a second instance on the same folder fails with [`codes::BUSY`].
pub struct Updater<'a> {
    config: UpdateConfig,
    platform: &'a dyn UpdatePlatform,
    transport: &'a dyn Transport,
    state: StateDir,
    installation_lock: Option<(PathBuf, ExclusiveLock)>,
    protected_paths: Vec<PathBuf>,
    profiles: Vec<PathBuf>,
    seen_profiles: Vec<PathBuf>,
    inventory_warnings: std::cell::RefCell<Vec<InventoryWarning>>,
    config_base: PathBuf,
    /// Folders the installation may sit inside but must never be or
    /// contain (the user's home).
    contained_paths: Vec<PathBuf>,
}

impl<'a> Updater<'a> {
    /// Refuses disabled or incomplete settings ([`Error::NotConfigured`]).
    pub fn open(
        state_dir: &Path,
        settings: &UpdateSettings,
        platform: &'a dyn UpdatePlatform,
        transport: &'a dyn Transport,
    ) -> Result<Self> {
        let mut config = UpdateConfig::from_settings(settings)?;
        if config.signer.is_none() {
            config.signer = platform.default_signer_pin().map(str::to_owned);
        }
        let state = StateDir::open(state_dir)?;
        Ok(Self {
            config,
            platform,
            transport,
            state,
            installation_lock: None,
            protected_paths: vec![state_dir.to_path_buf()],
            profiles: Vec::new(),
            seen_profiles: Vec::new(),
            inventory_warnings: Default::default(),
            config_base: PathBuf::new(),
            contained_paths: Vec::new(),
        })
    }

    /// Diagnostics from skipped secondary profile state during this operation.
    pub fn take_inventory_warnings(&mut self) -> Vec<InventoryWarning> {
        std::mem::take(self.inventory_warnings.get_mut())
    }

    /// Includes the native and active profile locations identified by the
    /// caller. They are resolved again at each replacement boundary.
    pub fn protect_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Result<()> {
        self.protected_paths.extend(paths);
        if let Some((root, _)) = &self.installation_lock {
            self.check_location(root)?;
        }
        Ok(())
    }

    /// Discover secondary state for checks without reading protected library locations.
    pub fn track_seen_profiles(&mut self, profiles: Vec<PathBuf>) {
        self.seen_profiles = profiles;
    }

    /// Protect discovered profiles and re-read their configured libraries at
    /// each filesystem boundary. Resolve any relative settings in the CLI's
    /// working directory even when the replacement runs in a detached helper.
    pub fn protect_profiles(
        &mut self,
        profiles: Vec<PathBuf>,
        config_base: PathBuf,
    ) -> Result<Vec<PathBuf>> {
        self.profiles = profiles;
        self.config_base = config_base;
        let mut paths = Vec::new();
        for data in &self.profiles {
            paths.extend([data.clone(), data.join("cache")]);
            paths.extend(self.configured_library(data)?);
        }
        paths.extend(
            paths
                .iter()
                .map(|path| broscribe_platform::stable_path(path))
                .collect::<Vec<_>>(),
        );
        self.protect_paths(paths.iter().cloned())?;
        Ok(paths)
    }

    fn configured_library(&self, data: &Path) -> Result<Option<PathBuf>> {
        Ok(broscribe_core::config::Config::load(data)?
            .library
            .map(|path| self.config_base.join(path)))
    }

    /// Folders that may contain the installation but must never be equal to
    /// it or inside it, such as the user's home: replacing an installation
    /// that contains one would move or delete it.
    pub fn protect_from_containing(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<()> {
        self.contained_paths.extend(paths);
        if let Some((root, _)) = &self.installation_lock {
            self.check_location(root)?;
        }
        Ok(())
    }

    fn check_location(&self, root: &Path) -> Result<()> {
        let stable_root = broscribe_platform::stable_path(root);
        if let Some(contained) = self
            .contained_paths
            .iter()
            .find(|path| broscribe_platform::stable_path(path).starts_with(&stable_root))
        {
            return Err(fail(
                codes::LOCATION_UNSUPPORTED,
                format!(
                    "the installation at {} is or contains your home folder {}; move the app to its own folder",
                    root.display(),
                    contained.display()
                ),
            ));
        }
        let overlap = |path: &Path| broscribe_platform::paths_overlap(root, path);
        if let Some(protected) = self.protected_paths.iter().find(|path| overlap(path)) {
            return Err(location_overlap(root, protected));
        }
        for data in &self.profiles {
            if let Some(path) = self.configured_library(data)?.filter(|path| overlap(path)) {
                return Err(location_overlap(root, &path));
            }
        }
        Ok(())
    }

    /// Replaces the default [`Limits`] (tighter bounds only make sense).
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.config.limits = limits;
        self
    }

    pub fn status(&self) -> Result<UpdateStatus> {
        journal_status(self.state.journal()?)
    }

    /// Fetches and verifies the release envelope and decides whether it
    /// offers an update for the running installation at time `now`.
    pub fn check(&mut self, now: DateTime<Utc>, cancel: &Cancel) -> Result<CheckOutcome> {
        cancel.check()?;
        let installation = self.platform.installation()?;
        let limits = self.config.limits;
        let mut body = Vec::new();
        let no_update = || CheckOutcome::NoVerifiedUpdate {
            installed: installation.identity.clone(),
        };
        match self.transport.fetch(
            &self.config.feed_url,
            limits.max_envelope_bytes,
            &mut body,
            cancel,
        ) {
            Ok(_) => {}
            Err(Error::NotFound(_))
                if self.config.feed_url == broscribe_core::config::RELEASE_FEED =>
            {
                return Ok(no_update());
            }
            Err(error) => return Err(error),
        }
        let manifest = match verify_envelope(&body, &self.config.anchors, &limits, now) {
            Ok(manifest) => manifest,
            Err(error) if error.code() == codes::EXPIRED => return Ok(no_update()),
            Err(error) => return Err(error),
        };
        if manifest.channel != self.config.channel {
            return Err(fail(
                codes::MANIFEST_INVALID,
                format!(
                    "the feed serves channel {}, not {}",
                    manifest.channel, self.config.channel
                ),
            ));
        }
        let installed = &installation.identity;
        if manifest.product != installed.product {
            return Err(fail(
                codes::WRONG_APP,
                format!(
                    "the feed is for {}, not {}",
                    manifest.product, installed.product
                ),
            ));
        }
        let artifact = manifest.artifact_for(&installed.target).cloned();
        if let Some(artifact) = &artifact
            && artifact.app_id != installed.app_id
        {
            return Err(fail(
                codes::WRONG_APP,
                format!(
                    "the artifact is {}, not {}",
                    artifact.app_id, installed.app_id
                ),
            ));
        }
        self.check_location(&installation.root)?;
        let _check_lock = if self.holds_installation(&installation.root) {
            None
        } else {
            Some(lock_exclusive(&sibling(
                &installation.root,
                ".update.lock",
            )?)?)
        };
        let scope = self.feed_scope();
        let highest = self.highest_seen()?;
        if manifest.build < highest {
            return Err(fail(
                codes::FEED_ROLLBACK,
                format!(
                    "the feed serves build {} after build {highest} was seen",
                    manifest.build
                ),
            ));
        }
        let mut seen = self.state.seen()?;
        if seen.feed_scope.as_deref() != Some(scope.as_str())
            || manifest.build > seen.highest_build
            || seen.highest_version.is_none()
        {
            if let Some(previous) = seen.feed_scope.replace(scope.clone())
                && previous != scope
            {
                seen.previous_scopes.insert(previous, seen.highest_build);
            }
            seen.previous_scopes.remove(&scope);
            seen.highest_build = manifest.build;
            seen.highest_version = Some(manifest.version.clone());
            self.state.write_seen(&seen, self.platform)?;
        }
        let Some(artifact) = artifact else {
            if self.config.feed_url == broscribe_core::config::RELEASE_FEED {
                return Ok(no_update());
            }
            return Err(fail(
                codes::WRONG_TARGET,
                format!("the release has no artifact for {}", installed.target),
            ));
        };
        let installed_version = release_version(&installed.version).ok_or_else(|| {
            Error::InvalidManifest(format!(
                "the installed version {:?} is not MAJOR.MINOR.PATCH",
                installed.version
            ))
        })?;
        if manifest.build <= installed.build {
            return Ok(CheckOutcome::UpToDate {
                installed: installed.clone(),
                latest_version: manifest.version.clone(),
                latest_build: manifest.build,
            });
        }
        if manifest.semver() < installed_version {
            return Err(fail(
                codes::DOWNGRADE,
                format!(
                    "build {} carries version {}, older than the installed {}",
                    manifest.build, manifest.version, installed.version
                ),
            ));
        }
        Ok(CheckOutcome::Available(Box::new(UpdateOffer {
            manifest,
            artifact,
            installation,
        })))
    }

    /// Downloads, checks, extracts and natively verifies `offer`, leaving a
    /// staged update journaled beside the installation. On failure the updater
    /// removes only recognized staging contents. `progress` receives the bytes
    /// downloaded so far.
    /// A staging folder another state folder's pending update keeps is
    /// refused ([`codes::FOREIGN_STAGE`]) and left alone.
    pub fn download(
        &mut self,
        offer: &UpdateOffer,
        cancel: &Cancel,
        progress: &mut dyn FnMut(u64),
    ) -> Result<StagedUpdate> {
        if self.journal()?.is_some() {
            return Err(fail(
                codes::PENDING,
                "an earlier update is pending; confirm, roll back or recover it first",
            ));
        }
        cancel.check()?;
        let installation = self.platform.installation()?;
        if !same_path(&installation.root, &offer.installation.root)
            || installation.identity != offer.installation.identity
            || installation.signer != offer.installation.signer
        {
            return Err(fail(
                codes::INSTALLATION_CHANGED,
                "the installation changed since the update was offered",
            ));
        }
        self.check_location(&installation.root)?;
        self.lock_installation(&installation.root)?;
        self.check_location(&installation.root)?;
        let staging = staging_dir(&installation.root)?;
        self.check_location(&staging)?;
        match self.leftover(&staging)? {
            Leftover::Nothing => {}
            Leftover::Removable => self.remove_stage(&staging)?,
            Leftover::Foreign(error) => return Err(error),
        }
        let staged = self
            .state
            .claim_stage(
                &staging,
                &installation.identity,
                &offer.expected(),
                self.platform,
            )
            .and_then(|()| self.stage(offer, &installation, &staging, cancel, progress));
        staged.map_err(|error| {
            let cleanup = match self.journal() {
                Ok(None) => self.leftover(&staging).and_then(|leftover| match leftover {
                    Leftover::Nothing => Ok(()),
                    Leftover::Removable => self.remove_stage(&staging),
                    Leftover::Foreign(error) => Err(error),
                }),
                Ok(Some(_)) => Err(fail(
                    codes::PENDING,
                    "the update journal was already published",
                )),
                Err(error) => Err(error),
            };
            match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => fail(
                    error.code(),
                    format!(
                        "{error}. Could not clear staging at {}: {cleanup_error}",
                        staging.display()
                    ),
                ),
            }
        })
    }

    fn stage(
        &mut self,
        offer: &UpdateOffer,
        installation: &Installation,
        staging: &Path,
        cancel: &Cancel,
        progress: &mut dyn FnMut(u64),
    ) -> Result<StagedUpdate> {
        let artifact = &offer.artifact;
        let download = staging.join("download.zip");
        let mut file = File::create_new(&download).map_err(|e| Error::io_at(&download, e))?;
        let mut sink = HashingSink {
            file: &mut file,
            hash: Sha256::new(),
            written: 0,
            progress,
        };
        let length = self
            .transport
            .fetch(&artifact.url, artifact.size, &mut sink, cancel)?;
        // A cancellation that arrived with the last bytes (or from the
        // progress callback) wins before anything is checked or extracted.
        cancel.check()?;
        let digest = sink.hash.finalize();
        if length != artifact.size {
            return Err(fail(
                codes::SIZE_MISMATCH,
                format!(
                    "downloaded {length} bytes; the release declares {}",
                    artifact.size
                ),
            ));
        }
        if digest[..] != digest_of(artifact)[..] {
            return Err(fail(
                codes::HASH_MISMATCH,
                format!(
                    "the download's SHA-256 is {}, not the signed {}",
                    hex_digest(&digest[..]),
                    artifact.sha256
                ),
            ));
        }
        file.sync_all().map_err(|e| Error::io_at(&download, e))?;
        drop(file);
        self.check_location(&installation.root)?;
        self.check_location(staging)?;
        let payload = archive::extract(
            &download,
            &staging.join("incoming"),
            artifact,
            &self.config.limits,
            self.platform,
            cancel,
            |entries| {
                self.state
                    .remember_archive_entries(staging, entries, self.platform)
            },
        )?;
        self.check_location(staging)?;
        std::fs::remove_file(&download).map_err(|e| Error::io_at(&download, e))?;
        cancel.check()?;
        let expected = offer.expected();
        let contents = inventory::capture(&payload, &self.config.limits, cancel)?;
        let signer = self.verify_payload(&payload, &expected, &installation.signer, &[])?;
        check_forward(&installation.identity, &expected)?;
        let journal = Journal {
            schema: JOURNAL_SCHEMA,
            phase: Phase::Staged,
            install_root: installation.root.clone(),
            staging: staging.to_path_buf(),
            payload_root: artifact.root.clone(),
            from: installation.identity.clone(),
            to: expected.clone(),
            installed_signer: installation.signer.clone(),
            signer: signer.clone(),
            inventory: Some(contents),
            preserved_files: Vec::new(),
            owner_pid: std::process::id(),
            fence: None,
            updated_at: Utc::now(),
        };
        // Last point of no publication: a cancelled download never becomes staged.
        cancel.check()?;
        self.check_location(&installation.root)?;
        self.check_location(staging)?;
        self.state.write_journal(&journal, self.platform)?;
        Ok(StagedUpdate {
            from: journal.from,
            to: expected,
            payload,
            signer,
        })
    }

    /// The update an earlier [`Self::download`] (in this or another
    /// process) staged, re-verified now: the journal's layout, the staging
    /// folder's owner record, the running installation (root, identity and
    /// signer) against what was staged over, and the staged payload natively
    /// against the journaled identity and signer, the configured pin and the
    /// installed signer. `Ok(None)` only without a journal: leftovers of a
    /// download that never staged are not an update. Reads only; nothing is
    /// deleted, moved or installed, and a failure leaves the stage for
    /// [`Self::discard`]. [`Self::install`] verifies everything again under
    /// its admission.
    pub fn staged(&mut self) -> Result<Option<StagedUpdate>> {
        let Some(journal) = self.journal()? else {
            return Ok(None);
        };
        if journal.phase != Phase::Staged {
            return Err(fail(
                codes::PENDING,
                "an update is installing or awaits confirmation; check its status",
            ));
        }
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        let (payload, signer) = self.verify_stage(&journal)?;
        Ok(Some(StagedUpdate {
            from: journal.from,
            to: journal.to,
            payload,
            signer,
        }))
    }

    /// [`Self::staged`] for a confirmation: the staged update must be
    /// exactly `expected` ([`codes::EXPECTATION_CHANGED`] otherwise), and
    /// it is fully re-verified. Call it before obtaining an install
    /// admission and keep this updater (and so its locks) until
    /// [`Self::install`]. Changes nothing.
    pub fn staged_expected(&mut self, expected: &UpdateExpectation) -> Result<StagedUpdate> {
        let journal = self.journal_in(Phase::Staged)?;
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        check_expected(expected, &journal)?;
        let (payload, signer) = self.verify_stage(&journal)?;
        Ok(StagedUpdate {
            from: journal.from,
            to: journal.to,
            payload,
            signer,
        })
    }

    /// Preflight for an install command before it asks for agent admission.
    /// A changed extraction is refused before any admission or replacement.
    pub fn prepare_install(
        &mut self,
        expected: Option<&UpdateExpectation>,
    ) -> Result<StagedUpdate> {
        let journal = self.journal_in(Phase::Staged)?;
        self.check_location(&journal.install_root)?;
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        self.check_release_tree(&journal.install_root, &journal, &journal.from)?;
        if let Some(expected) = expected {
            check_expected(expected, &journal)?;
        }
        let (payload, signer) = self.verify_install_stage(&journal, &Cancel::new())?;
        Ok(StagedUpdate {
            from: journal.from,
            to: journal.to,
            payload,
            signer,
        })
    }

    /// Clears a staged update that has not started installing. If its contents
    /// changed, the complete staging folder is moved aside and returned.
    pub fn discard(&mut self) -> Result<Option<PathBuf>> {
        let journal = self.journal_in(Phase::Staged)?;
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        self.check_no_retired_installation(&journal)?;
        let payload_refused = std::cell::Cell::new(false);
        let validate = |tree: &Path| {
            let result = self
                .platform
                .inspect(tree)
                .and_then(|identity| self.check_release_tree(tree, &journal, &identity));
            if result.is_err() {
                payload_refused.set(true);
            }
            result
        };
        match self.remove_replacement_stage_with(&journal, validate) {
            Ok(()) => {
                self.state.clear_journal(self.platform)?;
                Ok(None)
            }
            Err(error) if payload_refused.get() || error.code() == codes::RELEASE_TREE_UNSAFE => {
                let kept = journal.staging.with_extension(format!(
                    "update-kept-{}-{}",
                    std::process::id(),
                    Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                ));
                self.rename_new(&journal.staging, &kept)?;
                self.state.clear_journal(self.platform).map_err(|error| {
                    fail(codes::RECOVERY_FAILED, format!("The changed staging was kept at {}. Could not clear its journal: {error}", kept.display()))
                })?;
                Ok(Some(kept))
            }
            Err(error) => Err(error),
        }
    }

    /// Replaces the installation with the staged update `expected`.
    /// Everything is checked again first: the staged update must still be
    /// exactly `expected` ([`codes::EXPECTATION_CHANGED`]) and verify
    /// natively. `lease` must be the agent's held admission; it is confirmed
    /// only after those checks and again right before the first rename. A
    /// failure before that leaves the installation untouched and the update
    /// staged; a failure after it restores the previous copy.
    pub fn install(
        &mut self,
        expected: &UpdateExpectation,
        lease: &dyn InstallLease,
        cancel: &Cancel,
    ) -> Result<InstallOutcome> {
        let mut journal = self.journal_in(Phase::Staged)?;
        self.check_location(&journal.install_root)?;
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        check_expected(expected, &journal)?;
        cancel.check()?;
        self.check_release_tree(&journal.install_root, &journal, &journal.from)?;
        let (incoming, _) = self.verify_install_stage(&journal, cancel)?;
        let root = journal.install_root.clone();
        journal.preserved_files = self.platform.preserved_files(&root)?;
        cancel.check()?;
        lease.confirm()?;
        journal.phase = Phase::Swapping;
        journal.fence = Some(lease.fence());
        journal.owner_pid = std::process::id();
        journal.updated_at = Utc::now();
        self.state.write_journal(&journal, self.platform)?;
        if let Err(error) = self.verify_inventory(&journal, &incoming, cancel) {
            if cancel.check().is_err() {
                journal.phase = Phase::Staged;
                self.state.write_journal(&journal, self.platform)?;
                return Err(error);
            }
            return self.discard_changed_stage(&journal, error);
        }
        if let Err(error) = self.copy_preserved_files(&journal, &incoming) {
            return self.discard_changed_stage(&journal, error);
        }
        if let Err(error) = cancel
            .check()
            .and_then(|()| lease.confirm())
            .and_then(|()| self.check_release_tree(&root, &journal, &journal.from))
            .and_then(|()| self.check_release_tree(&incoming, &journal, &journal.to))
            .and_then(|()| self.check_location(&root))
            .and_then(|()| self.check_location(&journal.staging))
        {
            if !journal.preserved_files.is_empty() {
                return self.discard_changed_stage(&journal, error);
            }
            journal.phase = Phase::Staged;
            self.state.write_journal(&journal, self.platform)?;
            return Err(error);
        }
        self.reject_superseded_stage(&journal)?;
        let previous = journal.previous();
        if let Err(error) = self.swap_in(&root, &incoming, &previous) {
            self.restore(&mut journal)?;
            return Err(error);
        }
        journal.phase = Phase::Swapped;
        self.state.write_journal(&journal, self.platform)?;
        if let Err(error) = self.verify_payload(
            &root,
            &journal.to,
            &journal.installed_signer,
            &journal.preserved_files,
        ) {
            self.restore(&mut journal)?;
            return Err(fail(
                codes::VERIFICATION_FAILED,
                format!("the installed update failed verification and was rolled back: {error}"),
            ));
        }
        journal.phase = Phase::Committed;
        journal.updated_at = Utc::now();
        self.state.write_journal(&journal, self.platform)?;
        Ok(InstallOutcome {
            from: journal.from,
            to: journal.to,
            previous,
        })
    }

    fn swap_in(&self, root: &Path, incoming: &Path, previous: &Path) -> Result<()> {
        create_parent(previous)?;
        if self.exchange(root, incoming)? {
            self.rename_new(incoming, previous)
        } else {
            self.rename_new(root, previous)?;
            self.rename_new(incoming, root)
        }
    }

    fn exchange(&self, a: &Path, b: &Path) -> Result<bool> {
        self.check_location(a)?;
        self.check_location(b)?;
        self.platform.exchange(a, b)
    }

    fn rename_new(&self, from: &Path, to: &Path) -> Result<()> {
        self.check_location(from)?;
        self.check_location(to)?;
        self.platform.rename_new(from, to)
    }

    fn remove_stage(&self, staging: &Path) -> Result<()> {
        self.check_location(staging)?;
        remove_unjournaled_stage(staging, self.platform)
    }

    fn remove_replacement_stage(&self, journal: &Journal) -> Result<()> {
        self.remove_replacement_stage_with(journal, |tree| {
            self.check_release_tree(tree, journal, &self.platform.inspect(tree)?)
        })
    }

    fn remove_replacement_stage_with(
        &self,
        journal: &Journal,
        validate: impl Fn(&Path) -> Result<()>,
    ) -> Result<()> {
        self.check_location(&journal.staging)?;
        cleanup_release_slots(
            &journal.staging,
            journal.payload_root.as_ref(),
            journal.install_root.file_name().expect("installation name"),
            validate,
            self.platform,
        )?;
        remove_stage(&journal.staging)
    }

    /// Removes the previous copy once the new version runs. The journal
    /// goes last, so an interrupted removal is finished by confirming again.
    pub fn confirm(&mut self) -> Result<PayloadIdentity> {
        let journal = self.journal_in(Phase::Committed)?;
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        let installation = self.platform.installation()?;
        if !same_path(&installation.root, &journal.install_root)
            || installation.identity != journal.to
        {
            return Err(fail(
                codes::INSTALLATION_CHANGED,
                "the running installation is not the installed update",
            ));
        }
        self.check_release_tree(&journal.install_root, &journal, &journal.to)?;
        self.remove_replacement_stage(&journal)?;
        self.state.remember_installation(&journal, self.platform)?;
        self.state.clear_journal(self.platform)?;
        Ok(journal.to)
    }

    /// [`Self::rollback`]'s check for a confirmation: the installed update
    /// awaiting confirmation must be exactly `expected`
    /// ([`codes::EXPECTATION_CHANGED`] otherwise). Call it before obtaining
    /// an install admission and keep this updater until the rollback.
    /// Changes nothing.
    pub fn awaiting_expected(&mut self, expected: &UpdateExpectation) -> Result<()> {
        let journal = self.journal_in(Phase::Committed)?;
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        check_expected(expected, &journal)?;
        self.check_restore_trees(&journal)
    }

    /// Restores the previous copy of the installed, unconfirmed update
    /// `expected`; refused ([`codes::EXPECTATION_CHANGED`]) before the
    /// admission is confirmed when another update awaits confirmation.
    pub fn rollback(
        &mut self,
        expected: &UpdateExpectation,
        lease: &dyn InstallLease,
    ) -> Result<PayloadIdentity> {
        let mut journal = self.journal_in(Phase::Committed)?;
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        check_expected(expected, &journal)?;
        self.check_restore_trees(&journal)?;
        lease.confirm()?;
        journal.fence = Some(lease.fence());
        self.restore(&mut journal)
    }

    /// Qualifies the trees an interrupted swap would move before a restart or handoff.
    pub fn prepare_recovery(&mut self) -> Result<()> {
        if let Some(journal) = self.journal()?
            && matches!(
                journal.phase,
                Phase::Swapping | Phase::Swapped | Phase::RollingBack
            )
        {
            self.lock_installation(&journal.install_root)?;
            self.check_stage(&journal)?;
            self.check_restore_trees(&journal)?;
        }
        Ok(())
    }

    /// Completes or undoes whatever an earlier owner of this state folder
    /// left behind. An interrupted replacement or rollback always ends with
    /// the previous installation restored; an installed, verified update
    /// waits for [`Self::confirm`]. Holding this updater means the earlier
    /// owner is gone (it held the same exclusive lock). Without a journal,
    /// only this state folder's leftovers are removed: a staging folder
    /// another state folder's update keeps (its incoming payload or the
    /// previous installation it may still restore) is left to that owner.
    pub fn recover(&mut self, lease: &dyn InstallLease) -> Result<RecoveryOutcome> {
        let Some(mut journal) = self.journal()? else {
            if let Ok(installation) = self.platform.installation() {
                self.lock_installation(&installation.root)?;
                let staging = staging_dir(&installation.root)?;
                if let Leftover::Removable = self.leftover(&staging)? {
                    self.remove_stage(&staging)?;
                }
            }
            return Ok(RecoveryOutcome::Clean);
        };
        self.lock_installation(&journal.install_root)?;
        self.check_stage(&journal)?;
        match journal.phase {
            Phase::Staged => {
                self.discard_staged(&journal)?;
                Ok(RecoveryOutcome::DiscardedStaged)
            }
            Phase::Committed => Ok(RecoveryOutcome::AwaitingConfirmation {
                from: journal.from,
                to: journal.to,
            }),
            Phase::Swapping | Phase::Swapped | Phase::RollingBack => {
                self.check_restore_trees(&journal)?;
                lease.confirm()?;
                journal.fence = Some(lease.fence());
                let restored = self.restore(&mut journal)?;
                Ok(RecoveryOutcome::RolledBack { restored })
            }
        }
    }

    /// Puts `journal.from` back at the installation root from wherever it
    /// is, then removes the staging folder and the journal (the journal
    /// last). Idempotent: every step is decided from the identities on disk,
    /// so a restore interrupted at any point can run again. Nothing is
    /// deleted unless the previous installation is back in place.
    fn restore(&self, journal: &mut Journal) -> Result<PayloadIdentity> {
        self.check_location(&journal.install_root)?;
        self.check_location(&journal.staging)?;
        self.check_restore_trees(journal)?;
        journal.phase = Phase::RollingBack;
        journal.owner_pid = std::process::id();
        journal.updated_at = Utc::now();
        self.state.write_journal(journal, self.platform)?;
        let root = journal.install_root.clone();
        let slots = [journal.incoming(), journal.previous(), journal.discard()];
        if self.identity_at(&root).as_ref() != Some(&journal.from) {
            let old = slots
                .iter()
                .find(|slot| self.identity_at(slot).as_ref() == Some(&journal.from))
                .cloned()
                .ok_or_else(|| {
                    fail(
                        codes::RECOVERY_FAILED,
                        format!(
                            "the previous installation (build {}) was not found; nothing was deleted",
                            journal.from.build
                        ),
                    )
                })?;
            if exists(&root) {
                if !self.exchange(&root, &old)? {
                    let park = slots
                        .iter()
                        .find(|slot| **slot != old && !exists(slot))
                        .cloned()
                        .ok_or_else(|| {
                            fail(
                                codes::RECOVERY_FAILED,
                                "no free staging slot to park the rejected payload; nothing was deleted",
                            )
                        })?;
                    create_parent(&park)?;
                    self.rename_new(&root, &park)?;
                    self.rename_new(&old, &root)?;
                }
            } else {
                self.rename_new(&old, &root)?;
            }
            if self.identity_at(&root).as_ref() != Some(&journal.from) {
                return Err(fail(
                    codes::RECOVERY_FAILED,
                    "the restored installation is not the previous one; nothing was deleted",
                ));
            }
        }
        self.remove_replacement_stage(journal)?;
        self.state.clear_journal(self.platform)?;
        Ok(journal.from.clone())
    }

    fn check_release_tree(
        &self,
        root: &Path,
        journal: &Journal,
        identity: &PayloadIdentity,
    ) -> Result<()> {
        let prior = if identity == &journal.from {
            let current = self.platform.inspect(root)?;
            let record = self.state.installed_inventory(
                &self.profiles,
                (&journal.install_root, root, &current),
                self.platform,
                &mut self.inventory_warnings.borrow_mut(),
            )?;
            if let Some(record) = &record {
                record.check_identity(root, &current)?;
            }
            if &current != identity {
                return Err(fail(
                    codes::INSTALLATION_CHANGED,
                    format!(
                        "The installed build changed at {}. Check for updates again.",
                        root.display()
                    ),
                ));
            }
            record
        } else {
            None
        };
        validate_swap_tree(
            root,
            journal,
            identity,
            prior.as_ref(),
            self.platform,
            &self.config.limits,
        )
    }

    fn check_restore_trees(&self, journal: &Journal) -> Result<()> {
        let validate =
            |root: &Path| self.check_release_tree(root, journal, &self.platform.inspect(root)?);
        match std::fs::symlink_metadata(&journal.install_root) {
            Ok(_) => validate(&journal.install_root)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io_at(&journal.install_root, error)),
        }
        check_cleanup_slots(
            &journal.staging,
            journal.payload_root.as_ref(),
            journal.install_root.file_name().expect("installation name"),
            validate,
            self.platform,
        )
    }

    fn identity_at(&self, path: &Path) -> Option<PayloadIdentity> {
        if !exists(path) {
            return None;
        }
        self.platform.inspect(path).ok()
    }

    fn verify_payload(
        &self,
        payload: &Path,
        expected: &PayloadIdentity,
        installed: &SignerIdentity,
        preserved_files: &[String],
    ) -> Result<SignerIdentity> {
        let signer = self.platform.verify(
            payload,
            &VerifyRequest {
                expected,
                installed,
                pin: self.config.signer.as_deref(),
                preserved_files,
            },
        )?;
        let actual = self.platform.inspect(payload)?;
        if actual.target != expected.target {
            return Err(fail(
                codes::WRONG_TARGET,
                format!("the payload is built for {}", actual.target),
            ));
        }
        if actual.product != expected.product || actual.app_id != expected.app_id {
            return Err(fail(
                codes::WRONG_APP,
                format!("the payload is {} {}", actual.product, actual.app_id),
            ));
        }
        if actual != *expected {
            return Err(fail(
                codes::MANIFEST_INVALID,
                format!(
                    "the payload is {} build {}, not the signed {} build {}",
                    actual.version, actual.build, expected.version, expected.build
                ),
            ));
        }
        Ok(signer)
    }

    /// The running installation must be the one the journal staged over:
    /// same root, identity and signer.
    fn check_installation(&self, journal: &Journal) -> Result<()> {
        let installation = self.platform.installation()?;
        if !same_path(&installation.root, &journal.install_root)
            || installation.identity != journal.from
            || installation.signer != journal.installed_signer
        {
            return Err(fail(
                codes::INSTALLATION_CHANGED,
                "the installation changed since the update was staged; discard it",
            ));
        }
        Ok(())
    }

    /// The staged update re-verified: the running installation is the one
    /// staged over, and the payload verifies natively as the journaled
    /// identity with the journaled signer and moves forward. Returns the
    /// payload and its signer.
    fn verify_stage(&self, journal: &Journal) -> Result<(PathBuf, SignerIdentity)> {
        self.check_installation(journal)?;
        self.verify_staged_payload(journal, staged_payload(journal)?)
    }

    fn verify_staged_payload(
        &self,
        journal: &Journal,
        incoming: PathBuf,
    ) -> Result<(PathBuf, SignerIdentity)> {
        let signer = self.verify_payload(&incoming, &journal.to, &journal.installed_signer, &[])?;
        if signer != journal.signer {
            return Err(fail(
                codes::SIGNER_MISMATCH,
                "the staged update no longer verifies as signed by the signer it was downloaded \
                 with; discard it",
            ));
        }
        check_forward(&journal.from, &journal.to)?;
        Ok((incoming, signer))
    }

    fn verify_install_stage(
        &self,
        journal: &Journal,
        cancel: &Cancel,
    ) -> Result<(PathBuf, SignerIdentity)> {
        self.reject_superseded_stage(journal)?;
        self.check_installation(journal)?;
        let incoming = match staged_payload(journal) {
            Ok(incoming) => incoming,
            Err(error) => return self.discard_changed_stage(journal, error),
        };
        if journal.inventory.is_none() {
            return self.discard_changed_stage(
                journal,
                fail(
                    codes::VERIFICATION_FAILED,
                    "the staged update must be downloaded again to verify its contents",
                ),
            );
        }
        if let Err(error) = self.verify_inventory(journal, &incoming, cancel) {
            if cancel.check().is_err() {
                return Err(error);
            }
            return self.discard_changed_stage(journal, error);
        }
        self.verify_staged_payload(journal, incoming)
    }

    fn feed_scope(&self) -> String {
        self.config
            .anchors
            .feed_scope(&self.config.feed_url, &self.config.channel)
    }

    fn highest_seen(&self) -> Result<u64> {
        self.state.highest_seen(
            if self.seen_profiles.is_empty() {
                &self.profiles
            } else {
                &self.seen_profiles
            },
            &self.feed_scope(),
            &mut self.inventory_warnings.borrow_mut(),
        )
    }

    fn reject_superseded_stage(&self, journal: &Journal) -> Result<()> {
        let highest = self.highest_seen()?;
        if journal.to.build < highest {
            return self.discard_changed_stage(
                journal,
                fail(
                    codes::FEED_ROLLBACK,
                    format!(
                        "staged build {} is older than verified build {highest}",
                        journal.to.build
                    ),
                ),
            );
        }
        Ok(())
    }

    fn verify_inventory(&self, journal: &Journal, incoming: &Path, cancel: &Cancel) -> Result<()> {
        if let Some(expected) = &journal.inventory
            && inventory::capture(incoming, &self.config.limits, cancel)? == *expected
        {
            return Ok(());
        }
        Err(fail(
            codes::VERIFICATION_FAILED,
            "the staged update no longer matches the verified download; review the staged files before retrying",
        ))
    }

    fn check_no_retired_installation(&self, journal: &Journal) -> Result<()> {
        // No swap has happened when discarding an extraction. Retired
        // installation slots must remain untouched if they unexpectedly exist.
        for name in [
            "previous",
            "discard",
            "deleting-previous",
            "deleting-discard",
        ] {
            let path = journal.staging.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(fail(
                        codes::RELEASE_TREE_UNSAFE,
                        format!("Unexpected retired installation at {}", path.display()),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::io_at(&path, error)),
            }
        }
        Ok(())
    }

    fn discard_staged(&self, journal: &Journal) -> Result<()> {
        self.check_no_retired_installation(journal)?;
        self.remove_replacement_stage(journal)?;
        self.state.clear_journal(self.platform)
    }

    fn discard_changed_stage<T>(&self, journal: &Journal, error: Error) -> Result<T> {
        // The original refusal explains why the update cannot install. Its
        // extraction can be cleared only if it still qualifies for removal.
        // This can happen when the feed superseded it or the payload is gone.
        let _ = self.discard_staged(journal);
        Err(error)
    }

    fn copy_preserved_files(&self, journal: &Journal, incoming: &Path) -> Result<()> {
        for name in &journal.preserved_files {
            let source = journal.install_root.join(name);
            let destination = incoming.join(name);
            let mut input = File::open(&source).map_err(|e| Error::io_at(&source, e))?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|e| Error::io_at(&destination, e))?;
            std::io::copy(&mut input, &mut output)
                .and_then(|_| output.sync_all())
                .map_err(|e| Error::io_at(&destination, e))?;
        }
        if !journal.preserved_files.is_empty() {
            self.platform.sync_dir(incoming)?;
        }
        Ok(())
    }

    /// Under the installation lock: the staging folder beside a journaled
    /// update must be missing (the operations then report what is missing)
    /// or this state folder's record of that same update. Another state
    /// folder's is refused ([`codes::FOREIGN_STAGE`]), never used or removed.
    fn check_stage(&self, journal: &Journal) -> Result<()> {
        self.check_location(&journal.staging)?;
        let update = Some((&journal.from, &journal.to));
        match self.state.stage_claim(&journal.staging, update)? {
            StageClaim::Absent | StageClaim::Unclaimed | StageClaim::Ours => Ok(()),
            StageClaim::OursOtherUpdate => Err(fail(
                codes::RECOVERY_FAILED,
                "the staging folder holds another update than the journal; nothing was changed",
            )),
            StageClaim::Foreign(state_dir) => {
                Err(foreign_stage(&journal.staging, Some(state_dir.as_path())))
            }
            StageClaim::Unknown => Err(foreign_stage(&journal.staging, None)),
        }
    }

    /// Under the installation lock and without a journal: what this state
    /// folder may do with the staging folder at `staging`.
    fn leftover(&self, staging: &Path) -> Result<Leftover> {
        Ok(match self.state.stage_claim(staging, None)? {
            StageClaim::Absent => Leftover::Nothing,
            StageClaim::Unclaimed | StageClaim::Ours | StageClaim::OursOtherUpdate => {
                Leftover::Removable
            }
            StageClaim::Foreign(state_dir) => {
                Leftover::Foreign(foreign_stage(staging, Some(state_dir.as_path())))
            }
            StageClaim::Unknown => Leftover::Foreign(foreign_stage(staging, None)),
        })
    }

    /// The persisted journal, refused unless its paths have the layout
    /// [`Self::download`] writes, so nothing acts outside the staging folder.
    fn journal(&self) -> Result<Option<Journal>> {
        let journal = self.state.journal()?;
        if let Some(journal) = &journal {
            check_layout(journal)?;
        }
        Ok(journal)
    }

    fn journal_in(&self, phase: Phase) -> Result<Journal> {
        match self.journal()? {
            Some(journal) if journal.phase == phase => Ok(journal),
            Some(_) => Err(fail(
                codes::PENDING,
                "the pending update is in another state; check its status",
            )),
            None => Err(Error::NotFound("no pending update".into())),
        }
    }

    fn holds_installation(&self, root: &Path) -> bool {
        self.installation_lock
            .as_ref()
            .is_some_and(|(locked, _)| same_path(locked, root))
    }

    /// Takes the per-installation lock beside `root`, so updaters with
    /// different state folders (other users) never stage over each other.
    fn lock_installation(&mut self, root: &Path) -> Result<()> {
        self.check_location(root)?;
        if self.holds_installation(root) {
            return Ok(());
        }
        let path = sibling(root, ".update.lock")?;
        let file = lock_exclusive(&path)?;
        self.installation_lock = Some((root.to_path_buf(), file));
        Ok(())
    }
}

fn location_overlap(root: &Path, protected: &Path) -> Error {
    fail(
        codes::LOCATION_UNSUPPORTED,
        format!(
            "the installation at {} overlaps a BroScribe library, data or cache folder at {}; move the app to a separate location",
            root.display(),
            protected.display()
        ),
    )
}

/// What a state folder without a journal may do with the staging folder.
enum Leftover {
    Nothing,
    /// Its own leftover, or a claim interrupted before its owner record.
    Removable,
    /// Another state folder's: the [`codes::FOREIGN_STAGE`] refusal.
    Foreign(Error),
}

/// The pending update in `journal` must be exactly the confirmed one.
fn check_expected(expected: &UpdateExpectation, journal: &Journal) -> Result<()> {
    if expected.from == journal.from
        && expected.to == journal.to
        && expected.signer == journal.signer
    {
        return Ok(());
    }
    Err(fail(
        codes::EXPECTATION_CHANGED,
        format!(
            "the pending update is BroScribe {} (build {}, {}) over {} (build {}), not the one \
             confirmed; nothing was admitted or replaced",
            journal.to.version,
            journal.to.build,
            journal.to.target,
            journal.from.version,
            journal.from.build
        ),
    ))
}

/// `state_dir`: the owning state folder, when an owner record names one.
fn foreign_stage(staging: &Path, state_dir: Option<&Path>) -> Error {
    match state_dir {
        Some(state_dir) => fail(
            codes::FOREIGN_STAGE,
            format!(
                "{} keeps an update of the update state in {}; confirm, roll back or recover it \
                 from there; nothing was removed",
                staging.display(),
                state_dir.display()
            ),
        ),
        None => fail(
            codes::FOREIGN_STAGE,
            format!(
                "{} has no owner record; remove it only if no other BroScribe data folder has \
                 an update pending; nothing was removed",
                staging.display()
            ),
        ),
    }
}

/// `to` must be the same product, application and target as `from`, with
/// a strictly greater build and no lower version.
fn check_forward(from: &PayloadIdentity, to: &PayloadIdentity) -> Result<()> {
    if from.target != to.target {
        return Err(fail(
            codes::WRONG_TARGET,
            format!(
                "{} cannot replace a {} installation",
                to.target, from.target
            ),
        ));
    }
    if from.product != to.product || from.app_id != to.app_id {
        return Err(fail(
            codes::WRONG_APP,
            format!("{} cannot replace {}", to.app_id, from.app_id),
        ));
    }
    let older = match (release_version(&from.version), release_version(&to.version)) {
        (Some(from_version), Some(to_version)) => to_version < from_version,
        _ => true,
    };
    if to.build <= from.build || older {
        return Err(fail(
            codes::DOWNGRADE,
            format!(
                "{} build {} does not move forward from {} build {}",
                to.version, to.build, from.version, from.build
            ),
        ));
    }
    Ok(())
}

/// `<parent>/.<name>.update`: on the installation's volume, so the final
/// renames never cross filesystems.
fn staging_dir(root: &Path) -> Result<PathBuf> {
    sibling(root, ".update")
}

/// A journal names an absolute, normalized installation root, the staging
/// folder beside it and a payload root of one plain name; anything else
/// (an edited or corrupted journal) could redirect renames or removals.
pub(crate) fn check_layout(journal: &Journal) -> Result<()> {
    let refuse = |why: &str| {
        fail(
            codes::RECOVERY_FAILED,
            format!("the update journal {why}; nothing was changed"),
        )
    };
    let root = &journal.install_root;
    if !root.is_absolute()
        || root
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(refuse("names an installation that is not an absolute path"));
    }
    if staging_dir(root).ok().as_deref().map(plain_verbatim)
        != Some(plain_verbatim(&journal.staging))
    {
        return Err(refuse(
            "names a staging folder that is not beside the installation",
        ));
    }
    let mut parts = Path::new(&journal.payload_root).components();
    if !matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return Err(refuse(
            "names a payload root that is not a single folder name",
        ));
    }
    Ok(())
}

/// The staged payload: the staging folder, its `incoming` folder and the
/// payload must be real folders (never links), and on Unix on the
/// installation's volume, so the final renames stay within it. (Windows
/// has no stable volume identifier in std; there the sibling layout and
/// the refusal of links hold, and `rename_new` refuses crossing volumes.)
/// A missing one is [`Error::NotFound`].
fn staged_payload(journal: &Journal) -> Result<PathBuf> {
    let incoming = journal.incoming();
    #[cfg(unix)]
    let volume = {
        use std::os::unix::fs::MetadataExt;
        let parent = journal
            .install_root
            .parent()
            .expect("check_layout accepted the root");
        std::fs::metadata(parent)
            .map_err(|e| Error::io_at(parent, e))?
            .dev()
    };
    let incoming_dir = journal.staging.join("incoming");
    for dir in [
        journal.staging.as_path(),
        incoming_dir.as_path(),
        incoming.as_path(),
    ] {
        let meta = match std::fs::symlink_metadata(dir) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(format!(
                    "the staged update is missing {}; discard it",
                    dir.display()
                )));
            }
            Err(e) => return Err(Error::io_at(dir, e)),
        };
        if !meta.is_dir() {
            return Err(fail(
                codes::RECOVERY_FAILED,
                format!("{} is not a folder; the stage was not used", dir.display()),
            ));
        }
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::dev(&meta) != volume {
            return Err(fail(
                codes::RECOVERY_FAILED,
                format!(
                    "{} is on another volume than the installation; the stage was not used",
                    dir.display()
                ),
            ));
        }
    }
    Ok(incoming)
}

fn sibling(root: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = root.parent();
    let name = root.file_name();
    match (parent, name) {
        (Some(parent), Some(name)) => {
            let mut hidden = std::ffi::OsString::from(".");
            hidden.push(name);
            hidden.push(suffix);
            Ok(parent.join(hidden))
        }
        _ => Err(Error::InvalidInput(format!(
            "{} cannot be replaced as a unit",
            root.display()
        ))),
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn create_parent(path: &Path) -> Result<()> {
    let parent = path.parent().expect("slot paths have a parent");
    std::fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))
}

struct HashingSink<'f, 'p> {
    file: &'f mut File,
    hash: Sha256,
    written: u64,
    progress: &'p mut dyn FnMut(u64),
}

impl Write for HashingSink<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hash.update(&buf[..n]);
        self.written += n as u64;
        (self.progress)(self.written);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(version: &str, build: u64) -> PayloadIdentity {
        PayloadIdentity {
            product: "BroScribe".into(),
            app_id: "org.broscribe.BroScribe".into(),
            version: version.into(),
            build,
            target: "aarch64-apple-darwin".into(),
        }
    }

    #[test]
    fn forward_needs_a_greater_build_and_no_lower_version() {
        assert!(check_forward(&id("1.0.0", 10), &id("1.0.1", 11)).is_ok());
        assert!(check_forward(&id("1.0.0", 10), &id("1.0.0", 11)).is_ok());
        for (to, why) in [
            (id("1.0.0", 10), "same build"),
            (id("1.1.0", 9), "lower build"),
            (id("0.9.0", 11), "lower version"),
            (id("1.1.0-beta", 11), "pre-release"),
        ] {
            assert_eq!(
                check_forward(&id("1.0.0", 10), &to).unwrap_err().code(),
                codes::DOWNGRADE,
                "{why}"
            );
        }
        let mut other = id("1.1.0", 11);
        other.target = "x86_64-apple-darwin".into();
        assert_eq!(
            check_forward(&id("1.0.0", 10), &other).unwrap_err().code(),
            codes::WRONG_TARGET
        );
    }

    #[test]
    fn staging_lives_beside_the_installation() {
        let root = Path::new("/Applications/BroScribe.app");
        assert_eq!(
            staging_dir(root).unwrap(),
            Path::new("/Applications/.BroScribe.app.update")
        );
        assert!(staging_dir(Path::new("/")).is_err());
    }
}
