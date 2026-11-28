# Linux backend — runtime proof pending

This crate is empty on non-Linux targets. The workspace toolchain is Rust
1.100.0 (`rust-toolchain.toml`). The whole workspace, this crate included,
builds, passes native Clippy and passes its tests with
`scripts/docker-linux-test.sh` in a pinned linux/arm64 Rust 1.100.0 container
(docs/acceptance.md). Hosted x64 and arm64 CI pass, and the 0.1.0 release
shipped tar.gz archives for both. No hardware capture, credential-store
operation, desktop notification, autostart installation or GUI interaction has
been verified on a real Linux desktop. Compilation and
the synthetic unit tests are not desktop runtime proof.

## Implemented surfaces

- `LinuxSystemAudioCapture`: one 48 kHz mono system track, never mixed with
  the microphone. Only PipeWire playback streams (`media.class` =
  `Stream/Output/Audio`) are recorded, each through its own monitor
  (`stream.capture.sink`, `node.dont-reconnect`); they are summed on the
  delivery worker. Not recorded: BroScribe's own processes and the players it
  starts, the caller's excluded processes, and forwarding outputs
  (`node.link-group` loopback/filter-chain/echo-cancel/combine outputs,
  `node.virtual`, `stream.is-loopback`), whose original application streams
  are recorded instead. Sound that reaches a sink without a PipeWire playback
  stream (for example a client writing straight to an ALSA hardware device)
  is not captured. A playback stream without a process identity is recorded
  unless the caller excluded processes, in which case capture fails rather
  than risk recording an excluded process.
  The default sink's monitor supplies the graph clock only: it stores cycle
  metadata, never PCM. Streams are placed by sample count and their own
  graph ticks; a stream's first packet, or one whose ticks restart, joins at
  its own driver cycle time. A 50 ms graph-clock holdback lets late streams
  publish the same cycle. Audio that would land on frames already delivered
  fails the capture rather than being shifted later or silently dropped;
  audio arriving before the clock's first packet waits for it. Each chunk's
  `host_time` is the driver cycle time (`CLOCK_MONOTONIC`, the clock behind
  Rust's `Instant` on Linux) of its first sample, so holdback, delivery stalls
  and pauses never move audio on the recorder's timeline; output latency is
  not added. Changing exclusions, the default output route, route/format loss,
  overrun and late delivery end the session with a reported failure so the
  recorder can start a fresh segment. Whether a playback process is
  BroScribe's own is read from procfs once while it keeps playing.
- `LinuxMicCapture`: independent CPAL input on the PipeWire host (ALSA when
  the PipeWire host cannot initialize), native PCM formats/rates,
  preallocated timed callback ring and worker-thread delivery. Chunks carry
  CPAL's capture instant mapped onto `Instant` through the stream's own clock
  (`Stream::now`), re-paired on every delivery pass; a gap in the stream
  starts a new chunk at its own time. Device loss and ring overflow end
  capture explicitly.
- `LinuxMicUsageMonitor`: running PipeWire capture nodes linked to microphone
  source nodes, joined to their client metadata for process PID, application ID
  and `application.process.binary`. Sink-monitor capture and this process's own
  inputs are excluded. `LinuxDeviceInfo` supplies CPAL device metadata and rates;
  unavailable manufacturer/transport facts remain unknown rather than guessed.
- `LinuxWindowInspector`: X11 EWMH client list, PID, UTF-8 title and WM_CLASS.
  Wayland explicitly returns `Unsupported`, even when XWayland is available:
  XWayland cannot enumerate native Wayland windows. Detection must degrade to
  microphone usage/application identity.
- `LinuxAppUiReader`: AT-SPI over D-Bus, PID resolution, role mapping, text and
  DOM attributes, cycle detection, bounded node/depth/text/time budgets.
  Callbacks are only invoked by `LinuxAppUiReader::dispatch_events()` on GPUI's
  foreground executor. There is no second UI toolkit or UI loop.
  `AXManualAccessibility` is macOS-only: the getter returns `None`, and its
  setter reports `Unsupported` on Linux.
- `LinuxSecretStore`: keyring 4.2.0's Secret Service store only; no file-backed
  credentials, browser credentials, agent credentials or credential logging.
  `with_service(service)` scopes it to one data folder: `SECRET_SERVICE_BASE`
  (`BroScribe`) for the default one, `BroScribe.profile-<hash>` for another
  (chosen by `NativePlatform::secret_store(data_dir)` from `profile_identity`,
  the same bytes that key the daemon socket).
- `LinuxBrowserLauncher`: validates absolute HTTP(S) URLs before `xdg-open`,
  without a shell.
- `LinuxNotifier`: freedesktop notifications. `notify_actionable` returns
  `Pending` at once; connecting to the session bus, reading capabilities,
  `Notify` and `CloseNotification` all run on a per-prompt I/O thread, never on
  GPUI's loop. The outcome reaches the callback as `__delivered` (the server
  accepted `Notify`, not proof that a desktop displayed it) or `__denied`
  (no bus, no `actions` capability for an actionable prompt, or a refused
  `Notify`), followed by at most one declared action. `withdraw` removes the
  callback immediately; the I/O thread then closes the prompt, including one
  whose `Notify` was still in flight, and a prompt withdrawn while connecting
  is never shown. A replacement with the same id closes its predecessor.
  Listeners end on action/close/withdraw or after 180 s; prompts request
  120 s expiry. Plain `notify` posts an action-less prompt the same way.
