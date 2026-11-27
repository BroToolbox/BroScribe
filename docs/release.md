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
| `macos` | `macos-15` (Apple silicon) | checks for an arm64 host, Xcode and CMake; `fetch.sh --target osx-arm64`; Clippy with `-D warnings` and `cargo test --workspace` through `scripts/build-target.sh aarch64-apple-darwin`; checks the macOS third-party licence inventory; fails if `Cargo.lock` changed |
| `linux` (arm64) | `ubuntu-24.04-arm` | `scripts/docker-linux-test.sh`: the digest-pinned `rust:1.100.0-trixie` image, linux-aarch64 sherpa-onnx by checksum, rejection of any `metal` feature, Clippy, `cargo build --locked --all-targets`, `cargo test --locked` and the licence inventory for the workspace; fails if `Cargo.lock` changed |
| `linux` (x64) | `ubuntu-24.04` | `scripts/docker-linux-test.sh --amd64`: the same steps with linux-x64 sherpa-onnx, using the pinned Rust distribution on a digest-pinned Ubuntu 24.04 userland |
| `windows` | `windows-2025` | loads the x64 environment with `VsDevCmd.bat`, puts the MSVC linker first in Git Bash, uses a short Cargo target path and one Cargo job, and checks for CMake; `fetch.sh --target win-x64`; Clippy with `-D warnings`, debug and release `cargo build --all-targets`, and workspace fake/synthetic tests through the explicit `x86_64-pc-windows-msvc` wrapper; runs `cleanup-matcher.Tests.ps1` in separate Windows PowerShell 5.1 and PowerShell 7 steps; checks the Windows third-party licence inventory; fails if `Cargo.lock` changed. No Windows hardware or desktop-runtime claim |
| `merge-gate` (**BroScribe CI**) | `ubuntu-24.04` | succeeds only when every job above succeeded; this is the required status check |

Rules the workflow follows:

- **No actions.** There is no `uses:` step, so no third-party or GitHub-authored action code runs. Checkout is a shallow `git fetch` of the exact commit. The job token reaches git only in that one step, through `GIT_CONFIG_*` environment variables (never argv, never written to git config), and is masked; build and test steps never receive it.
- **Least privilege.** `permissions: contents: read`. The workflow triggers on `push` and `pull_request` for `main` and on `workflow_dispatch`, never `pull_request_target`, so untrusted pull-request code never runs with a privileged token.
- **Pinned inputs.** Rust 1.100.0 is installed explicitly and must equal the `rust-toolchain.toml` channel; every Cargo command uses `--locked`; native libraries come only from `fetch.sh` against `SHA256SUMS`; the Linux images are pinned by digest. Nothing is installed from an unpinned package manager on the macOS and Windows runners: a missing prerequisite fails the job instead. (The Linux containers install build packages from the Debian or Ubuntu archive for their pinned base; their versions are not pinned.)
- **No caches.** No dependency or build cache is restored, so a poisoned cache cannot reach a build.

A green run is compile and unit-test evidence on hosted runners with fakes and synthetic fixtures. It does not show that capture, permissions, readers, notifications, credential stores, sign-in or the GUI work on a real machine ([acceptance.md](acceptance.md)).

Local equivalents: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `scripts/check-publish-age.py`, `scripts/docker-linux-test.sh`, and on Windows `scripts/build-target.sh x86_64-pc-windows-msvc build --locked --workspace --all-targets`, the same with `--release`, and `scripts/build-target.sh x86_64-pc-windows-msvc test --locked --workspace`. Windows builds require the wrapper's CMake runtime policy and a fresh native cache when migrating ([install.md](install.md#linux-and-windows-development-builds)).

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
- [ ] Attach the four archives, their checksum and provenance files, and the
  signed `broscribe-update.json` to one draft. Verify and publish it. Do not
  enable release immutability: the refresh workflow must replace
  `broscribe-update.json` every month. Protect the `v*` tags instead. Either
  maintainer may authorize the release they prepared.

### macOS artifact

