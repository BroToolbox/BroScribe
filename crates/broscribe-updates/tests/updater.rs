//! Public updater scenarios over the deterministic platform, transport and
//! admission doubles, with real files, real archives and real Ed25519
//! signatures from fixed test seeds (no real release key).

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use broscribe_core::config::{RELEASE_FEED, UpdateSettings};
use broscribe_platform::update::{
    FakeFault, FakeUpdatePlatform, PayloadIdentity, SignerIdentity, SigningFacility, UpdatePlatform,
};
use broscribe_updates::{
    Cancel, CheckOutcome, ENVELOPE_FORMAT, FakeInstallAdmission, FakeTransport, InstallAdmission,
    InstallLease, KeyId, MANIFEST_FORMAT, RecoveryOutcome, SIGNING_CONTEXT, StagedUpdate,
    UpdateExpectation, UpdateOffer, UpdateStatus, Updater, codes,
};
use chrono::{DateTime, Duration, Utc};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

const TARGET: &str = "aarch64-apple-darwin";
const APP_ID: &str = "org.broscribe.BroScribe";
const FEED: &str = "https://updates.example/stable.json";
const ARTIFACT: &str = "https://updates.example/BroScribe-1.1.0-11.zip";
const SIGNER: &str = "TEAMID1234";

fn identity(version: &str, build: u64) -> PayloadIdentity {
    PayloadIdentity {
        product: "BroScribe".into(),
        app_id: APP_ID.into(),
        version: version.into(),
        build,
        target: TARGET.into(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn key(seed: u8) -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap()
}

enum Entry {
    Dir(String),
    File(String, Vec<u8>, bool),
    Link(String, String),
}

fn dir(name: &str) -> Entry {
    Entry::Dir(name.into())
}

fn file(name: &str, body: impl Into<Vec<u8>>) -> Entry {
    Entry::File(name.into(), body.into(), false)
}

fn build_info(id: &PayloadIdentity) -> Vec<u8> {
    json!({
        "schema": 1, "product": id.product, "app_id": id.app_id,
        "version": id.version, "build": id.build, "target": id.target,
    })
    .to_string()
    .into_bytes()
}

/// A well-formed payload for `id` signed (in the fake's sense) by `signer`.
fn payload(id: &PayloadIdentity, signer: &str) -> Vec<Entry> {
    vec![
        dir("BroScribe.app/"),
        file("BroScribe.app/build-info.json", build_info(id)),
        file("BroScribe.app/signature", signer),
        dir("BroScribe.app/bin/"),
        Entry::File(
            "BroScribe.app/bin/broscribe".into(),
            b"#!tool".to_vec(),
            true,
        ),
    ]
}

/// Zip bytes plus the entry count and installed size a manifest declares.
fn zip_bytes(entries: &[Entry]) -> (Vec<u8>, u64, u64) {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut installed = 0u64;
    for entry in entries {
        match entry {
            Entry::Dir(name) => writer
                .add_directory(name.as_str(), SimpleFileOptions::default())
                .unwrap(),
            Entry::File(name, body, executable) => {
                let options = SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(if *executable { 0o755 } else { 0o644 });
                writer.start_file(name.as_str(), options).unwrap();
                writer.write_all(body).unwrap();
                installed += body.len() as u64;
            }
            Entry::Link(name, target) => {
                writer
                    .add_symlink(name.as_str(), target.as_str(), SimpleFileOptions::default())
                    .unwrap();
                installed += target.len() as u64;
            }
        }
    }
    let bytes = writer.finish().unwrap().into_inner();
    (bytes, entries.len() as u64, installed)
}

fn release_fixture(
    layout: broscribe_platform::release_layout::ReleaseLayout,
    id: &PayloadIdentity,
) -> Vec<Entry> {
    let mut entries = vec![dir("BroScribe.app/")];
    for name in layout.required_files() {
        let body = match name.as_str() {
            "build-info.json" => build_info(id),
            "NOTICE" => SIGNER.as_bytes().to_vec(),
            _ => b"Synthetic packaged resource\n".to_vec(),
        };
        entries.push(file(&format!("BroScribe.app/{name}"), body));
    }
    entries
}

#[test]
fn release_tree_protection_fences_install_rollback_recovery_and_confirm() {
    use broscribe_platform::release_layout::ReleaseLayout;

    for layout in [ReleaseLayout::Linux, ReleaseLayout::Windows] {
        let mut world = World::new();
        world.platform.release_layout = Some(layout);
        if matches!(layout, ReleaseLayout::Windows) {
            world.platform.replacement = broscribe_platform::update::Replacement::AfterExit;
            world.platform.atomic_exchange = false;
        }
        std::fs::remove_file(world.root.join("signature")).unwrap();
        for entry in release_fixture(layout, &identity("1.0.0", 10)) {
            if let Entry::File(path, body, _) = entry {
                let path = world
                    .root
                    .join(path.strip_prefix("BroScribe.app/").unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, body).unwrap();
            }
        }
        world.publish(
            "1.1.0",
            11,
            &release_fixture(layout, &identity("1.1.0", 11)),
        );
        let mut updater = world.updater();
        let expected = expect(&stage(&world, &mut updater));
        let lease = world.admission.acquire().unwrap();
        let library = world.root.join("meeting-library");
        let add_library = |path: &std::path::Path| {
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("keep.md"), "synthetic meeting").unwrap();
        };
        let protected = |error: broscribe_core::Error, path: &std::path::Path| {
            assert_eq!(error.code(), codes::RELEASE_TREE_UNSAFE, "{error}");
            assert!(error.to_string().contains("meeting-library"), "{error}");
            assert_eq!(
                std::fs::read_to_string(path.join("keep.md")).unwrap(),
                "synthetic meeting"
            );
        };

        add_library(&library);
        protected(
            updater.prepare_install(Some(&expected)).unwrap_err(),
            &library,
        );
        protected(
            updater
                .install(&expected, &*lease, &Cancel::new())
                .unwrap_err(),
            &library,
        );
        assert!(world.platform.log().is_empty());
        std::fs::remove_dir_all(&library).unwrap();

        let installed = updater.install(&expected, &*lease, &Cancel::new()).unwrap();
        assert_eq!(world.installed(), identity("1.1.0", 11));
        let previous_library = installed.previous.join("meeting-library");
        add_library(&previous_library);
        protected(updater.confirm().unwrap_err(), &previous_library);
        std::fs::remove_dir_all(&previous_library).unwrap();

        add_library(&library);
        protected(updater.rollback(&expected, &*lease).unwrap_err(), &library);
        edit_journal(&world, |journal| journal["phase"] = json!("swapped"));
        protected(updater.recover(&*lease).unwrap_err(), &library);
        assert_eq!(world.installed(), identity("1.1.0", 11));
        std::fs::remove_dir_all(&library).unwrap();
        assert!(matches!(
            updater.recover(&*lease).unwrap(),
            RecoveryOutcome::RolledBack { .. }
        ));
        assert_eq!(world.installed(), identity("1.0.0", 10));

        let expected = expect(&stage(&world, &mut updater));
        updater.install(&expected, &*lease, &Cancel::new()).unwrap();
        updater.confirm().unwrap();
        assert!(!world.staging().exists());
    }
}

#[test]
fn future_layout_installs_and_confirmed_inventory_protects_the_next_rollback_copy() {
    use broscribe_platform::release_layout::ReleaseLayout;

    let mut world = World::new();
    let layout = ReleaseLayout::Linux;
    world.platform.release_layout = Some(layout);
    std::fs::remove_file(world.root.join("signature")).unwrap();
    for entry in release_fixture(layout, &identity("1.0.0", 10)) {
        if let Entry::File(path, body, _) = entry {
            let path = world
                .root
                .join(path.strip_prefix("BroScribe.app/").unwrap());
            if path.ends_with("uninstall.sh") {
                continue; // Simulate the older layout, before uninstall.sh shipped.
            }
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
        }
    }
    let mut updater = world.updater();
    let lease = world.admission.acquire().unwrap();
    let publish = |version, build, filename| {
        let mut entries = release_fixture(layout, &identity(version, build));
        entries.push(file(filename, "original release bytes"));
        world.publish(version, build, &entries);
    };
    publish("1.1.0", 11, "BroScribe.app/future-file");
    let expected = expect(&stage(&world, &mut updater));
    let first = updater.install(&expected, &*lease, &Cancel::new()).unwrap();
    updater.confirm().unwrap();
    assert!(!first.previous.exists());
    assert!(world.root.join("future-file").is_file());
    let record = world.state.join("installed-inventory.json");
    let confirmed = std::fs::read(&record).unwrap();

    // A changed build-info file cannot make the same directory fall back to
    // the archive layout during direct uninstall.
    let info = world.root.join("build-info.json");
    std::fs::write(&info, build_info(&identity("1.1.0", 111))).unwrap();
    let error = broscribe_updates::removal_inventory(
        &world.root,
        std::slice::from_ref(&world.state),
        &world.platform,
    )
    .err()
    .expect("the directory-only match must refuse uninstall");
    assert_eq!(error.code(), codes::INSTALLATION_CHANGED_OUTSIDE);
    std::fs::write(&info, build_info(&identity("1.1.0", 11))).unwrap();

    publish("1.2.0", 12, "BroScribe.app/another-future-file");
    let expected = expect(&stage(&world, &mut updater));
    updater.install(&expected, &*lease, &Cancel::new()).unwrap();
    updater.rollback(&expected, &*lease).unwrap();
    assert_eq!(std::fs::read(&record).unwrap(), confirmed);
    assert!(world.root.join("future-file").is_file());

    let expected = expect(&stage(&world, &mut updater));
    let next = updater.install(&expected, &*lease, &Cancel::new()).unwrap();
    let previous_file = next.previous.join("future-file");
    std::fs::write(&previous_file, "modified release bytes").unwrap();
    let error = updater.confirm().unwrap_err();
    assert_eq!(error.code(), codes::RELEASE_TREE_UNSAFE);
    assert!(error.to_string().contains("future-file"), "{error}");
    assert_eq!(
        std::fs::read(&previous_file).unwrap(),
        b"modified release bytes"
    );
    std::fs::write(&previous_file, "original release bytes").unwrap();

    let extra = next.previous.join("other-file");
    std::fs::write(&extra, "keep").unwrap();
    assert_eq!(
        updater.confirm().unwrap_err().code(),
        codes::RELEASE_TREE_UNSAFE
    );
    assert!(extra.is_file());
    std::fs::remove_file(extra).unwrap();
    updater.confirm().unwrap();
    assert!(!next.previous.exists());
    assert_ne!(std::fs::read(&record).unwrap(), confirmed);
}

#[test]
fn interrupted_sealed_bundle_deletion_is_resumed_without_a_seal_check() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut updater = world.updater();
    let expected = expect(&stage(&world, &mut updater));
    let lease = world.admission.acquire().unwrap();
    let installed = updater.install(&expected, &*lease, &Cancel::new()).unwrap();
    let previous = installed.previous.parent().unwrap();
    let tombstone = world.staging().join("deleting-previous");
    std::fs::rename(previous, &tombstone).unwrap();
    let record = world.staging().join("deleting-previous.json");
    let identity = world
        .platform
        .directory_identity(&tombstone)
        .unwrap()
        .unwrap();
    let bundle = tombstone.join("BroScribe.app");

    // The strict check protects a tombstone whose rename was never journaled.
    // Missing build information prevents inspection, as deleting sealed
    // resources would prevent native validation of a partially deleted app.
    std::fs::remove_file(bundle.join("build-info.json")).unwrap();
    assert!(updater.confirm().is_err());
    assert!(bundle.join("signature").exists());
    std::fs::write(
        &record,
        serde_json::to_vec(&serde_json::json!({
            "slot": identity,
            "release": world.platform.directory_identity(&bundle).unwrap()
        }))
        .unwrap(),
    )
    .unwrap();
    drop(updater);
    world.updater().confirm().unwrap();
    assert!(!world.staging().exists());
}

