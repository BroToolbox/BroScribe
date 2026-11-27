//! The recording controls as a nonactivating overlay (design §4).
//!
//! GPUI creates `WindowKind::PopUp` windows as an `NSPanel` with the
//! `NonactivatingPanel` style at `NSPopUpWindowLevel`. The app creates the
//! panel hidden (`show: false`, `focus: false`) and hands its borrowed window
//! handle here before it is ever shown. This adapter only changes what the
//! style allows after construction: it lowers the level to
//! `NSFloatingWindowLevel` (above ordinary apps, never above system
//! prompts), keeps the panel on deactivation and on every Space (full-screen
//! ones included), places it (at the right edge of the screen under the
//! pointer, or where GPUI opened it when the app restores the frame the user
//! left it at), and shows it with `orderFrontRegardless`, so BroScribe never
//! becomes active and keyboard focus stays where it was. The style mask
//! itself is never changed.
//!
//! Everything runs on the main thread inside GPUI's loop: no second event
//! loop, no polling, no window lookups by title. The only observer is the
//! screen-configuration notification, owned by the returned presence and
//! removed when the panel (which owns that presence) is dropped.

use std::cell::RefCell;
use std::ptr::NonNull;

use block2::RcBlock;
use broscribe_platform::overlay::{
    CaptureExclusion, OVERLAY_INSET, OverlayCapability, OverlayFocus, OverlayPlacement,
    OverlayPresence, OverlayRect, RecordingOverlay, clamp_into, pick_screen, right_center,
};
use broscribe_platform::{Error, Result};
use objc2::MainThreadMarker;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSApplication, NSApplicationDidChangeScreenParametersNotification, NSEvent,
    NSFloatingWindowLevel, NSScreen, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSOperationQueue, NSPoint, NSProcessInfo, NSRect, NSSize,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// The macOS recording overlay.
#[derive(Default)]
pub struct MacRecordingOverlay {
    previous_key: RefCell<Option<Weak<NSWindow>>>,
}

fn rect(r: NSRect) -> OverlayRect {
    OverlayRect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn ns_rect(r: OverlayRect) -> NSRect {
    NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.width, r.height))
}

fn unsupported(why: impl Into<String>) -> Error {
    Error::Unsupported(why.into())
}

/// The `NSWindow` of the app's own view behind `window`.
fn ns_window(window: &dyn HasWindowHandle, _mtm: MainThreadMarker) -> Result<Retained<NSWindow>> {
    let handle = window
        .window_handle()
        .map_err(|e| unsupported(format!("recording controls has no native window: {e}")))?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return Err(unsupported("recording controls is not an AppKit window"));
    };
    // SAFETY: the handle is borrowed from the live window for this call, so
    // its NSView is alive; we are on the main thread (`_mtm`), where AppKit
    // views may be used.
    let view: &NSView = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    view.window()
        .ok_or_else(|| unsupported("recording controls view is not in a window"))
}

/// The screen a first presentation uses: under the pointer, else the main
/// screen, else the primary one.
fn first_screen(mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    let screens: Vec<Retained<NSScreen>> = NSScreen::screens(mtm).iter().collect();
    let frames: Vec<OverlayRect> = screens.iter().map(|s| rect(s.frame())).collect();
    let pointer = NSEvent::mouseLocation();
    let main = NSScreen::mainScreen(mtm).and_then(|main| {
        screens
            .iter()
            .position(|s| Retained::as_ptr(s) == Retained::as_ptr(&main))
    });
    let index = pick_screen(&frames, Some((pointer.x, pointer.y)), main)?;
    screens.into_iter().nth(index)
}

/// Moves `window` back inside its screen (the main one when it is on none)
/// only when it no longer fits there; a window the user placed stays put.
fn reclamp(window: &NSWindow, mtm: MainThreadMarker) {
    let Some(screen) = window.screen().or_else(|| NSScreen::mainScreen(mtm)) else {
        return;
    };
    if let Some(frame) = clamp_into(
        rect(window.frame()),
        rect(screen.visibleFrame()),
        OVERLAY_INSET,
    ) {
        window.setFrame_display(ns_rect(frame), true);
    }
}

/// Keeps the panel placed while it exists.
struct MacOverlayPresence {
    observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl OverlayPresence for MacOverlayPresence {}

impl Drop for MacOverlayPresence {
    fn drop(&mut self) {
        let observer: &AnyObject = (*self.observer).as_ref();
        // SAFETY: `observer` is the token `addObserverForName:` returned.
        unsafe {
            NSNotificationCenter::defaultCenter().removeObserver(observer);
        }
    }
}

impl RecordingOverlay for MacRecordingOverlay {
    fn capability(&self) -> OverlayCapability {
        OverlayCapability::NonactivatingPanel
    }

