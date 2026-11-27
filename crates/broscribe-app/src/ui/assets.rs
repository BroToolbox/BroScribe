//! The app's assets: the BroScribe brand files (`assets/brand/`, generated
//! by `examples/render_brand.rs` from the design system's sources) compiled
//! in, then gpui-kit's default assets (gpui-component's Lucide icons) for
//! everything else. GPUI loads them by path through `svg()` and `img()`.

use std::borrow::Cow;
use std::sync::{Arc, LazyLock};

use gpui::{AssetSource, Result, SharedString, WindowOptions};

/// The mascot drawings (full colour, through `img()`).
pub const MASCOT_HERO: &str = "brand/mascot-hero.svg";
pub const MASCOT_IDLE: &str = "brand/mascot-idle.svg";
/// The app-icon tile (full colour).
pub const TILE_HERO: &str = "brand/tile-hero.svg";
/// Single-ink glyphs, painted in the text colour through `svg()`.
pub const WAVEFORM: &str = "brand/waveform.svg";
pub const STATUS_DONE: &str = "brand/status-done.svg";
pub const STATUS_FAILED: &str = "brand/status-failed.svg";
pub const STATUS_ATTENTION: &str = "brand/status-attention.svg";
pub const STATUS_PENDING: &str = "brand/status-pending.svg";
pub const STATUS_PAUSED: &str = "brand/status-paused.svg";
pub const VISOR_SMILE: &str = "brand/visor-smile.svg";

const BRAND: [(&str, &[u8]); 10] = [
    (
        MASCOT_HERO,
        include_bytes!("../../assets/brand/mascot-hero.svg"),
    ),
    (
        MASCOT_IDLE,
        include_bytes!("../../assets/brand/mascot-idle.svg"),
    ),
    (
        TILE_HERO,
        include_bytes!("../../assets/brand/tile-hero.svg"),
    ),
    (WAVEFORM, include_bytes!("../../assets/brand/waveform.svg")),
    (
        STATUS_DONE,
        include_bytes!("../../assets/brand/status-done.svg"),
    ),
    (
        STATUS_FAILED,
        include_bytes!("../../assets/brand/status-failed.svg"),
    ),
    (
        STATUS_ATTENTION,
        include_bytes!("../../assets/brand/status-attention.svg"),
    ),
    (
        STATUS_PENDING,
        include_bytes!("../../assets/brand/status-pending.svg"),
    ),
    (
        STATUS_PAUSED,
        include_bytes!("../../assets/brand/status-paused.svg"),
    ),
    (
        VISOR_SMILE,
        include_bytes!("../../assets/brand/visor-smile.svg"),
    ),
];

/// The app's reverse-DNS identity. Linux desktops match a window to its
/// `.desktop` file by it (the Wayland app id, X11's `WM_CLASS`); it is also
/// the Windows notification identity.
pub const APP_ID: &str = "org.broscribe.BroScribe";

/// The app icon at 128 px, for X11 window managers (GPUI sets a window
/// icon on X11 only; macOS and Windows take the bundle's and the
/// executable's icon).
const WINDOW_ICON: &[u8] = include_bytes!("../../assets/brand/linux/window-icon.png");

fn window_icon() -> Option<Arc<image::RgbaImage>> {
    static ICON: LazyLock<Option<Arc<image::RgbaImage>>> = LazyLock::new(|| {
        image::load_from_memory_with_format(WINDOW_ICON, image::ImageFormat::Png)
            .ok()
            .map(|icon| Arc::new(icon.into_rgba8()))
    });
    ICON.clone()
}

/// The options every BroScribe window starts from: its identity and icon.
pub fn window_options() -> WindowOptions {
    WindowOptions {
        app_id: Some(APP_ID.into()),
        icon: window_icon(),
        ..Default::default()
    }
}

gpui::assets::icon_assets!(ShellIcons, [Users, Menu, Bookmark, StickyNote, TagPlus]);

/// The brand files first, then gpui-kit's assets.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = BRAND.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        if let Some(icon) = ShellIcons.load(path)? {
            return Ok(Some(icon));
        }
        gpui::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = BRAND
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect();
        names.extend(ShellIcons.list(path)?);
        names.extend(gpui::assets::Assets.list(path)?);
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::component::{IconName, IconNamed as _};

    #[test]
    fn brand_files_load_and_everything_else_falls_through() {
        for (name, _) in BRAND {
            let bytes = Assets.load(name).unwrap().expect(name);
            assert!(bytes.starts_with(b"<svg"), "{name} is not an SVG");
            // Generated with text as paths: no font needed to draw it.
            assert!(!String::from_utf8_lossy(&bytes).contains("<text"), "{name}");
        }
        assert_eq!(Assets.list("brand/").unwrap().len(), BRAND.len());
        // gpui-component's own icons still resolve through gpui-kit's assets.
        let icon = IconName::Check.path();
        assert!(Assets.load(&icon).unwrap().is_some(), "{icon}");
        // A missing file is an error, as in gpui-kit's own source.
        assert!(Assets.load("brand/missing.svg").is_err());
    }

    #[test]
    fn every_window_carries_the_app_identity_and_icon() {
        let options = window_options();
        assert_eq!(options.app_id.as_deref(), Some(APP_ID));
        let icon = options.icon.expect("the window icon decodes");
        assert_eq!(icon.dimensions(), (128, 128));
        // The tile's rounded corner is transparent, its middle opaque.
        assert_eq!(icon.get_pixel(0, 0)[3], 0);
        assert_eq!(icon.get_pixel(64, 64)[3], 255);
    }
}
