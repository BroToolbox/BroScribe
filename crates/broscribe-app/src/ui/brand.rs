//! Colours the design system defines that gpui-component's theme has no key
//! for: cards and inset boxes, the selected sidebar pill, the soft fills and
//! edges of each state, what an AI provider wrote, people's hues, the
//! recording mark and meters, find highlights, the visor (the one dark
//! panel, in both appearances) and the shadows of cards, pills and the
//! primary button.
//!
//! The values come from `palette_gen.rs`, which
//! `scripts/palette.py --theme-json` writes from the design
//! system's `color.css` together with the theme JSON, so the two never
//! disagree. Each role has a light and a dark value and an Increase Contrast
//! version of both, chosen from the theme's mode and the display settings;
//! nothing else in the app writes a colour literal.

use gpui::component::ActiveTheme as _;
use gpui::{App, BoxShadow, Hsla, Pixels, Rgba, px, rgb};

use super::palette_gen::{self, Palette, ShadowLayer};

/// The radius of a card on the desk.
pub const RADIUS_CARD: Pixels = px(palette_gen::RADIUS_CARD);

/// How many person hues there are; a person's stored hue is `1..=PERSON_HUES`.
pub const PERSON_HUES: u8 = 7;

/// The brand colours for the current appearance.
#[derive(Clone, Copy, Debug)]
pub struct Brand {
    palette: Palette,
    /// Every block of content on the desk.
    pub card: Hsla,
    /// A box inside a card: the desk colour.
    pub surface_inset: Hsla,
    /// The selected item of a sidebar: a card-coloured pill.
    pub sidebar_selected: Hsla,
    /// Toolbars and window chrome.
    pub chrome: Hsla,
    /// BroScribe violet.
    pub primary: Hsla,
    /// What marks a selection beyond its tint (the bar on a selected row's
    /// leading edge): violet, or the user's accent colour on macOS.
    pub selection_mark: Hsla,
    /// The selected row's tint (follows the accent colour too).
    pub selected: Hsla,
    /// Text on a selection.
    pub selected_foreground: Hsla,
    /// Failure text and marks.
    pub danger: Hsla,
    /// A failure banner's fill.
    pub danger_soft: Hsla,
    /// A failure banner's edge.
    pub danger_border: Hsla,
    /// Decision text and marks.
    pub warning: Hsla,
    /// An attention banner's fill and unresolved badges.
    pub warning_soft: Hsla,
    /// An attention banner's edge.
    pub warning_border: Hsla,
    /// Done text and marks.
    pub success: Hsla,
    /// A done banner's fill and the *Named by you* badge.
    pub success_soft: Hsla,
    /// A done banner's edge.
    pub success_border: Hsla,
    /// Information text and marks.
    pub info: Hsla,
    /// An information banner's fill.
    pub info_soft: Hsla,
    /// An information banner's edge.
    pub info_border: Hsla,
    /// Behind what an AI provider wrote.
    pub ai_soft: Hsla,
    /// The violet rule on the leading edge of what an AI provider wrote.
    pub ai_border: Hsla,
    /// The AI caution line.
    pub ai_foreground: Hsla,
    /// The Review count.
    pub badge: Hsla,
    /// The Review count's number.
    pub badge_foreground: Hsla,
    /// The Me disc: slate, so it never reads as a button.
    pub me: Hsla,
    /// Initials on a person's disc: white in light, charcoal in dark.
    pub on_person: Hsla,
    /// The capture dot and the word *Recording*, only while audio is
    /// really being captured.
    pub recording: Hsla,
    /// Lit segments of the Me (microphone) meter.
    pub meter_me: Hsla,
    /// Lit segments of the Others (system audio) meter: a different step
    /// from Me, and their labels tell them apart.
    pub meter_others: Hsla,
    /// Unlit meter segments.
    pub meter_track: Hsla,
    /// A find highlight: yellow, apart from the amber of decisions.
    pub find: Hsla,
    /// Text in a find highlight.
    pub find_foreground: Hsla,
    /// The visor panel.
    pub visor: Hsla,
    /// The visor's 2px edge.
    pub visor_rim: Hsla,
    /// Eyes and status words inside the visor.
    pub visor_foreground: Hsla,
    /// The `Bro_` caret, the smile, percentages and the focus ring inside
    /// the visor.
    pub visor_accent: Hsla,
    /// The recording dot, and a failure's mark, inside the visor.
    pub visor_recording: Hsla,
}