fn manifest(now: DateTime<Utc>, version: &str, build: u64, artifact: Value) -> Value {
    json!({
        "format": MANIFEST_FORMAT,
        "schema": 1,
        "product": "BroScribe",
        "channel": "stable",
        "version": version,
        "build": build,
        "published_at": (now - Duration::hours(1)).to_rfc3339(),
        "expires_at": (now + Duration::days(30)).to_rfc3339(),
        "artifacts": [artifact],
    })
}

fn artifact_for(bytes: &[u8], entries: u64, installed: u64) -> Value {
    json!({
        "target": TARGET,
        "app_id": APP_ID,
        "url": ARTIFACT,
        "size": bytes.len(),
        "sha256": hex(&Sha256::digest(bytes)),
        "archive": "zip",
        "root": "BroScribe.app",
        "entries": entries,
        "installed_size": installed,
    })
}

fn envelope(signer: &Ed25519KeyPair, manifest: &Value) -> Vec<u8> {
    let text = manifest.to_string();
    let mut message = SIGNING_CONTEXT.to_vec();
    message.extend_from_slice(text.as_bytes());
    let signature = signer.sign(&message);
    json!({
        "format": ENVELOPE_FORMAT,
        "schema": 1,
        "manifest": text,
        "signatures": [{
            "key_id": KeyId::of(signer.public_key().as_ref()).to_string(),
            "ed25519": hex(signature.as_ref()),
        }],
    })
    .to_string()
    .into_bytes()
}

struct World {
    temp: tempfile::TempDir,
    apps: PathBuf,
    root: PathBuf,
    state: PathBuf,
    platform: FakeUpdatePlatform,
    transport: FakeTransport,
    admission: FakeInstallAdmission,
    settings: UpdateSettings,
    key: Ed25519KeyPair,
    now: DateTime<Utc>,
}

impl World {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let apps = temp.path().join("Applications");
        let root = apps.join("BroScribe.app");
        FakeUpdatePlatform::write_payload(&root, &identity("1.0.0", 10), SIGNER).unwrap();
        let key = key(7);
        let settings = UpdateSettings {
            enabled: true,
            feed_url: Some(FEED.into()),
            channel: "stable".into(),
            trusted_keys: vec![hex(key.public_key().as_ref())],
            signer: Some(SIGNER.into()),
        };
        Self {
            state: temp.path().join("state"),
            platform: FakeUpdatePlatform::new(root.clone(), TARGET),
            transport: FakeTransport::new(),
            admission: FakeInstallAdmission::default(),
            settings,
            key,
            now: Utc::now(),
            apps,
            root,
            temp,
        }
    }

    /// Publishes `entries` as release `version`/`build` with honest metadata.
    fn publish(&self, version: &str, build: u64, entries: &[Entry]) {
        let (bytes, count, installed) = zip_bytes(entries);
        let artifact = artifact_for(&bytes, count, installed);
        self.publish_with(version, build, artifact, bytes);
    }

    fn publish_with(&self, version: &str, build: u64, artifact: Value, bytes: Vec<u8>) {
        let manifest = manifest(self.now, version, build, artifact);
        self.transport.serve(FEED, envelope(&self.key, &manifest));
        self.transport.serve(ARTIFACT, bytes);
    }

    fn updater(&self) -> Updater<'_> {
        Updater::open(&self.state, &self.settings, &self.platform, &self.transport).unwrap()
    }

    fn installed(&self) -> PayloadIdentity {
        self.platform.inspect(&self.root).unwrap()
    }

    fn staging(&self) -> PathBuf {
        self.apps.join(".BroScribe.app.update")
    }
}

fn offer(updater: &mut Updater<'_>, now: DateTime<Utc>) -> UpdateOffer {
    match updater.check(now, &Cancel::new()).unwrap() {
        CheckOutcome::Available(offer) => *offer,
        other => panic!("expected an offer, got {other:?}"),
    }
}

fn stage(world: &World, updater: &mut Updater<'_>) -> StagedUpdate {
    let offer = offer(updater, world.now);
    updater
        .download(&offer, &Cancel::new(), &mut |_| {})
        .unwrap()
}

/// What a user confirmed after seeing `staged`.
fn expect(staged: &StagedUpdate) -> UpdateExpectation {
    staged.into()
}

/// The fake's verified signer for `signer`.
fn signed_by(signer: &str) -> SignerIdentity {
    SignerIdentity {
        facility: SigningFacility::Simulated,
        signer: Some(signer.into()),
        requirement: None,
    }
}

fn download_error(world: &World) -> String {
    let mut updater = world.updater();
    let offer = offer(&mut updater, world.now);
    let error = updater
        .download(&offer, &Cancel::new(), &mut |_| {})
        .unwrap_err();
    if world.staging().exists() {
        // The archive may have written files outside a known layout before
        // validation failed. Our fixture moves the preserved tree aside.
        assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
        let kept = tempfile::tempdir_in(&world.apps).unwrap();
        std::fs::rename(world.staging(), kept.path().join("preserved")).unwrap();
    }
    assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
    assert_eq!(world.installed(), identity("1.0.0", 10));
    error.code().to_owned()
}