- `LinuxStatusItem`: zbus StatusNotifierItem plus DBusMenu, title/review updates
  and menu actions sent through `StatusClickSender` to GPUI. A desktop must have
  a StatusNotifierWatcher and a DBusMenu-capable host (GNOME may need a shell
  extension) when the item is created. The item follows the watcher's
  `NameOwnerChanged` signal and registers again whenever the watcher (panel or
  shell extension) restarts. Menu rendering belongs to that host; direct
  `ContextMenu` and scroll commands return D-Bus `NotSupported` rather than
  start another toolkit's event loop.
- `LinuxAutostart`: mode-0600 XDG desktop entry starting the same systemd user
  unit enabled at login (idempotent, never a duplicate daemon). `LinuxDaemon`:
  install/enable/start/stop/restart/status/remove the user unit, with the
  resolved absolute executable escaped for systemd. Only installation writes
  the unit's executable and folder overrides (`BROSCRIBE_DATA_DIR`,
  `BROSCRIBE_LIBRARY`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`,
  made absolute, as set for the installer). `start` and `restart` keep an
  installed unit byte for byte whatever their caller's environment; they
  create the unit only when none exists. systemd errors propagate, including a
  missing user manager; no root service is installed.
  Its internal helper lookup resolves known siblings, then PATH.
- `decoder_chain()`: Symphonia, then optional ffprobe/ffmpeg. `FfmpegEncoder` and
  `FfplayPlayer` re-export the real portable subprocess implementations. Runtime
  fallbacks require the corresponding ffmpeg/ffprobe/ffplay executables in PATH.
- `LinuxPaths`: XDG data/cache roots and a home-relative default library.
  `LinuxProcessProbe`, `LinuxSystemInfo`, `LinuxResourceUsage`,
  `LinuxProcessPriority`: Linux process/procfs/statvfs/getrusage/nice APIs. Raising
  priority may require `CAP_SYS_NICE`/an appropriate resource limit; refusal is
  returned to the caller, never disguised as successful prioritization.
- `LinuxPeerCredentials`: exact-sized `SO_PEERCRED`, used with the shared Unix
  transport's same-user check. `socket_location(data_dir)` selects a checked
  private `$XDG_RUNTIME_DIR/broscribe` directory, falling back to a checked
  `/run/user/<uid>/broscribe` and only then `/tmp/broscribe-<uid>` for
  absent/invalid/too-long runtime paths.

## Synthetic coverage

Unit tests drive the public seams without PipeWire, D-Bus, systemd or
desktop processes: timeline placement (jittered ticks, skipped clock cycles,
streams joining at tick zero, late first packets inside and beyond the
holdback, packets before the clock, rebased ticks) and `host_time` stamps on
synthetic graph and microphone clocks; notification delivery against a fake
server (pending while connecting, withdrawal before and after `Notify`,
replacement, denial, expiry); the daemon unit against a recording fake
`systemctl` (installed unit preserved by start/restart, private install,
first-start creation, status codes); watcher-restart detection; and the
own-process cache.

## Owner-run runtime checklist (all pending)

1. On a real PipeWire/WirePlumber desktop, record synthetic test audio with
   independent microphone and system tracks. Verify monitor routing, per-app
   exclusions (including helpers), application arrival/departure, silence,
   44.1/48 kHz graphs, mic/system alignment through `host_time`, pause and
   resume boundaries, route changes, capture overrun, muted inputs,
   unplug/replug and failure-driven segment restarts. Confirm microphone
   ownership against known synthetic capture clients. Containers do not prove
   these behaviors or host/session-manager permission policies.
2. On X11 verify background-window titles; on Wayland verify explicit
   unsupported reporting and detection's microphone/app-identity fallback.
3. On GPUI's foreground loop, observe a synthetic accessible application.
   Verify bounds, event delivery without polling, app restart/exit and stopping
   while events are queued.
4. With a user-unlocked Secret Service, create/read/delete a dedicated invented
   test key. Verify locked-store, denied-access and missing-service errors.
5. Verify accepted/denied/unavailable notification services, that the UI
   stays responsive while the bus is slow, visible action buttons, real action
   delivery, replacement/withdraw races, expiry and stale prompt dismissal.
   No notifications were sent during implementation.
6. On KDE and a supported GNOME host verify the status icon, DBusMenu entries,
   separators, disabled items, title/count updates, GPUI action delivery and
   that the icon returns after the panel or shell extension restarts.
7. In a disposable user account, install/restart/stop/uninstall the daemon user
   unit and enable/disable XDG autostart. Verify login startup, GUI-to-daemon
   independence, that a restart from a shell without the installer's folder
   overrides keeps the daemon's data folder, paths containing
   spaces/quotes/percent/dollar characters, missing executables and a missing
   systemd user bus.
8. Verify ffmpeg encoding/track flags, playback/seek/track selection, watcher
   updates, free-space/resource counters and allowed/refused priority changes,
   and the real daemon/UI pair under the same UID.

Dependencies are Linux-target-only. PipeWire/libspa 0.10.1, AT-SPI 0.30.0,
CPAL 0.18.2, x11rb 0.14.0, notify 8.2.2 and keyring 4.2.0 implement the named
OS facilities; zbus is pinned to 5.19.0, the generation shared by AT-SPI and
Secret Service. async-io 2.6.0 provides bounded D-Bus waits, and futures
provides the GPUI/channel bridge. The workspace lock must pass the
repository's publish-age script.