fn colour(value: u32) -> Hsla {
    rgb(value).into()
}

fn shadow(layers: &[ShadowLayer]) -> Vec<BoxShadow> {
    layers
        .iter()
        .map(|layer| {
            let ink = Rgba {
                r: ((layer.rgba >> 24) & 0xff) as f32 / 255.,
                g: ((layer.rgba >> 16) & 0xff) as f32 / 255.,
                b: ((layer.rgba >> 8) & 0xff) as f32 / 255.,
                a: (layer.rgba & 0xff) as f32 / 255.,
            };
            BoxShadow::new(px(layer.x), px(layer.y), ink.into())
                .blur_radius(px(layer.blur))
                .spread_radius(px(layer.spread))
        })
        .collect()
}

impl Brand {
    fn from_palette(p: Palette) -> Self {
        Self {
            palette: p,
            card: colour(p.card),
            surface_inset: colour(p.surface_inset),
            sidebar_selected: colour(p.sidebar_selected),
            chrome: colour(p.chrome),
            primary: colour(p.primary),
            selection_mark: colour(p.primary),
            selected: colour(p.selected),
            selected_foreground: colour(p.selected_foreground),
            danger: colour(p.danger),
            danger_soft: colour(p.danger_soft),
            danger_border: colour(p.danger_border),
            warning: colour(p.warning),
            warning_soft: colour(p.warning_soft),
            warning_border: colour(p.warning_border),
            success: colour(p.success),
            success_soft: colour(p.success_soft),
            success_border: colour(p.success_border),
            info: colour(p.info),
            info_soft: colour(p.info_soft),
            info_border: colour(p.info_border),
            ai_soft: colour(p.ai_soft),
            ai_border: colour(p.ai_border),
            ai_foreground: colour(p.ai_foreground),
            badge: colour(p.badge),
            badge_foreground: colour(p.badge_foreground),
            me: colour(p.me),
            on_person: colour(p.on_person),
            recording: colour(p.recording),
            meter_me: colour(p.meter_me),
            meter_others: colour(p.meter_others),
            meter_track: colour(p.meter_track),
            find: colour(p.find),
            find_foreground: colour(p.find_foreground),
            visor: colour(p.visor),
            visor_rim: colour(p.visor_rim),
            visor_foreground: colour(p.visor_foreground),
            visor_accent: colour(p.visor_accent),
            visor_recording: colour(p.visor_recording),
        }
    }

    /// The disc colour of a person's stored hue (`1..=PERSON_HUES`; any
    /// other value wraps into that range, so a damaged record still draws).
    pub fn person_hue(&self, hue: u8) -> Hsla {
        let index = usize::from(hue.saturating_sub(1) % PERSON_HUES);
        colour(self.palette.hues[index])
    }

    /// A card's shadow: soft and ink-tinted in light, a 1px dark line in dark.
    pub fn card_shadow(&self) -> Vec<BoxShadow> {
        shadow(self.palette.shadow_card)
    }

    /// The selected sidebar pill's hairline and lift.
    pub fn pill_shadow(&self) -> Vec<BoxShadow> {
        shadow(self.palette.shadow_pill)
    }

    /// The primary button's violet shadow (none in dark).
    pub fn primary_shadow(&self) -> Vec<BoxShadow> {
        shadow(self.palette.shadow_primary)
    }

    /// Menus, popovers and dialogs.
    pub fn popover_shadow(&self) -> Vec<BoxShadow> {
        shadow(self.palette.shadow_popover)
    }
}

/// The palette for a theme mode and the Increase Contrast setting.
fn palette(dark: bool, more_contrast: bool) -> Palette {
    match (dark, more_contrast) {
        (false, false) => palette_gen::LIGHT,
        (true, false) => palette_gen::DARK,
        (false, true) => palette_gen::LIGHT_CONTRAST,
        (true, true) => palette_gen::DARK_CONTRAST,
    }
}

/// Focus and selection in the user's accent colour (HIG color.md › App
/// accent colors), as `0xRRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccentRoles {
    /// The focus ring and the selection's leading bar: the accent itself.
    pub mark: u32,
    /// The selected row's tint: the accent over the card, light enough for
    /// body text at 7:1.
    pub selected: u32,
}