#[test]
fn signed_update_installs_under_admission_and_confirms() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut updater = world.updater();
    let staged = stage(&world, &mut updater);
    assert_eq!(staged.to, identity("1.1.0", 11));
    assert_eq!(
        updater.status().unwrap(),
        UpdateStatus::Staged {
            from: identity("1.0.0", 10),
            to: identity("1.1.0", 11),
            signer: signed_by(SIGNER),
        }
    );
    assert_eq!(
        world.installed(),
        identity("1.0.0", 10),
        "staging changes nothing"
    );

    let lease = world.admission.acquire().unwrap();
    let outcome = updater
        .install(&expect(&staged), &*lease, &Cancel::new())
        .unwrap();
    assert_eq!(world.installed(), identity("1.1.0", 11));
    assert_eq!(
        world.platform.inspect(&outcome.previous).unwrap(),
        identity("1.0.0", 10)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(world.root.join("bin/broscribe"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    drop(lease);
    assert_eq!(updater.confirm().unwrap(), identity("1.1.0", 11));
    assert!(!world.staging().exists());
    assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
}

#[test]
fn envelope_signatures_must_come_from_a_pinned_key() {
    let world = World::new();
    let (bytes, count, installed) = zip_bytes(&payload(&identity("1.1.0", 11), SIGNER));
    let manifest = manifest(
        world.now,
        "1.1.0",
        11,
        artifact_for(&bytes, count, installed),
    );

    let mut manifest = manifest;
    manifest["release_notes"] = json!("A synthetic improvement.");
    let signed = envelope(&world.key, &manifest);
    world.transport.serve(FEED, signed.clone());
    let CheckOutcome::Available(offer) = world.updater().check(world.now, &Cancel::new()).unwrap()
    else {
        panic!("the signed release is available");
    };
    assert_eq!(
        offer.manifest.release_notes.as_deref(),
        Some("A synthetic improvement.")
    );
    let mut edited_notes: Value = serde_json::from_slice(&signed).unwrap();
    edited_notes["manifest"] = json!(
        edited_notes["manifest"]
            .as_str()
            .unwrap()
            .replace("A synthetic improvement.", "Different notes.")
    );

    // A flipped signature bit.
    let mut envelope_value: Value =
        serde_json::from_slice(&envelope(&world.key, &manifest)).unwrap();
    let signature = envelope_value["signatures"][0]["ed25519"]
        .as_str()
        .unwrap()
        .to_owned();
    let flipped = format!(
        "{}{}",
        if &signature[..1] == "0" { "1" } else { "0" },
        &signature[1..]
    );
    envelope_value["signatures"][0]["ed25519"] = json!(flipped);
    // The manifest edited after signing.
    let mut edited: Value = serde_json::from_slice(&envelope(&world.key, &manifest)).unwrap();
    edited["manifest"] = json!(
        edited["manifest"]
            .as_str()
            .unwrap()
            .replace("1.1.0", "9.9.9")
    );
    for (body, case) in [
        (envelope_value.to_string().into_bytes(), "corrupt signature"),
        (edited.to_string().into_bytes(), "edited manifest"),
        (
            edited_notes.to_string().into_bytes(),
            "edited release notes",
        ),
        (envelope(&key(9), &manifest), "unpinned key"),
    ] {
        world.transport.serve(FEED, body);
        let error = world
            .updater()
            .check(world.now, &Cancel::new())
            .unwrap_err();
        assert_eq!(error.code(), codes::SIGNATURE_INVALID, "{case}");
    }
    assert!(
        world.transport.requests().iter().all(|url| url == FEED),
        "no artifact fetched"
    );
}

#[test]
fn expired_manifests_are_quiet_and_wrong_channels_are_refused() {
    let world = World::new();
    let (bytes, count, installed) = zip_bytes(&payload(&identity("1.1.0", 11), SIGNER));
    world.publish_with("1.1.0", 11, artifact_for(&bytes, count, installed), bytes);
    assert!(matches!(
        world.updater().check(world.now, &Cancel::new()).unwrap(),
        CheckOutcome::Available(_)
    ));
    let later = world.now + Duration::days(31);
    assert!(matches!(
        world.updater().check(later, &Cancel::new()).unwrap(),
        CheckOutcome::NoVerifiedUpdate { .. }
    ));
    let mut settings = world.settings.clone();
    settings.channel = "beta".into();
    let mut updater =
        Updater::open(&world.state, &settings, &world.platform, &world.transport).unwrap();
    assert_eq!(
        updater.check(world.now, &Cancel::new()).unwrap_err().code(),
        codes::MANIFEST_INVALID
    );
    drop(updater);
    world.publish("1.0.0", 10, &payload(&identity("1.0.0", 10), SIGNER));
    let error = world
        .updater()
        .check(world.now, &Cancel::new())
        .unwrap_err();
    assert_eq!(
        error.code(),
        codes::FEED_ROLLBACK,
        "expiry preserves the seen build"
    );
}

#[test]
fn an_unsigned_latest_github_release_has_no_verified_update() {
    let world = World::new();
    let settings = UpdateSettings::default();
    let mut updater =
        Updater::open(&world.state, &settings, &world.platform, &world.transport).unwrap();
    assert_eq!(
        updater.check(world.now, &Cancel::new()).unwrap(),
        CheckOutcome::NoVerifiedUpdate {
            installed: world.installed()
        }
    );
    assert_eq!(world.transport.requests(), [RELEASE_FEED]);
}

#[test]
fn an_authenticated_release_without_this_target_still_fences_older_releases() {
    let world = World::new();
    let mut settings = world.settings.clone();
    settings.feed_url = None;
    let (bytes, count, installed) = zip_bytes(&payload(&identity("1.2.0", 12), SIGNER));
    let mut other = artifact_for(&bytes, count, installed);
    other["target"] = json!("x86_64-apple-darwin");
    world.transport.serve(
        RELEASE_FEED,
        envelope(&world.key, &manifest(world.now, "1.2.0", 12, other)),
    );
    let mut updater =
        Updater::open(&world.state, &settings, &world.platform, &world.transport).unwrap();
    assert!(matches!(
        updater.check(world.now, &Cancel::new()).unwrap(),
        CheckOutcome::NoVerifiedUpdate { .. }
    ));
    drop(updater);
    let seen: Value =
        serde_json::from_slice(&std::fs::read(world.state.join("seen.json")).unwrap()).unwrap();
    assert_eq!(seen["highest_build"], 12);
    assert_eq!(seen["highest_version"], "1.2.0");

    world.transport.serve(
        RELEASE_FEED,
        envelope(
            &world.key,
            &manifest(
                world.now,
                "1.1.0",
                11,
                artifact_for(&bytes, count, installed),
            ),
        ),
    );
    let mut updater =
        Updater::open(&world.state, &settings, &world.platform, &world.transport).unwrap();
    assert_eq!(
        updater.check(world.now, &Cancel::new()).unwrap_err().code(),
        codes::FEED_ROLLBACK
    );
}

#[test]
fn install_discards_a_stage_superseded_by_a_release_without_this_target() {
    for preflight in [true, false] {
        let world = World::new();
        let staged = staged_elsewhere(&world);
        let expected = UpdateExpectation::from(&staged);
        let mut settings = world.settings.clone();
        settings.feed_url = None;
        let (bytes, count, installed) = zip_bytes(&payload(&identity("1.2.0", 12), SIGNER));
        let mut other = artifact_for(&bytes, count, installed);
        other["target"] = json!("x86_64-apple-darwin");
        world.transport.serve(
            RELEASE_FEED,
            envelope(&world.key, &manifest(world.now, "1.2.0", 12, other)),
        );
        let mut updater =
            Updater::open(&world.state, &settings, &world.platform, &world.transport).unwrap();
        assert!(matches!(
            updater.check(world.now, &Cancel::new()).unwrap(),
            CheckOutcome::NoVerifiedUpdate { .. }
        ));
        let error = if preflight {
            updater.prepare_install(Some(&expected)).unwrap_err()
        } else {
            let lease = world.admission.acquire().unwrap();
            updater
                .install(&expected, &*lease, &Cancel::new())
                .unwrap_err()
        };
        assert_eq!(error.code(), codes::FEED_ROLLBACK);
        assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
        assert!(!world.staging().exists());
        assert_eq!(world.installed(), identity("1.0.0", 10));
    }
}

#[test]
fn prepare_install_refuses_changed_staging_and_discard_moves_it_aside() {
    let world = World::new();
    let staged = staged_elsewhere(&world);
    std::fs::write(staged.payload.join("not-native-signed"), "changed").unwrap();
    let mut updater = world.updater();
    assert!(updater.staged().unwrap().is_some());
    assert_eq!(
        updater
            .prepare_install(Some(&UpdateExpectation::from(&staged)))
            .unwrap_err()
            .code(),
        codes::VERIFICATION_FAILED
    );
    assert!(matches!(
        updater.status().unwrap(),
        UpdateStatus::Staged { .. }
    ));
    let kept = updater.discard().unwrap().expect("changed staging is kept");
    assert_eq!(
        std::fs::read(kept.join("incoming/BroScribe.app/not-native-signed")).unwrap(),
        b"changed"
    );
    assert!(!world.staging().exists());
    assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
    let offer = offer(&mut updater, world.now);
    updater
        .download(&offer, &Cancel::new(), &mut |_| {})
        .unwrap();
}

#[test]
fn discard_quarantines_a_stage_with_missing_build_info_and_allows_another_download() {
    let world = World::new();
    let staged = staged_elsewhere(&world);
    std::fs::remove_file(staged.payload.join("build-info.json")).unwrap();
    let mut updater = world.updater();
    let kept = updater.discard().unwrap().expect("unreadable payload kept");
    assert!(kept.join("incoming/BroScribe.app/signature").is_file());
    assert!(!world.staging().exists());
    assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
    assert!(stage(&world, &mut updater).payload.is_dir());
}

#[test]
fn recovery_cleans_an_unjournaled_authenticated_future_layout_and_refuses_foreign_entries() {
    let world = World::new();
    let target = identity("1.1.0", 11);
    let mut entries = payload(&target, SIGNER);
    entries.push(file("BroScribe.app/future/data.blob", "future file"));
    world.publish("1.1.0", 11, &entries);
    let mut updater = world.updater();
    let staged = stage(&world, &mut updater);
    // Simulate an interruption after some files were written but before the
    // update became staged. The entry list must already be on disk then.
    std::fs::remove_file(world.state.join("journal.json")).unwrap();
    std::fs::remove_file(staged.payload.join("signature")).unwrap();
    let found = world.staging().join("archive-entries.json");
    assert!(found.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(found).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let new_file = staged.payload.join("future/data.blob");
    let foreign = staged.payload.join("future/foreign.txt");
    std::fs::write(&foreign, "leave this alone").unwrap();
    let lease = world.admission.acquire().unwrap();
    let err = updater.recover(&*lease).unwrap_err();
    assert_eq!(err.code(), codes::RELEASE_TREE_UNSAFE);
    assert!(err.to_string().contains("foreign.txt"));
    assert!(new_file.is_file() && foreign.is_file());
    std::fs::remove_file(foreign).unwrap();
    assert_eq!(updater.recover(&*lease).unwrap(), RecoveryOutcome::Clean);
    assert!(!world.staging().exists());
}

#[test]
fn disabled_settings_never_open_an_updater() {
    let world = World::new();
    let settings = UpdateSettings {
        enabled: false,
        ..UpdateSettings::default()
    };
    let error = Updater::open(&world.state, &settings, &world.platform, &world.transport)
        .err()
        .unwrap();
    assert_eq!(error.code(), "not_configured");
    assert!(world.transport.requests().is_empty());
}

#[test]
fn wrong_target_app_or_signer_is_refused() {
    let world = World::new();
    // The feed has no artifact for this target.
    let (bytes, count, installed) = zip_bytes(&payload(&identity("1.1.0", 11), SIGNER));
    let mut artifact = artifact_for(&bytes, count, installed);
    artifact["target"] = json!("x86_64-apple-darwin");
    world.publish_with("1.1.0", 11, artifact, bytes.clone());
    assert_eq!(
        world
            .updater()
            .check(world.now, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::WRONG_TARGET
    );
    // The artifact is another application.
    let mut artifact = artifact_for(&bytes, count, installed);
    artifact["app_id"] = json!("org.example.Other");
    world.publish_with("1.1.0", 11, artifact, bytes);
    assert_eq!(
        world
            .updater()
            .check(world.now, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::WRONG_APP
    );

    // Honest manifest, but the payload inside is built for another architecture.
    let mut foreign = identity("1.1.0", 11);
    foreign.target = "x86_64-apple-darwin".into();
    world.publish("1.1.0", 11, &payload(&foreign, SIGNER));
    assert_eq!(download_error(&world), codes::WRONG_TARGET);
    // Signed by someone else.
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), "MALLORY000"));
    assert_eq!(download_error(&world), codes::SIGNER_MISMATCH);
    // Contents no longer match their signature.
    let mut tampered = payload(&identity("1.1.0", 11), SIGNER);
    tampered.push(file("BroScribe.app/tampered", "x"));
    world.publish("1.1.0", 11, &tampered);
    assert_eq!(download_error(&world), codes::SIGNATURE_INVALID);
    // The payload is a different release than the manifest names.
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 12), SIGNER));
    assert_eq!(download_error(&world), codes::MANIFEST_INVALID);
}

