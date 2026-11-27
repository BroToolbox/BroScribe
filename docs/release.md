# Release checklist

The first release will publish macOS arm64, Windows x64, Linux x64 and Linux
arm64 together. Each package must identify the same reviewed source commit,
version and build number.

## Continuous integration

[CI](../.github/workflows/ci.yml) runs on pull requests into `main` and pushes
to `main`. Its required result is **BroScribe CI**. The aggregate succeeds
only if the supply-chain, macOS arm64, Windows x64 and both Linux jobs succeed.
The Linux x64 job uses an Ubuntu 24.04 userland and Linux arm64 uses Debian
Trixie. Rust 1.100.0, the native library checksums and the container bases are
pinned. Fork code uses a read-only token and hosted runners.

Hosted jobs verify builds and synthetic tests. Use the native checks below to
validate permissions, device capture and the signed packages.

| Job | Runner | Runs |
|---|---|---|
| `supply-chain` | `ubuntu-24.04` | `scripts/check-publish-age.py Cargo.lock` (three-day minimum age, yanked versions, no git sources); the shipping-script unit tests (`python3 -m unittest discover -s scripts/tests`, offline fixtures); `cargo fmt --all --check` |
| `macos` | `macos-15` (Apple silicon) | checks for an arm64 host, Xcode and CMake; `fetch.sh --target osx-arm64`; Clippy with `-D warnings` and `cargo test --workspace` through `scripts/build-target.sh aarch64-apple-darwin`, keeping only line tables in dev/test debug info; checks the macOS third-party licence inventory; fails if `Cargo.lock` changed |
| `linux` (arm64) | `ubuntu-24.04-arm` | `scripts/docker-linux-test.sh`: the digest-pinned `rust:1.100.0-trixie` image, linux-aarch64 sherpa-onnx by checksum, rejection of any `metal` feature, Clippy, `cargo build --locked --all-targets`, `cargo test --locked` and the licence inventory for the workspace, with four Cargo jobs and line-table-only dev/test debug info; fails if `Cargo.lock` changed |
| `linux` (x64) | `ubuntu-24.04` | `scripts/docker-linux-test.sh --amd64`: the same steps with linux-x64 sherpa-onnx, using the pinned Rust distribution on a digest-pinned Ubuntu 24.04 userland |
| `windows` | `windows-2025` | two parallel lanes on separate runners. Both load the x64 environment with `VsDevCmd.bat`, put the MSVC linker first in Git Bash, use a short Cargo target path and the runner's default Cargo job count, check for CMake and run `fetch.sh --target win-x64`; dev/test builds keep only line tables (`CARGO_PROFILE_DEV_DEBUG=line-tables-only`). **tests:** workspace fake/synthetic tests (which also compile the examples) through the explicit `x86_64-pc-windows-msvc` wrapper. **clippy and release build:** Clippy with `-D warnings`, then release `cargo build --workspace` of the libraries and executables; runs `cleanup-matcher.Tests.ps1` in separate Windows PowerShell 5.1 and PowerShell 7 steps; checks the Windows third-party licence inventory. Each lane fails if `Cargo.lock` changed. No Windows hardware or desktop-runtime claim |
| `merge-gate` (**BroScribe CI**) | `ubuntu-24.04` | succeeds only when every job above succeeded; this is the required status check |

Rules the workflow follows:

- **No actions.** There is no `uses:` step, so no third-party or GitHub-authored action code runs. Checkout is a shallow `git fetch` of the exact commit. The job token reaches git only in that one step, through `GIT_CONFIG_*` environment variables (never argv, never written to git config), and is masked; build and test steps never receive it.
- **Least privilege.** `permissions: contents: read`. The workflow triggers on `push` and `pull_request` for `main` and on `workflow_dispatch`, never `pull_request_target`, so untrusted pull-request code never runs with a privileged token.
- **Pinned inputs.** Rust 1.100.0 is installed explicitly and must equal the `rust-toolchain.toml` channel; every Cargo command uses `--locked`; native libraries come only from `fetch.sh` against `SHA256SUMS`; the Linux images are pinned by digest. Nothing is installed from an unpinned package manager on the macOS and Windows runners: a missing prerequisite fails the job instead. (The Linux containers install build packages from the Debian or Ubuntu archive for their pinned base; their versions are not pinned.)
- **No caches.** No dependency or build cache is restored, so a poisoned cache cannot reach a build.

A green run is compile and unit-test evidence on hosted runners with fakes and synthetic fixtures. It does not show that capture, permissions, readers, notifications, credential stores, sign-in or the GUI work on a real machine ([acceptance.md](acceptance.md)).

