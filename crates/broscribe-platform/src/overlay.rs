//! The recording panel as a persistent, nonactivating overlay (design §4).
//!
//! The recorder's controls stay visible above ordinary apps while BroScribe
//! is in the background, without taking keyboard focus. macOS supplies a
//! nonactivating NSPanel and Windows a decorated topmost window. Platforms
//! without an overlay keep an ordinary floating window.
//!
//! Geometry is in points with any origin convention, as long as one call
//! uses one convention throughout: the placement is symmetric vertically, so
//! it works in both bottom-left (AppKit) and top-left (GPUI) coordinates.

pub use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{Error, Result};

/// Distance kept from the visible area's edges.
pub const OVERLAY_INSET: f64 = 16.0;

/// What the platform can do for an always-visible recorder window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayCapability {
    /// A panel that floats above other apps' windows, survives the app's
    /// deactivation and never becomes active by being shown.
    NonactivatingPanel,
    /// A decorated topmost window shown without activating it. A click can
    /// still focus its controls.
    DecoratedTopmost,
    /// An ordinary decorated window that does not stay above other apps; the
    /// persistent overlay is unavailable.
    Standard,
}

/// A rectangle in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl OverlayRect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn top(&self) -> f64 {
        self.y + self.height
    }

    /// Whether `(x, y)` lies inside (left and lower edges included).
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.top()
    }

    /// Whether `other` lies entirely inside.
    pub fn contains(&self, other: &OverlayRect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.top() <= self.top()
    }
}

/// The largest size that fits `visible` with `inset` on every side (at
/// least one point each way, for degenerate areas).
fn fitted(width: f64, height: f64, visible: &OverlayRect, inset: f64) -> (f64, f64) {
    let max_w = (visible.width - 2.0 * inset).max(1.0);
    let max_h = (visible.height - 2.0 * inset).max(1.0);
    (width.min(max_w), height.min(max_h))
}

/// The complete window frame of `width × height` placed at the right edge of
/// `visible` (the screen area outside the Dock, menu bar or taskbar), `inset`
/// from it and vertically centred; shrunk to fit smaller areas. Works with
/// negative origins (screens left of or below the primary one).
pub fn right_center(visible: OverlayRect, width: f64, height: f64, inset: f64) -> OverlayRect {
    let (w, h) = fitted(width, height, &visible, inset);
    OverlayRect::new(
        visible.right() - inset - w,
        visible.y + (visible.height - h) / 2.0,
        w,
        h,
    )
}

/// Leave the main window's controls clear when the right-hand strip has room
/// for at least the panel's minimum width. The panel keeps its regular frame
/// when the strip is too narrow, or when the regular frame is already clear.
pub fn beside_main_window(
    visible: OverlayRect,
    default: OverlayRect,
    main: Option<OverlayRect>,
    min_width: f64,
) -> OverlayRect {
    let Some(main) = main.filter(|main| {
        default.x < main.right()
            && default.right() > main.x
            && default.y < main.top()
            && default.top() > main.y
    }) else {
        return default;
    };
    let available = visible.right() - OVERLAY_INSET - main.right();
    if available >= min_width {
        right_center(
            visible,
            default.width.min(available),
            default.height,
            OVERLAY_INSET,
        )
    } else {
        default
    }
}

/// `frame` moved (and shrunk when needed) back inside `visible`, keeping it
/// as close to where it was as possible; `None` when it already lies inside,
/// so a window the user placed is left alone.
pub fn clamp_into(frame: OverlayRect, visible: OverlayRect, inset: f64) -> Option<OverlayRect> {
    if visible.contains(&frame) {
        return None;
    }
    let (w, h) = fitted(frame.width, frame.height, &visible, inset);
    let min_x = visible.x + inset;
    let min_y = visible.y + inset;
    let max_x = (visible.right() - inset - w).max(min_x);
    let max_y = (visible.top() - inset - h).max(min_y);
    Some(OverlayRect::new(
        frame.x.clamp(min_x, max_x),
        frame.y.clamp(min_y, max_y),
        w,
        h,
    ))
}