#[test]
fn downgrades_and_feed_rollbacks_are_refused() {
    let world = World::new();
    world.publish("0.9.0", 9, &payload(&identity("0.9.0", 9), SIGNER));
    assert!(matches!(
        world.updater().check(world.now, &Cancel::new()).unwrap(),
        CheckOutcome::UpToDate {
            latest_build: 9,
            ..
        }
    ));
    world.publish("0.9.0", 12, &payload(&identity("0.9.0", 12), SIGNER));
    assert_eq!(
        world
            .updater()
            .check(world.now, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::DOWNGRADE,
        "a newer build may not carry an older version"
    );
    // Build 12 was seen; a replayed build 11 manifest is refused.
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    assert_eq!(
        world
            .updater()
            .check(world.now, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::FEED_ROLLBACK
    );
}

#[cfg(unix)]
#[test]
fn zip_sets_payload_and_parent_directory_modes_under_umask_002() {
    use std::os::unix::fs::PermissionsExt as _;
    const CHILD: &str = "BROSCRIBE_TEST_ZIP_UMASK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new("sh")
            .args(["-c", "umask 002; exec \"$@\"", "sh"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "zip_sets_payload_and_parent_directory_modes_under_umask_002",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}"
        );
        return;
    }
    let world = World::new();
    let mut entries = payload(&identity("1.1.0", 11), SIGNER);
    entries.push(file("BroScribe.app/implicit/nested/item", "test"));
    world.publish("1.1.0", 11, &entries);
    let mut updater = world.updater();
    let offer = offer(&mut updater, world.now);
    let staged = updater
        .download(&offer, &Cancel::new(), &mut |_| {})
        .unwrap();
    for dir in [&world.state, &world.staging()] {
        let mode = std::fs::metadata(dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
    }
    for record in [
        world.state.join("journal.json"),
        world.state.join("seen.json"),
        world.staging().join("owner.json"),
    ] {
        let mode = std::fs::metadata(&record).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", record.display());
    }
    for dir in ["", "bin", "implicit", "implicit/nested"] {
        let mode = std::fs::metadata(staged.payload.join(dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "directory {dir}");
    }
}

#[test]
fn an_equal_build_with_refreshed_timestamps_keeps_the_offer_or_reports_up_to_date() {
    for build in [10, 11] {
        let world = World::new();
        let version = "1.0.0";
        let (bytes, entries, size) = zip_bytes(&payload(&identity(version, build), SIGNER));
        let artifact = artifact_for(&bytes, entries, size);
        world.publish_with(version, build, artifact.clone(), bytes);
        let mut updater = world.updater();
        let check = |outcome| match outcome {
            CheckOutcome::UpToDate { latest_build, .. } if build == 10 => {
                assert_eq!(latest_build, build)
            }
            CheckOutcome::Available(offer) if build == 11 => {
                assert_eq!(offer.manifest.build, build);
                assert_eq!(offer.manifest.version, version);
                assert_eq!(offer.artifact.sha256, artifact["sha256"]);
            }
            other => panic!("unexpected result for build {build}: {other:?}"),
        };
        check(updater.check(world.now, &Cancel::new()).unwrap());
        let later = world.now + Duration::days(31);
        assert!(
            matches!(
                updater.check(later, &Cancel::new()).unwrap(),
                CheckOutcome::NoVerifiedUpdate { .. }
            ),
            "the original envelope expired"
        );
        let renewed = manifest(later, version, build, artifact.clone());
        world.transport.serve(FEED, envelope(&world.key, &renewed));
        check(updater.check(later, &Cancel::new()).unwrap());
    }
}

#[test]
fn download_bytes_must_match_the_signed_length_and_hash() {
    let world = World::new();
    let (bytes, count, installed) = zip_bytes(&payload(&identity("1.1.0", 11), SIGNER));
    let artifact = artifact_for(&bytes, count, installed);
    let mut corrupt = bytes.clone();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0xff;
    world.publish_with("1.1.0", 11, artifact.clone(), corrupt);
    assert_eq!(download_error(&world), codes::HASH_MISMATCH);
    let mut longer = bytes.clone();
    longer.push(0);
    world.publish_with("1.1.0", 11, artifact.clone(), longer);
    assert_eq!(download_error(&world), codes::SIZE_MISMATCH);
    world.publish_with("1.1.0", 11, artifact, bytes[..bytes.len() - 1].to_vec());
    assert_eq!(download_error(&world), codes::SIZE_MISMATCH);
}

#[test]
fn redirects_are_refused() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    world.transport.redirect(ARTIFACT);
    assert_eq!(download_error(&world), codes::REDIRECT_REFUSED);
}

/// Publishes a release whose archive is large and incompressible, so its
/// download takes many reads.
fn publish_noisy(world: &World) {
    let mut entries = payload(&identity("1.1.0", 11), SIGNER);
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let noise: Vec<u8> = (0..200_000)
        .map(|_| {
            // xorshift64*: incompressible for deflate.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56) as u8
        })
        .collect();
    entries.push(file("BroScribe.app/noise.bin", noise));
    world.publish("1.1.0", 11, &entries);
}

fn assert_cancelled_cleanly(world: &World, updater: &Updater<'_>, error: broscribe_core::Error) {
    assert_eq!(error.code(), codes::CANCELLED);
    assert!(
        !world.staging().exists(),
        "partial download and extraction removed"
    );
    assert_eq!(
        updater.status().unwrap(),
        UpdateStatus::Idle,
        "nothing staged"
    );
    assert_eq!(world.installed(), identity("1.0.0", 10));
}

#[test]
fn cancelling_a_download_leaves_nothing_behind() {
    let world = World::new();
    publish_noisy(&world);
    let mut updater = world.updater();
    let offer = offer(&mut updater, world.now);
    assert!(offer.artifact.size > 64 * 1024, "{}", offer.artifact.size);
    let cancel = Cancel::new();
    world.transport.cancel_after(cancel.clone(), 16 * 1024);
    let mut seen = 0;
    let error = updater
        .download(&offer, &cancel, &mut |bytes| seen = bytes)
        .unwrap_err();
    assert!(seen >= 16 * 1024 && seen < offer.artifact.size, "{seen}");
    assert_cancelled_cleanly(&world, &updater, error);
}

#[test]
fn a_cancel_arriving_with_the_end_of_the_body_still_wins() {
    let world = World::new();
    publish_noisy(&world);
    let mut updater = world.updater();
    let offer = offer(&mut updater, world.now);
    let size = offer.artifact.size;
    // Every byte arrives; the cancellation lands on the end-of-body read.
    let cancel = Cancel::new();
    world.transport.cancel_after(cancel.clone(), size);
    let mut seen = 0;
    let error = updater
        .download(&offer, &cancel, &mut |bytes| seen = bytes)
        .unwrap_err();
    assert_eq!(seen, size);
    assert_cancelled_cleanly(&world, &updater, error);

    // The user cancels from the progress callback on the final chunk.
    let cancel = Cancel::new();
    let from_callback = cancel.clone();
    let error = updater
        .download(&offer, &cancel, &mut |bytes| {
            if bytes == size {
                from_callback.cancel();
            }
        })
        .unwrap_err();
    assert_cancelled_cleanly(&world, &updater, error);
}

#[test]
fn failed_download_reports_its_retained_stage_when_cleanup_refuses_a_foreign_file() {
    let world = World::new();
    publish_noisy(&world);
    let mut updater = world.updater();
    let offer = offer(&mut updater, world.now);
    let cancel = Cancel::new();
    let unknown = world.staging().join("foreign.txt");
    let error = updater
        .download(&offer, &cancel, &mut |_| {
            std::fs::write(&unknown, "synthetic user file").unwrap();
            cancel.cancel();
        })
        .unwrap_err();
    assert_eq!(error.code(), codes::CANCELLED);
    assert!(
        error.to_string().contains("Could not clear staging")
            && error.to_string().contains("foreign.txt")
    );
    assert!(unknown.is_file());
}

#[test]
fn archive_escapes_are_refused_without_writing_outside() {
    let world = World::new();
    let id = identity("1.1.0", 11);
    let cases: Vec<(Vec<Entry>, &str)> = vec![
        (
            vec![file("BroScribe.app/../escaped", "x")],
            "parent component",
        ),
        (vec![file("Other.app/x", "x")], "outside the root"),
        (vec![file("/abs", "x")], "absolute"),
        (
            vec![Entry::Link(
                "BroScribe.app/link".into(),
                "../../escaped".into(),
            )],
            "link above the root",
        ),
        (
            vec![Entry::Link("BroScribe.app/link".into(), "/etc".into())],
            "absolute link",
        ),
        (
            vec![
                Entry::Link("BroScribe.app/dir".into(), "bin".into()),
                file("BroScribe.app/dir/x", "x"),
            ],
            "write through a link",
        ),
        (
            vec![file("BroScribe.app/BIN/broscribe", "x")],
            "case-insensitive duplicate",
        ),
        (
            vec![file("BroScribe.app/._build-info.json", "x")],
            "AppleDouble",
        ),
    ];
    for (extra, case) in cases {
        let mut entries = payload(&id, SIGNER);
        entries.extend(extra);
        world.publish("1.1.0", 11, &entries);
        assert_eq!(download_error(&world), codes::ARCHIVE_UNSAFE, "{case}");
        assert!(!world.apps.join("escaped").exists(), "{case}");
        assert!(!world.temp.path().join("escaped").exists(), "{case}");
    }
}

#[cfg(unix)]
#[test]
fn links_inside_the_payload_are_kept() {
    let world = World::new();
    let mut entries = payload(&identity("1.1.0", 11), SIGNER);
    entries.push(Entry::Link(
        "BroScribe.app/tool".into(),
        "bin/broscribe".into(),
    ));
    world.publish("1.1.0", 11, &entries);
    let mut updater = world.updater();
    let staged = stage(&world, &mut updater);
    let link = staged.payload.join("tool");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        std::path::Path::new("bin/broscribe")
    );
}

#[test]
fn size_bombs_and_miscounted_archives_are_refused() {
    let world = World::new();
    let id = identity("1.1.0", 11);
    // Eight megabytes of zeros compress to a few kilobytes.
    let mut entries = payload(&id, SIGNER);
    entries.push(file("BroScribe.app/bomb", vec![0u8; 8 << 20]));
    let (bytes, count, installed) = zip_bytes(&entries);
    assert!(bytes.len() < 1 << 20);
    world.publish_with(
        "1.1.0",
        11,
        artifact_for(&bytes, count, installed - (8 << 20)),
        bytes.clone(),
    );
    assert_eq!(
        download_error(&world),
        codes::ARCHIVE_TOO_LARGE,
        "declared size"
    );
    world.publish_with(
        "1.1.0",
        11,
        artifact_for(&bytes, count - 1, installed),
        bytes.clone(),
    );
    assert_eq!(
        download_error(&world),
        codes::ARCHIVE_TOO_LARGE,
        "entry count"
    );
    // Signed, honest metadata beyond the configured bound.
    world.publish_with("1.1.0", 11, artifact_for(&bytes, count, installed), bytes);
    let mut updater = world.updater().with_limits(broscribe_updates::Limits {
        max_installed_bytes: 1 << 20,
        ..Default::default()
    });
    assert_eq!(
        updater.check(world.now, &Cancel::new()).unwrap_err().code(),
        codes::ARCHIVE_TOO_LARGE
    );
}

#[test]
fn installs_need_the_agents_admission_and_no_capture() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut updater = world.updater();
    let staged = stage(&world, &mut updater);

    world.admission.start_capture().unwrap();
    assert_eq!(
        world.admission.acquire().err().unwrap().code(),
        codes::RECORDING_ACTIVE
    );
    world.admission.stop_capture();

    let lease = world.admission.acquire().unwrap();
    assert!(
        world.admission.start_capture().is_err(),
        "capture cannot start while an install is admitted"
    );
    world.admission.revoke();
    let error = updater
        .install(&expect(&staged), &*lease, &Cancel::new())
        .unwrap_err();
    assert_eq!(error.code(), codes::ADMISSION_LOST);
    assert_eq!(world.installed(), identity("1.0.0", 10));
    assert!(matches!(
        updater.status().unwrap(),
        UpdateStatus::Staged { .. }
    ));
    assert!(world.platform.log().is_empty(), "nothing was renamed");
    drop(lease);

    let lease = world.admission.acquire().unwrap();
    updater
        .install(&expect(&staged), &*lease, &Cancel::new())
        .unwrap();
    assert_eq!(world.installed(), identity("1.1.0", 11));
}

fn crash_during_install(world: &World, crash_before: usize) {
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut updater = world.updater();
    let staged = stage(world, &mut updater);
    let lease = world.admission.acquire().unwrap();
    world
        .platform
        .set_fault(Some(FakeFault::CrashBeforeChange(crash_before)));
    let crashed = catch_unwind(AssertUnwindSafe(move || {
        let _ = updater.install(&expect(&staged), &*lease, &Cancel::new());
    }));
    assert!(crashed.is_err(), "the simulated crash happened");
    world.platform.set_fault(None);
}

