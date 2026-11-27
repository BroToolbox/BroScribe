# Windows backend — runtime acceptance pending

This crate is empty off Windows. All native dependencies are target-gated.
Implementation is not evidence of a successful Windows runtime test. The
orchestrator observed `cargo check -p broscribe-platform-windows -p
broscribe-platform-native --target x86_64-pc-windows-gnu` pass on macOS with
mingw-w64 (compiler warnings remain for SDK constant casing and tray cleanup).
The macOS check of both OS crates also passed. No Windows runtime or native
MSVC link test has been run.

## Toolchain and ABI

`x86_64-pc-windows-gnu` with mingw-w64 can check these Win32/COM/WinRT bindings.
It is **not** ABI-compatible with the separately downloaded sherpa-onnx
`win-x64` MSVC `.lib` binaries. Checking this crate does not demonstrate that
CLI, daemon, worker, or GPUI application linked or ran. Production native
inference needs an MSVC toolchain with the matching MSVC sherpa archive, or
an independently built GNU-compatible sherpa library. No inference library
is linked by this crate itself. Verify native GPUI/toolchain packaging on an
actual Windows installation separately.

## Integration surface

- `WinPaths::new`, `WinProcessProbe`, `WinSystemInfo`, `WinResourceUsage`,
  `WinProcessPriority`, and `WinDeviceInfo` implement their shared traits.
- `WinMicCapture`, `WinSystemAudioCapture::default`, and
  `WinMicUsageMonitor` implement capture/session enumeration. WASAPI process
  loopback requires Windows build 20348 or later. It captures all system
  output except one helper process tree, rooted at the current process by
  default. `set_excluded_pids` accepts only PIDs in one actual helper tree
  containing the current process; unrelated trees are rejected, not silently
  recorded or excluded by a broad common ancestor. Exclusion changes apply
  at the next capture start. Launch playback/worker helpers beneath that root.
  Mic delivery uses preallocated timestamped SPSC blocks; user sinks run only
  on the capture worker, never CPAL's audio callback. An overrun or data
  discontinuity terminates the segment with an observable failure.
- `WinWindowInspector` uses `EnumWindows`; process identity is the executable
  path. `WinAppUiReader` uses UI Automation, maps roles to the shared AX
  vocabulary, caps depth/nodes and text fields (16,384 characters), and bounds
  individual provider connections/transactions to one second. It cannot read
  secure desktops or higher-integrity providers without Windows permitting
  access. Windows has no macOS `AXManualAccessibility` attribute: query returns
  `None`, setting it returns an explicit unsupported error. Chromium's Windows
  accessibility is obtained through UIA, not a fabricated property.
- Construct observers and `WinStatusItem::new(clicks)` on GPUI's main thread.
  UIA COM event handlers post only change identifiers to message-only HWNDs;
  callbacks execute in that thread's Win32 dispatch. There is no second UI
  loop: GPUI dispatches all native windows on the same thread. The status
  item owns a hidden top-level window, receives Explorer's `TaskbarCreated`
  broadcast and re-adds its icon itself. The
  popup menu shows a snapshot of the items: setters may run re-entrantly
  during its modal loop without aliasing, and nested popups are refused.
- `WinSecretStore::with_service(service)` uses only one Credential Manager
  service of this application: `SECRET_SERVICE_BASE`
  (`org.broscribe.BroScribe`) for the default data folder, or
  `org.broscribe.BroScribe.profile-<hash>` for another (chosen by
  `NativePlatform::secret_store(data_dir)` from `profile_identity`, the same
  folded identity as the pipe name). Manifest, chunk and pending entries all
  use that service. Changes to one key are serialized across processes by a
  per-key named mutex `Local\<service>.secret.<hash>` (named by a hash, never
  the key; the default profile keeps the earlier name); chunk generations a change
  may leave behind are recorded first and removed by the next change of that
  key unless the manifest names them — never deleted by prefix guessing.
  `WinBrowserLauncher` validates absolute HTTP(S) before ShellExecute.
- `WinAutostart::new(executable)` controls a per-user Run-key entry, never a
  Windows service. `install`, `uninstall`, `start`, `stop`, `restart`, and
  `is_running` support lifecycle; stop uses authenticated `daemon.stop` IPC
  and waits for orderly exit, never forcibly kills a recording. The daemon
  path is canonicalized (shortcuts/symlinks never register themselves);
  network shares and the temporary folder are refused before registration
  instead of moving the installation. `BROSCRIBE_DATA_DIR` /
  `BROSCRIBE_LIBRARY` set at registration are kept as `--data-dir` /
  `--library` arguments, since the Run key carries no environment. Only
  packaged `.exe` siblings are resolved, not arbitrary PATH programs.
- `WinProcessTreeControl` contains helper children in a kill-on-close Job
  Object: the child starts suspended, joins the job, then resumes, so every
  descendant ends on exit, timeout or cancellation.
