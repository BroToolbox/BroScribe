# Changelog

All notable user-facing changes to BroScribe are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **Identify voices**: a focused, keyboard-driven mode in the review queue that
  asks about one voice at a time, then walks possible duplicate people. Open it
  with **Start identifying** in the queue or **Identify voices** in a meeting's
  Speakers tab.
- **Speaking now** dock in the recording window: shows who is speaking and the
  people expected in the call, and names the current turn with a tap or keys 1–4.
  The agent now publishes the matching calendar event's invitees as expected
  names.
- Switch between downloaded models and delete models you no longer use from a
  compact list in Settings → Transcription, or with
  `broscribe models remove <id> --yes`. Models in use are protected.
- New models: Whisper base and small for speech, WeSpeaker ResNet34-LM for
  speaker recognition, and Potion Base 8M, Potion Retrieval 32M and Blink
  Multilingual INT8 (covers Portuguese) for search.
- Remove saved words and phrases from the Word list in Settings, the CLI and
  MCP.

### Fixed

- The Word list entry field in Settings no longer shrinks until each letter
  wraps, and it shows the saved terms.

## [0.1.0] - 2026-11-28

First public release: a notarized macOS arm64 disk image and ZIP, an unsigned
Windows x64 ZIP, and Linux x64 and arm64 tar.gz archives, with a signed update
envelope.

- Record meetings or import supported audio and video files, then transcribe and
  identify speakers locally.
- Search transcriptions from the desktop app, CLI or MCP server.
- Optionally connect calendars and enable providers for summaries and semantic search.
- Check GitHub Releases for signed updates, install them after confirmation and
  roll back a rejected update.
- Uninstall from the CLI or Settings after previewing the plan, keeping the
  library and data unless you choose to remove them.
- Install on macOS from a signed and notarized disk image: drag BroScribe to
  Applications. The release ZIP stays available and is what the updater installs.

See the [verification page](docs/acceptance.md) for current platform evidence.

[Unreleased]: https://github.com/BroToolbox/BroScribe/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/BroToolbox/BroScribe/releases/tag/v0.1.0
