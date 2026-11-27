# BroScribe

BroScribe is a local-first meeting recorder and transcription app, written in
Rust with a GPUI interface.

- It detects and records meetings, with a live side panel, or imports any audio
  or video file.
- It labels the speakers in each transcript and recognizes voices you have
  named before. You name any new speaker once, and BroScribe learns the voice.
- It extracts topics, keywords, decisions, action items and tags with local
  models or the AI CLIs you already have installed.
- A CLI and an MCP server let your agents search and use your meetings.

## Availability

The first binary release is in preparation. The initial targets are Apple
silicon on macOS 14.2 or later, x64 on Windows 11, and Linux x64 and arm64
with glibc. The reference Linux desktop is Ubuntu 24.04 LTS with GNOME, Wayland
and PipeWire. Intel Macs are not supported. See the
[release checklist](docs/release.md) for the checks that qualify each package
and the [verification page](docs/acceptance.md) for what has been observed so
far. Source builds and prerequisites are documented in the
[installation guide](docs/install.md).

## Privacy

Transcription and speaker recognition run locally. BroScribe keeps audio and
voice embeddings on your machine. AI providers are off by default. When you
enable a provider, it may receive transcript text, notes or instructions to
generate summaries, or transcript excerpts and queries for semantic search.
Calendar connections, model downloads, MCP clients and update checks have their
own data flows. See the [security policy](SECURITY.md) for details.

`broscribe --json diagnostics` exports counts-only diagnostics without meeting
content, personal identifiers or paths. The same report is available through
MCP's read-only `get_diagnostics` tool and **About → Copy Diagnostics**.

## Updates and removal

BroScribe checks GitHub Releases for signed updates and installs one only after
you confirm. You can turn the check off in Settings. `broscribe uninstall`, or
**Settings → Privacy → Uninstall BroScribe…**, shows its plan first and acts
only with `--yes`. It keeps your library and data unless you ask to remove
them, and then deletes only the files BroScribe wrote. Keep your own files
outside the installation folder: updates and uninstall refuse to replace or
remove a folder that holds anything else. The
[installation guide](docs/install.md) covers path requirements, manual upgrades
and platform details.

## Project links

- [Contributing](CONTRIBUTING.md) covers setup, tests and pull requests.
- [Installation](docs/install.md) covers the desktop app, CLI, MCP, updates and
  removal.
- [Design](docs/design.md) explains the architecture.
- [Changelog](CHANGELOG.md) lists user-facing changes.
- [Security policy](SECURITY.md) explains how to report vulnerabilities.
- [Apache License 2.0](LICENSE) and [notices](NOTICE).
