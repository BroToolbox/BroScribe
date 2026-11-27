//! The menu-bar item: `NSStatusItem` + `NSMenu`, created on the main thread
//! inside GPUI's event loop (see `spikes/gpui-status-item`). Menu clicks go
//! through an Objective-C target object that sends the item id on the click
//! channel; the app drains it on GPUI's foreground executor.

use std::cell::RefCell;

use broscribe_platform::{Error, MenuItemSpec, Result, StatusClickSender, StatusIcon, StatusItem};
use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAccessibility, NSCellImagePosition, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSObject, NSSize, NSString, ns_string};

/// The status item's template glyphs at 2x (48×36 pixels, 24×18 points),
/// rendered with the app's brand files (crates/broscribe-app/assets/brand,
/// `cargo run -p broscribe-app --example render_brand`).
const IDLE_GLYPH: &[u8] = include_bytes!("../../broscribe-app/assets/brand/menubar/idle@2x.png");
const RECORDING_GLYPH: &[u8] =
    include_bytes!("../../broscribe-app/assets/brand/menubar/recording@2x.png");
const PAUSED_GLYPH: &[u8] =
    include_bytes!("../../broscribe-app/assets/brand/menubar/paused@2x.png");

/// A template image (drawn in the menu bar's own ink, light or dark) from a
/// 2x PNG, sized in points.
fn template_image(png: &[u8]) -> Option<Retained<NSImage>> {
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(png))?;
    let pixels = image.size();
    image.setSize(NSSize::new(pixels.width / 2.0, pixels.height / 2.0));
    image.setTemplate(true);
    image.setAccessibilityDescription(Some(ns_string!("BroScribe")));
    Some(image)
}

#[derive(Debug, PartialEq, Eq)]
struct ButtonPresentation {
    title: String,
    tooltip: String,
    accessibility_label: String,
    image_position: NSCellImagePosition,
}

fn button_presentation(title: &str, badge_count: usize, has_image: bool) -> ButtonPresentation {
    let words = title.to_owned();
    let shown = if has_image {
        match badge_count {
            0 => String::new(),
            n => n.to_string(),
        }
    } else {
        words.clone()
    };
    let image_position = if shown.is_empty() {
        NSCellImagePosition::ImageOnly
    } else {
        NSCellImagePosition::ImageLeft
    };
    ButtonPresentation {
        title: shown,
        tooltip: words.clone(),
        accessibility_label: words,
        image_position,
    }
}

fn apply_presentation(button: &NSStatusBarButton, presentation: &ButtonPresentation) {
    button.setTitle(&NSString::from_str(&presentation.title));
    button.setImagePosition(presentation.image_position);
    button.setToolTip(Some(&NSString::from_str(&presentation.tooltip)));
    button.setAccessibilityLabel(Some(&NSString::from_str(&presentation.accessibility_label)));
}

struct TargetIvars {
    clicks: StatusClickSender,
    /// Item id per menu tag.
    ids: RefCell<Vec<String>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; MenuTarget adds no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "BroScribeStatusMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(menuItemClicked:))]
        fn menu_item_clicked(&self, sender: &NSMenuItem) {
            let Ok(index) = usize::try_from(sender.tag()) else {
                return;
            };
            let id = self.ivars().ids.borrow().get(index).cloned();
            if let Some(id) = id {
                // Fails only once the app's receiver is gone (quitting).
                let _ = self.ivars().clicks.unbounded_send(id);
            }
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, clicks: StatusClickSender) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars {
            clicks,
            ids: RefCell::new(Vec::new()),
        });
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// Owns the status item and its menu target (`NSMenuItem.target` is weak).
/// Dropping it removes the item from the menu bar.
pub struct MacStatusItem {
    mtm: MainThreadMarker,
    item: Retained<NSStatusItem>,
    target: Retained<MenuTarget>,
    title: String,
    icon: StatusIcon,
    badge_count: usize,
}

impl MacStatusItem {
    pub const DEFAULT_TITLE: &'static str = "BroScribe";

    /// Must run on the main thread inside the UI event loop; errors otherwise.
    pub fn new(clicks: StatusClickSender) -> Result<Self> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            Error::Unsupported("the status item must be created on the main thread".into())
        })?;
        let target = MenuTarget::new(mtm, clicks);
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let this = Self {
            mtm,
            item,
            target,
            title: Self::DEFAULT_TITLE.into(),
            icon: StatusIcon::Idle,
            badge_count: 0,
        };
        this.refresh_icon();
        this.refresh_title();
        Ok(this)
    }

    /// The glyph shows the state; beside it only the review count, if any.
    /// The title's words become both the tooltip and the spoken accessibility
    /// label. Should the glyph fail to load, the words are shown instead.
    fn refresh_title(&self) {
        let Some(button) = self.item.button(self.mtm) else {
            return;
        };
        let presentation =
            button_presentation(&self.title, self.badge_count, button.image().is_some());
        apply_presentation(&button, &presentation);
    }

    fn refresh_icon(&self) {
        let Some(button) = self.item.button(self.mtm) else {
            return;
        };
        let png = match self.icon {
            StatusIcon::Idle => IDLE_GLYPH,
            StatusIcon::Recording => RECORDING_GLYPH,
            StatusIcon::Paused => PAUSED_GLYPH,
        };
        let image = template_image(png);
        button.setImage(image.as_deref());
    }
}

