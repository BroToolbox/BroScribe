# Installing, upgrading and removing BroScribe

Release packages for macOS, Linux and Windows are on the [Releases page](https://github.com/BroToolbox/BroScribe/releases), starting with 0.1.0 (2026-11-28). These instructions cover those release archives, local macOS builds, and native development builds. Platform acceptance is tracked in [acceptance.md](acceptance.md).

| Platform | How to install today | Status |
|---|---|---|
| macOS 14.2 or later, Apple silicon | [release disk image](#macos-release-archive) (DMG; the ZIP is an alternative) or build `BroScribe.app` from source and sign it locally | supported build; the 0.1.0 disk image and ZIP were signed, notarized and stapled by the release workflow; the clean-account Gatekeeper checks remain *verify* |
| Linux x86-64 or ARM64 (glibc) | [per-user tar.gz install](#linux-release-archive) or build from source | tar.gz published since 0.1.0. Native desktop checks remain *verify* |
| Windows x86-64 | [per-user ZIP install](#windows-release-archive) or build from source | unsigned ZIP published since 0.1.0. Authenticode and native desktop checks remain *verify* |

There is no Intel Mac build: only the `osx-arm64` native libraries are pinned.

## macOS

### macOS release archive

A notarized release offers the same signed, notarized and stapled app two ways: `BroScribe-<version>-<build>-macos-arm64.dmg`, a disk image for first installs, and `BroScribe-<version>-<build>-macos-arm64.zip`, which the in-app updater installs and which also works as a manual download. An ad-hoc preview build has a ZIP only and is not for distribution.

1. Download `BroScribe-<version>-<build>-macos-arm64.dmg`. Optionally check it: download `<dmg>.sha256` into the same folder; `shasum -a 256 <dmg>` must print its first field. With the checkout, and the ZIP and `.provenance.json` from the same release in that folder too, `python3 scripts/verify-macos-artifact.py --dmg <dmg> --compare-zip <zip> --provenance <stem>.provenance.json --codesign --require-stapled` also checks the signature, the stapled tickets and that the disk image holds exactly the ZIP's app; it needs the `.dmg.sha256` beside the DMG and refuses without it.
2. Open the disk image and drag **BroScribe** onto the **Applications** folder beside it.
3. Eject the disk image (Finder's eject button, or drag it to the Trash), then open BroScribe from **Applications**. Do not run BroScribe from the disk image: Gatekeeper may translocate it, and the agent registration and updates refuse a read-only volume or translocated copy.
4. Continue with [Install](#install) from step 2 (open BroScribe and follow the setup assistant), and with [Command line and MCP](#command-line-and-mcp).

The disk image is for first installs. Later versions arrive through the in-app updater, which installs the release ZIP. Do not drag a newer copy over a running installation: replace the app by hand only with BroScribe quit and its agent stopped, as [Manual upgrade](#manual-upgrade) describes.

To use the ZIP instead: download it and its two sidecars (`.sha256`, `.provenance.json`). With the checkout, run `python3 scripts/verify-macos-artifact.py <zip> --provenance <zip-stem>.provenance.json --codesign --require-stapled`, or for a checksum-only check compare `shasum -a 256 <zip>` with the first field in `<zip>.sha256`. Double-click the ZIP (or `ditto -x -k <zip> .`) and move `BroScribe.app` to `/Applications` before opening it, so Gatekeeper does not translocate it. Then continue with step 4 above.

### Prerequisites

- An Apple silicon Mac with macOS 14.2 or later (the minimum the bundle declares; system-audio capture uses the Core Audio process tap introduced in 14.2).
- Xcode or the Xcode Command Line Tools (`xcode-select --install`).
- CMake (builds whisper.cpp).
- Python 3.11 or later (repository scripts).
- [rustup](https://rustup.rs). The repository pins Rust **1.100.0** in `rust-toolchain.toml`; rustup selects it inside the checkout. Keep `registry.global-min-publish-age = "3 days"` in `~/.cargo/config.toml` too if you run Cargo elsewhere (AGENTS.md).
- Optional: `ffmpeg` on `PATH`, a decoding/encoding fallback for formats AVFoundation and symphonia do not handle.

### Build

```sh
third_party/sherpa-onnx/fetch.sh            # pinned native libraries, checked against SHA256SUMS
scripts/bundle-macos.sh                     # → target/BroScribe.app
```

`bundle-macos.sh` builds the app, CLI, agent and worker with `--locked` for `aarch64-apple-darwin` (it refuses `RUSTFLAGS` overrides and any `MACOSX_DEPLOYMENT_TARGET` other than 14.2), writes the bundle with its licence files, third-party notices and build information, keeps debug symbols in a separate archive under `target/dist/symbols/`, and signs it inside out with the hardened runtime. The macOS licence inventory is complete (reviewed texts pinned in the repository); if a dependency change adds a licence without reviewed text, the notices are marked incomplete, which does not affect local use but blocks distribution ([release.md](release.md)).

Signing identity (`BROSCRIBE_SIGN_IDENTITY`):

- unset: ad-hoc signature. Works, but macOS treats every rebuild as a new app, so microphone, system-audio and calendar permissions must be granted again after each build.
- `Apple Development: …`: a stable local identity. Permission grants survive rebuilds as long as the identity stays the same. Local use only; cannot be notarized.
- `Developer ID Application: …`: distribution. See [release.md](release.md).

### Install

1. Copy `target/BroScribe.app` to `/Applications`. Run it from there, not from the build folder, a disk image or Downloads: a quarantined app started from elsewhere may be translocated by Gatekeeper, and the agent is registered from wherever the app is. `broscribe agents install` and `broscribe mcp enable` refuse, before writing anything, when the CLI runs from a translocated copy, a read-only volume or a temporary folder.
2. Open BroScribe. The setup assistant explains each permission before asking for it, installs the recording agent (a login item inside the app, `Contents/Library/LoginItems/BroScribe Agent.app`) and walks through language, models, your voice and an audio check.
3. If macOS reports that the agent needs approval, allow BroScribe in **System Settings → General → Login Items**.
4. Check the result:

   ```sh
   /Applications/BroScribe.app/Contents/MacOS/broscribe doctor
   /Applications/BroScribe.app/Contents/MacOS/broscribe daemon status
   /Applications/BroScribe.app/Contents/MacOS/broscribe app about
   ```

   `doctor` marks each check `ok`, `problem` or `unknown`; an unknown check (for example meeting readers on macOS, whose permission belongs to the app rather than the CLI) is not a pass. `app about` shows the version, build, architecture, signature and licences of the installation and any mismatch between them; it never claims a build is notarized. `broscribe library check` lists library integrity problems without changing anything, and `broscribe library repair --only …` fixes only the scopes you name. For a support request, `broscribe diagnostics` (or About › Copy Diagnostics) prints a counts-only report with no titles, names, text, paths or identifiers; read it before sharing it.

The same steps from a terminal: `broscribe setup`, then `broscribe daemon install`.

Permissions belong to the agent (it records and reads calendars), and macOS shows them under BroScribe. Screen Recording is optional (window titles for detection) and never requested by default.

### Command line and MCP

The CLI is `BroScribe.app/Contents/MacOS/broscribe`.

- `broscribe agents install` links `~/.local/bin/broscribe` to it and writes the BroScribe skill for detected agent CLIs. It never replaces files BroScribe did not write; `broscribe agents status` shows the link, the skills and their owners.
- `broscribe mcp enable` allows MCP access and registers `broscribe mcp` with each detected agent CLI (Claude Code, Codex, Gemini) using the CLI's canonical path inside the app. `broscribe mcp disable` refuses access and removes those registrations; `broscribe mcp status` shows both.
- Registrations hold the app's path. After moving or renaming the app, run `broscribe agents install` and `broscribe mcp enable` again from the new location.

### Credentials

Tokens, OAuth client secrets and provider API keys are stored only in the Keychain, service `org.broscribe.BroScribe` for the default data folder. A data folder chosen with `BROSCRIBE_DATA_DIR` (or the agent's `--data-dir`) is a separate profile with its own service, `org.broscribe.BroScribe.profile-<hash>`, so profiles never see or change each other's secrets. Add and remove them through BroScribe:

- `broscribe providers set-key <id>` reads an API key from stdin (pipe or redirect it; never an argument). `broscribe providers delete-key <id>` removes it.
- `broscribe integrations login|logout <integration>` and `add-account` / `remove-account` manage calendar sign-ins; `logout` and `remove-account` delete the tokens they own. A browser sign-in can be cancelled from another terminal with `broscribe integrations cancel-login <integration> --attempt <id>` (the ID is printed when the sign-in starts) or with Ctrl-C; a cancelled sign-in saves nothing.
- Sign-out is local: the provider keeps the access it granted. `broscribe integrations logout google --revoke-at-google` also asks Google to revoke it; Google then revokes every permission granted to your OAuth project, for every data folder, Mac or tool signed in with it, and may take a while to apply it.
- Logout keeps the Google OAuth client secret. Once signed out, `broscribe integrations setup google --clear-client-secret` removes it (save it again with `--client-secret-stdin` before the next sign-in).

Do not export tokens from the Keychain to move or back them up (for example with `security find-generic-password -w`): a printed token lands in shell history, logs and backups. Sign in again on the new machine instead.

### Where data lives

| What | Default location |
|---|---|
| Library (meetings, people, voiceprints, index, queue) | `~/BroScribe`, chosen in setup |
| Settings, models, recording journals, note templates, benchmark sets and results, integration state, update records | `~/Library/Application Support/BroScribe` |
| Caches (playback and temporary setup recordings) | `~/Library/Caches/BroScribe` |
| Secrets | Keychain, service `org.broscribe.BroScribe` (another data folder: `org.broscribe.BroScribe.profile-<hash>`) |

`BROSCRIBE_DATA_DIR` and `BROSCRIBE_LIBRARY` override the first two; the agent keeps the values it was installed with.

Secrets are scoped to the data folder. Only `BROSCRIBE_DATA_DIR` / `--data-dir` select a profile: an override naming the default folder keeps the default secrets, and moving the default folder itself (through `HOME`, `XDG_DATA_HOME` or `APPDATA`) keeps them too. An install that already used another data folder before this change starts with an empty secret store there: set the provider keys and sign in to integrations again under that profile. Before this change every folder shared the default service, so what such a profile stored earlier stays under it, now visible only to the default profile, and is never deleted automatically.

Recording journals under `journal/` hold audio that has not been saved as a meeting yet. Never delete them while the agent runs or while a stopped recording is still processing.

### Manual upgrade

Never replace the app while a recording is running or a stopped recording is still being saved. `scripts/update-macos.py` enforces this and replaces only the app bundle: the library, journals, queue and settings are never touched.

BroScribe checks GitHub Releases for a signed stable update one minute after startup and every 12 hours. Turn checks off in **Settings → Updates**, or set `enabled = false` in `[updates]`. Administrators can override the update source and public keys through `broscribe update settings set --spec-stdin`. Downloads and installs require confirmation, and BroScribe refuses to install while a recording is active or unsaved. A draft release, a published release without an envelope, and an expired envelope offer no verified update.

The macOS updater checks the Apple team and designated requirement. On Linux and on Windows without a configured Authenticode pin, the Ed25519-signed manifest and archive SHA-256 establish publisher trust. Windows also checks each executable shipped in the release against a configured Authenticode certificate fingerprint. Before replacing any installation, BroScribe checks that every staged file still matches the verified download. If the staged contents changed, run `broscribe update discard` before downloading again. Discard keeps a changed or unreadable owned staging folder beside the installation under a name ending in `.update-kept-…` and reports its path. Uninstall also lists these siblings as kept, so you can inspect them and remove them yourself. If an interrupted download has no staged update, move the unexpected files out, run `broscribe update recover`, then download the update again.

Before offering or installing an update, BroScribe checks the highest verified build in this profile and in discovered profiles with the same feed URL, channel and trusted keys. An unreadable secondary update record is skipped with a warning. Detached update warnings are recorded in `broscribe update status`. On Unix, BroScribe tightens owner-controlled update state to 0700 for folders and 0600 for records when a command changes state. It refuses state owned by another user or writable by a group or anyone else, and reports the path and the `chown` and `chmod` commands to check after verifying the contents.

Self-update replaces the installation folder, so the updater refuses a folder that is, contains or sits inside the meeting library, a profile's data or cache folder, and a folder that is or contains your home folder, including through a symbolic link. BroScribe protects the library selected by an option, the environment and the configuration, including paths shadowed by a higher-priority choice. An installation inside your home folder in its own folder, such as the standard `~/.local/lib/broscribe` on Linux or `%LOCALAPPDATA%\Programs\BroScribe` on Windows, updates normally.

`update install`, `update rollback` and `update recover` select the profile from `BROSCRIBE_DATA_DIR` when nonempty and otherwise use the native default. Both paths must be absolute because BroScribe also checks the native default for other profile state. Every library path given by `--library`, `BROSCRIBE_LIBRARY` or the selected profile's config `library` must be absolute too, including one that another of them overrides, because the updater also protects the overridden library. If BroScribe refuses a relative path, make it absolute where it is set. On Windows, run these commands from outside the installation if they need to replace it. If BroScribe refuses because of your current folder, change to a folder outside the installation and run the same command again without changing `BROSCRIBE_DATA_DIR`. If the runner still reports that the installation is in use, close other programs or change the current folder in any terminal using it, then follow `broscribe update status`.

Keep personal files outside the installation. Before replacing or removing an installation, BroScribe verifies the sealed macOS bundle or the Linux/Windows release layout. Windows also allows the three standalone FFmpeg tools documented below. These checks cover rollback and removal of the previous copy at update confirmation. Move any unexpected files out of the installation before retrying. If BroScribe reports that the installation was changed outside BroScribe, finish all recordings and jobs, quit the app and agent, delete the old installation folder, and install the current release in a clean folder. Keep the data folder and library. On macOS and Linux, an update that requires restarting the agent also checks which library its service will open. If it differs from the running agent's library, restart the agent so it opens the named library, then retry the update.

1. Build and sign the new bundle with the **same** signing identity as the installed one, and a higher build number. The script requires a real signer (Apple Development or Developer ID, not ad-hoc) whose designated requirements match the installed app's and agent's: macOS keeps permission grants only for an unchanged requirement. An ad-hoc install cannot be updated this way; reinstall it and grant permissions again.
2. Let current work finish: no active recording (`broscribe record status`), no stopped recording still being saved, and no job running (`broscribe queue status`). Queued jobs may stay queued.
3. Quit the BroScribe app (menu **Quit**). Closing the window is not enough.
4. Preview, then run the update:

   ```sh
   scripts/update-macos.py --new target/BroScribe.app --dry-run
   scripts/update-macos.py --new target/BroScribe.app
   ```

   The script checks the bundle identifiers, build information, signature, designated requirements and build number. It refuses to continue while the app is open, while the agent has active or unsaved work, or when a registered or running agent cannot prove the installed build. It runs `broscribe doctor` for a baseline and reads the profile's data directory from the CLI. `--dry-run` stops there. `--installed <path>` selects an app outside `/Applications`. `--backup-dir` overrides the default backup folder at `~/Library/Application Support/BroScribe/app-backups/`. If the backup folder already exists, the script creates a private `BroScribe-backup-<stamp>` folder inside it and preserves the parent’s permissions.

   For a running agent the script takes durable **install admission**, then calls `daemon uninstall --admission-fence` with that fence. Both bundles must ship a CLI that supports this guarded uninstall. The replacement also needs `--admission-fence-if-idle` for rollback if its agent fails to start. The CLI verifies the login job under the per-user registration lock before changing it. The script waits for the agent and its workers to exit. Even with no registered agent, the script checks both the IPC endpoint and the profile's instance claim. It holds the profile startup lock from the final absence check through the swap and releases it immediately before starting the replacement agent. A previously unregistered agent stays unregistered.

   A newly started agent must prove its version, build number and target and show that it still holds the admission. The script reruns `broscribe doctor` and releases the admission only after both checks pass. If the replacement login item is registered but its agent never starts, the rollback CLI checks the durable admission on disk, proves the agent is absent and verifies the idle registration belongs to the selected profile. It then unregisters the login item. The script holds the startup lock through the bundle restore and asks any rejected build's menu-bar app to exit first. If the app stays open, rollback reports a failure and preserves the recovery bundle and any install admission. When the previous agent is restarted, it must prove its build before the admission is released. Real signed upgrades and rollback remain *verify*.

5. Open BroScribe again. If the app moved, refresh the CLI link and MCP registrations (above).
6. Restart agent-CLI sessions that were running `broscribe mcp`, and any other long-running `broscribe` command: they are still the old binary. When the IPC protocol version changed between the two builds the new agent refuses them (`unsupported_version`) rather than run their commands.

Use the update procedure above for a bundle replacement. For an ordinary stop, `broscribe daemon stop` waits for the idle agent for the selected data folder to exit and blocks startup for that profile during the stop. It waits for launchd to settle if a restart is in progress. It unloads an idle login job configured for the selected profile, or a running job whose PID still matches the agent holding the stop admission. An idle job for another profile stays loaded, as does a running job configured for another profile when its PID cannot be matched to the selected endpoint. If launchd has no GUI domain or service, there is no login job to unload. An unloaded job stays unloaded until `broscribe daemon start` or the next login.

On macOS and Linux, the final profile check and unload share a per-user lock with BroScribe's login registration changes, including starts and autostart changes from other profiles. macOS uses `org.broscribe.login-registration.lock` in the system's per-user directory reported by `confstr(_CS_DARWIN_USER_DIR)`. Linux uses `/run/user/<uid>/broscribe-login-registration.lock` when the runtime directory exists. If that directory is absent, Linux uses `/tmp/broscribe-<uid>/login-registration.lock` in a mode-0700 folder. The system directories must be real directories owned by the user with no group or other write access. BroScribe refuses an invalid system directory. The lock must be a regular file owned by the user, and BroScribe tightens group or other permissions to mode 0600.

If the lock stays held for 15 seconds, the operation reports `busy`. Each `launchctl` or `systemctl` call has a 10-second deadline. While waiting for the login job to settle, a stop only starts another manager query if there is time for its full deadline. On Linux, the subsequent D-Bus profile lookup shares the same settle deadline. A result that arrives after that deadline cannot authorize unloading the job. If the job remains unverified, the stop reports `login_job_unverified` and says whether the selected agent already exited. A timed-out unload reports an error. Retrying checks the job afresh. On Linux, a `daemon status` query that times out reports an error.

On macOS, a data-folder socket path that is too long uses the `broscribe` subfolder of the same system per-user directory. On Linux, sockets use `$XDG_RUNTIME_DIR/broscribe` when the runtime directory is valid and the socket path fits, then `/run/user/<uid>/broscribe`. The `/tmp/broscribe-<uid>` fallback applies only when `/run/user/<uid>` is absent. If the system runtime directory exists and fails validation, BroScribe reports the error. The app's single-instance socket shares the agent's socket directory. If the `/tmp` fallback reports that the directory belongs to another user or is invalid, ask an administrator to inspect the conflicting entry before retrying.

If the login agent is running for another profile while an agent also answers for the selected one, the stop is refused with `login_job_unverified` and nothing is stopped; stop the login agent's profile first. The stop also reports `login_job_unverified` if it cannot establish which profile an idle job serves or if the login manager does not settle in time. On macOS, this includes a LaunchAgent configured to run the daemon through a wrapper. It reports `agent_unreachable` if the profile's instance lock stays held without a reachable agent endpoint. If a restarted agent reports `busy`, stop checks its current readiness. It retries admission while the agent is still recovering and returns the agent's own `busy` message once it has finished startup. If startup recovery times out, wait for recovery or saving to finish, then run `broscribe daemon stop` again. MCP `stop_daemon` follows the same rules.

On all three platforms, `broscribe daemon uninstall` refuses with `login_job_unverified` when the login registration serves another profile or cannot be verified. This includes Windows Run entries and installed macOS jobs that launchd has unloaded. With `--admission-fence`, the agent must answer and prove that it still holds that durable install admission.

If an agent holds the profile's instance claim without a reachable endpoint, `daemon status --json` reports `running: true`, `observation: "unreachable"` and unassessed inventory. An offline `library switch` holds the profile's startup lock through the config write. It returns `agent_unreachable` if the instance claim is held, or `busy` if the startup lock stays held through its timeout.

### Uninstall

Open **Settings → Privacy → Uninstall BroScribe…**, or quit the app and preview the command from a terminal.

```sh
broscribe uninstall --dry-run
broscribe uninstall --yes
```

Add `--json` for a single report of the plan or completed steps. A bare `broscribe uninstall` also previews. Read the plan with the same options you will use to apply it. A ready preview exits `0`, a blocked preview exits `2`, and an apply with a failed step exits `1`. The JSON report stays on stdout and lists the outcome of each step. The app is kept if cleanup fails so you can retry. Recording audio left in the journal without its manifest blocks only `--remove-data`: the message names the folder, which you can move away or remove with `--remove-data --discard-unsaved`; a plain uninstall that keeps your data proceeds. A journal folder that is a link cannot be verified, so uninstall refuses until it is a regular folder. On Windows, both a CLI and a Settings uninstall record where their after-exit helper writes its receipt (`%APPDATA%\BroScribe.uninstall-helper.json`), and the report lists that reference as pending cleanup. On success, the helper removes the reference if it still points to its own receipt.

The uninstaller saves a private list of discovered secondary profiles in the default data folder before it stops their agents or removes their registrations. It records each directory identity, BroScribe profile marker, credential namespace and known library paths. BroScribe writes the profile marker when it first opens the profile. A discovered folder without a valid marker, or a retry whose directory or marker changed, is eligible for credential cleanup only. Include a legacy profile explicitly with `--data-dir PATH` to inspect it for removal.

On Windows, a Settings uninstall run from a secondary profile schedules cleanup of that profile after the app exits. The helper then removes BroScribe-created files from the default data folder if you requested `--remove-data`, or clears its pending list if you kept the data. It also removes an empty default folder created solely to hold the list. It removes the app after profile cleanup, then removes update state if staging and app removal both succeeded. The CLI reports these steps and app removal as pending. The receipt records the helper’s status. A sibling `.details` file lists removals, kept entries and failures. When there is more than 64 KiB of detail, it reports how many lines were omitted. If the helper fails to launch or times out, retry after the app exits. On retry, uninstall reads the finished or stale receipt and removes the old helper files after preflight. It reports any unexpected entries it kept. If the recorded helper folder is gone, the preview reports the obsolete reference and applying the plan clears it. If the folder still exists outside the current temporary folder or the reference is unreadable, check that the helper has finished, then delete the reference file named in the report and retry.

On Linux, run uninstall from your desktop session so BroScribe can remove its Secret Service credentials. After the Linux agent stops, uninstall removes its regular endpoint lock and the regular tray icon files, then removes empty runtime directories. If this account never stored credentials, you can preview and run uninstall with `--skip-credentials`. This skips integration sign-out and credential removal. Any stored credentials remain. The archive wrapper calls the regular executable at `~/.local/lib/broscribe/broscribe`. If uninstall removed the launchers but kept the installation, run the retry command from the report. The command pins the selected profile paths, the primary library path even when keeping data, and the Linux home and XDG data and cache roots. It previews first. Add `--yes` after checking the plan. If a selected profile could not be inspected or its library could not be pinned, the report gives the reason and omits the command.

The uninstaller protects the default, configured, environment and active libraries, including libraries reported by running agents or named in systemd units, launchd jobs and Windows Run entries. A temporary override still protects the library it shadows. If a folder BroScribe would remove recursively contains a library, uninstall refuses to remove it. Move the library out before retrying. Use `--remove-library` to remove BroScribe files from the explicitly selected library. Every uninstall preview and apply requires an absolute native default data folder, plus absolute paths for all `--data-dir` options and for `BROSCRIBE_DATA_DIR` if nonempty. With `--remove-data` or `--remove-library`, `--library`, `BROSCRIBE_LIBRARY` and profile config `library` paths used by the plan must also be absolute. This includes paths protected by the plan and the default library when it is the effective selection. Fix the source named in the refusal and run a fresh `--dry-run` before applying. Move a portable installation out of any protected location before uninstalling it. The uninstaller skips update staging it cannot verify as belonging to a discovered profile and leaves the updater’s sibling `.<installation-name>.update.lock` file in place. It is a shared coordination file. A clean reinstall can reuse the same path.

- The default removes the app, the agent and its login registration, caches, runtime and update files, credential-store entries, and AI tool registrations, skills and the BroScribe CLI link.
- `--remove-data` permanently deletes BroScribe-created files from verified profiles, including model downloads, note templates and benchmark sets that can contain transcript excerpts. Data, cache and runtime folders keep unrecognized entries and list their paths. A folder is removed when empty. If update state is kept for recovery, the report lists the retained update folder, configuration, profile marker and pending uninstall list that exist, and marks data removal as partial.
- `--remove-library` removes BroScribe files from the selected profile’s library, including recognized meetings, people and voiceprints. BroScribe checks for its index, a meeting identity or an empty library skeleton. Inside a meeting folder, removal requires a readable BroScribe `meeting.json` or BroScribe front matter in `meeting.md`. The recording file is eligible only when the meeting JSON names it as local media. When only Markdown identifies the meeting, media files are kept and listed. The uninstaller keeps and lists unknown entries and their parent folders, and removes directories only when empty. The Settings dialog offers both choices unchecked. Include each additional library's profile with `--data-dir PATH`. If discovered profiles share a library, explicitly select every profile that uses it before deleting it.
- Uninstall preview leaves existing files and permissions alone. Local integration sign-out resolves the profile directory first. Inside the resolved profile it requires regular directories, a regular state file and a regular lock file. It removes cached photos with BroScribe person IDs from the photo root and canonical numbered generation folders.
- Finish active recordings and processing before uninstalling. Offline journals containing unsaved or processing recordings block removal. Surviving microphone or system chunks count as unsaved audio even when their session manifest is missing. `--remove-data --discard-unsaved` explicitly authorizes permanent deletion of those offline journals.
- BroScribe checks the default and current profile and discovers profiles recorded in login registrations and Windows Start-menu shortcuts. Supply any other profile with repeated `--data-dir PATH` options. BroScribe has no complete list of profiles. Supply every profile that uses a library before requesting its deletion. `--all-profiles-secrets` also removes credentials in undiscovered BroScribe profile namespaces.
- `--revoke-at-google` asks Google to revoke access across every client of the same OAuth project. Without that option BroScribe forgets its local credentials. Review Google grants at [Google Account permissions](https://myaccount.google.com/permissions).
- `--keep-app` retains the binaries for development. macOS otherwise moves the verified app bundle to the Trash. Review its permission entries in **System Settings → Privacy & Security** and its **General → Login Items & Extensions** entry manually.

## Linux release archive

The x86-64 and ARM64 release archives are built on Ubuntu 24.04 and require glibc 2.39 or newer. The app uses GPUI's X11 and Wayland backends, system fonts, and the Vulkan loader. BroScribe's Linux capture backend uses PipeWire, and CPAL also links ALSA.

| Runtime component | Debian / Ubuntu packages | Fedora packages |
|---|---|---|
| Audio capture | `pipewire`, `libpipewire-0.3-0t64`, `libasound2t64` | `pipewire`, `pipewire-libs`, `alsa-lib` |
| Keyboard handling | `libxkbcommon0`, `libxkbcommon-x11-0` | `libxkbcommon`, `libxkbcommon-x11` |
| X11 and XCB | `libx11-6`, `libx11-xcb1`, `libxcb1` | `libX11`, `libxcb` |
| Wayland client and cursor | `libwayland-client0`, `libwayland-cursor0` | `libwayland-client`, `libwayland-cursor` |
| System fonts | `libfontconfig1`, `libfreetype6` | `fontconfig`, `freetype` |
| Graphics loaders | `libvulkan1`, `libegl1` | `vulkan-loader`, `libglvnd-egl` |

Use a running PipeWire session and a working graphics driver for your desktop. Most full desktop installations already provide these libraries. On Debian or Ubuntu releases that use the older package names, `libpipewire-0.3-0` and `libasound2` provide the corresponding libraries. The glibc requirement still applies.

1. Choose `BroScribe-<version>-<build>-linux-<arch>.tar.gz` for `uname -m` (`x86_64` or `aarch64`). Keep the matching `.sha256` and `.provenance.json` beside it. With the repository checkout, run `python3 scripts/verify-desktop-artifact.py <archive>`. To check just the checksum without the checkout, run `sha256sum -c <archive>.sha256` next to the archive.
2. Install FFmpeg through your distribution. On Debian or Ubuntu use `sudo apt install ffmpeg`. Check that `ffmpeg`, `ffplay` and `ffprobe` are on the desktop session's `PATH`. BroScribe uses `ffmpeg` to encode compressed recordings and `ffplay` with `ffprobe` for playback and track inspection.
3. Extract and install as your desktop user. Choose a directory where the archive will create a `BroScribe/` folder separate from `~/BroScribe`, the default meeting library. Run the commands from that directory and use the path to the archive you downloaded.

   ```sh
   tar -xzf BroScribe-1.2.3-42-linux-x86_64.tar.gz
   ./BroScribe/install.sh
   ```

   The installer places the application, agent, CLI, worker and update runner together in `~/.local/lib/broscribe`. It writes a desktop launcher with an absolute executable path into `${XDG_DATA_HOME:-$HOME/.local/share}/applications` and adds icon symlinks under the same XDG data directory. It also links `~/.local/bin/broscribe` to the installed CLI. Open BroScribe from the applications menu and complete setup. Run `~/.local/lib/broscribe/broscribe doctor` and `~/.local/lib/broscribe/broscribe daemon status` to check it. The extracted folder is also relocatable. Move the entire folder together and choose a durable location before launching it if you plan to register the agent.

A portable installation updates itself when it is in its own folder, apart from the library, data and cache. If you extracted the archive with a group-writable umask, run `chmod -R go-w <folder>` on the extracted BroScribe folder before updating.

### Remove the Linux installation

From the extracted archive, run `./BroScribe/uninstall.sh --dry-run`, then `./BroScribe/uninstall.sh --yes` with the options you chose in [Uninstall](#uninstall). The script forwards the options to the installed CLI; `./BroScribe/install.sh --uninstall …` does the same. For a portable installation, run `./broscribe uninstall` from that installation. BroScribe removes the generated launcher (`applications/org.broscribe.BroScribe.desktop`), the icon links and `~/.local/bin/broscribe` when they point into the installation being removed.

Use the built-in updater for Linux upgrades from a session with a systemd user manager. Without a user manager, the updater refuses an in-place install, rollback or recovery that would require it to restart BroScribe Agent. Use a clean reinstall for an upgrade on that system.

For a clean reinstall of the standard per-user installation:

1. Finish recordings and jobs, quit the app, and run `./BroScribe/uninstall.sh --yes` from the extracted release. The default options keep your data and meeting library.
2. If uninstall reports that it kept an install folder with changed or unexpected files, it removes the BroScribe-owned desktop, CLI and icon launchers when that is the only failed check. Resolve any other failed checks and rerun uninstall first. If login startup is enabled, run `"<kept-installation>/broscribe" daemon uninstall` for the registered profile. If you launched BroScribe Agent directly, stop it with `"<kept-installation>/broscribe" daemon stop`. Move any files you want to keep out of the install folder. Delete the kept install folder at the path in the report.
3. Extract the new release for your architecture and run its `./BroScribe/install.sh`. If you run the agent directly on a system without a user manager, start it from the new installation the same way. After a full uninstall, sign back in to integrations, set up credentials again, and re-register MCP and agent tools.

On Linux, `broscribe daemon stop` applies the same profile and stop-admission checks to the systemd user unit, using its `MainPID` and configuration. If systemd has dropped an inactive unit from memory, it reads the definition from disk to verify the profile. When `systemctl` or the user manager is absent, the command can still stop an idle agent launched directly through the selected profile's endpoint. Success also requires the endpoint and profile instance lock to clear.

## Windows release archive

1. Download `BroScribe-<version>-<build>-windows-x64.zip` and its two sidecars. With the checkout, run `python scripts/verify-desktop-artifact.py <archive>`. For a checksum-only check, run `Get-FileHash -Algorithm SHA256 <archive>` in PowerShell and compare its hash with the first field in `<archive>.sha256`.
2. Install a Windows x64 FFmpeg build that includes `ffmpeg.exe`, `ffplay.exe` and `ffprobe.exe` from the [FFmpeg download page](https://ffmpeg.org/download.html#build-windows). Extract it and add its `bin` folder to your user `Path`, then start a fresh session to pick up the change. BroScribe also looks for the tools next to its executables. Self-update carries these three regular files forward regardless of filename case. Keep shared FFmpeg builds, including their DLLs, in a separate folder on your `Path`. `ffmpeg.exe` encodes compressed recordings. `ffplay.exe` and `ffprobe.exe` handle playback and track inspection.
3. Extract the ZIP into `%LOCALAPPDATA%\Programs` using **Extract All**, so `broscribe-app.exe` and its sibling executables are together in `%LOCALAPPDATA%\Programs\BroScribe`. Run `broscribe-app.exe` and complete setup. The app creates the Start-menu shortcut when it sets up notifications. SmartScreen may warn on the unsigned Release 1 ZIP. Keep the executable folder in the same location after setup so the shortcuts and logon registration keep working.
4. In PowerShell, run `& "$env:LOCALAPPDATA\Programs\BroScribe\broscribe.exe" doctor` and repeat with `daemon status`. The ZIP installation provides no automatic `Path` entry.

### Remove the Windows installation

Use Settings or run the installed `broscribe.exe uninstall` as described in [Uninstall](#uninstall). If the terminal is inside the installation, uninstall refuses to remove the app and shows the resolved absolute paths of any data folders and libraries selected for removal. Change to a folder outside the installation, run `uninstall --dry-run` again and check the plan before applying with `--yes`.

On Windows, BroScribe removes the profile shortcuts, notification identities and jump lists. The background helper waits up to 15 minutes for BroScribe processes to exit before removing the install folder. When you uninstall from Settings, the helper also removes the active profile's runtime files, or BroScribe-created data if you chose to remove it. If a process still holds the update runner folder open, the helper retries that removal after exit. The completion report shows the path of the background result file. Close any other CLI or MCP processes using that installation so removal can finish. Change the current folder in any other terminals using the installation as well.

To upgrade manually, finish recordings and jobs, disable logon startup, stop the agent and close the app and CLI sessions. Delete the old extracted folder first. Extract the current release into a clean folder and open the new app to register it again. Keep the data folder and library. Restart MCP sessions and refresh registrations if the executable path changed.

On Windows, `broscribe.exe daemon stop` stops the selected profile for the current session when idle and verifies that the profile's named pipe claim has cleared. The Run entry stays registered for the next logon.

## Linux and Windows (development builds)

Both platforms have native implementations of BroScribe's platform traits. An isolated Windows debug snapshot ran GPUI, synthetic transcription/search/MCP and authenticated agent IPC checks; this is not capture, accessibility-reader, credential-store, notification, tray or login acceptance. Linux has no real-desktop runtime proof. Platform-level evidence is recorded in [acceptance.md](acceptance.md). The release archive instructions above cover both platforms.

- **Linux:** `scripts/docker-linux-test.sh` builds and tests the workspace in the digest-pinned Rust 1.100.0 container (arm64), installing PipeWire, ALSA, GPUI and CMake build dependencies and rejecting any Metal feature. Native builds use `third_party/sherpa-onnx/fetch.sh --target linux-x64|linux-aarch64` and `scripts/build-target.sh <triple> build --locked`. `broscribe daemon install` writes a systemd user unit and an XDG autostart entry; `broscribe daemon uninstall` removes them. Secrets go to the Secret Service.
- **Windows:** use `x86_64-pc-windows-msvc` with Visual Studio's C++ tools and Windows SDK, CMake, and a build-only libclang DLL (set `LIBCLANG_PATH` to its directory when it is not discoverable). From Git Bash, run `third_party/sherpa-onnx/fetch.sh --target win-x64`, then `scripts/build-target.sh x86_64-pc-windows-msvc build --locked --workspace --all-targets`. Use a fresh, short target directory, for example `export CARGO_TARGET_DIR="$HOME/bs-msvc-clean"`; deeply nested MSBuild tracking paths can exceed Windows path limits. Git Bash's own `/usr/bin/link.exe` shadows the MSVC linker, so put the MSVC `bin` folder (the one holding `cl.exe`) first in `PATH` after loading the Developer Command Prompt environment. For the live-timing test fixture, you can also set `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER` to the full path of the MSVC `link.exe` or `BROSCRIBE_MSVC_BIN` to its directory. With neither variable set, the fixture looks for `link.exe` next to the first `cl.exe` on `PATH`. Without an installed LLVM, the `libclang.dll` from the PyPI `libclang` wheel works as the build-only DLL; whisper-rs-sys's bundled bindings (`WHISPER_DONT_GENERATE_BINDINGS`) do not, because they were generated against glibc and their layout checks fail on MSVC.

  Always use the wrapper for Windows builds. The pinned sherpa libraries use `/MT`; target Cargo flags align Rust and cc, while the wrapper selects the static release CRT for every whisper.cpp CMake configuration (including debug). Bare `cargo build` does not apply that CMake policy. `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS` and `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS` replace target-config flags, so the wrapper requires them to be unset, including empty overrides.

  When migrating an existing target directory or mixing bare Cargo and wrapper builds, stop other Cargo builds and use a new target directory, or remove its `x86_64-pc-windows-msvc/{debug,release}/build/whisper-rs-sys*` cache directories before rebuilding. `cargo clean -p` does not reliably remove Cargo 1.100's `build/<package>/<hash>` layout. The wrapper never deletes caches before Cargo acquires its build lock.

  The GNU target can only type-check. A native MSVC debug checkpoint linked all five production executables and all targets. The release build and installed-desktop workflow remain *verify*. Static CRT/STL redistribution rights are separate from a successful link. By design, `broscribe daemon install` registers a per-user Run key for `broscribed.exe`, and secrets go to Credential Manager; neither registration nor real credentials were exercised by the isolated checks. The desktop agent starts at logon from that Run key or from the app; `broscribe daemon start` run over SSH or in CI starts an agent that lives only as long as that session, by design (design §3.1). The `~/.local/bin/broscribe` link of `broscribe agents install` is Unix-only: on Windows `agents status` reports it as not available, `install` skips it and nothing adds BroScribe to `PATH`, so agents call `broscribe.exe` by the full path the skill records (without the `\\?\` prefix when the plain `C:\…` path names the same file).

The data locations follow XDG directories on Linux and `%APPDATA%` / `%LOCALAPPDATA%` on Windows. Secrets live in Secret Service on Linux (service `BroScribe`; another data folder: `BroScribe.profile-<hash>`) and in Credential Manager on Windows (targets ending `.org.broscribe.BroScribe`; another data folder: `.org.broscribe.BroScribe.profile-<hash>`), scoped to the data folder as on macOS. The uninstaller uses these namespaces to scope credential removal to BroScribe.
