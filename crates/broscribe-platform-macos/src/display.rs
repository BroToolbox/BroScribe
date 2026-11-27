//! Cocoa display accessibility settings, read on GPUI's main loop.

use std::cell::Cell;

use block2::RcBlock;
use broscribe_platform::{DisplayAccessibility, DisplayPreferences};
use objc2::{MainThreadMarker, rc::autoreleasepool};
use objc2_app_kit::{
    NSAppearance, NSAppearanceName, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSColor,
    NSColorSpace, NSWorkspace,
};
use objc2_foundation::{NSUserDefaults, ns_string};

#[derive(Clone, Copy, Debug, Default)]
pub struct MacDisplayAccessibility;

impl DisplayAccessibility for MacDisplayAccessibility {
    fn requires_main_thread(&self) -> bool {
        true
    }

    fn increase_contrast(&self) -> bool {
        self.preferences().increase_contrast
    }

    fn reduce_transparency(&self) -> bool {
        self.preferences().reduce_transparency
    }

    fn forced_colors(&self) -> bool {
        false
    }

    fn accent(&self, dark: bool) -> Option<u32> {
        self.preferences().accent(dark)
    }

    fn preferences(&self) -> DisplayPreferences {
        let Some(_main_thread) = MainThreadMarker::new() else {
            return DisplayPreferences::default();
        };
        autoreleasepool(|_| {
            let workspace = NSWorkspace::sharedWorkspace();
            let mut preferences = DisplayPreferences {
                increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
                reduce_transparency: workspace.accessibilityDisplayShouldReduceTransparency(),
                ..DisplayPreferences::default()
            };
            // Multicolor leaves the key unset and uses the app's own accent.
            // The preference key is not public API.
            let defaults = NSUserDefaults::standardUserDefaults();
            if defaults
                .objectForKey(ns_string!("AppleAccentColor"))
                .is_some()
            {
                // SAFETY: these immutable names are supplied by AppKit.
                preferences.light_accent = resolve_accent(unsafe { NSAppearanceNameAqua });
                preferences.dark_accent = resolve_accent(unsafe { NSAppearanceNameDarkAqua });
            }
            preferences
        })
    }
}

fn resolve_accent(name: &NSAppearanceName) -> Option<u32> {
    let appearance = NSAppearance::appearanceNamed(name)?;
    let accent = Cell::new(None);
    // AppKit restores the previous drawing appearance after the block.
    let block = RcBlock::new(|| {
        if let Some(colour) =
            NSColor::controlAccentColor().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
        {
            let channel = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
            accent.set(Some(
                (channel(colour.redComponent()) << 16)
                    | (channel(colour.greenComponent()) << 8)
                    | channel(colour.blueComponent()),
            ));
        }
    });
    appearance.performAsCurrentDrawingAppearance(&block);
    accent.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_settings_require_the_main_loop() {
        assert!(MacDisplayAccessibility.requires_main_thread());
        let off_main = std::thread::spawn(|| MacDisplayAccessibility.preferences());
        assert_eq!(off_main.join().unwrap(), DisplayPreferences::default());
    }
}