/// `top` at `alpha` over `ground`, on the encoded channels (as GPUI blends).
fn blend(top: u32, alpha: f64, ground: u32) -> u32 {
    [16, 8, 0].iter().fold(0, |out, shift| {
        let t = f64::from((top >> shift) & 0xff);
        let g = f64::from((ground >> shift) & 0xff);
        out | (((t * alpha + g * (1.0 - alpha)).round() as u32) << shift)
    })
}

/// WCAG 2.1 contrast of two `0xRRGGBB` colours.
pub(crate) fn contrast(a: u32, b: u32) -> f64 {
    let luminance = |colour: u32| {
        let [r, g, b] = [16, 8, 0].map(|shift| {
            let c = f64::from((colour >> shift) & 0xff) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The accent roles for `accent` in a mode, or `None` to keep violet: no
/// accent chosen (Multicolor, Windows, Linux), or one that would not reach
/// 3:1 as a marker on the desk, cards and the selected tint.
pub fn accent_roles(accent: Option<u32>, dark: bool, more_contrast: bool) -> Option<AccentRoles> {
    let accent = accent?;
    let p = palette(dark, more_contrast);
    if contrast(accent, p.surface_inset) < 3.0 || contrast(accent, p.card) < 3.0 {
        return None;
    }
    let selected = [0.28, 0.2, 0.14, 0.08]
        .into_iter()
        .filter(|alpha| dark || *alpha <= 0.16)
        .map(|alpha| blend(accent, alpha, p.card))
        .find(|tint| contrast(p.foreground, *tint) >= 7.0 && contrast(accent, *tint) >= 3.0)?;
    Some(AccentRoles {
        mark: accent,
        selected,
    })
}

/// The brand colours for `cx`'s theme mode and display settings.
pub fn brand(cx: &App) -> Brand {
    let preferences = super::theme::display_preferences(cx);
    let dark = cx.theme().mode.is_dark();
    let mut brand = Brand::from_palette(palette(dark, preferences.more_contrast()));
    if let Some(accent) = accent_roles(preferences.accent(dark), dark, preferences.more_contrast())
    {
        brand.selection_mark = colour(accent.mark);
        brand.selected = colour(accent.selected);
    }
    brand
}

#[cfg(test)]
mod tests {
    //! A port of `palette.py`'s checks, over every variant the app ships:
    //! each contrast pair at its threshold, the three state colours apart
    //! under protanopia, deuteranopia and tritanopia, and their soft fills
    //! apart too. `palette.py` runs the same checks on `color.css`; these
    //! hold the generated table the app builds from to them.

    use super::*;

    #[test]
    fn blue_marker_has_contrast_against_its_dark_selection_tint() {
        let roles = accent_roles(Some(0x007aff), true, false).unwrap();
        assert!(contrast(roles.mark, roles.selected) >= 3.0);
    }
    use crate::ui::theme;

    /// `0xRRGGBB` as encoded sRGB channels in `0.0..=1.0`.
    fn channels(colour: u32) -> [f64; 3] {
        [16, 8, 0].map(|shift| f64::from((colour >> shift) & 0xff) / 255.0)
    }

    fn linear(c: f64) -> f64 {
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG 2.1 relative luminance.
    fn luminance(colour: u32) -> f64 {
        let [r, g, b] = channels(colour).map(linear);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// WCAG 2.1 contrast ratio.
    fn contrast(a: u32, b: u32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Machado, Oliveira and Fernandes (2009) at full severity, on linear
    /// sRGB: the matrices `palette.py` uses.
    const PROTAN: [[f64; 3]; 3] = [
        [0.152286, 1.052583, -0.204868],
        [0.114503, 0.786281, 0.099216],
        [-0.003882, -0.048116, 1.051998],
    ];
    const DEUTAN: [[f64; 3]; 3] = [
        [0.367322, 0.860646, -0.227968],
        [0.280085, 0.672501, 0.047413],
        [-0.011820, 0.042940, 0.968881],
    ];
    const TRITAN: [[f64; 3]; 3] = [
        [1.255528, -0.076749, -0.178779],
        [-0.078411, 0.930809, 0.147602],
        [0.004733, 0.691367, 0.303900],
    ];

    /// CIELAB (D65) of linear sRGB.
    fn lab([r, g, b]: [f64; 3]) -> [f64; 3] {
        let x = 0.4124 * r + 0.3576 * g + 0.1805 * b;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let z = 0.0193 * r + 0.1192 * g + 0.9505 * b;
        let f = |t: f64| {
            if t > 0.008856 {
                t.cbrt()
            } else {
                7.787 * t + 16. / 116.
            }
        };
        let (fx, fy, fz) = (f(x / 0.95047), f(y), f(z / 1.08883));
        [116. * fy - 16., 500. * (fx - fy), 200. * (fy - fz)]
    }

    /// `colour` as someone with the deficiency `matrix` sees it, in CIELAB.
    fn seen(colour: u32, matrix: &[[f64; 3]; 3]) -> [f64; 3] {
        let v = channels(colour).map(linear);
        lab([0, 1, 2].map(|i| {
            (0..3)
                .map(|j| matrix[i][j] * v[j])
                .sum::<f64>()
                .clamp(0., 1.)
        }))
    }

    /// CIE76 colour difference.
    fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
        (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt()
    }

    const GROUNDS: [&str; 7] = [
        "background",
        "chrome",
        "tray",
        "sidebar",
        "surface",
        "popover",
        "muted",
    ];

    /// `palette.py`'s `RULES`: (foreground, grounds, minimum, what).
    fn rules() -> Vec<(&'static str, Vec<&'static str>, f64, &'static str)> {
        let g = |extra: &[&'static str]| {
            let mut grounds = GROUNDS.to_vec();
            grounds.extend_from_slice(extra);
            grounds
        };
        let state = |soft: &'static str| {
            vec![
                "background",
                "chrome",
                "tray",
                "surface",
                "popover",
                "muted",
                soft,
            ]
        };
        vec![
            (
                "foreground",
                g(&["selected", "sidebar_selected"]),
                7.,
                "body text",
            ),
            ("muted_foreground", g(&[]), 4.5, "secondary text"),
            (
                "selected_foreground",
                vec!["selected", "sidebar_selected"],
                7.,
                "text on a selection",
            ),
            (
                "link",
                vec![
                    "background",
                    "chrome",
                    "tray",
                    "surface",
                    "popover",
                    "muted",
                    "ai_soft",
                ],
                4.5,
                "links",
            ),
            (
                "primary_foreground",
                vec!["primary", "primary_hover", "primary_active"],
                4.5,
                "label on primary",
            ),
            (
                "primary",
                vec![
                    "background",
                    "chrome",
                    "tray",
                    "sidebar",
                    "sidebar_selected",
                    "selected",
                ],
                3.,
                "accent mark",
            ),
            (
                "ring",
                vec![
                    "background",
                    "chrome",
                    "tray",
                    "sidebar",
                    "surface",
                    "popover",
                ],
                3.,
                "focus ring",
            ),
            (
                "input_border",
                vec!["background", "chrome", "tray", "surface", "popover"],
                3.,
                "control edge",
            ),
            ("danger", state("danger_soft"), 4.5, "failure text"),
            ("warning", state("warning_soft"), 4.5, "decision text"),
            ("success", state("success_soft"), 4.5, "done text"),
            (
                "info",
                vec!["background", "tray", "surface", "info_soft"],
                4.5,
                "information text",
            ),
            (
                "danger_foreground",
                vec!["danger"],
                4.5,
                "label on a destructive button",
            ),
            (
                "foreground",
                vec![
                    "danger_soft",
                    "warning_soft",
                    "success_soft",
                    "info_soft",
                    "ai_soft",
                ],
                7.,
                "text on a tinted fill",
            ),
            (
                "ai_foreground",
                vec!["ai_soft", "tray", "background"],
                4.5,
                "AI label",
            ),
            (
                "recording",
                vec!["background", "chrome", "tray", "sidebar"],
                4.5,
                "Recording word",
            ),
            ("meter_me", vec!["meter_track"], 3., "meter"),
            ("meter_others", vec!["meter_track"], 3., "meter"),
            (
                "section_today",
                vec!["sidebar", "sidebar_selected"],
                3.,
                "section icon",
            ),
            (
                "section_meetings",
                vec!["sidebar", "sidebar_selected"],
                3.,
                "section icon",
            ),
            (
                "section_people",
                vec!["sidebar", "sidebar_selected"],
                3.,
                "section icon",
            ),
            (
                "section_review",
                vec!["sidebar", "sidebar_selected"],
                3.,
                "section icon",
            ),
            ("badge_foreground", vec!["badge"], 4.5, "Review count"),
            ("visor_foreground", vec!["visor"], 7., "visor words"),
            ("visor_accent", vec!["visor"], 4.5, "visor marks"),
            ("visor_recording", vec!["visor"], 4.5, "visor recording"),
            (
                "find_foreground",
                vec!["find"],
                7.,
                "text in a find highlight",
            ),
            ("visor_accent", vec!["visor"], 3., "focus ring on the visor"),
            (
                "primary",
                vec!["selected", "card", "sidebar_selected"],
                3.,
                "selected-row bar",
            ),
            ("on_person", vec!["me"], 4.5, "initials on the Me disc"),
        ]
    }

    fn roles(name: &str) -> &'static [(&'static str, u32)] {
        palette_gen::ROLES
            .iter()
            .find(|(variant, _)| *variant == name)
            .unwrap_or_else(|| panic!("no roles for {name}"))
            .1
    }

    fn role(name: &str, roles: &[(&str, u32)], key: &str) -> u32 {
        roles
            .iter()
            .find(|(role, _)| *role == key)
            .unwrap_or_else(|| panic!("{name} lacks the role {key}"))
            .1
    }

    const VARIANTS: [&str; 4] = [
        theme::LIGHT,
        theme::DARK,
        theme::LIGHT_CONTRAST,
        theme::DARK_CONTRAST,
    ];

    #[test]
    fn every_pair_meets_its_contrast() {
        let mut failures = Vec::new();
        for name in VARIANTS {
            let roles = roles(name);
            let of = |key: &str| role(name, roles, key);
            for (fg, grounds, minimum, what) in rules() {
                for bg in grounds {
                    let ratio = contrast(of(fg), of(bg));
                    if ratio < minimum {
                        failures.push(format!(
                            "{name}: {fg} on {bg} is {ratio:.2}:1, needs {minimum}:1 ({what})"
                        ));
                    }
                }
            }
            for n in 1..=PERSON_HUES {
                let hue = of(&format!("hue_{n}"));
                let initials = contrast(of("on_person"), hue);
                if initials < 4.5 {
                    failures.push(format!(
                        "{name}: initials on person hue {n} are {initials:.2}:1"
                    ));
                }
                for ground in ["background", "tray"] {
                    let disc = contrast(hue, of(ground));
                    if disc < 4.5 {
                        failures.push(format!("{name}: person hue {n} on {ground} is {disc:.2}:1"));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn states_stay_apart_under_colour_vision_deficiencies() {
        let deficiencies = [
            ("protanopia", PROTAN),
            ("deuteranopia", DEUTAN),
            ("tritanopia", TRITAN),
        ];
        let pairs = |a: &'static str, b: &'static str, c: &'static str| [(a, b), (a, c), (b, c)];
        for name in VARIANTS {
            let roles = roles(name);
            let of = |key: &str| role(name, roles, key);
            // Increase Contrast trades a little hue for contrast.
            let minimum = if name.contains("more contrast") {
                15.
            } else {
                20.
            };
            for (deficiency, matrix) in &deficiencies {
                for (a, b) in pairs("danger", "warning", "success") {
                    let d = distance(seen(of(a), matrix), seen(of(b), matrix));
                    assert!(
                        d >= minimum,
                        "{name}: {a} and {b} are {d:.1} apart under {deficiency}, need {minimum}"
                    );
                }
                for (a, b) in pairs("danger_soft", "warning_soft", "success_soft") {
                    let d = distance(seen(of(a), matrix), seen(of(b), matrix));
                    assert!(
                        d >= 5.,
                        "{name}: {a} and {b} are {d:.1} apart under {deficiency}, need 5"
                    );
                }
            }
        }
    }

    #[test]
    fn the_brand_table_and_the_theme_come_from_the_same_roles() {
        let set: serde_json::Value = serde_json::from_str(theme::THEME_JSON).unwrap();
        for (name, dark, more) in [
            (theme::LIGHT, false, false),
            (theme::DARK, true, false),
            (theme::LIGHT_CONTRAST, false, true),
            (theme::DARK_CONTRAST, true, true),
        ] {
            let roles = roles(name);
            let of = |key: &str| role(name, roles, key);
            let variant = set["themes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|theme| theme["name"] == name)
                .unwrap();
            let key = |key: &str| {
                let hex = variant["colors"][key].as_str().unwrap();
                u32::from_str_radix(&hex[1..7], 16).unwrap()
            };
            for (json, css) in [
                ("background", "tray"),
                ("sidebar.background", "sidebar"),
                ("sidebar.accent.background", "sidebar_selected"),
                ("popover.background", "popover"),
                ("group_box.background", "card"),
                ("foreground", "foreground"),
                ("muted.foreground", "muted_foreground"),
                ("border", "border"),
                ("input.border", "input_border"),
                ("primary.background", "primary"),
                ("list.active.background", "selected"),
                ("ring", "ring"),
                ("link", "link"),
                ("danger.background", "danger"),
                ("warning.background", "warning"),
                ("success.background", "success"),
                ("info.background", "info"),
            ] {
                assert_eq!(key(json), of(css), "{name}: {json} is not --{css}");
            }
            let p = palette(dark, more);
            assert_eq!(p.card, of("card"), "{name}");
            assert_eq!(p.recording, of("recording"), "{name}");
            assert_eq!(p.visor, of("visor"), "{name}");
            assert_eq!(p.hues[0], of("hue_1"), "{name}");
        }
    }

    #[test]
    fn increase_contrast_strengthens_state_text_edges_and_links() {
        let light = roles(theme::LIGHT);
        let more = roles(theme::LIGHT_CONTRAST);
        // The light state text of color.css's prefers-contrast block.
        assert_eq!(role("light", more, "danger"), 0x880033);
        assert_eq!(role("light", more, "warning"), 0x674103);
        assert_eq!(role("light", more, "success"), 0x035065);
        for (name, base, more) in [
            ("light", light, more),
            ("dark", roles(theme::DARK), roles(theme::DARK_CONTRAST)),
        ] {
            let tray = role(name, base, "tray");
            for key in ["border", "input_border", "link", "ring", "muted_foreground"] {
                assert!(
                    contrast(role(name, more, key), tray) > contrast(role(name, base, key), tray),
                    "{name}: Increase Contrast does not strengthen {key}"
                );
            }
        }
        // The brand roles follow: the light recording mark darkens.
        assert_ne!(
            palette_gen::LIGHT_CONTRAST.recording,
            palette_gen::LIGHT.recording
        );
    }

    #[test]
    fn shadows_are_plain_layers_and_dark_has_only_a_line() {
        let light = Brand::from_palette(palette_gen::LIGHT);
        let dark = Brand::from_palette(palette_gen::DARK);
        assert_eq!(light.card_shadow().len(), 2);
        assert!(!light.primary_shadow().is_empty());
        assert!(
            dark.primary_shadow().is_empty(),
            "no primary shadow in dark"
        );
        let [ref line] = dark.card_shadow()[..] else {
            panic!("dark cards carry one 1px shadow");
        };
        assert_eq!(line.offset.y, px(1.));
        assert_eq!(RADIUS_CARD, px(16.));
    }

    #[test]
    fn person_hues_wrap_into_range() {
        let brand = Brand::from_palette(palette_gen::LIGHT);
        assert_eq!(brand.person_hue(1), colour(palette_gen::LIGHT.hues[0]));
        assert_eq!(brand.person_hue(7), colour(palette_gen::LIGHT.hues[6]));
        assert_eq!(brand.person_hue(8), brand.person_hue(1));
        assert_eq!(brand.person_hue(0), brand.person_hue(1));
    }

    #[test]
    fn the_contrast_formula_and_the_simulation_are_the_references() {
        assert!((contrast(0x000000, 0xffffff) - 21.0).abs() < 1e-9);
        // #767676 is the lightest grey that passes 4.5:1 on white.
        assert!((contrast(0x767676, 0xffffff) - 4.54).abs() < 0.005);
        // Neutral greys look the same under every deficiency.
        for matrix in [PROTAN, DEUTAN, TRITAN] {
            let grey = seen(0x808080, &matrix);
            assert!(distance(grey, lab(channels(0x808080).map(linear))) < 1.0);
        }
    }
}
