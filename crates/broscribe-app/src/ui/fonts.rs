//! The bundled typefaces (design system typography): Inter for everything
//! read, Manrope for page and window titles. GPUI cannot fetch web fonts, so
//! both are compiled in and registered before the first window opens.
//!
//! Both are under the SIL Open Font License 1.1, pinned with their licence
//! texts in `assets/ASSET_LICENSES.toml` and shipped in
//! THIRD_PARTY_NOTICES.txt (scripts/third-party-notices.py).

use std::borrow::Cow;

use gpui::App;

/// The interface's text family: the theme's `font.family`.
pub const SANS: &str = "Inter";
/// The family of page and window titles, in its SemiBold.
pub const DISPLAY: &str = "Manrope";

/// The static instances the interface uses: Inter 400/500/600 (body, labels,
/// emphasis) and Manrope 600/700 (titles, the sidebar's name).
const FONTS: [&[u8]; 5] = [
    include_bytes!("../../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/Manrope-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/Manrope-Bold.ttf"),
];

/// Registers the bundled fonts with GPUI's text system. A failure is logged
/// and leaves the platform's fonts in their place: text still renders.
pub fn register(cx: &mut App) {
    let fonts = FONTS.iter().map(|font| Cow::Borrowed(*font)).collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::error!("cannot register the bundled fonts: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typographic family (name ID 16, else 1) a TrueType file declares
    /// for Windows Unicode (platform 3), which is how GPUI groups weights.
    fn family(font: &[u8]) -> String {
        let be16 = |at: usize| u16::from_be_bytes([font[at], font[at + 1]]) as usize;
        let be32 = |at: usize| {
            u32::from_be_bytes([font[at], font[at + 1], font[at + 2], font[at + 3]]) as usize
        };
        assert_eq!(be32(0), 0x0001_0000, "not a TrueType font");
        let table = (0..be16(4))
            .map(|i| 12 + 16 * i)
            .find(|&record| &font[record..record + 4] == b"name")
            .map(|record| be32(record + 8))
            .expect("no name table");
        let strings = table + be16(table + 4);
        let mut names = std::collections::HashMap::new();
        for i in 0..be16(table + 2) {
            let record = table + 6 + 12 * i;
            if be16(record) == 3 {
                let (id, len, offset) = (be16(record + 6), be16(record + 8), be16(record + 10));
                let units = font[strings + offset..strings + offset + len]
                    .chunks(2)
                    .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                    .collect::<Vec<_>>();
                names.insert(id, String::from_utf16(&units).unwrap());
            }
        }
        names.get(&16).or(names.get(&1)).cloned().unwrap()
    }

    #[test]
    fn the_bundled_fonts_are_the_two_families_the_theme_names() {
        // GPUI's test platform has no text system, so check the files: three
        // Inter weights and two Manrope weights, grouped under one name each.
        let families: Vec<String> = FONTS.iter().map(|font| family(font)).collect();
        assert_eq!(families, [SANS, SANS, SANS, DISPLAY, DISPLAY]);
        let theme: serde_json::Value =
            serde_json::from_str(super::super::theme::THEME_JSON).unwrap();
        for variant in theme["themes"].as_array().unwrap() {
            assert_eq!(variant["font.family"], SANS);
        }
    }
}