#[test]
fn an_interrupted_two_step_replacement_is_rolled_back() {
    let mut world = World::new();
    world.platform.atomic_exchange = false;
    // Change 0 moves the installation aside; the crash comes before change 1
    // moves the update in, so nothing is installed at all.
    crash_during_install(&world, 1);
    assert!(!world.root.exists());

    let mut updater = world.updater();
    assert!(matches!(
        updater.status().unwrap(),
        UpdateStatus::Interrupted { .. }
    ));
    let lease = world.admission.acquire().unwrap();
    assert_eq!(
        updater.recover(&*lease).unwrap(),
        RecoveryOutcome::RolledBack {
            restored: identity("1.0.0", 10)
        }
    );
    assert_eq!(world.installed(), identity("1.0.0", 10));
    assert!(!world.staging().exists());
    assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
}

#[test]
fn an_interrupted_exchange_is_rolled_back() {
    let world = World::new();
    // Change 0 exchanged the trees; the crash comes before the old tree is
    // moved to its rollback slot.
    crash_during_install(&world, 1);
    assert_eq!(world.installed(), identity("1.1.0", 11));

    let mut updater = world.updater();
    let lease = world.admission.acquire().unwrap();
    assert!(matches!(
        updater.recover(&*lease).unwrap(),
        RecoveryOutcome::RolledBack { .. }
    ));
    assert_eq!(world.installed(), identity("1.0.0", 10));
    assert!(!world.staging().exists());
}

#[test]
fn recovery_needs_the_admission_and_a_single_owner() {
    let mut world = World::new();
    world.platform.atomic_exchange = false;
    crash_during_install(&world, 1);

    let mut owner = world.updater();
    let busy = Updater::open(
        &world.state,
        &world.settings,
        &world.platform,
        &world.transport,
    )
    .err()
    .unwrap();
    assert_eq!(busy.code(), codes::BUSY, "one owner per state folder");

    let lease = world.admission.acquire().unwrap();
    world.admission.revoke();
    assert_eq!(
        owner.recover(&*lease).unwrap_err().code(),
        codes::ADMISSION_LOST
    );
    assert!(!world.root.exists(), "nothing changed without admission");
    drop(lease);

    let lease = world.admission.acquire().unwrap();
    assert!(matches!(
        owner.recover(&*lease).unwrap(),
        RecoveryOutcome::RolledBack { .. }
    ));
    drop(owner);
    // The lock died with its owner; a new owner finds nothing to do.
    let mut next = world.updater();
    assert_eq!(next.recover(&*lease).unwrap(), RecoveryOutcome::Clean);
}

#[test]
fn updaters_of_other_state_folders_never_share_an_installation() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));

    // Another user's updater (other state folder, same installation).
    let other_state = world.state.with_file_name("other-state");
    let mut other = Updater::open(
        &other_state,
        &world.settings,
        &world.platform,
        &world.transport,
    )
    .unwrap();
    let offer = offer(&mut other, world.now);
    let mut first = world.updater();
    let staged = stage(&world, &mut first);
    assert_eq!(
        other.check(world.now, &Cancel::new()).unwrap_err().code(),
        codes::BUSY,
    );
    let error = other
        .download(&offer, &Cancel::new(), &mut |_| {})
        .unwrap_err();
    assert_eq!(error.code(), codes::BUSY);
    assert!(staged.payload.exists(), "the first staging is untouched");
}

#[test]
fn an_installed_update_can_be_rolled_back_until_confirmed() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut updater = world.updater();
    let staged = stage(&world, &mut updater);
    let lease = world.admission.acquire().unwrap();
    updater
        .install(&expect(&staged), &*lease, &Cancel::new())
        .unwrap();
    drop(updater);

    let mut updater = world.updater();
    assert!(matches!(
        updater.recover(&*lease).unwrap(),
        RecoveryOutcome::AwaitingConfirmation { .. }
    ));
    assert_eq!(
        updater.rollback(&expect(&staged), &*lease).unwrap(),
        identity("1.0.0", 10)
    );
    assert_eq!(world.installed(), identity("1.0.0", 10));
    assert!(!world.staging().exists());
    assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
}

/// Stages release 1.1.0 build 11 in an updater that is closed again, as
/// `update download` does in its own process.
fn staged_elsewhere(world: &World) -> StagedUpdate {
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut updater = world.updater();
    stage(world, &mut updater)
}

fn edit_journal(world: &World, edit: impl FnOnce(&mut Value)) {
    let path = world.state.join("journal.json");
    let mut journal: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut journal);
    std::fs::write(&path, journal.to_string()).unwrap();
}

#[test]
fn offline_inspector_distinguishes_active_journals_without_taking_the_state_lock() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut owner = world.updater();
    stage(&world, &mut owner);
    let path = world.state.join("journal.json");

    for phase in ["staged", "swapping", "swapped", "rolling_back", "committed"] {
        edit_journal(&world, |journal| journal["phase"] = json!(phase));
        let before = std::fs::read(&path).unwrap();
        let observed = broscribe_updates::pending_replacement(&world.state).unwrap();
        match phase {
            "staged" => assert!(matches!(observed, UpdateStatus::Staged { .. })),
            "committed" => assert!(matches!(
                observed,
                UpdateStatus::AwaitingConfirmation { .. }
            )),
            _ => assert!(matches!(observed, UpdateStatus::Interrupted { .. })),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "inspection repaired a journal"
        );
        assert_eq!(world.installed(), identity("1.0.0", 10));
    }
    // The public inspector worked while the real updater retained its lock.
    drop(owner);
}

#[test]
fn offline_inspector_refuses_damaged_or_redirected_journals_without_repair() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut owner = world.updater();
    stage(&world, &mut owner);
    let path = world.state.join("journal.json");
    let healthy = std::fs::read(&path).unwrap();
    let mut unknown: Value = serde_json::from_slice(&healthy).unwrap();
    unknown["schema"] = json!(999);
    for bytes in [
        b"not a journal".to_vec(),
        serde_json::to_vec(&unknown).unwrap(),
        vec![b' '; 1024 * 1024],
    ] {
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            broscribe_updates::pending_replacement(&world.state)
                .unwrap_err()
                .code(),
            codes::RECOVERY_FAILED
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[cfg(unix)]
    {
        let target = world.temp.path().join("unrelated-control-target.json");
        std::fs::write(&target, &healthy).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            broscribe_updates::pending_replacement(&world.state)
                .unwrap_err()
                .code(),
            codes::RECOVERY_FAILED
        );
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(target).unwrap(), healthy);
    }
    drop(owner);
}

#[test]
fn a_staged_update_is_found_again_and_installed_by_another_process() {
    let world = World::new();
    let staged = staged_elsewhere(&world);

    let mut updater = world.updater();
    assert_eq!(updater.staged().unwrap(), Some(staged.clone()));
    assert_eq!(
        updater.staged().unwrap(),
        Some(staged.clone()),
        "reading changes nothing"
    );
    assert!(world.platform.log().is_empty(), "nothing was renamed");
    assert!(staged.payload.exists());
    assert_eq!(world.installed(), identity("1.0.0", 10));
    drop(updater);

    let mut updater = world.updater();
    let found = updater.staged().unwrap().expect("the staged update");
    let lease = world.admission.acquire().unwrap();
    let outcome = updater
        .install(&expect(&found), &*lease, &Cancel::new())
        .unwrap();
    assert_eq!(outcome.to, identity("1.1.0", 11));
    assert_eq!(world.installed(), identity("1.1.0", 11));
    assert_eq!(
        updater.staged().unwrap_err().code(),
        codes::PENDING,
        "an installed update awaits confirmation, it is not staged"
    );
    drop(lease);
    updater.confirm().unwrap();
    assert_eq!(updater.staged().unwrap(), None);
}

#[test]
fn without_a_journal_nothing_is_staged_and_nothing_is_removed() {
    let world = World::new();
    assert_eq!(world.updater().staged().unwrap(), None);

    let staged = staged_elsewhere(&world);
    std::fs::remove_file(world.state.join("journal.json")).unwrap();
    let mut updater = world.updater();
    assert_eq!(
        updater.staged().unwrap(),
        None,
        "an unjournaled payload is not an update"
    );
    assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
    assert!(staged.payload.exists(), "reading deletes nothing");
    let lease = world.admission.acquire().unwrap();
    assert_eq!(
        updater
            .install(&expect(&staged), &*lease, &Cancel::new())
            .unwrap_err()
            .code(),
        "not_found"
    );
    assert_eq!(world.installed(), identity("1.0.0", 10));
}