    fn present_new(
        &self,
        window: &dyn HasWindowHandle,
        placement: OverlayPlacement,
    ) -> Result<Box<dyn OverlayPresence>> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| unsupported("the recording overlay runs on the main thread only"))?;
        let panel = ns_window(window, mtm)?;
        // Only a panel created nonactivating may float over other apps
        // without taking focus; its style cannot be changed afterwards.
        if !panel
            .styleMask()
            .contains(NSWindowStyleMask::NonactivatingPanel)
        {
            return Err(unsupported(
                "recording controls was not created as a nonactivating panel",
            ));
        }
        panel.setLevel(NSFloatingWindowLevel);
        panel.setHidesOnDeactivate(false);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        match placement {
            OverlayPlacement::RightEdge => {
                if let Some(screen) = first_screen(mtm) {
                    let size = panel.frame().size;
                    let frame = right_center(
                        rect(screen.visibleFrame()),
                        size.width,
                        size.height,
                        OVERLAY_INSET,
                    );
                    panel.setFrame_display(ns_rect(frame), false);
                }
            }
            // GPUI created it on the remembered display at the remembered
            // frame (PNL-56): moved only when it no longer fits there.
            OverlayPlacement::AsOpened => reclamp(&panel, mtm),
        }
        panel.orderFrontRegardless();

        let weak = Weak::from_retained(&panel);
        let block = RcBlock::new(move |_note: NonNull<NSNotification>| {
            if let (Some(panel), Some(mtm)) = (weak.load(), MainThreadMarker::new()) {
                reclamp(&panel, mtm);
            }
        });
        // SAFETY: the name is AppKit's own notification; the block runs on
        // the main queue (where `weak` and AppKit may be used) and only until
        // the presence removes it.
        let observer = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &block,
            )
        };
        Ok(Box::new(MacOverlayPresence { observer }))
    }

    fn present_existing(&self, window: &dyn HasWindowHandle) -> Result<()> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| unsupported("the recording overlay runs on the main thread only"))?;
        let panel = ns_window(window, mtm)?;
        reclamp(&panel, mtm);
        panel.orderFrontRegardless();
        Ok(())
    }

    fn capture_exclusion_supported(&self) -> bool {
        // SharingType is not a reliable exclusion mechanism for the macOS 15+
        // capture stack. Report the capability conservatively until native verification.
        NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion
            < 15
    }

    fn set_capture_excluded(
        &self,
        window: &dyn HasWindowHandle,
        excluded: bool,
    ) -> Result<CaptureExclusion> {
        if !self.capture_exclusion_supported() {
            return Ok(CaptureExclusion::Unsupported);
        }
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| unsupported("capture exclusion requires the main thread"))?;
        ns_window(window, mtm)?.setSharingType(if excluded {
            NSWindowSharingType::None
        } else {
            NSWindowSharingType::ReadOnly
        });
        Ok(CaptureExclusion::Applied)
    }

    fn focus_for_user_request(&self, window: &dyn HasWindowHandle) -> Result<OverlayFocus> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| unsupported("panel focus requires the main thread"))?;
        let app = NSApplication::sharedApplication(mtm);
        if !app.isActive() {
            return Ok(OverlayFocus::Ignored);
        }
        let panel = ns_window(window, mtm)?;
        if !panel.isKeyWindow() {
            *self.previous_key.borrow_mut() = app.keyWindow().as_ref().map(Weak::from_retained);
            panel.makeKeyWindow();
        }
        Ok(OverlayFocus::Native)
    }

    fn return_user_focus(&self, window: &dyn HasWindowHandle) -> Result<bool> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| unsupported("panel focus requires the main thread"))?;
        let panel = ns_window(window, mtm)?;
        let app = NSApplication::sharedApplication(mtm);
        if !app.isActive() || !panel.isKeyWindow() {
            return Ok(false);
        }
        let previous = self
            .previous_key
            .borrow_mut()
            .take()
            .and_then(|previous| previous.load());
        let usable = |window: &Retained<NSWindow>| {
            Retained::as_ptr(window) != Retained::as_ptr(&panel) && window.isVisible()
        };
        if let Some(previous) = previous
            .filter(usable)
            .or_else(|| app.mainWindow().filter(usable))
        {
            previous.makeKeyWindow();
            return Ok(true);
        }
        Ok(false)
    }
}
