# Verification and release readiness

This page tracks what must be verified on the exact source revision and
packages selected for release. Automated checks use synthetic fixtures.
Hardware and installation checks exercise the packaged app. Anything not yet
observed on a real system stays open here and *verify* in the design document.

## Platform status

| Platform | Evidence so far | Release checks still open |
| --- | --- | --- |
| macOS arm64 | The workspace and shipping-script suites pass from source. A Developer ID build of an earlier revision was signed, notarized, stapled and accepted by Gatekeeper in a release dry run. An unsigned local disk image of a release build passed the disk image verifier (2026-11-27). Hosted CI (`macos-15`) passed on a pre-release revision (2026-11-27). | A signed, notarized and stapled disk image, hosted CI and the release workflow on the release commit, a clean-account Gatekeeper check of the DMG and the ZIP, helper permissions, the live `daemon stop` check, and a signed update, rollback and uninstall of the published package. |
| Windows x64 | The full workspace suite passed natively with MSVC on Windows 11 x64. The uninstall clean-up matcher passes under Windows PowerShell 5.1 and PowerShell 7. Hosted CI (`windows-2025`, MSVC) passed Clippy, the fake/synthetic workspace tests and the release build on a pre-release revision (2026-11-27). | Hosted CI on the release commit, Authenticode, dependency redistribution, and clean-machine capture, playback, login, update, rollback and uninstall checks. |
| Linux x64 and arm64 | The arm64 container suite and bounded desktop tests have run. Hosted x64 and arm64 container CI passed on a pre-release revision (2026-11-27). | Hosted x64 and arm64 CI on the release commit, the supported Ubuntu desktop baseline, package dependencies and permissions, native capture, update, rollback and uninstall checks. |

## First release

- [ ] Every [CI lane](release.md#continuous-integration) passed on the release commit.
- [ ] The release workflow produced and verified every archive, checksum and provenance file and the notarized macOS disk image, and the signed update envelope verified with the compiled release key.
- [ ] Each final archive installed and ran on a clean instance of its supported OS.
- [ ] On macOS, the notarized DMG downloaded in Safari, dragged to Applications in Finder, ejected and opened from Applications as a standard user passed Gatekeeper without translocation, and running directly from the mounted image was refused for agent registration and updates.
- [ ] A synthetic recording captured, saved and played the expected tracks.
- [ ] Permissions, launch at login, and CLI and MCP integration were verified.
- [ ] Installation, manual upgrade and removal preserved user data.
- [ ] Each platform passed the signed update, rollback and recovery rehearsal from the published envelope.
- [ ] Each platform passed a native uninstall, including the refusal and retry cases in the [release checklist](release.md#publication-checklist).
- [ ] The envelope refresh workflow ran once by hand and once on its schedule.
- [ ] The published archives and update artifacts matched the verified exact hashes.

See [release.md](release.md#publication-checklist) for the publication checklist.