#[test]
fn install_refuses_same_length_changes_that_native_verification_would_accept() {
    let world = World::new();
    let mut files = payload(&identity("1.1.0", 11), SIGNER);
    files.push(file("BroScribe.app/bin/worker", "original"));
    world.publish("1.1.0", 11, &files);
    let mut updater = world.updater();
    let staged = stage(&world, &mut updater);
    std::fs::write(staged.payload.join("bin/worker"), "modified").unwrap();
    assert_eq!(updater.staged().unwrap(), Some(staged.clone()));
    let lease = world.admission.acquire().unwrap();
    assert_eq!(
        updater
            .install(&expect(&staged), &*lease, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::VERIFICATION_FAILED
    );
    assert!(matches!(
        updater.status().unwrap(),
        UpdateStatus::Staged { .. }
    ));
    assert_eq!(
        std::fs::read(staged.payload.join("bin/worker")).unwrap(),
        b"modified"
    );
    assert_eq!(world.installed(), identity("1.0.0", 10));
}

#[test]
fn install_preserves_only_regular_side_by_side_tools_and_rollback_restores_them() {
    let mut world = World::new();
    world.platform.atomic_exchange = false;
    world.platform.side_by_side = &["ffmpeg.exe", "ffplay.exe", "ffprobe.exe"];
    std::fs::write(world.root.join("ffmpeg.exe"), "local encoder").unwrap();
    std::fs::write(world.root.join("ffplay.exe"), "local player").unwrap();
    std::fs::create_dir(world.root.join("ffprobe.exe")).unwrap();
    std::fs::write(world.root.join("other.exe"), "not preserved").unwrap();
    let staged = staged_elsewhere(&world);
    let mut updater = world.updater();
    let lease = world.admission.acquire().unwrap();
    let expected = expect(&staged);
    let installed = updater.install(&expected, &*lease, &Cancel::new()).unwrap();
    assert_eq!(
        std::fs::read(world.root.join("ffmpeg.exe")).unwrap(),
        b"local encoder"
    );
    assert_eq!(
        std::fs::read(world.root.join("ffplay.exe")).unwrap(),
        b"local player"
    );
    assert!(!world.root.join("ffprobe.exe").exists());
    assert!(!world.root.join("other.exe").exists());
    assert_eq!(
        std::fs::read(installed.previous.join("ffmpeg.exe")).unwrap(),
        b"local encoder"
    );
    let journal: Value =
        serde_json::from_slice(&std::fs::read(world.state.join("journal.json")).unwrap()).unwrap();
    assert_eq!(
        journal["preserved_files"],
        json!(["ffmpeg.exe", "ffplay.exe"])
    );

    std::fs::write(world.root.join("ffmpeg.exe"), "changed after swap").unwrap();
    updater.rollback(&expected, &*lease).unwrap();
    assert_eq!(
        std::fs::read(world.root.join("ffmpeg.exe")).unwrap(),
        b"local encoder"
    );
    assert!(world.root.join("ffprobe.exe").is_dir());
    assert!(world.root.join("other.exe").is_file());
}

#[test]
fn a_tampered_stale_or_missing_payload_is_refused_and_kept() {
    let world = World::new();
    let staged = staged_elsewhere(&world);
    let mut updater = world.updater();

    std::fs::write(staged.payload.join("tampered"), b"").unwrap();
    assert_eq!(
        updater.staged().unwrap_err().code(),
        codes::SIGNATURE_INVALID
    );
    std::fs::remove_file(staged.payload.join("tampered")).unwrap();

    std::fs::write(staged.payload.join("signature"), "OTHERTEAM").unwrap();
    assert_eq!(updater.staged().unwrap_err().code(), codes::SIGNER_MISMATCH);

    // Another build than the journal promises.
    FakeUpdatePlatform::write_payload(&staged.payload, &identity("1.1.0", 12), SIGNER).unwrap();
    assert_eq!(
        updater.staged().unwrap_err().code(),
        codes::MANIFEST_INVALID
    );

    FakeUpdatePlatform::write_payload(&staged.payload, &identity("1.1.0", 11), SIGNER).unwrap();
    assert_eq!(
        updater.staged().unwrap(),
        Some(staged.clone()),
        "the original payload verifies again"
    );

    std::fs::remove_dir_all(&staged.payload).unwrap();
    assert_eq!(updater.staged().unwrap_err().code(), "not_found");
    assert!(matches!(
        updater.status().unwrap(),
        UpdateStatus::Staged { .. }
    ));
    assert!(world.staging().exists(), "a failed read deletes nothing");
    assert!(world.platform.log().is_empty());
    assert_eq!(world.installed(), identity("1.0.0", 10));
}

#[test]
fn a_changed_installation_invalidates_the_stage() {
    let world = World::new();
    let staged = staged_elsewhere(&world);
    let mut updater = world.updater();
    let lease = world.admission.acquire().unwrap();

    // Same build, re-signed by someone else.
    FakeUpdatePlatform::write_payload(&world.root, &identity("1.0.0", 10), "OTHERTEAM").unwrap();
    assert_eq!(
        updater.staged().unwrap_err().code(),
        codes::INSTALLATION_CHANGED
    );
    assert_eq!(
        updater
            .install(&expect(&staged), &*lease, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::INSTALLATION_CHANGED
    );

    // Another version installed since.
    FakeUpdatePlatform::write_payload(&world.root, &identity("1.0.5", 10), SIGNER).unwrap();
    assert_eq!(
        updater.staged().unwrap_err().code(),
        codes::INSTALLATION_CHANGED
    );
    assert!(staged.payload.exists());
    assert!(world.platform.log().is_empty());
}

#[test]
fn a_journal_pointing_outside_its_staging_folder_is_refused() {
    let world = World::new();
    let staged = staged_elsewhere(&world);
    let victim = world.temp.path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    std::fs::write(victim.join("keep"), b"synthetic").unwrap();
    let journal = world.state.join("journal.json");
    let original = std::fs::read(&journal).unwrap();
    let lease = world.admission.acquire().unwrap();

    for (field, value) in [
        ("payload_root", json!("../../victim")),
        ("payload_root", json!(victim)),
        ("payload_root", json!("")),
        ("staging", json!(victim)),
        (
            "install_root",
            json!(world.apps.join("..").join("Applications/BroScribe.app")),
        ),
        ("install_root", json!("BroScribe.app")),
    ] {
        edit_journal(&world, |j| j[field] = value.clone());
        let mut updater = world.updater();
        let why = format!("{field} = {value}");
        assert_eq!(
            updater.staged().unwrap_err().code(),
            codes::RECOVERY_FAILED,
            "{why}"
        );
        assert_eq!(
            updater.status().unwrap_err().code(),
            codes::RECOVERY_FAILED,
            "{why}"
        );
        assert_eq!(
            updater
                .install(&expect(&staged), &*lease, &Cancel::new())
                .unwrap_err()
                .code(),
            codes::RECOVERY_FAILED,
            "{why}"
        );
        assert_eq!(
            updater.discard().unwrap_err().code(),
            codes::RECOVERY_FAILED,
            "{why}"
        );
        assert_eq!(
            updater.recover(&*lease).unwrap_err().code(),
            codes::RECOVERY_FAILED,
            "{why}"
        );
        std::fs::write(&journal, &original).unwrap();
    }

    assert!(victim.join("keep").exists(), "nothing outside was touched");
    assert!(staged.payload.exists());
    assert!(world.platform.log().is_empty());
    assert_eq!(world.updater().staged().unwrap(), Some(staged));
}

#[cfg(unix)]
#[test]
fn a_staging_folder_replaced_by_a_link_is_refused() {
    let world = World::new();
    staged_elsewhere(&world);
    let elsewhere = world.temp.path().join("elsewhere");
    std::fs::rename(world.staging(), &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, world.staging()).unwrap();

    let mut updater = world.updater();
    assert_eq!(updater.staged().unwrap_err().code(), codes::RECOVERY_FAILED);
    assert!(elsewhere.join("incoming/BroScribe.app").exists());
    assert!(world.platform.log().is_empty());
    assert_eq!(world.installed(), identity("1.0.0", 10));
}

#[test]
fn an_interrupted_install_is_not_reported_as_staged() {
    let mut world = World::new();
    world.platform.atomic_exchange = false;
    crash_during_install(&world, 1);

    let mut updater = world.updater();
    assert_eq!(updater.staged().unwrap_err().code(), codes::PENDING);
    assert!(!world.root.exists(), "reading restored nothing");
    assert!(matches!(
        updater.status().unwrap(),
        UpdateStatus::Interrupted { .. }
    ));
}

#[test]
fn a_cancelled_install_keeps_the_stage_for_another_process() {
    let world = World::new();
    let staged = staged_elsewhere(&world);
    let mut updater = world.updater();
    let found = updater.staged().unwrap().unwrap();
    let cancel = Cancel::new();
    cancel.cancel();
    let lease = world.admission.acquire().unwrap();
    assert_eq!(
        updater
            .install(&expect(&found), &*lease, &cancel)
            .unwrap_err()
            .code(),
        codes::CANCELLED
    );
    drop(updater);

    let mut updater = world.updater();
    let found = updater.staged().unwrap();
    assert_eq!(found, Some(staged));
    updater
        .install(&expect(&found.unwrap()), &*lease, &Cancel::new())
        .unwrap();
    assert_eq!(world.installed(), identity("1.1.0", 11));
}

/// An updater of another state folder (another data folder) for the same
/// installation.
fn other_updater(world: &World) -> Updater<'_> {
    Updater::open(
        &world.state.with_file_name("other-state"),
        &world.settings,
        &world.platform,
        &world.transport,
    )
    .unwrap()
}

#[test]
fn another_state_folder_never_removes_a_pending_rollback_copy_after_its_owner_exits() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut owner = world.updater();
    let staged = stage(&world, &mut owner);
    let lease = world.admission.acquire().unwrap();
    let installed = owner
        .install(&expect(&staged), &*lease, &Cancel::new())
        .unwrap();
    // The owner exits awaiting confirmation: both locks are released.
    drop(owner);

    world.publish("1.2.0", 12, &payload(&identity("1.2.0", 12), SIGNER));
    let mut other = other_updater(&world);
    assert_eq!(other.status().unwrap(), UpdateStatus::Idle);
    assert_eq!(other.recover(&*lease).unwrap(), RecoveryOutcome::Clean);
    assert_eq!(
        world.platform.inspect(&installed.previous).unwrap(),
        identity("1.0.0", 10),
        "recovery kept the owner's previous installation"
    );
    let offer = offer(&mut other, world.now);
    assert_eq!(
        other
            .download(&offer, &Cancel::new(), &mut |_| {})
            .unwrap_err()
            .code(),
        codes::FOREIGN_STAGE
    );
    assert_eq!(
        world.platform.inspect(&installed.previous).unwrap(),
        identity("1.0.0", 10),
        "a refused download kept it too"
    );
    drop(other);

    let mut owner = world.updater();
    assert_eq!(
        owner.status().unwrap(),
        UpdateStatus::AwaitingConfirmation {
            from: identity("1.0.0", 10),
            to: identity("1.1.0", 11),
            signer: signed_by(SIGNER),
        }
    );
    assert_eq!(
        owner.rollback(&expect(&staged), &*lease).unwrap(),
        identity("1.0.0", 10)
    );
    assert_eq!(world.installed(), identity("1.0.0", 10));
    assert!(!world.staging().exists());
}

#[test]
fn another_state_folder_never_removes_a_staged_update_after_its_owner_exits() {
    let world = World::new();
    let staged = staged_elsewhere(&world);

    let lease = world.admission.acquire().unwrap();
    let mut other = other_updater(&world);
    assert_eq!(other.recover(&*lease).unwrap(), RecoveryOutcome::Clean);
    assert!(staged.payload.exists(), "recovery kept the owner's stage");
    let offer = offer(&mut other, world.now);
    assert_eq!(
        other
            .download(&offer, &Cancel::new(), &mut |_| {})
            .unwrap_err()
            .code(),
        codes::FOREIGN_STAGE
    );
    assert!(staged.payload.exists(), "a refused download kept it too");
    drop(other);

    let mut owner = world.updater();
    assert_eq!(owner.staged().unwrap(), Some(staged.clone()));
    owner
        .install(&expect(&staged), &*lease, &Cancel::new())
        .unwrap();
    assert_eq!(world.installed(), identity("1.1.0", 11));
}

#[test]
fn a_staging_folder_without_an_owner_record_is_never_removed() {
    let world = World::new();
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let unknown = world.staging().join("previous");
    std::fs::create_dir_all(&unknown).unwrap();
    std::fs::write(unknown.join("keep"), b"synthetic").unwrap();

    let lease = world.admission.acquire().unwrap();
    let mut updater = world.updater();
    assert_eq!(updater.recover(&*lease).unwrap(), RecoveryOutcome::Clean);
    let offer = offer(&mut updater, world.now);
    assert_eq!(
        updater
            .download(&offer, &Cancel::new(), &mut |_| {})
            .unwrap_err()
            .code(),
        codes::FOREIGN_STAGE
    );
    assert!(unknown.join("keep").exists());
    assert_eq!(world.installed(), identity("1.0.0", 10));
}

/// A held admission that counts its confirmations.
#[derive(Default)]
struct CountingLease {
    confirms: std::sync::atomic::AtomicUsize,
}

