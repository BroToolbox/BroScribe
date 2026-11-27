# Spike: GPUI window + `NSStatusItem` on one main loop

Archived evidence, not a runnable package. The application now owns this integration;
`src/main.rs` and these findings remain for design §13. The obsolete standalone
manifest and lockfile were removed rather than maintaining a second dependency graph.

Question (design §13, "Early spikes"): can the menu-bar item live on the main loop that GPUI owns,
next to a GPUI window, without a second UI loop or thread and without continuous redraws?

**Answer: yes.** Tested on 2026-11-14, macOS 27.2 (arm64), Rust 1.99.1.

## What it does

`src/main.rs` opens a 360×160 GPUI window with a label and a counter, and adds an `NSStatusItem`
titled `BS` with a menu: *Show window*, *Increment*, *Quit*. The counter changes only when
*Increment* is chosen. *Show window* re-activates the window, or opens a new one if the user
closed it. *Quit* calls `cx.quit()`.

Commands used for the original proof (the archived directory no longer has a manifest):

```sh
cargo run                       # Dock icon + menu-bar item
SPIKE_ACCESSORY=1 cargo run     # menu-bar utility (ActivationPolicy::Accessory, no Dock icon)
SPIKE_SELFTEST=1 cargo run      # fires Increment, Increment, Show window, Increment, Quit
                                # through NSMenu::performActionForItemAtIndex, 2 s apart
```

Every step is logged to stderr with a timestamp (AppKit action, GPUI receipt, each render).

## Crate versions

At the original proof, all 610 locked packages passed `check-publish-age.py`. The table below records that historical dependency set, not a currently maintained package.

| Crate | Version | Why |
|---|---|---|
| `gpui-pre` | =0.3.12 (2026-11-09) | GPUI itself (the line `gpui-component` 0.9.1 needs; design §13) |
| `gpui-pre-platform` | =0.3.12, feature `font-kit` | `gpui_platform::application()`, the macOS backend (`gpui-pre-macos` 0.3.12) |
| `objc2` | 0.6.5 | `define_class!` for the menu target, `MainThreadMarker` |
| `objc2-app-kit` | 0.3.2 | `NSStatusBar`, `NSStatusItem`, `NSMenu`, `NSMenuItem` (same version `gpui-pre-macos` uses, so no duplicate) |
| `objc2-foundation` | 0.3.2 | `NSString`, `NSObject` |
| `futures` | 0.3.34 | `mpsc::unbounded` channel from the menu target to GPUI |

`gpui-component` was not needed. `accesskit_macos` 0.26.3 (via `gpui-pre-macos`) also pulls in
`objc2` 0.5.3 / `objc2-app-kit` 0.2.2, so two objc2 lines are compiled; that is GPUI's, not ours.

The first resolve with Cargo 1.99.1 locked 19 crates younger than 3 days (`wgpu*` and `naga*` 30.0.2,
`zbus` 6.3.0, `cc` 1.11.2, `ctor` 1.0.14, `bumpalo` 3.20.4, `errno` 0.3.15, `link-section` 0.19.4,
`zruntime` 0.2.3, …). The check's suggested `cargo update -p … --precise …` pins cleared all of them
in one pass.

## Findings

**Window and status item share GPUI's loop.** The status item is created inside the
`Application::run` launch callback, on the main thread (`MainThreadMarker::new()` succeeds).
`sample` of the running process shows one `-[NSApplication run]`, on the main thread, called from
`gpui_macos::MacPlatform::run`. The other threads are AppKit's own `com.apple.NSEventThread` and
idle GCD workqueue threads (6 threads in total, all blocked). No second `NSApplication`, run
loop or UI thread was started.

**How clicks reach GPUI.** `NSMenuItem.target` is an Objective-C object defined with
`objc2::define_class!` (`menuItemClicked:`). Its ivar holds a
`futures::channel::mpsc::UnboundedSender<MenuCommand>`; the item's `tag` identifies the command.
A task started with `cx.spawn` on GPUI's foreground executor awaits the receiver and applies the
command inside `cx.update(…)`. The log shows the AppKit action and the GPUI receipt on the same
thread (`ThreadId(1)`, main thread) within about 1 ms, and the re-render within 5 ms:

```text
[   8.014s] AppKit action Increment (main thread: true, thread ThreadId(1))
[   8.015s] GPUI received Increment
[   8.020s] render #4 (counter = 1)
```

The target has no `&mut App`, so it cannot touch GPUI state directly; the channel is the bridge.
`NSMenuItem.target` is weak, so the target is kept alive by the `StatusItem` struct, which the
receiving task owns.

**Verified paths.**
- `SPIKE_SELFTEST=1`, both Regular and Accessory policy: all five commands went through AppKit
  target-action to GPUI; exit status 0.
- Real menu items, pressed from outside through macOS accessibility (`AXPress` on the
  `AXMenuItem`s under the `BS` `AXMenuBarItem`): Increment twice (window showed "Counter: 2"),
  closing the window with its close button (the app stayed running: GPUI's default quit mode on
  macOS is explicit), *Show window* (a new window opened showing "Counter: 2"), and *Quit*
  (process exited).
- The `BS` item was visible in the menu bar on a desktop screenshot; accessibility reports it as
  `AXMenuBarItem "BS"` in the app's extras menu bar, with an `AXMenu` of the three items.

**Idle CPU: no continuous redraw.** With the window open and idle, `ps -o %cpu` sampled once a
second for 10 s read 0.0–0.1 (one 0.4 sample); `top -l 3 -s 5` showed CPU time going from
0.34 s to 0.36 s over 10 s (about 0.1 %). Renders happen only at window creation (3 frames:
initial layout, then two from display/appearance settling) and once per *Increment*; *Show window*
on an existing window does not re-render.

## Problems and workarounds

1. **No text without `font-kit`.** `gpui-pre-platform`'s default features do not enable
   `font-kit`, and without it `gpui-pre-macos` uses `NoopTextSystem`: quads paint, text is
   silently absent. The only warning goes through the `log` crate, which prints nothing unless a
   logger is installed. Fix: `features = ["font-kit"]` on `gpui-pre-platform`. The app should also
   install a logger so GPUI warnings are visible.
2. **`cx.quit()` does not return from `Application::run`.** The process exits inside AppKit's
   `terminate:`, so `Drop` for the status item never runs and code after `run` never executes.
   Harmless for the status item (the system removes it with the process); anything that must run
   at exit belongs in `cx.on_app_quit`.
3. **`activate(true)` uses the deprecated `activateIgnoringOtherApps:`.** When *Show window*
   reopened the window after an accessibility press from another process, the new window was not
   reported as focused. [INFERENCE] macOS 14+ cooperative activation can decline to activate an app
   the user is not interacting with; a real click on the status item counts as interaction. Check
   this again with the real menu-bar app in M3/M4.
4. **Testing note.** `AXPress` on the status item itself opens the menu, which runs a modal
   tracking loop, so the accessibility call times out (`AXError -25204`) and the menu does not stay
   open for a background client. Pressing the `AXMenuItem` children directly works without opening
   the menu. In code, `NSMenu::performActionForItemAtIndex` drives the same target-action path.
5. **Dependency weight.** The original lock had 610 packages for all targets (wgpu, zbus, Windows crates)
   though macOS builds only a subset; `cargo tree -e normal,build` lists 310 distinct crates for
   this host.