Local equivalents: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `scripts/check-publish-age.py`, `scripts/docker-linux-test.sh`, and on Windows `scripts/build-target.sh x86_64-pc-windows-msvc test --locked --workspace` and `scripts/build-target.sh x86_64-pc-windows-msvc build --locked --workspace --release`. Windows builds require the wrapper's CMake runtime policy and a fresh native cache when migrating ([install.md](install.md#linux-and-windows-development-builds)).

### README screenshots

The [screenshots workflow](../.github/workflows/screenshots.yml) is not a
required check. It runs on every push to `main`, on pull requests into `main`
that change `crates/`, `third_party/`, the Cargo files, `rust-toolchain.toml`,
`.cargo/`, `scripts/linux-desktop/`, `scripts/docker-linux-test.sh` or the
workflow itself, and on `workflow_dispatch`; never on
`pull_request_target`. Its top-level permissions are empty. The `capture` job
(`ubuntu-24.04-arm`, `contents: read`) checks out the exact commit as CI does
and runs `scripts/linux-desktop/screenshots.sh`: the pinned desktop image, the
app built from that commit, and four screens of invented data captured without
network. It uploads only the four PNGs and `manifest.json`; on a pull request
that artifact shows reviewers the UI change. When the capture fails, its logs
and AT-SPI dumps (invented data only) go to a separate
`screenshots-diagnostics` artifact kept for three days, which `publish` never
downloads. Runs on `main` share one concurrency group: a newer run never
cancels the active one, though a newer pending run may replace an older
pending run. Any other ref gets its own concurrency group and cancels its
older runs. The `publish` job, the only one
with `contents: write`, runs only for pushes and manual runs on `main` in
`BroToolbox/BroScribe`. It validates the artifact as data (exact file names,
PNG structure, 1280×800, size limits, recomputed hashes, the manifest's commit),
does nothing when `main` has moved on, when the published screenshots come
from the same or a newer commit, or when the pixels are unchanged (a failed
fetch or history comparison fails the job instead), and
otherwise force-pushes one orphan commit to the generated `screenshots`
branch with `--force-with-lease`. The README links to that branch's raw files.
If the published manifest names a commit GitHub no longer serves (for
example after `main` was rewritten), publication fails until a maintainer
deletes the `screenshots` branch and reruns the workflow on `main`.
Pull requests from forks get the read-only token and no publication. Changes
to this workflow and to `scripts/linux-desktop/` get the same line-by-line
review as `ci.yml`. A branch ruleset covering every branch must leave
`screenshots` writable by GitHub Actions.

## Merge policy

Every change to `main` goes through a pull request with passing **BroScribe CI**.
`@raphaelbarreiros` and `@Renan-Brasilio` can merge their own PRs from
branches in this repository after CI.
Other PRs need an approval from either maintainer on the current head commit.
A maintainer's outstanding request for changes blocks a contributor PR.
Only the two maintainers have write access to the repository, so no one else
can push to a branch that merges without review. Keep it that way: before
anyone else gets write access, including through organization roles, teams or
outside collaborators, require review for maintainer PRs from in-repository
branches too.

GitHub's native approval requirement applies to all PR authors. Configure these
two independent rulesets for the author-specific policy:

1. For `main`, require a pull request and up-to-date **BroScribe CI** from
   GitHub Actions. Set the native approval count to zero. Give this ruleset no
   bypass actor. Disallow force pushes and deletion.
2. In a second `main` ruleset, restrict updates and allow only the dedicated
   BroScribe merge GitHub App to bypass that update restriction. Give the app
   contents write and pull requests read access. Install the app on this
   repository only. Enable squash merging.

Create the `merge` environment with the selected deployment branch `main`.
Set its `MERGE_APP_ID` variable and `MERGE_APP_PRIVATE_KEY` secret. The
[merge workflow](../.github/workflows/merge.yml) runs from protected `main`
only for the two maintainers. To merge, run **Merge a pull request** and enter
the PR number. The workflow checks the current PR author, reviews and head
through GitHub, verifies the required rules are active, including the update restriction,
and requests a squash
merge of that head. GitHub enforces
the separate CI ruleset at merge time.

A pull request runs its own version of `ci.yml`, so a change to
`.github/workflows/` or to the scripts CI runs can alter the check it is judged
by. Maintainers review such changes line by line before approving them, and
rerun CI on `main` after merging.

Prove the setup with a failing CI, a maintainer-authored PR and a fork PR
before accepting external contributions. Exercise a missing approval, a new
head after approval and a valid approval on that head. Verify that the GitHub
App cannot merge while the CI ruleset is unsatisfied.

## Publication checklist

Either maintainer can authorize preparation and publication under this workflow. Credentials (Developer ID certificate, notary keychain profile, tenant accounts) never enter the repository, scripts' arguments or logs. One narrow exception: on GitHub's single-use macOS runner, the workflow passes the temporary keychain password (masked) and the `.p12` password to Apple's `security` tool as arguments, after the build has finished, because `security import` and `unlock-keychain` take them no other way.

### Before any public commit

- [ ] A maintainer has explicitly authorized the audited initial commit and the first push.
- [ ] The new root commit includes only audited files and public commit metadata. Review text, assets, examples, fixtures, vendored materials and model licences. Exclude real meeting data, credentials and local paths.
- [ ] `SECURITY.md` names a private reporting channel.
- [ ] Enable GitHub private vulnerability reporting in the repository settings.
- [ ] `CHANGELOG.md` describes the current availability. Move release entries from `[Unreleased]` under the version when publishing binaries.

### Gates on the release commit

- [ ] On the release commit, replace the "in preparation" statements with the release's status: README.md (Availability), SECURITY.md (Status), CHANGELOG.md (a versioned section), docs/install.md (opening paragraph and platform table).

- [ ] `cargo fmt --all --check`, Clippy with `-D warnings`, `cargo test --workspace` on macOS.
- [ ] `scripts/check-publish-age.py` passes for `Cargo.lock`.
- [ ] `scripts/docker-linux-test.sh` passes (Linux build, tests, no Metal).
- [ ] Windows MSVC and both Linux targets pass the hosted workspace gate.
- [ ] **BroScribe CI** is green for that commit.
- [ ] Update [acceptance.md](acceptance.md) with the observed results, review the design document’s *verify* marks, and reflect the qualified systems in the README and release notes.

### Joint release

- [ ] Reserve one explicit, increasing build number and use the same source SHA
  and version for all four archives. Create the protected `vX.Y.Z` tag after the
  exact `main` SHA passes CI.
- [ ] Windows x64, Linux x64 and Linux arm64 include the desktop app, CLI,
  daemon, worker and update runner, valid `build-info.json`, dependencies and
  notices.
  Verify the package layouts and builder-path scans. On Windows verify the
  MSVC dependency closure and timestamped Authenticode signatures. On Linux
  verify permissions, dependency resolution and the Ubuntu 24.04 glibc floor.
  Archive the macOS debug symbols privately.
- [ ] Rehearse install, recording, playback, manual ZIP upgrade, rollback and
  removal on each clean target, with the library preserved. Rehearse the
  Windows certificate transition and the macOS Development-to-Developer ID
  migration.
- [ ] Attach the four archives, their checksum and provenance files, the
  notarized macOS disk image and its checksum, and the signed
  `broscribe-update.json` to one draft. Verify and publish it. Do not
  enable release immutability: the refresh workflow must replace
  `broscribe-update.json` every month. Protect the `v*` tags instead. Either
  maintainer may authorize the release they prepared.

### macOS artifact

- [ ] Version in the workspace `Cargo.toml` raised; build number (`BROSCRIBE_BUILD_NUMBER`, an integer from 1 to 999999999, default `git rev-list --count HEAD`) higher than any earlier release. Set it explicitly for every official build.
- [ ] The licence inventory is complete for the release target. Reviewed texts are pinned in `scripts/third-party-licenses.toml` and `third_party/sherpa-onnx/NATIVE_LICENSES.toml`; the latter covers eleven linked archive constituents for macOS arm64, Linux arm64/x64 and Windows x64, with strict inventories checked on 2026-11-18. Rerun whenever Cargo.lock/native pins change. Developer ID bundling/notarization refuses incomplete inventories. For Windows, separately verify final MSVC runtime linkage and the builder's CRT/STL redistribution rights; archive notices alone do not establish those terms.
- [ ] `BROSCRIBE_SIGN_IDENTITY='Developer ID Application: …' scripts/bundle-macos.sh` succeeds from a clean tree, without `RUSTFLAGS` and with `MACOSX_DEPLOYMENT_TARGET` unset or 14.2 (every binary's minimum macOS is checked). It writes `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.txt`, `third-party-inventory.json` and `build-info.json` into `Contents/Resources`, strips the executables, remaps builder paths, ships GPUI's Metal shaders as source compiled at run time (a prebuilt metallib would record the build folder), and keeps debug symbols in `target/dist/symbols/BroScribe-<version>-<build>-macos-arm64.zip` (dSYMs and `UUIDS.txt`), to archive privately for symbolication. The symbols stage removes build-only `Relocations` maps, matches every dSYM UUID to its executable, checks every file for builder paths and AppleDouble, archives with `ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent`, and checks the ZIP again. Earlier locally signed packages exposed app and symbol-path leaks, which were corrected before later packages passed. A Developer ID build of an earlier revision was signed, notarized, stapled and accepted by Gatekeeper in a dry run; this item stays open until the release commit's own Developer ID bundle passes.
  To compile before making signing credentials available, run `scripts/bundle-macos.sh --build-unsigned` and then `BROSCRIBE_SIGN_IDENTITY='Developer ID Application: …' scripts/sign-macos.sh target/BroScribe.app`. The second command signs the prebuilt bundle in place. Run notarization after signing completes.
  Build provenance counts tracked changes and nonignored untracked files, including new source folders. Ignored build output does not make the tree dirty; failure to read git status aborts bundling instead of claiming a clean tree.
- [ ] `BROSCRIBE_SIGN_IDENTITY='Developer ID Application: …' scripts/notarize-macos.sh <keychain-profile>` notarizes and staples the app, then writes `target/dist/BroScribe-<version>-<build>-macos-arm64.zip`. It then runs `scripts/package-macos-dmg.sh`, which copies the stapled app without metadata into a private stage beside an `Applications` link, makes an APFS ULFO disk image with `hdiutil`, signs it with the same identity (timestamped, without hardened runtime, entitlements or `--deep`), notarizes and staples it, and only then writes `BroScribe-<version>-<build>-macos-arm64.dmg` and its `.sha256` after `verify-macos-artifact.py --dmg … --compare-zip <zip> --codesign --require-stapled` passed. Only after that does `release-metadata.py provenance` write the one `.provenance.json` recording both artifacts (`disk_image` for the image) and, with it, the ZIP's `.sha256`; both artifacts are then verified against it. Any failure from the moment the ZIP enters `target/dist/` until that verification passed removes every release-named file of the build (ZIP, DMG, both checksums, provenance). On rejection it saves a sanitized `notarization-<submission-id>.log` (or `notarization-dmg-<submission-id>.log`) there and fails; fix the cause and resubmit. Each `notarytool --wait` stops after `BROSCRIBE_NOTARY_TIMEOUT` (default 45m), and each whole submission after that plus a 10-minute upload grace. Only `notarize-macos.sh` produces a consistent set: `package-macos-dmg.sh` run alone removes any existing provenance of that build. To try the disk image layout without credentials, `scripts/package-macos-dmg.sh --unsigned` builds and checks an unsigned image of a local app in `target/dmg-unsigned/`; never publish it.
- [ ] Before offering a release through the updater: on a test installation of the previous release, with synthetic data, `update check`, `download`, `install --yes` (refused while recording), the restart reporting the new build, `confirm`, and in a second run `rollback --yes` all behave as designed. Not done yet.
- [ ] `scripts/verify-macos-artifact.py target/dist/<zip> --provenance <provenance.json> --require-complete-licences --codesign --require-stapled` passes (structure, licences, signature, checksum, stapling, readable bundle modes, no group or other write access, and no builder or personal paths; add `--forbid <text>` for any other string that must not appear).
- [ ] `scripts/verify-macos-artifact.py --dmg target/dist/<dmg> --compare-zip target/dist/<zip> --provenance <provenance.json> --require-complete-licences --codesign --require-stapled` passes. Before anything is attached: the checksum; the provenance `disk_image`, checked with the same rules as when it was recorded (team `3CHB92P2NM`, equal to the app's; a Developer ID Application authority; an Accepted, stapled notarization with a submission id; ULFO with APFS); an unencrypted ULFO image without a licence agreement; a timestamped Developer ID signature of team `3CHB92P2NM` on the image whose authorities and team equal the recorded ones; its stapled ticket; and Gatekeeper accepting it (`spctl -a -t open --context context:primary-signature`). Any failure there ends the check without mounting. Then it is attached read-only at a private mount point and always detached: an APFS volume whose root is exactly `BroScribe.app` and the `Applications` link to `/Applications` (no `.DS_Store`, AppleDouble, `.fseventsd` or other hidden entries; APFS creates none); the mounted app identical to the ZIP's in paths, types, link targets, modes and contents (extended attributes, ACLs and BSD flags are not compared by anything), with the same bundle and builder-path checks; and a valid deep signature and stapled ticket on the mounted app.
- [ ] On a clean Mac (a fresh user account at minimum, signed in as a standard user): download the DMG in Safari, open it, drag BroScribe to Applications in Finder, eject the image, open BroScribe from Applications, and confirm Gatekeeper accepts it without translocation, the agent registers, permissions are requested under BroScribe and `broscribe doctor` is clean. Repeat with the ZIP: move the app to `/Applications` and open it.
- [ ] With the DMG mounted, also open BroScribe directly from the disk image and confirm the setup assistant refuses to register the agent (read-only volume or translocated copy), `broscribe agents install`, `broscribe mcp enable` and `broscribe update install` refuse, and nothing is registered from the image. Eject it afterwards.
- [ ] Native checks of the update and uninstall paths: on Linux, an in-app update under a user with umask 002 (the Ubuntu default) and a forced failure that rolls back; on Windows, **Settings → Privacy → Uninstall BroScribe…** with and without deleting data (the folders the app held are gone after it quits), `update recover` with no Run entry, and an update that keeps `FFPLAY.EXE` beside the executables.
- [ ] After the first release is published, run **Actions → Refresh the stable update envelope** once by hand and confirm it replaced `broscribe-update.json` on that release with new dates and the same build; the monthly schedule then keeps the envelope inside its 90-day window.
- [ ] After publication, check **Actions → Refresh the stable update envelope** for a run triggered by the monthly schedule. Confirm its refresh job executed successfully and replaced the envelope with new dates and the same build.
- [ ] With that installed, signed bundle's agent running and idle, run `broscribe --json daemon stop` three times (starting the agent again between runs with `broscribe daemon start`). Each must exit 0 with `login_job_unloaded: true` and leave `launchctl print gui/$(id -u)/org.broscribe.broscribed` reporting the job not found. The SMAppService job prints only `program identifier = …/broscribed (mode: 2)`; fixtures cannot prove it.

### Linux artifacts (x86-64 and ARM64)

- [ ] On a native glibc Linux builder for each architecture, use the pinned Rust/Cargo 1.100.0 toolchain and a clean checkout of the release commit. Run formatting, native Clippy with `-D warnings`, workspace tests and the no-Metal gate. For Rust gates use `scripts/build-target.sh <triple> … --locked`.
- [ ] Fetch the native input using `third_party/sherpa-onnx/fetch.sh --target linux-x64` or `--target linux-aarch64`. Set the release's `BROSCRIBE_BUILD_NUMBER` and run `python3 scripts/package-desktop.py <triple>` with `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`. Use a fresh Cargo target directory if native build flags have changed, so CMake rebuilds its native objects.
- [ ] Verify `target/dist/BroScribe-<version>-<build>-linux-<arch>.tar.gz` with `python3 scripts/verify-desktop-artifact.py <archive>`. Keep its `.sha256` and `.provenance.json` with it. Add `--forbid <path>` for any extra build or scratch roots. This checks the target ELF architecture, executable modes, `$ORIGIN` runpath, desktop launcher, hicolor icons, complete notices, archive SHA-256 and provenance.
- [ ] Before calling an architecture supported, install this archive per user on a real desktop for that architecture, launch from the applications menu, complete setup, check `doctor` and agent status, record and play a short synthetic meeting over PipeWire, check tray, notifications, Secret Service and autostart, and remove the installation. Record the evidence in [acceptance.md](acceptance.md).

### Windows artifact (x86-64 MSVC)

- [ ] Use Windows x86-64 with the MSVC developer environment and the pinned Rust/Cargo 1.100.0 toolchain. On the clean release commit run formatting and the Windows MSVC native Clippy and workspace-test gates with `--locked`. Fetch the pinned archive in Git Bash with `third_party/sherpa-onnx/fetch.sh --target win-x64`. Make the MSVC tools available first in Git Bash's `PATH`, including `dumpbin.exe`. Select a fresh, short Cargo target directory to avoid stale CMake settings and MSBuild path limits. Use build roots without spaces, quotes, percent signs or semicolons because the cmake crate flattens the native compiler arguments. The packager resolves Bash once and prefers the launcher beside the Git for Windows installation.
- [ ] Set the same `BROSCRIBE_BUILD_NUMBER`. Run `python scripts/package-desktop.py x86_64-pc-windows-msvc`. The packager builds the five production executables through `scripts/build-target.sh`, which selects the static MSVC runtime. To sign before archiving, run with `--build-only`, sign and verify all five executables in the triple's `release` directory, then run with `--package-only` using the same target directory and build number.
- [ ] Verify `target/dist/BroScribe-<version>-<build>-windows-x64.zip` with `python scripts/verify-desktop-artifact.py <archive>` and distribute both sidecars with it. Authenticode is optional. SmartScreen can warn on an unsigned build. The updater verifies the signed manifest and the artifact SHA-256 for every Windows package. When a certificate pin is configured, it additionally verifies the Authenticode signatures of the shipped executables. Check that self-update and rollback keep locally installed side-by-side FFmpeg tools.
- [ ] On the real extracted artifact, use `dumpbin /DEPENDENTS` to check final MSVC CRT/STL and sherpa-onnx linkage for each executable. Verify the builder's Microsoft CRT/STL redistribution rights for the exact runtime shipped or linked into those executables. A complete third-party inventory and successful MSVC link do not establish redistribution rights.
- [ ] Run a native Windows uninstall from the CLI and from Settings, confirm that successful helpers clear their own references, and interrupt each helper once to check that a retry retires the old helper folder. Verify obsolete-reference recovery and the guidance for an existing helper folder outside the current temporary folder. Include orphan and foreign journal chunks, a linked journal (refused), a leftover `.broscribe-update-runner.<pid>.tmp` and a similarly named file whose number exceeds `u32::MAX` (kept).
- [ ] On an account whose `%TEMP%` uses a short (8.3) name, run the Windows workspace tests and an uninstall; also hold `%APPDATA%\BroScribe.uninstall-helper.json` open without read sharing during a helper run and confirm the receipt reports it as kept.
- [ ] During the packaged uninstall with the helper reference held open, check the receipt for `Helper reference kept.` and the matching `.details` line.
- [ ] Run the cleanup matcher fixture under Windows PowerShell 5.1 and PowerShell 7. The Rust workspace test checks the fixture against BroScribe’s allowlist. The script tests matcher parity, library protection, interrupted model promotion and receipt bounds.
- [ ] Before calling Windows supported, extract the ZIP on a clean Windows desktop, run setup, confirm the app creates the user Start-menu shortcut and the recording agent registers at logon, and check `doctor`, IPC, two-track synthetic recording and playback, tray, notifications, Credential Manager and uninstall. Record the evidence in [acceptance.md](acceptance.md).

The Linux and Windows archives ship `LICENSE`, `NOTICE`, the complete third-party notices and inventory, and `build-info.json`. The currently pinned Linux and Windows sherpa-onnx inputs link statically. The packages carry their reviewed notices and archive checksums in provenance. The Linux verifier and Windows packager reject linkage to an unshipped sherpa-onnx runtime.

### Upgrade and uninstall

- [ ] From the previous release with synthetic data: queued jobs and journals survive `scripts/update-macos.py`; it refuses while a recording is active, a stopped recording is unsaved, a job runs, the agent runs another build or does not answer, or the app is open; it takes the agent's install admission before unregistering anything and no capture can start until it is released; a forced failure after admission, including when the new login item registers but its agent never starts, rolls back to the previous app and agent, which must prove their build before admission is released; the new agent proves its exact version, build and target; CLI link and MCP registrations work afterwards; the designated requirement is unchanged so permissions persist. Not done yet.
- [ ] The uninstall steps in [install.md](install.md#uninstall) leave no agent, login item, link or registration behind.

Before publishing, run an update and a rollback on an isolated previous-release installation for each supported platform with synthetic data. Record the actual package and runtime results in [acceptance.md](acceptance.md). The automated fixtures exercise the signed feed, redirects, archive staging, staged-file modification and recovery but do not establish a native update on each platform.

Archive installations without a saved inventory use the union of historical Linux or Windows file paths and types. Keep [release_layouts.json](../crates/broscribe-platform/src/release_layouts.json) append-only. When adding, moving, renaming or removing a file in a release, append the complete new layout for that platform and keep every earlier entry. The script suite checks that the packager's current layouts are present. The updater checks an incoming payload against the inventory of the authenticated extraction and saves that inventory after confirmation. Later removal checks its paths, types, modes, sizes and hashes. An interrupted removal can resume from a journaled tombstone. macOS also checks the bundle seal.

## Release workflow

`.github/workflows/release.yml` runs on `v*` tag pushes and manual dispatch. Four native jobs use the pinned Rust toolchain and SHA-256-verified sherpa-onnx inputs to build, package and verify macOS ARM64, Linux x86-64, Linux ARM64 and Windows x64 MSVC. Tag names must match the workspace version. Each job uploads an archive, its checksum and its provenance; a notarized macOS job also uploads the disk image and its checksum. The build number is the workflow run number plus `BROSCRIBE_BUILD_NUMBER_BASE` (repository variable, default 300000000). Keep it above the previous release. A matching tag push with all jobs green creates a draft GitHub Release. Only that job has `contents: write`. Its final signing step assembles a single stable manifest and checks the version, build, Git commit and lockfile agree across the artifacts. It includes only Developer ID–signed, notarized and stapled macOS packages for team `3CHB92P2NM`. It offers the original Linux tarballs and Windows ZIP. The macOS disk image is never in the manifest. Before signing and again before creating the draft, `release-metadata.py assets` checks the exact asset names (a numeric count would accept a wrong mix): three files per package (archive, `.sha256`, `.provenance.json`) of one build, plus the `.dmg` and `.dmg.sha256` for a notarized macOS build and none for an ad-hoc one, and `broscribe-update.json` exactly when signing succeeded. That is 14 assets for a notarized build (15 with the envelope) and 12 for an ad-hoc preview (13). A half disk image pair, a disk image beside an ad-hoc ZIP or any other file fails the job. The draft notes list the DMG first for macOS and the ZIP as the update payload and alternative download. The owner publishes the draft. The envelope uses its generation time as `published_at` and expires 90 days later, so keep draft review within that window.

Before creating the first release tag, use **Actions → release → Run workflow** to dispatch the workflow on the branch at the intended release commit. Configure the release signing credentials before dispatch. Wait for all four hosted jobs and download their archives, checksums and provenance. Check each signing status and confirm the draft job was skipped. Manual dispatch only uploads artifacts.

- For macOS, configure `MACOS_CERTIFICATE_P12` and `NOTARY_API_KEY_P8` as base64, and set `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGN_IDENTITY` (Developer ID Application), `NOTARY_KEY_ID` and `NOTARY_ISSUER_ID`. The job builds the unsigned bundle and verifies its symbols before the signing step receives any secrets. On every macOS run, ad-hoc or notarized, it then builds and verifies an unsigned test disk image of that bundle (`package-macos-dmg.sh --unsigned` into `target/dmg-unsigned/`, never `dist` or an upload), so a runner difference in `hdiutil` (an unexpected hidden root entry such as `.fseventsd`, or a persistent “Resource busy”) fails a manual dispatch before any tag. The signing step sets `BROSCRIBE_NOTARY_TIMEOUT=30m` for each of the two notarizations, and the macOS job's 210-minute limit leaves room for the build, the tests and both bounded submissions (the other jobs keep 150). The signing step uses umask 077 for the temporary keychain and signing secrets, then umask 022 for signing, notarizing and packaging. `MACOS_SIGN_IDENTITY` reaches both `sign-macos.sh` and `notarize-macos.sh`, which signs the disk image with it. After notarization the step verifies the ZIP and then the disk image (`--dmg … --compare-zip … --codesign --require-stapled`) against the final provenance. It deletes the temporary keychain at exit. Without credentials, the job signs ad hoc and uploads a clearly labeled preview ZIP and fails if any disk image exists. Never publish that ZIP as the macOS release.
- For Windows, optional `WINDOWS_CERTIFICATE_PFX` (base64) and `WINDOWS_CERTIFICATE_PASSWORD` enable Authenticode and signature verification before archiving. When signing, set the optional repository variable `WINDOWS_TIMESTAMP_SERVER` to the URL of your Authenticode timestamp service. A configured timestamp must verify. Without this variable, signatures are untimestamped and the uploaded artifact label and draft notes say so. Without the certificate and password, the artifact and draft identify the build as unsigned. Partial or failing credentials fail the relevant job.

### Provision the first release update key

On macOS or Linux, create the first seed on a trusted workstation with the release CLI. The command prints the public key and key ID. Add the public key to `crates/broscribe-core/release-keys.txt`, keep the seed in your password manager, and delete the local temporary seed after provisioning. Keep the seed out of the repository.

```sh
seed_dir="$(mktemp -d)"
seed="$seed_dir/release.seed"
BROSCRIBE_DATA_DIR="$seed_dir/profile" broscribe --json update release-key new --out "$seed"
base64 < "$seed" | tr -d '\r\n' | gh secret set UPDATE_SIGNING_KEY --repo BroToolbox/BroScribe
```

The current release key (key_id `8f2ce0c9193a3cc6`, created 2026-11-25) is already in `crates/broscribe-core/release-keys.txt`, and its seed is kept in the owner's password manager. The workflow exposes `UPDATE_SIGNING_KEY` only to the final signing step after builds and verification, decodes it to a temporary mode-0600 file, deletes the file and verifies the result with the compiled key before staging the envelope. If the secret is absent, the workflow logs that signing was skipped and creates the draft without the envelope.

### Rotate an established release update key

1. Generate the new seed and store it securely. Add its public key to `crates/broscribe-core/release-keys.txt` alongside the old public key. Publish a stable transition release for all supported platforms with both public keys compiled in. Keep `UPDATE_SIGNING_KEY` on the **old** seed through the transition so old clients can authenticate that release, including during monthly envelope refreshes.
2. Allow at least 90 days after publishing the transition release, and confirm that supported installations have upgraded or have a documented manual route to the transition build. Offline clients can miss the transition regardless of the waiting period. Then switch `UPDATE_SIGNING_KEY` to the new seed. Clients still on the old key alone need the authenticated transition archive or a manually verified newer release.
3. Once there is a stable envelope signed by the new key and the refresh workflow has successfully verified and renewed it, a later release may remove the old public key. Keep both public keys on the refresh workflow's branch until the currently published envelope can be verified with the new key alone.

The envelope format accepts multiple signatures. Release and refresh CI each sign with the single `UPDATE_SIGNING_KEY` secret, which is sufficient for this sequence.

Keep the published release's assets editable and keep `UPDATE_SIGNING_KEY` available to the repository. **Refresh the stable update envelope** (`.github/workflows/refresh-update.yml`) runs monthly on the default branch. It authenticates the latest published stable release's envelope against the compiled release keys, even if its validity window has ended. It checks all four archives and their checksum and provenance files. It verifies the macOS checksum sidecar for both the notarized release and the ad-hoc preview. A notarized macOS release must have exactly its `.dmg` and `.dmg.sha256` (an ad-hoc preview none); after the ZIP has matched the authenticated envelope, the workflow runs the full disk image check against that ZIP (`--dmg … --compare-zip … --codesign --require-stapled`) before it emits any manifest. A substituted disk image is rejected if its signer, stapled ticket, layout or app differs from the envelope-authenticated ZIP, even when its checksum and provenance were rewritten to match. A different disk image that is validly signed by the team, notarized and holds the identical app would pass: the disk image's own digest is not in the signed envelope. Changing the ZIP and the disk image together fails, because the ZIP no longer matches the envelope. With `--allow-missing-baseline` (the manual bootstrap), there is no envelope to authenticate the ZIP, so the disk image is compared with an unauthenticated ZIP; the team pin, the tickets and Gatekeeper still apply. No release was published before disk images, so there is no notarized release without one to accept; keep this rule unless such a release is ever published. Each Linux and Windows archive must match one complete published file layout in `crates/broscribe-platform/src/release_layouts.json`. Keep historical layouts when adding a new one. The workflow requires the manifest to match the authenticated one except for the two dates. An ad-hoc macOS preview stays out of the update feed. After upload it downloads and verifies the replacement. On a failed upload or verification it attempts to restore the authenticated previous envelope and fails the run.

The workflow fails when `UPDATE_SIGNING_KEY` is missing. A release without an envelope also fails unless the owner runs **Run workflow** with **Bootstrap an unsigned release after manually verifying its assets** enabled. Use that option only after verifying the published assets yourself.

GitHub disables scheduled workflows on public repositories after 60 days without repository activity. Re-enable the schedule or run the workflow by hand before the envelope's 90-day expiry. [GitHub Actions schedule documentation](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#schedule)

Protect the `v*` tags with a repository ruleset. Withdrawing the latest release makes clients that have seen its build report `update_feed_rollback` when they encounter an older signed release. Ship a higher build to restore updates for those clients.


### Builder paths in release artifacts

The macOS bundler and desktop packager remap both the supplied and symlink-resolved forms of the checkout, Cargo target directory, Cargo home, user home and `RUNNER_TEMP` when set. The verifiers check both spellings. They skip the explicit home prefix when it contains a pinned third-party source tree. On GitHub's macOS runner, `/Users/runner` contains the sherpa-onnx and ONNX Runtime paths in the pinned archive. The generic scan still rejects other paths in that home, including the checkout and Cargo directories. Pass `--forbid <path>` for any additional build root. Explicitly forbidden paths are checked before masking upstream paths.

Generic prefixes require a path component ending at a separator or a string boundary, so both `/Users/runner/` and a standalone `/Users/runner` are caught. POSIX paths are case-sensitive, so lowercase `/users/` calendar API routes pass. Windows paths are case-insensitive. The same component rule applies to `/private/tmp/` and `/private/var/tmp/`, including nested paths such as `/private/tmp/private/var/tmp-build/obj`.

The exception list in `third_party/sherpa-onnx/UPSTREAM_PATHS.toml` applies only to native executable contents and macOS DWARF. It lists the project roots in the SHA-256-pinned static libraries. To rederive it after a pin change, fetch all four archives using `fetch.sh` and run the following from `third_party/sherpa-onnx`. Inspect the results for the most specific common source roots. Counted strings include duplicates and archive members the linker may discard.

```sh
shasum -a 256 -c SHA256SUMS
while read -r _ archive; do
    printf '\n%s\n' "$archive"
    tar -tf "$archive" | grep -E '\.(a|lib)$' |
        tar -xOf "$archive" -T /dev/stdin |
        LC_ALL=C strings -a |
        LC_ALL=C grep -Eio '(/(Users|home)/[^/]+/[^/]+/[^/]+/[^/]+/|/k2-fsa/[^/]+/|[a-z]:\\a\\[^\\]+\\[^\\]+\\)' |
        sort | uniq -c
done < SHA256SUMS
```

The owner reviews the exact artifacts and Windows redistribution terms, completes the real-desktop smoke checks on all supported targets, and decides when to publish the draft. Hosted tests exercise the build and archive checks.