- [ ] Version in the workspace `Cargo.toml` raised; build number (`BROSCRIBE_BUILD_NUMBER`, an integer from 1 to 999999999, default `git rev-list --count HEAD`) higher than any earlier release. Set it explicitly for every official build.
- [ ] The licence inventory is complete for the release target. Reviewed texts are pinned in `scripts/third-party-licenses.toml` and `third_party/sherpa-onnx/NATIVE_LICENSES.toml`; the latter covers eleven linked archive constituents for macOS arm64, Linux arm64/x64 and Windows x64, with strict inventories checked on 2026-11-18. Rerun whenever Cargo.lock/native pins change. Developer ID bundling/notarization refuses incomplete inventories. For Windows, separately verify final MSVC runtime linkage and the builder's CRT/STL redistribution rights; archive notices alone do not establish those terms.
- [ ] `BROSCRIBE_SIGN_IDENTITY='Developer ID Application: …' scripts/bundle-macos.sh` succeeds from a clean tree, without `RUSTFLAGS` and with `MACOSX_DEPLOYMENT_TARGET` unset or 14.2 (every binary's minimum macOS is checked). It writes `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.txt`, `third-party-inventory.json` and `build-info.json` into `Contents/Resources`, strips the executables, remaps builder paths, ships GPUI's Metal shaders as source compiled at run time (a prebuilt metallib would record the build folder), and keeps debug symbols in `target/dist/symbols/BroScribe-<version>-<build>-macos-arm64.zip` (dSYMs and `UUIDS.txt`), to archive privately for symbolication. The symbols stage removes build-only `Relocations` maps, matches every dSYM UUID to its executable, checks every file for builder paths and AppleDouble, archives with `ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent`, and checks the ZIP again. Earlier locally signed packages exposed app and symbol-path leaks, which were corrected before later packages passed. A Developer ID build of an earlier revision was signed, notarized, stapled and accepted by Gatekeeper in a dry run; this item stays open until the release commit's own Developer ID bundle passes.
  To compile before making signing credentials available, run `scripts/bundle-macos.sh --build-unsigned` and then `BROSCRIBE_SIGN_IDENTITY='Developer ID Application: …' scripts/sign-macos.sh target/BroScribe.app`. The second command signs the prebuilt bundle in place. Run notarization after signing completes.
  Build provenance counts tracked changes and nonignored untracked files, including new source folders. Ignored build output does not make the tree dirty; failure to read git status aborts bundling instead of claiming a clean tree.
- [ ] `scripts/notarize-macos.sh <keychain-profile>` notarizes and staples the app, then writes `target/dist/BroScribe-<version>-<build>-macos-arm64.zip`, its `.sha256` and a `.provenance.json`. On rejection it saves a sanitized `notarization-<submission-id>.log` there and fails; fix the cause and resubmit.
- [ ] Before offering a release through the updater: on a test installation of the previous release, with synthetic data, `update check`, `download`, `install --yes` (refused while recording), the restart reporting the new build, `confirm`, and in a second run `rollback --yes` all behave as designed. Not done yet.
- [ ] `scripts/verify-macos-artifact.py target/dist/<zip> --provenance <provenance.json> --require-complete-licences --codesign --require-stapled` passes (structure, licences, signature, checksum, stapling, readable bundle modes, no group or other write access, and no builder or personal paths; add `--forbid <text>` for any other string that must not appear).
- [ ] On a clean Mac (a fresh user account at minimum): download the zip, move the app to `/Applications`, open it, and confirm Gatekeeper accepts it without translocation, the agent registers, permissions are requested under BroScribe and `broscribe doctor` is clean.
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

`.github/workflows/release.yml` runs on `v*` tag pushes and manual dispatch. Four native jobs use the pinned Rust toolchain and SHA-256-verified sherpa-onnx inputs to build, package and verify macOS ARM64, Linux x86-64, Linux ARM64 and Windows x64 MSVC. Tag names must match the workspace version. Each job uploads an archive, its checksum and its provenance. The build number is the workflow run number plus `BROSCRIBE_BUILD_NUMBER_BASE` (repository variable, default 300000000). Keep it above the previous release. A matching tag push with all jobs green creates a draft GitHub Release. Only that job has `contents: write`. Its final signing step assembles a single stable manifest and checks the version, build, Git commit and lockfile agree across the artifacts. It includes only Developer ID–signed, notarized and stapled macOS packages for team `3CHB92P2NM`. It offers the original Linux tarballs and Windows ZIP. The draft has 12 assets plus `broscribe-update.json` when signing succeeds. The owner publishes the draft. The envelope uses its generation time as `published_at` and expires 90 days later, so keep draft review within that window.

Before creating the first release tag, use **Actions → release → Run workflow** to dispatch the workflow on the branch at the intended release commit. Configure the release signing credentials before dispatch. Wait for all four hosted jobs and download their archives, checksums and provenance. Check each signing status and confirm the draft job was skipped. Manual dispatch only uploads artifacts.

- For macOS, configure `MACOS_CERTIFICATE_P12` and `NOTARY_API_KEY_P8` as base64, and set `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGN_IDENTITY` (Developer ID Application), `NOTARY_KEY_ID` and `NOTARY_ISSUER_ID`. The job builds the unsigned bundle and verifies its symbols before the signing step receives any secrets. The signing step uses umask 077 for the temporary keychain and signing secrets, then umask 022 for signing, notarizing and packaging. It deletes the temporary keychain at exit. Without credentials, the job signs ad hoc and uploads a clearly labeled preview ZIP. Never publish that ZIP as the macOS release.
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

Keep the published release's assets editable and keep `UPDATE_SIGNING_KEY` available to the repository. **Refresh the stable update envelope** (`.github/workflows/refresh-update.yml`) runs monthly on the default branch. It authenticates the latest published stable release's envelope against the compiled release keys, even if its validity window has ended. It checks all four archives and their checksum and provenance files. It verifies the macOS checksum sidecar for both the notarized release and the ad-hoc preview. Each Linux and Windows archive must match one complete published file layout in `crates/broscribe-platform/src/release_layouts.json`. Keep historical layouts when adding a new one. The workflow requires the manifest to match the authenticated one except for the two dates. An ad-hoc macOS preview stays out of the update feed. After upload it downloads and verifies the replacement. On a failed upload or verification it attempts to restore the authenticated previous envelope and fails the run.

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