- `WinNotifier` creates a per-user Start-menu shortcut targeting packaged
  `broscribe-app.exe` with AppUserModelID `org.broscribe.BroScribe`, then uses
  WinRT toasts. Actionable toasts return stable requested action IDs and
  report asynchronous failure as `__denied`. Disabled notifications return
  `Denied`; successful Show means Windows accepted delivery, not proof the
  user saw a banner. In-process action callbacks require the app to remain
  running; cold-start toast activation is not a replacement for a daemon IPC
  action router. No shortcut is registered for another application's binary.
  Actionable IDs must contain 1–64 UTF-16 units and there may be at most five
  actions (Windows limits). `withdraw(id)` fences callbacks, hides active
  banners and removes the matching grouped tag from toast history, including
  after a process restart. Banner timeouts retain in-process callbacks for
  subsequent Action Center activation until answered/withdrawn.
- `decoder_chain()` is Media Foundation → symphonia → ffmpeg. MF enumerates
  audio tracks and decodes selected tracks to mono float PCM. Container
  creation time and optional per-track language/title are not populated by
  MF. ffmpeg/ffprobe/ffplay must be installed/packaged for the corresponding
  portable fallback. `FfmpegEncoder` and `FfplayPlayer` are re-exported, with
  `WinAudioEncoder` / `WinAudioPlayer` type names as well.
- `WinLibraryWatcher` uses notify's ReadDirectoryChangesW backend and shared
  library filtering/debouncing; lost events request a full rescan.
- `WinPipeStream::connect(data_dir, deadline)` authenticates the server's
  process-token SID **before sending any bytes**; `read_within` /
  `write_all_within` honour caller deadlines. `PipeSecurity`,
  `verify_pipe_client`, `pipe_name` and `is_resource_exhaustion` back the
  async listener in `broscribe_platform_native::ipc`, which owns protocol
  framing, request reconciliation and shutdown. The endpoint is
  `\\.\pipe\broscribed-<current-user-SID>-<data-folder-key>`, with protected
  current-user-only DACL, remote clients rejected, and FIRST_PIPE_INSTANCE
  acquired atomically and kept for the listener's lifetime.

## Owner-run acceptance checklist (all pending)

1. On build 20348+ capture a synthetic stereo tone from an unrelated app,
   verify mono timing/rate, then play a different tone from a BroScribe helper
   and prove its entire child tree is excluded. Change the exclusion list,
   restart the segment, and repeat. Exercise unplug/reconnect/default-route
   changes, silence, sample discontinuity, and sink backpressure.
2. Enumerate multiple USB/Bluetooth/built-in inputs and outputs; compare
   default IDs, reported transport/rates/channels to Windows settings. Record
   synthetic mic input with a deliberately slow sink and verify overrun is
   reported, with no user sink on the audio callback and no false timestamps.
3. Start/stop a mic-using application and verify active input sessions across
   all endpoints are enumerated and removed. Compare executable identities
   for browser helpers with the detection rules.
4. With synthetic app text only, verify background EnumWindows titles and
   bounded UIA snapshots in Teams/Zoom/Chromium. Exercise window creation,
   property/structure/live-region/focus events, observer drop, denied/elevated
   access and hung providers. Assert callbacks run on GPUI's main thread and
   that GPUI dispatches the message-only HWNDs without an extra loop.
5. Exercise tray menus, disabled items, review count, keyboard selection,
   Explorer restart forwarding, icon cleanup, and application close while
   capture continues in the daemon.
6. Verify toast shortcut target/AUMID, visible delivery, all requested action
   IDs, notification-settings denial, late failure, dismissal and repeated
   notifications. Confirm in-process callbacks while the app is backgrounded;
   test OS cold activation separately (do not claim it delivers a callback to
   a terminated process).
7. Round-trip a synthetic secret, verify absent/delete handling in Credential
   Manager, and verify invalid/custom/file URLs never reach ShellExecute.
8. Enable/disable logon startup as an ordinary user, sign out/in, then use
   start/status/stop/restart/uninstall during a synthetic recording. Verify
   orderly journal completion and no elevation/service registration.
9. Probe/decode WAV, compressed audio, video and multi-audio containers;
   compare per-track channels/rates, mono samples, empty/end-of-stream and
   sink-error behavior. Exercise MF codec absence → symphonia → ffmpeg,
   ffmpeg encoding, and ffplay pause/seek/position/volume/end-of-playback.
10. Verify library edits/renames/deletes/rescans and watcher stop semantics;
    compare memory/CPU/disk and priority readings against Windows tools.
11. IPC: prove a pre-created endpoint defeats startup, another user/remote
    machine cannot connect, a wrong-SID server is rejected before receiving
    any request, concurrent authenticated clients work, oversize/timeout/EOF
    responses fail explicitly, and daemon exit releases all pipe instances.

Use generated tones and invented names throughout; never commit real audio,
UI trees, credential values, or meeting-derived data as test evidence.