impl CountingLease {
    fn confirms(&self) -> usize {
        self.confirms.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl InstallLease for CountingLease {
    fn fence(&self) -> u64 {
        1
    }

    fn confirm(&self) -> broscribe_core::Result<()> {
        self.confirms
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn a_confirmed_install_refuses_a_stage_that_changed_since() {
    let world = World::new();
    let confirmed = expect(&staged_elsewhere(&world));

    // Another process replaces the stage while the confirmation is shown.
    let mut elsewhere = world.updater();
    elsewhere.discard().unwrap();
    world.publish("1.2.0", 12, &payload(&identity("1.2.0", 12), SIGNER));
    let replacement = stage(&world, &mut elsewhere);
    drop(elsewhere);

    let mut updater = world.updater();
    let changes = world.platform.log();
    let lease = CountingLease::default();
    assert_eq!(
        updater.staged_expected(&confirmed).unwrap_err().code(),
        codes::EXPECTATION_CHANGED
    );
    assert_eq!(
        updater
            .install(&confirmed, &lease, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::EXPECTATION_CHANGED
    );
    let resigned = UpdateExpectation {
        signer: signed_by("OTHERTEAM"),
        ..expect(&replacement)
    };
    assert_eq!(
        updater
            .install(&resigned, &lease, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::EXPECTATION_CHANGED,
        "the signer is part of what was confirmed"
    );
    assert_eq!(lease.confirms(), 0, "nothing was admitted");
    assert_eq!(world.platform.log(), changes, "install renamed nothing");
    assert_eq!(world.installed(), identity("1.0.0", 10));
    assert_eq!(
        updater.status().unwrap(),
        UpdateStatus::Staged {
            from: identity("1.0.0", 10),
            to: identity("1.2.0", 12),
            signer: signed_by(SIGNER),
        },
        "the newer stage is unchanged"
    );

    assert_eq!(
        updater.staged_expected(&expect(&replacement)).unwrap(),
        replacement
    );
    updater
        .install(&expect(&replacement), &lease, &Cancel::new())
        .unwrap();
    assert_eq!(world.installed(), identity("1.2.0", 12));
}

#[test]
fn a_confirmed_rollback_refuses_an_update_installed_since() {
    let world = World::new();
    let first = staged_elsewhere(&world);
    let lease = CountingLease::default();
    let mut updater = world.updater();
    updater
        .install(&expect(&first), &lease, &Cancel::new())
        .unwrap();
    let UpdateStatus::AwaitingConfirmation { from, to, signer } = updater.status().unwrap() else {
        panic!("the update awaits confirmation");
    };
    let confirmed = UpdateExpectation { from, to, signer };
    drop(updater);

    // Another process keeps 11, then installs 12 while the rollback
    // confirmation for 11 is shown.
    let mut elsewhere = world.updater();
    elsewhere.confirm().unwrap();
    world.publish("1.2.0", 12, &payload(&identity("1.2.0", 12), SIGNER));
    let second = stage(&world, &mut elsewhere);
    elsewhere
        .install(&expect(&second), &lease, &Cancel::new())
        .unwrap();
    drop(elsewhere);
    let confirms = lease.confirms();
    let renames = world.platform.log().len();

    let mut updater = world.updater();
    assert_eq!(
        updater.awaiting_expected(&confirmed).unwrap_err().code(),
        codes::EXPECTATION_CHANGED
    );
    assert_eq!(
        updater.rollback(&confirmed, &lease).unwrap_err().code(),
        codes::EXPECTATION_CHANGED
    );
    assert_eq!(lease.confirms(), confirms, "nothing was admitted");
    assert_eq!(world.platform.log().len(), renames, "nothing was renamed");
    assert_eq!(world.installed(), identity("1.2.0", 12));

    updater.awaiting_expected(&expect(&second)).unwrap();
    assert_eq!(
        updater.rollback(&expect(&second), &lease).unwrap(),
        identity("1.1.0", 11)
    );
    assert_eq!(world.installed(), identity("1.1.0", 11));
}

#[cfg(unix)]
#[test]
fn one_file_signed_release_stages_each_artifact_over_the_github_redirect_chain() {
    use broscribe_updates::sign_manifest_file;
    use std::os::unix::fs::OpenOptionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let seed = temp.path().join("test.seed");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&seed)
        .unwrap()
        .write_all(&[7; 32])
        .unwrap();
    let now = Utc::now();
    let mut artifacts = Vec::new();
    let transport = FakeTransport::new();
    let base = "https://github.com/BroToolbox/BroScribe/releases/download/v1.1.0";
    let envelope_url = format!("{base}/broscribe-update.json");
    let envelope_cdn = "https://release-assets.githubusercontent.com/envelope?synthetic=1";
    transport.redirect_to(RELEASE_FEED, &envelope_url);
    transport.redirect_to(&envelope_url, envelope_cdn);
    let targets = [
        ("aarch64-apple-darwin", "BroScribe.app", "zip", APP_ID),
        (
            "x86_64-unknown-linux-gnu",
            "BroScribe",
            "tar.gz",
            "BroScribe",
        ),
        (
            "aarch64-unknown-linux-gnu",
            "BroScribe",
            "tar.gz",
            "BroScribe",
        ),
        ("x86_64-pc-windows-msvc", "BroScribe", "zip", "BroScribe"),
    ];
    for (target, root, archive, app_id) in targets {
        let expected = PayloadIdentity {
            target: target.into(),
            app_id: app_id.into(),
            ..identity("1.1.0", 11)
        };
        let info = build_info(&expected);
        let (bytes, count, installed) = if archive == "tar.gz" {
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::fast(),
            ));
            for (name, bytes) in [
                ("build-info.json", info.as_slice()),
                ("signature", SIGNER.as_bytes()),
            ] {
                let mut header = tar::Header::new_ustar();
                header.set_mode(0o644);
                header.set_size(bytes.len() as u64);
                tar.append_data(&mut header, format!("{root}/{name}"), bytes)
                    .unwrap();
            }
            (
                tar.into_inner().unwrap().finish().unwrap(),
                2,
                (info.len() + SIGNER.len()) as u64,
            )
        } else {
            zip_bytes(&[
                file(&format!("{root}/build-info.json"), info),
                file(&format!("{root}/signature"), SIGNER),
            ])
        };
        let url = format!("{base}/{target}.{archive}");
        let cdn = format!("https://objects.githubusercontent.com/{target}");
        let mut artifact = artifact_for(&bytes, count, installed);
        artifact["target"] = json!(target);
        artifact["app_id"] = json!(app_id);
        artifact["url"] = json!(url);
        artifact["archive"] = json!(archive);
        artifact["root"] = json!(root);
        transport.redirect_to(&url, &cdn);
        transport.serve(&cdn, bytes);
        artifacts.push(artifact);
    }
    let mut release = manifest(now, "1.1.0", 11, artifacts[0].clone());
    release["artifacts"] = json!(artifacts);
    let signed = sign_manifest_file(&seed, &release.to_string(), now, &Cancel::new()).unwrap();
    let settings = UpdateSettings {
        trusted_keys: vec![signed.signers[0].public_key_hex()],
        signer: Some(SIGNER.into()),
        ..UpdateSettings::default()
    };
    transport.serve(envelope_cdn, signed.envelope.into_bytes());
    for (target, root, _, app_id) in targets {
        let installed = PayloadIdentity {
            target: target.into(),
            app_id: app_id.into(),
            ..identity("1.0.0", 10)
        };
        let root = temp.path().join(target).join(root);
        FakeUpdatePlatform::write_payload(&root, &installed, SIGNER).unwrap();
        let platform = FakeUpdatePlatform::new(root, target);
        let mut updater = Updater::open(
            &temp.path().join(format!("state-{target}")),
            &settings,
            &platform,
            &transport,
        )
        .unwrap();
        let ready = offer(&mut updater, now);
        assert_eq!(ready.artifact.target, target);
        let staged = updater
            .download(&ready, &Cancel::new(), &mut |_| {})
            .unwrap();
        assert_eq!(
            staged.to,
            PayloadIdentity {
                target: target.into(),
                app_id: app_id.into(),
                ..identity("1.1.0", 11)
            }
        );
        assert!(matches!(
            updater
                .check(now + Duration::days(91), &Cancel::new())
                .unwrap(),
            CheckOutcome::NoVerifiedUpdate { .. }
        ));
    }
}

#[test]
fn updater_recover_and_uninstall_agree_on_partial_unjournaled_stage() {
    for uninstall in [false, true] {
        let world = World::new();
        let stage = world.staging();
        let payload = stage.join("incoming/BroScribe.app/Contents");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("Info.plist"), "partial").unwrap();
        std::fs::write(payload.join("foreign.txt"), "synthetic").unwrap();
        std::fs::write(
            stage.join("owner.json"),
            json!({
                "schema":1, "state_dir":broscribe_platform::stable_path(&world.state),
                "from":identity("1.0.0", 10), "to":identity("1.1.0", 11),
            })
            .to_string(),
        )
        .unwrap();
        let mut updater = world.updater();
        let mut remove = || {
            if uninstall {
                broscribe_updates::remove_owned_stage(
                    &stage,
                    world.root.file_name().unwrap(),
                    std::slice::from_ref(&world.state),
                    &world.platform,
                )
                .map(|_| ())
            } else {
                updater
                    .recover(&*world.admission.acquire().unwrap())
                    .map(|_| ())
            }
        };
        let error = remove().unwrap_err();
        assert_eq!(error.code(), codes::RELEASE_TREE_UNSAFE);
        assert!(error.to_string().contains("broscribe update recover"));
        assert!(payload.join("Info.plist").exists());
        std::fs::remove_file(payload.join("foreign.txt")).unwrap();
        remove().unwrap();
        assert!(!stage.exists());
    }
}

#[cfg(unix)]
#[test]
fn read_only_status_accepts_legacy_modes_and_refuses_world_writable_records() {
    use std::os::unix::fs::PermissionsExt as _;
    let world = World::new();
    let staged = staged_elsewhere(&world);
    let journal = world.state.join("journal.json");
    for (path, mode) in [(&world.state, 0o755), (&journal, 0o644)] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    assert!(matches!(
        broscribe_updates::pending_replacement(&world.state).unwrap(),
        UpdateStatus::Staged { .. }
    ));
    assert_eq!(
        std::fs::metadata(&world.state)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
        0o644
    );
    drop(world.updater());
    assert_eq!(
        std::fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o606)).unwrap();
    let error = broscribe_updates::pending_replacement(&world.state).unwrap_err();
    assert_eq!(error.code(), codes::RECOVERY_FAILED);
    assert!(
        error.to_string().contains("chmod -- 600")
            && error.to_string().contains("chown --")
            && error.to_string().contains("journal.json")
    );
    assert!(staged.payload.is_dir());
}

#[test]
fn confirmed_inventory_crosses_profiles_and_explains_manual_overwrites() {
    let mut world = World::new();
    let profile_a = world.temp.path().join("profile-a");
    world.state = profile_a.join("updates");
    let first = staged_elsewhere(&world);
    {
        let mut updater = world.updater();
        let lease = world.admission.acquire().unwrap();
        updater
            .install(&expect(&first), &*lease, &Cancel::new())
            .unwrap();
        drop(lease);
        updater.confirm().unwrap();
    }
    let profile_b = world.temp.path().join("profile-b");
    let state_b = profile_b.join("updates");
    let mut updater =
        Updater::open(&state_b, &world.settings, &world.platform, &world.transport).unwrap();
    updater
        .protect_profiles(vec![profile_b, profile_a], world.temp.path().to_path_buf())
        .unwrap();
    world.publish("1.3.0", 13, &payload(&identity("1.3.0", 13), SIGNER));
    let next = stage(&world, &mut updater);
    let executable = world.root.join("bin/broscribe");
    let saved = std::fs::read(&executable).unwrap();
    std::fs::write(&executable, "manually edited existing build").unwrap();
    let error = updater.prepare_install(Some(&expect(&next))).unwrap_err();
    assert_eq!(error.code(), codes::RELEASE_TREE_UNSAFE);
    std::fs::write(&executable, saved).unwrap();
    updater.discard().unwrap();

    // A newer archive extracted over the old directory keeps its file ID.
    FakeUpdatePlatform::write_payload(&world.root, &identity("1.2.0", 12), SIGNER).unwrap();
    let next = stage(&world, &mut updater);
    let error = updater.prepare_install(Some(&expect(&next))).unwrap_err();
    assert_eq!(error.code(), codes::INSTALLATION_CHANGED_OUTSIDE);
    assert!(
        error
            .to_string()
            .contains("delete the old installation folder")
    );
    let error = broscribe_updates::removal_inventory(
        &world.root,
        &[state_b, world.state.clone()],
        &world.platform,
    )
    .err()
    .unwrap();
    assert_eq!(error.code(), codes::INSTALLATION_CHANGED_OUTSIDE);
    assert!(world.root.exists());
}

#[test]
fn inventory_selection_prefers_recorded_at_and_skips_unusable_secondary_state() {
    use std::fs::{self, FileTimes, OpenOptions};
    use std::time::{Duration as StdDuration, SystemTime};
    let mut world = World::new();
    let profile = world.temp.path().join("current");
    world.state = profile.join("updates");
    let staged = staged_elsewhere(&world);
    {
        let mut updater = world.updater();
        let lease = world.admission.acquire().unwrap();
        updater
            .install(&expect(&staged), &*lease, &Cancel::new())
            .unwrap();
        drop(lease);
        updater.confirm().unwrap();
    }
    let current_path = world.state.join("installed-inventory.json");
    let original = fs::read(&current_path).unwrap();
    let current: Value = serde_json::from_slice(&original).unwrap();
    assert!(current["recorded_at"].as_str().unwrap().ends_with('Z'));
    let older = world.temp.path().join("older/updates");
    let invalid = world.temp.path().join("invalid/updates");
    fs::create_dir_all(&older).unwrap();
    fs::create_dir_all(&invalid).unwrap();
    let older_path = older.join("installed-inventory.json");
    let mut historical = current.clone();
    historical["recorded_at"] = json!("2020-01-01T00:00:00Z");
    historical["identity"]["build"] = json!(1);
    fs::write(&older_path, serde_json::to_vec(&historical).unwrap()).unwrap();
    fs::write(invalid.join("installed-inventory.json"), b"broken").unwrap();
    OpenOptions::new()
        .write(true)
        .open(&current_path)
        .unwrap()
        .set_times(
            FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + StdDuration::from_secs(100)),
        )
        .unwrap();
    let read = |warnings: &mut Vec<_>| {
        broscribe_updates::removal_inventory_with_warnings(
            &world.root,
            &world.state,
            &[older.clone(), invalid.clone()],
            &world.platform,
            warnings,
        )
    };
    let mut warnings = Vec::new();
    assert!(read(&mut warnings).unwrap().is_some());
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].state_dir, invalid);

    // Only a legacy record uses mtime. A newer legacy mtime can select it.
    historical.as_object_mut().unwrap().remove("recorded_at");
    fs::write(&older_path, serde_json::to_vec(&historical).unwrap()).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&older_path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now() + StdDuration::from_secs(60)))
        .unwrap();
    assert_eq!(
        read(&mut warnings).err().unwrap().code(),
        codes::INSTALLATION_CHANGED_OUTSIDE
    );
    fs::remove_file(&older_path).unwrap();

    // The installation path exercises newest_inventory independently of uninstall.
    {
        let mut updater = world.updater();
        updater
            .protect_profiles(
                vec![profile, invalid.parent().unwrap().to_path_buf()],
                world.temp.path().to_path_buf(),
            )
            .unwrap();
        world.publish("1.3.0", 13, &payload(&identity("1.3.0", 13), SIGNER));
        let next = stage(&world, &mut updater);
        updater.prepare_install(Some(&expect(&next))).unwrap();
        let warns = updater.take_inventory_warnings();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].state_dir, invalid);
    }
    fs::write(&current_path, b"broken current record").unwrap();
    assert_eq!(
        read(&mut warnings).err().unwrap().code(),
        codes::RECOVERY_FAILED
    );
}