/// The screen a first presentation uses: the one under the pointer, else the
/// main screen, else the primary (first) one. `screens` are full frames.
pub fn pick_screen(
    screens: &[OverlayRect],
    pointer: Option<(f64, f64)>,
    main: Option<usize>,
) -> Option<usize> {
    pointer
        .and_then(|(x, y)| screens.iter().position(|s| s.contains_point(x, y)))
        .or(main.filter(|&i| i < screens.len()))
        .or((!screens.is_empty()).then_some(0))
}

/// Where a new panel goes when it is first shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayPlacement {
    /// The right edge of the visible area of the screen under the pointer,
    /// vertically centred ([`right_center`]).
    RightEdge,
    /// Where it was opened: the frame it had when the user last left it
    /// (PNL-56). Moved back only when it doesn't fit its screen
    /// ([`clamp_into`]).
    AsOpened,
}

/// Native state that lives exactly as long as the panel (observers,
/// retained references); dropping it tears them down.
pub trait OverlayPresence {}

/// Outcome of applying the platform's screen-capture policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureExclusion {
    Applied,
    Unsupported,
}

/// How an explicit keyboard-focus request was handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayFocus {
    Native,
    ActivateWindow,
    Ignored,
}

/// Presents the recording panel without activating BroScribe. Main thread
/// only, on the UI loop that owns the window; `window` is borrowed for the
/// call and never retained strongly.
pub trait RecordingOverlay {
    fn capability(&self) -> OverlayCapability;

    /// Platform placement before opening a decorated window, for a new frame.
    fn default_frame(
        &self,
        _visible: OverlayRect,
        default: OverlayRect,
        _main: Option<OverlayRect>,
        _min_width: f64,
    ) -> OverlayRect {
        default
    }

    /// A newly created, still hidden panel: raise it above ordinary apps
    /// (never above system prompts), keep it visible when BroScribe is
    /// inactive and on every Space, place it as `placement` says, and show
    /// it without making it key. The returned presence keeps it placed when
    /// displays change.
    fn present_new(
        &self,
        _window: &dyn HasWindowHandle,
        _placement: OverlayPlacement,
    ) -> Result<Box<dyn OverlayPresence>> {
        Err(unsupported())
    }

    /// An already open panel: bring it to the front without activating,
    /// where it is (moved back only when it is off its screen).
    fn present_existing(&self, _window: &dyn HasWindowHandle) -> Result<()> {
        Err(unsupported())
    }

    /// Bring an ordinary floating panel forward when the window system
    /// supports doing so without focus.
    fn present_standard(&self, _window: &dyn HasWindowHandle) -> Result<()> {
        Ok(())
    }

    fn capture_exclusion_supported(&self) -> bool {
        false
    }

    fn set_capture_excluded(
        &self,
        _window: &dyn HasWindowHandle,
        _excluded: bool,
    ) -> Result<CaptureExclusion> {
        Ok(CaptureExclusion::Unsupported)
    }

    /// Only for the app's own Show recording controls command.
    fn focus_for_user_request(&self, _window: &dyn HasWindowHandle) -> Result<OverlayFocus> {
        Ok(OverlayFocus::ActivateWindow)
    }

    /// Returns focus from a key panel to the previously key app window.
    fn return_user_focus(&self, _window: &dyn HasWindowHandle) -> Result<bool> {
        Ok(false)
    }
}

/// The overlay of platforms without a nonactivating panel.
pub struct StandardOverlay;

fn unsupported() -> Error {
    Error::Unsupported(
        "this platform has no nonactivating panel; the recording window does not stay above \
         other apps"
            .into(),
    )
}

