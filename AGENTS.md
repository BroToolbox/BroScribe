# Working on BroScribe

Read [docs/design.md](docs/design.md) first. It is the source of truth for what BroScribe is, its architecture and its roadmap. This file holds the rules that apply to every change.

## Non-negotiables

- **Rust + GPUI for the whole application.** No Electron, no embedded web views, no second UI toolkit.
- **Licence:** Apache-2.0. Every dependency must be compatible with it; record each model's licence in the model registry.
- **Clean implementation.** Never copy code, prompts, assets or identifiers from proprietary apps. Implement behaviour from the design doc and public documentation.
- **Local-first.** Remote providers are off by default and enabled per provider. Audio and voice embeddings never go to an LLM provider.
- **Privacy in the repo.** Never commit recordings, transcripts, people's names, e-mail addresses, voice embeddings, tokens or anything derived from a real meeting. Tests use synthetic or openly licensed audio and invented names. Files matching `*.private.*` are git-ignored and must stay out of the repo.
- **Credentials.**
  - Secrets live only in the platform `SecretStore`.
  - Never read another application's credential store, including browser cookies and agent CLIs' logins.
  - Never bypass MFA or conditional access.
  - Never log tokens.
- **Sensitive text and child processes.** Transcripts go to child processes on stdin or in a mode-0600 temp file, never in argv.

## Dependencies (supply chain)

- No crate version younger than **3 days**. `.cargo/config.toml` sets `registry.global-min-publish-age = "3 days"`, enforced by the pinned **Rust/Cargo 1.100.0** toolchain (`rust-toolchain.toml`).
- **Every change to `Cargo.lock` must pass `scripts/check-publish-age.py`** before it is committed. The script checks every locked crates.io package against its publish time. When it flags a package, run the `cargo update -p … --precise …` it suggests, then check again. Cargo 1.100 keeps whatever is already locked, so this check stays required after the upgrade.
- Rust 1.100.0 was adopted on **2026-11-16**, after its three-day minimum age. Keep the same `[registry]` age setting in `~/.cargo/config.toml` for Cargo commands outside this repository.
- Before the first build, run `third_party/sherpa-onnx/fetch.sh`: it fetches the native libraries and checks them against `SHA256SUMS` (design §13).
- Keep spikes and experiments inside this repo (`spikes/`, excluded from the workspace) so they inherit its Cargo configuration.
- No git dependencies, and `cargo install` only at pinned versions older than 3 days with `--locked`; the age setting does not cover either. Native archives downloaded by build scripts are pinned by SHA-256 (design §13).
- Commit `Cargo.lock`. Don't loosen or remove the age setting or skip the check to make a resolve succeed; pin the previous release.
- Prefer few, well-maintained crates. State why each new dependency is needed in the commit message.

## Architecture rules

- **Platform access goes through the traits in `broscribe-platform`.** `cfg(target_os)` appears only in the `broscribe-platform-*` crates. Every trait has a fake used in tests.
- **One event-loop owner.** GPUI owns the app's main loop; status item, accessibility observers and notifications run on it. Don't start a second UI loop.
- **Redraw on demand.** Never request frames continuously; repaint only when state changes, and throttle meters.
- **Recording is independent.** Capture runs in `broscribed` and must survive a crash of the UI or of a processing job. Journal audio to disk before any processing.
- **Two tracks.** Never mix microphone and system audio before analysis.
- **Nothing hard-coded.** Models and providers come from the registry and config; adding a model on an existing runtime is a config change.
- **Files are the truth.** The SQLite index must be rebuildable from the library files.
- **Parity.** Anything the UI can do must be reachable from the CLI and, where it makes sense, from MCP. Every CLI command supports `--json`.

## Workflow

- Conventional commit messages (`feat:`, `fix:`, `docs:`, `chore:`…), small and focused.
- Before finishing a change, run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`. The default tests use fakes and small fixtures; tests that need downloaded models are `#[ignore]`d and run explicitly.
- Mark anything unverified in the design doc with *verify*, and remove the mark when you have checked it against a primary source or a test.
- Update `docs/design.md` when a decision changes; don't leave the doc and the code disagreeing.
- **Publication.** The first public push and publication of packages or releases require explicit maintainer authorization. All merges into `main` pass CI. PRs authored by `@raphaelbarreiros` or `@Renan-Brasilio` from a branch in this repository can merge without review. Other PRs require approval of the current head from either maintainer.
- **No importers for specific third-party apps in this repo.** Migration goes through the generic import manifest (design §6.5); converters for other tools live outside the repository.
