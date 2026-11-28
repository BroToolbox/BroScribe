# Verification and release readiness

This page tracks what has been verified on each published release and what
must be verified on the exact source revision and packages of the next one.
Automated checks use synthetic fixtures. Hardware and installation checks exercise the packaged app. Anything not yet
observed on a real system stays open here and *verify* in the design document.

## Platform status

| Platform | Evidence so far | Checks still open |
| --- | --- | --- |
| macOS arm64 | The workspace and shipping-script suites pass from source. The 0.1.0 release workflow (tag `v0.1.0`, commit `c3e2147`) built, signed with Developer ID, notarized, stapled and verified the disk image and ZIP; hosted CI (`macos-15`) passed on that commit (2026-11-27). | A clean-account Gatekeeper check of the DMG and the ZIP, helper permissions, the live `daemon stop` check, and a signed update, rollback and uninstall of the published package. |
| Windows x64 | The full workspace suite passed natively with MSVC on Windows 11 x64. The uninstall clean-up matcher passes under Windows PowerShell 5.1 and PowerShell 7. Hosted CI (`windows-2025`, MSVC) passed Clippy, the fake/synthetic workspace tests and the release build on the 0.1.0 commit, and the release workflow built and verified the unsigned 0.1.0 ZIP (2026-11-27). | Authenticode, dependency redistribution, and clean-machine capture, playback, login, update, rollback and uninstall checks. |
| Linux x64 and arm64 | The arm64 container suite and bounded desktop tests have run. Hosted x64 and arm64 container CI passed on the 0.1.0 commit, and the release workflow built and verified both 0.1.0 tar.gz archives (2026-11-27). | The supported Ubuntu desktop baseline, package dependencies and permissions, native capture, update, rollback and uninstall checks. |

## Release 0.1.0 (published 2026-11-28)

- [x] Every [CI lane](release.md#continuous-integration) passed on the release commit (`c3e2147`).
- [x] The release workflow produced and verified every archive, checksum and provenance file and the notarized macOS disk image, and the signed update envelope verified with the compiled release key.
- [ ] The envelope refresh workflow ran once by hand and once on its schedule.

The native checks below were not completed before 0.1.0 was published. They
carry over to the next release.

## Next release

The next release is the first one that existing installations receive through
the updater, so the update checks run from the published 0.1.0 envelope.

- [ ] Every [CI lane](release.md#continuous-integration) passed on the release commit.
- [ ] The release workflow produced and verified every archive, checksum and provenance file and the notarized macOS disk image, and the signed update envelope verified with the compiled release key.
- [ ] Each final archive installed and ran on a clean instance of its supported OS.
- [ ] On macOS, the notarized DMG downloaded in Safari, dragged to Applications in Finder, ejected and opened from Applications as a standard user passed Gatekeeper without translocation, and running directly from the mounted image was refused for agent registration and updates.
- [ ] A synthetic recording captured, saved and played the expected tracks.
- [ ] Permissions, launch at login, and CLI and MCP integration were verified.
- [ ] Installation, manual upgrade and removal preserved user data.
- [ ] Each platform updated from an installed 0.1.0 through the in-app updater, then passed the rollback and recovery rehearsal.
- [ ] Each platform passed a native uninstall, including the refusal and retry cases in the [release checklist](release.md#publication-checklist).
- [ ] The published archives and update artifacts matched the verified exact hashes.

See [release.md](release.md#publication-checklist) for the publication checklist.