impl StatusItem for MacStatusItem {
    fn set_title(&mut self, title: &str) -> Result<()> {
        self.title = title.into();
        self.refresh_title();
        Ok(())
    }

    fn set_icon(&mut self, icon: StatusIcon) -> Result<()> {
        if self.icon != icon {
            self.icon = icon;
            self.refresh_icon();
            self.refresh_title();
        }
        Ok(())
    }

    fn set_menu(&mut self, items: Vec<MenuItemSpec>) -> Result<()> {
        let mtm = self.mtm;
        let menu = NSMenu::new(mtm);
        // Enabled state comes from the specs, not from AppKit's validation.
        menu.setAutoenablesItems(false);
        for (index, spec) in items.iter().enumerate() {
            if spec.is_separator() {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            }
            // SAFETY: `menuItemClicked:` is implemented by `MenuTarget` with
            // the `(id sender)` signature AppKit uses for menu actions.
            let menu_item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(&spec.title),
                    Some(sel!(menuItemClicked:)),
                    &NSString::from_str(""),
                )
            };
            // SAFETY: the target is retained by `self` as long as the menu exists.
            unsafe { menu_item.setTarget(Some(&self.target)) };
            menu_item.setTag(isize::try_from(index).unwrap_or(isize::MAX));
            menu_item.setEnabled(spec.enabled);
            menu.addItem(&menu_item);
        }
        *self.target.ivars().ids.borrow_mut() = items.into_iter().map(|s| s.id).collect();
        self.item.setMenu(Some(&menu));
        Ok(())
    }

    fn set_badge_count(&mut self, n: usize) -> Result<()> {
        if self.badge_count != n {
            self.badge_count = n;
            self.refresh_title();
        }
        Ok(())
    }
}

impl Drop for MacStatusItem {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE_1X: &[u8] = include_bytes!("../../broscribe-app/assets/brand/menubar/idle.png");
    const RECORDING_1X: &[u8] =
        include_bytes!("../../broscribe-app/assets/brand/menubar/recording.png");
    const PAUSED_1X: &[u8] = include_bytes!("../../broscribe-app/assets/brand/menubar/paused.png");

    fn png_dimensions(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        (width, height)
    }

    #[test]
    fn every_glyph_is_a_twenty_four_by_eighteen_template_image_with_description() {
        for (name, png_1x, png_2x) in [
            ("idle", IDLE_1X, IDLE_GLYPH),
            ("recording", RECORDING_1X, RECORDING_GLYPH),
            ("paused", PAUSED_1X, PAUSED_GLYPH),
        ] {
            assert_eq!(png_dimensions(png_1x), (24, 18), "{name} 1x");
            assert_eq!(png_dimensions(png_2x), (48, 36), "{name} 2x");
            let image = template_image(png_2x).expect("a PNG AppKit reads");
            assert!(image.isTemplate(), "{name}");
            assert_eq!(image.size(), NSSize::new(24.0, 18.0), "{name}");
            assert_eq!(
                image.accessibilityDescription().map(|s| s.to_string()),
                Some("BroScribe".into()),
                "{name}"
            );
        }
    }

    #[test]
    fn recording_state_sets_spoken_name_and_centers_icon_at_zero_count() {
        let idle = button_presentation("BroScribe", 0, true);
        assert_eq!(idle.title, "");
        assert_eq!(idle.tooltip, "BroScribe");
        assert_eq!(idle.accessibility_label, "BroScribe");
        assert_eq!(idle.image_position, NSCellImagePosition::ImageOnly);

        let recording = button_presentation("BroScribe · Recording", 0, true);
        assert_eq!(recording.title, "");
        assert!(recording.accessibility_label.contains("Recording"));
        assert_eq!(recording.accessibility_label, "BroScribe · Recording");
        assert_eq!(recording.tooltip, recording.accessibility_label);
        assert_eq!(recording.image_position, NSCellImagePosition::ImageOnly);

        let recording_with_count =
            button_presentation("BroScribe · Recording · 3 speakers to review", 3, true);
        assert_eq!(recording_with_count.title, "3");
        assert!(
            recording_with_count
                .accessibility_label
                .contains("Recording")
        );
        assert_eq!(
            recording_with_count.accessibility_label,
            "BroScribe · Recording · 3 speakers to review"
        );
        assert_eq!(
            recording_with_count.tooltip,
            recording_with_count.accessibility_label
        );
        assert_eq!(
            recording_with_count.image_position,
            NSCellImagePosition::ImageLeft
        );

        let fallback = button_presentation("BroScribe · Recording", 0, false);
        assert_eq!(fallback.title, "BroScribe · Recording");
        assert_eq!(fallback.accessibility_label, "BroScribe · Recording");
        assert_eq!(fallback.image_position, NSCellImagePosition::ImageLeft);
    }

    #[test]
    fn refuses_off_the_main_thread() {
        // A spawned thread is never the main thread.
        let refused = std::thread::spawn(|| {
            let (tx, _rx) = futures::channel::mpsc::unbounded();
            MacStatusItem::new(tx).is_err()
        })
        .join()
        .unwrap();
        assert!(refused);
    }
}