#[cfg(windows)]
#[test]
fn recorded_windows_verbatim_root_and_plain_uninstall_paths_share_inventory_and_staging() {
    let mut world = World::new();
    world.root = std::fs::canonicalize(&world.root).unwrap();
    world.platform = FakeUpdatePlatform::new(world.root.clone(), TARGET);
    let staged = staged_elsewhere(&world);
    let mut updater = world.updater();
    let lease = world.admission.acquire().unwrap();
    updater
        .install(&expect(&staged), &*lease, &Cancel::new())
        .unwrap();
    drop(lease);
    updater.confirm().unwrap();
    drop(updater);
    let plain = broscribe_platform::plain_verbatim(&world.root);
    assert_ne!(plain, world.root);
    assert!(
        broscribe_updates::removal_inventory(
            &plain,
            std::slice::from_ref(&world.state),
            &world.platform
        )
        .unwrap()
        .is_some()
    );
    world.publish("1.2.0", 12, &payload(&identity("1.2.0", 12), SIGNER));
    let mut updater = world.updater();
    stage(&world, &mut updater);
    drop(updater);
    let stage =
        broscribe_platform::plain_verbatim(&std::fs::canonicalize(world.staging()).unwrap());
    let owner = broscribe_platform::plain_verbatim(&std::fs::canonicalize(&world.state).unwrap());
    assert!(
        broscribe_updates::remove_owned_stage(
            &stage,
            plain.file_name().unwrap(),
            &[owner],
            &world.platform
        )
        .unwrap()
    );
    assert!(!stage.exists());
}

#[cfg(unix)]
#[test]
fn discard_quarantines_an_incoming_slot_that_cannot_be_enumerated() {
    use std::os::unix::fs::PermissionsExt as _;
    let world = World::new();
    staged_elsewhere(&world);
    let incoming = world.staging().join("incoming");
    std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o000)).unwrap();
    // A privileged test runner can enumerate a mode-000 directory.
    if std::fs::read_dir(&incoming).is_ok() {
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let mut updater = world.updater();
    let kept = updater
        .discard()
        .unwrap()
        .expect("unreadable incoming kept");
    let incoming = kept.join("incoming");
    std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(incoming.join("BroScribe.app/signature").is_file());
    assert!(!world.staging().exists());
    assert_eq!(updater.status().unwrap(), UpdateStatus::Idle);
}

#[test]
fn switching_feeds_uses_each_recorded_build_ceiling() {
    let world = World::new();
    let preview = "https://updates.example/preview.json";
    let mut preview_settings = world.settings.clone();
    preview_settings.feed_url = Some(preview.into());
    preview_settings.channel = "preview".into();
    let (bytes, count, installed) = zip_bytes(&payload(&identity("1.3.0", 30), SIGNER));
    let mut preview_manifest = manifest(
        world.now,
        "1.3.0",
        30,
        artifact_for(&bytes, count, installed),
    );
    preview_manifest["channel"] = json!("preview");
    world
        .transport
        .serve(preview, envelope(&world.key, &preview_manifest));
    let open_preview = || {
        Updater::open(
            &world.state,
            &preview_settings,
            &world.platform,
            &world.transport,
        )
        .unwrap()
    };
    assert_eq!(offer(&mut open_preview(), world.now).manifest.build, 30);
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    assert_eq!(offer(&mut world.updater(), world.now).manifest.build, 11);

    preview_manifest["build"] = json!(29);
    world
        .transport
        .serve(preview, envelope(&world.key, &preview_manifest));
    assert_eq!(
        open_preview()
            .check(world.now, &Cancel::new())
            .unwrap_err()
            .code(),
        codes::FEED_ROLLBACK
    );
}

#[test]
fn verified_builds_cross_profiles_only_under_the_same_feed_policy() {
    use broscribe_core::config::Config;
    let world = World::new();
    let staged = staged_elsewhere(&world);
    let matching = world.temp.path().join("matching-profile");
    let unrelated = world.temp.path().join("other-feed-profile");
    let broken = world.temp.path().join("broken-profile");
    let mut current = Config::default();
    current.updates = world.settings.clone();
    current.updates.enabled = false;
    let mut other = current.clone();
    current.save(&unrelated).unwrap();
    other.updates.feed_url = Some("https://updates.example/other.json".into());
    other.save(&matching).unwrap();
    std::fs::create_dir_all(unrelated.join("updates")).unwrap();
    std::fs::write(
        unrelated.join("updates/seen.json"),
        r#"{"highest_build":100}"#,
    )
    .unwrap();
    std::fs::create_dir_all(broken.join("updates")).unwrap();
    std::fs::write(broken.join("updates/seen.json"), "synthetic corrupt record").unwrap();
    world.publish("1.2.0", 12, &payload(&identity("1.2.0", 12), SIGNER));
    {
        let mut foreign = Updater::open(
            &matching.join("updates"),
            &world.settings,
            &world.platform,
            &world.transport,
        )
        .unwrap();
        assert_eq!(offer(&mut foreign, world.now).manifest.build, 12);
    }
    world.publish("1.1.0", 11, &payload(&identity("1.1.0", 11), SIGNER));
    let mut local = world.updater();
    local.track_seen_profiles(vec![unrelated.clone(), matching.clone(), broken.clone()]);
    let error = local.check(world.now, &Cancel::new()).unwrap_err();
    assert_eq!(error.code(), codes::FEED_ROLLBACK);
    assert!(error.to_string().contains("build 12"));
    let warnings = local.take_inventory_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(broscribe_platform::same_path(
        &warnings[0].state_dir,
        &broken.join("updates")
    ));

    drop(local);
    let mut local = world.updater();
    local
        .protect_profiles(
            vec![unrelated, matching, broken],
            world.temp.path().to_path_buf(),
        )
        .unwrap();
    let lease = world.admission.acquire().unwrap();
    let error = local
        .install(&expect(&staged), &*lease, &Cancel::new())
        .unwrap_err();
    assert_eq!(error.code(), codes::FEED_ROLLBACK);
    assert!(error.to_string().contains("build 12"));
    assert_eq!(world.installed().build, 10);
}