impl RecordingOverlay for StandardOverlay {
    fn capability(&self) -> OverlayCapability {
        OverlayCapability::Standard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1440×900 screen whose menu bar (25) and Dock (80, bottom) are
    /// outside the visible area, in bottom-left coordinates.
    fn laptop_visible() -> OverlayRect {
        OverlayRect::new(0.0, 80.0, 1440.0, 795.0)
    }

    #[test]
    fn panel_sits_at_the_right_edge_vertically_centred_outside_dock_and_menu_bar() {
        let frame = right_center(laptop_visible(), 420.0, 708.0, OVERLAY_INSET);
        assert_eq!(frame, OverlayRect::new(1004.0, 123.5, 420.0, 708.0));
        assert!(laptop_visible().contains(&frame));
        assert_eq!(laptop_visible().right() - frame.right(), OVERLAY_INSET);
    }

    #[test]
    fn small_displays_shrink_the_frame_to_fit() {
        let small = OverlayRect::new(0.0, 0.0, 400.0, 500.0);
        let frame = right_center(small, 420.0, 708.0, OVERLAY_INSET);
        assert_eq!(frame, OverlayRect::new(16.0, 16.0, 368.0, 468.0));
        assert!(small.contains(&frame));
    }

    #[test]
    fn screens_left_of_or_below_the_primary_keep_negative_origins() {
        let left = OverlayRect::new(-1920.0, -200.0, 1920.0, 1040.0);
        let frame = right_center(left, 420.0, 708.0, OVERLAY_INSET);
        assert_eq!(frame, OverlayRect::new(-436.0, -34.0, 420.0, 708.0));
        assert!(left.contains(&frame));
    }

    #[test]
    fn clamping_leaves_a_placed_window_alone_and_brings_back_a_stranded_one() {
        let visible = laptop_visible();
        let dragged = OverlayRect::new(100.0, 200.0, 420.0, 600.0);
        assert_eq!(clamp_into(dragged, visible, OVERLAY_INSET), None);
        // Its display went away: it now hangs off the right of this one.
        let stranded = OverlayRect::new(1700.0, 300.0, 420.0, 600.0);
        assert_eq!(
            clamp_into(stranded, visible, OVERLAY_INSET),
            Some(OverlayRect::new(1004.0, 259.0, 420.0, 600.0))
        );
        // Taller than the area: shrunk, inset kept.
        let tall = OverlayRect::new(-50.0, 0.0, 420.0, 900.0);
        assert_eq!(
            clamp_into(tall, visible, OVERLAY_INSET),
            Some(OverlayRect::new(16.0, 96.0, 420.0, 763.0))
        );
    }

    #[test]
    fn first_screen_is_under_the_pointer_then_main_then_primary() {
        let screens = [
            OverlayRect::new(0.0, 0.0, 1440.0, 900.0),
            OverlayRect::new(-1920.0, 0.0, 1920.0, 1080.0),
        ];
        assert_eq!(
            pick_screen(&screens, Some((-10.0, 500.0)), Some(0)),
            Some(1)
        );
        assert_eq!(pick_screen(&screens, Some((5000.0, 0.0)), Some(1)), Some(1));
        assert_eq!(pick_screen(&screens, None, None), Some(0));
        assert_eq!(pick_screen(&screens, None, Some(7)), Some(0));
        assert_eq!(pick_screen(&[], Some((0.0, 0.0)), None), None);
    }

    struct NoHandle;
    impl HasWindowHandle for NoHandle {
        fn window_handle(
            &self,
        ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
        {
            Err(raw_window_handle::HandleError::NotSupported)
        }
    }

    #[test]
    fn standard_platforms_report_the_overlay_as_unsupported() {
        let overlay = StandardOverlay;
        assert_eq!(overlay.capability(), OverlayCapability::Standard);
        assert!(matches!(
            overlay.present_new(&NoHandle, OverlayPlacement::RightEdge),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            overlay.present_existing(&NoHandle),
            Err(Error::Unsupported(_))
        ));
    }
}
