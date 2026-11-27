//! The BroScribe theme: the design system's light and dark variants of
//! gpui-component's theme, following the system appearance, and their "more
//! contrast" variants, following the system's display accessibility settings
//! (ACC-32: macOS Increase Contrast and Reduce Transparency, a Windows
//! contrast theme, the XDG portal's contrast setting).
//!
//! `assets/themes/broscribe.json` is generated, with `ui::palette_gen`, by
//! `docs/redesign/tools/palette.py --theme-json` from the design system's
//! `color.css`: every window is a desk, every block of content a card, and
//! one colour means one thing (violet is BroScribe, crimson recording or
//! failure, amber a decision, teal done, blue information). Every key is a
//! gpui-component theme key, so views keep reading `cx.theme()`. Roles the
//! theme has no key for (cards, soft state fills, people's hues, the visor)
//! live in `ui::brand`. Tests here and there hold every variant to the
//! contrast the design system promises.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use broscribe_core::config::ThemeChoice;
use broscribe_platform::{DisplayAccessibility, DisplayPreferences};
use gpui::base::input::CursorBlink;
use gpui::base::{LayoutDirection, ReduceMotionPreference, set_reduce_motion_preference};
use gpui::component::scroll::ScrollbarMode;
use gpui::component::{Theme, ThemeConfig, ThemeMode, ThemeRegistry};
use gpui::{App, Global, Window, WindowAppearance};

/// The four variants, as gpui-component's theme-set JSON.
pub const THEME_JSON: &str = include_str!("../../assets/themes/broscribe.json");
/// The variant used under a light system appearance.
pub const LIGHT: &str = "BroScribe Light";
/// The variant used under a dark system appearance.
pub const DARK: &str = "BroScribe Dark";
/// The light variant under Increase Contrast or a contrast theme: stronger
/// edges and secondary text, the text colour as the focus ring, and opaque
/// overlays and drop targets.
pub const LIGHT_CONTRAST: &str = "BroScribe Light (more contrast)";
/// The dark variant under Increase Contrast or a contrast theme.
pub const DARK_CONTRAST: &str = "BroScribe Dark (more contrast)";
const DISPLAY_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Initialises gpui-kit, switches its motion off (including steady carets and
/// always-visible scroll bars) and installs the BroScribe theme in the
/// system's current appearance. Call once, before the first window opens; each
/// window then follows appearance changes through [`follow_appearance`].
pub fn init(cx: &mut App) {
    gpui::init(cx);
    // States swap instantly: every gpui-component transition (a Switch's
    // thumb, a Checkbox's mark, a TabBar's indicator) jumps to its end state
    // instead of requesting frames. gpui-kit otherwise follows the system's
    // preference, which allows motion by default; the explicit choice also
    // keeps a later system reading from switching motion back on.
    set_reduce_motion_preference(ReduceMotionPreference::Reduce, cx);
    super::fonts::register(cx);
    // The interface is English only; a right-to-left system language would
    // otherwise mirror gpui-component's controls beside unmirrored views.
    LayoutDirection::Ltr.set_global(cx);
    install(cx);
    // A focused field otherwise blinks at 2 Hz indefinitely, and Scrolling
    // scroll bars run an idle hide timer. Setting both on gpui-component's
    // global theme before syncing keeps them projected onto `gpui::base::Theme`.
    {
        let theme = Theme::global_mut(cx);
        theme.cursor_blink = CursorBlink::steady();
        theme.scrollbar_mode = ScrollbarMode::Always;
    }
    Theme::sync_system_appearance(None, cx);
}

/// Registers all variants and installs the regular light and dark palettes.
/// A theme file that fails to parse leaves gpui-component's
/// defaults in place: the app stays usable, only unbranded.
fn install(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    if let Err(error) = registry.load_themes_from_str(THEME_JSON) {
        tracing::error!("the BroScribe theme does not parse: {error}");
        return;
    }
    let themes = registry.themes();
    let (Some(light), Some(dark)) = (themes.get(LIGHT).cloned(), themes.get(DARK).cloned()) else {
        tracing::error!("the BroScribe theme lacks its light or dark variant");
        return;
    };
    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;
}

/// Applies `window`'s appearance now and whenever the system switches
/// between light and dark. GPUI redraws every window when the theme changes;
/// nothing repaints otherwise.
pub fn follow_appearance(window: &mut Window, cx: &mut App) {
    sync_appearance(window, cx);
    window.observe_window_appearance(sync_appearance).detach();
}

fn sync_appearance(window: &mut Window, cx: &mut App) {
    let mode =
        chosen_mode(theme_choice(cx)).unwrap_or_else(|| ThemeMode::from(window.appearance()));
    apply_mode(mode, cx);
}

/// Installs `mode`'s palette (in the current display settings) if another
/// one is showing. GPUI then redraws every window once.
fn apply_mode(mode: ThemeMode, cx: &mut App) {
    let theme = Theme::global(cx);
    if theme.mode != mode {
        let config = if mode.is_dark() {
            theme.dark_theme.clone()
        } else {
            theme.light_theme.clone()
        };
        Theme::update(cx, |theme| apply_palette(theme, &config));
    }
}

/// The Appearance setting the windows follow (`[appearance] theme`).
#[derive(Default)]
struct ChoiceState {
    choice: ThemeChoice,
}

impl Global for ChoiceState {}

/// The Appearance setting in force: System until the config says otherwise.
pub fn theme_choice(cx: &App) -> ThemeChoice {
    cx.try_global::<ChoiceState>()
        .map(|state| state.choice)
        .unwrap_or_default()
}

/// The mode a choice forces, or `None` to follow the system.
fn chosen_mode(choice: ThemeChoice) -> Option<ThemeMode> {
    match choice {
        ThemeChoice::System => None,
        ThemeChoice::Light => Some(ThemeMode::Light),
        ThemeChoice::Dark => Some(ThemeMode::Dark),
    }
}

/// Makes every window light, dark, or follow the system (Settings ›
/// General › Appearance). On macOS the choice also sets the application's
/// appearance, so window chrome matches; elsewhere GPUI has no such
/// override and only BroScribe's own colours change. The same choice again
/// costs nothing; a change updates the theme at most once.
pub fn apply_theme_choice(choice: ThemeChoice, cx: &mut App) {
    if cx.has_global::<ChoiceState>() && theme_choice(cx) == choice {
        return;
    }
    cx.set_global(ChoiceState { choice });
    cx.set_window_appearance(match choice {
        ThemeChoice::System => None,
        ThemeChoice::Light => Some(WindowAppearance::Light),
        ThemeChoice::Dark => Some(WindowAppearance::Dark),
    });
    if !cx.has_global::<Theme>() {
        return;
    }
    let mode = chosen_mode(choice).unwrap_or_else(|| ThemeMode::from(cx.window_appearance()));
    apply_mode(mode, cx);
}

/// The BroScribe palettes share one type ramp. Keep the active text size
/// when applying colors so the reading-size observer does not redraw again.
fn apply_palette(theme: &mut Theme, config: &Rc<ThemeConfig>) {
    let font_size = theme.font_size;
    let mono_font_size = theme.mono_font_size;
    theme.apply_config(config);
    theme.font_size = font_size;
    theme.mono_font_size = mono_font_size;
}

/// The display settings the installed variants were chosen for.
#[derive(Default)]
struct DisplayState {
    preferences: DisplayPreferences,
}

impl Global for DisplayState {}

/// The light and dark variant names `preferences` call for.
pub fn variant_names(preferences: DisplayPreferences) -> (&'static str, &'static str) {
    if preferences.more_contrast() {
        (LIGHT_CONTRAST, DARK_CONTRAST)
    } else {
        (LIGHT, DARK)
    }
}

/// The display settings the theme follows now.
pub fn display_preferences(cx: &App) -> DisplayPreferences {
    cx.try_global::<DisplayState>()
        .map(|state| state.preferences)
        .unwrap_or_default()
}

/// Whether the more-contrast variants are installed (`ui::brand` picks its
/// matching palette from this).
pub fn more_contrast(cx: &App) -> bool {
    display_preferences(cx).more_contrast()
}

/// `base` with the opaque overlay and drop target of `opaque`, for Reduce
/// Transparency on its own. gpui-component always draws list selections and
/// text selections translucent (it clamps their alpha), so those stay.
fn with_opaque_fills(base: &Rc<ThemeConfig>, opaque: &ThemeConfig) -> Rc<ThemeConfig> {
    let mut config = ThemeConfig::clone(base);
    config.colors.overlay = opaque.colors.overlay.clone();
    config.colors.drop_target = opaque.colors.drop_target.clone();
    Rc::new(config)
}

/// `base` with focus and selection in the user's accent colour, where
/// [`super::brand::accent_roles`] accepts it for this mode.
fn with_accent(base: &Rc<ThemeConfig>, preferences: DisplayPreferences) -> Rc<ThemeConfig> {
    let dark = base.mode.is_dark();
    let Some(accent) =
        super::brand::accent_roles(preferences.accent(dark), dark, preferences.more_contrast())
    else {
        return base.clone();
    };
    let hex = |colour: u32| -> gpui::SharedString { format!("#{colour:06x}").into() };
    let mut config = ThemeConfig::clone(base);
    let colors = &mut config.colors;
    if !preferences.more_contrast() {
        colors.ring = Some(hex(accent.mark));
    }
    colors.list_active = Some(hex(accent.selected));
    colors.list_active_border = Some(hex(accent.mark));
    colors.table_active = Some(hex(accent.selected));
    colors.table_active_border = Some(hex(accent.mark));
    // Text selection: the accent at the alpha the violet uses.
    colors.selection =
        Some(format!("#{:06x}{}", accent.mark, if dark { "47" } else { "3d" }).into());
    Rc::new(config)
}

/// The light and dark configurations for `preferences`, if the theme file
/// parsed.
fn configs(
    preferences: DisplayPreferences,
    cx: &App,
) -> Option<(Rc<ThemeConfig>, Rc<ThemeConfig>)> {
    let themes = ThemeRegistry::global(cx).themes();
    let (light, dark) = variant_names(preferences);
    let (mut light, mut dark) = (themes.get(light)?.clone(), themes.get(dark)?.clone());
    if preferences.opaque() && !preferences.more_contrast() {
        light = with_opaque_fills(&light, themes.get(LIGHT_CONTRAST)?);
        dark = with_opaque_fills(&dark, themes.get(DARK_CONTRAST)?);
    }
    Some((
        with_accent(&light, preferences),
        with_accent(&dark, preferences),
    ))
}

/// Installs the variants `preferences` call for, in the current light or
/// dark mode, and returns whether anything changed. The same settings cost
/// nothing; a change updates the theme once, which redraws each window once.
pub fn apply_display_preferences(preferences: DisplayPreferences, cx: &mut App) -> bool {
    let previous = display_preferences(cx);
    if previous == preferences || !cx.has_global::<Theme>() {
        return false;
    }
    if previous.more_contrast() == preferences.more_contrast()
        && previous.opaque() == preferences.opaque()
        && previous.light_accent == preferences.light_accent
        && previous.dark_accent == preferences.dark_accent
    {
        cx.set_global(DisplayState { preferences });
        return false;
    }
    let Some((light, dark)) = configs(preferences, cx) else {
        tracing::error!("the BroScribe theme lacks a variant for the display settings");
        return false;
    };
    cx.set_global(DisplayState { preferences });
    Theme::update(cx, |theme| {
        let current = if theme.mode.is_dark() {
            dark.clone()
        } else {
            light.clone()
        };
        theme.light_theme = light;
        theme.dark_theme = dark;
        apply_palette(theme, &current);
    });
    true
}

/// Reads system settings at startup and while a window is open. Cocoa
/// sources read on GPUI's loop. At most one platform read is in flight.
pub fn follow_display_accessibility(source: Arc<dyn DisplayAccessibility>, cx: &mut App) {
    let executor = cx.background_executor().clone();
    let main_thread = source.requires_main_thread();
    cx.spawn(async move |cx| {
        let mut read = true;
        loop {
            if read {
                if main_thread {
                    cx.update(|cx| apply_display_preferences(source.preferences(), cx));
                } else {
                    let source = source.clone();
                    let preferences = executor.spawn(async move { source.preferences() }).await;
                    cx.update(|cx| apply_display_preferences(preferences, cx));
                }
            }
            executor.timer(DISPLAY_POLL_INTERVAL).await;
            read = cx.update(|cx| !cx.windows().is_empty());
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui::component::ActiveTheme as _;

    #[gpui::test]
    fn both_variants_load_and_follow_the_mode(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            assert_eq!(cx.theme().theme_name().as_ref(), LIGHT);
            let light_background = cx.theme().background;
            Theme::change(ThemeMode::Dark, None, cx);
            assert_eq!(cx.theme().theme_name().as_ref(), DARK);
            assert_ne!(cx.theme().background, light_background);
            // The radii are the design system's, not gpui-component's.
            assert_eq!(cx.theme().radius, gpui::px(8.));
            assert_eq!(cx.theme().radius_lg, gpui::px(12.));
        });
    }

    /// ACC-32: the display settings pick the variants in either mode, an
    /// unchanged reading changes nothing, and a change updates the theme
    /// once (GPUI then redraws each window once) and keeps motion off.
    #[gpui::test]
    fn display_settings_switch_to_the_contrast_variants_with_one_update(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeDisplayAccessibility;
        use std::cell::Cell;

        let updates = Rc::new(Cell::new(0));
        let _watch = cx.update(|cx| {
            init(cx);
            let updates = updates.clone();
            cx.observe_global::<Theme>(move |_| updates.set(updates.get() + 1))
        });
        let off = DisplayPreferences::default();
        let contrast = DisplayPreferences {
            increase_contrast: true,
            ..off
        };
        let forced = DisplayPreferences {
            forced_colors: true,
            ..off
        };
        let transparency = DisplayPreferences {
            reduce_transparency: true,
            ..off
        };
        let opaque_overlay = |cx: &App| cx.theme().overlay.a >= 1.0;
        cx.update(|cx| {
            Theme::change(ThemeMode::Light, None, cx);
            assert!(!opaque_overlay(cx), "the ordinary overlay is translucent");
        });
        cx.run_until_parked();

        // The same settings: nothing to do, no theme update.
        let before = updates.get();
        assert!(!cx.update(|cx| apply_display_preferences(off, cx)));
        cx.run_until_parked();
        assert_eq!(updates.get(), before);

        for (preferences, light, dark, opaque, changed) in [
            (contrast, LIGHT_CONTRAST, DARK_CONTRAST, true, true),
            (forced, LIGHT_CONTRAST, DARK_CONTRAST, true, false),
            (transparency, LIGHT, DARK, true, true),
            (off, LIGHT, DARK, false, true),
        ] {
            let before = updates.get();
            cx.update(|cx| {
                assert_eq!(apply_display_preferences(preferences, cx), changed);
                assert_eq!(display_preferences(cx), preferences);
                assert_eq!(cx.theme().theme_name().as_ref(), light);
                assert_eq!(opaque_overlay(cx), opaque, "{preferences:?}");
            });
            cx.run_until_parked();
            assert_eq!(
                updates.get(),
                before + usize::from(changed),
                "{preferences:?}"
            );
            cx.update(|cx| {
                assert!(cx.reduce_motion());
                assert_eq!(cx.theme().cursor_blink, CursorBlink::steady());
                // The appearance still switches between the chosen pair.
                Theme::change(ThemeMode::Dark, None, cx);
                assert_eq!(cx.theme().theme_name().as_ref(), dark);
                assert_eq!(opaque_overlay(cx), opaque, "{preferences:?} dark");
                Theme::change(ThemeMode::Light, None, cx);
            });
        }

        // Following a source reads it off the loop and applies the result.
        // With a window open, it sees later changes without a focus change.
        struct WindowView;
        impl gpui::Render for WindowView {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div()
            }
        }
        cx.add_window(|_, _| WindowView);
        let largest = cx.update(|cx| {
            super::super::apply_reading_size(broscribe_core::config::ReadingSize::Largest, cx);
            cx.theme().font_size
        });
        let source = FakeDisplayAccessibility::new(contrast);
        let before = updates.get();
        cx.update(|cx| follow_display_accessibility(Arc::new(source.clone()), cx));
        cx.run_until_parked();
        assert_eq!(updates.get(), before + 1);
        assert_eq!(source.reads(), 1);
        cx.update(|cx| {
            assert!(more_contrast(cx));
            assert_eq!(cx.theme().theme_name().as_ref(), LIGHT_CONTRAST);
            assert_eq!(cx.theme().font_size, largest);
        });
        let before = updates.get();
        cx.executor().advance_clock(DISPLAY_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            updates.get(),
            before,
            "an unchanged reading redraws nothing"
        );
        source.set(off);
        cx.executor().advance_clock(DISPLAY_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(source.reads(), 3);
        cx.update(|cx| {
            assert!(!more_contrast(cx));
            assert_eq!(cx.theme().theme_name().as_ref(), LIGHT);
            assert_eq!(cx.theme().font_size, largest);
        });
    }

    /// macOS accent colour (*verify* on a Mac set to a colour other than
    /// Multicolor): focus and selection follow it; buttons stay violet; an
    /// accent too light to show focus, and Multicolor, keep violet.
    #[gpui::test]
    fn focus_and_selection_follow_the_accent_colour(cx: &mut TestAppContext) {
        use gpui::Hsla;
        let hsla = |colour: u32| -> Hsla { gpui::rgb(colour).into() };
        cx.update(|cx| {
            init(cx);
            Theme::change(ThemeMode::Light, None, cx);
        });
        let violet = cx.update(|cx| cx.theme().ring);
        let violet_button = cx.update(|cx| cx.theme().primary);
        // macOS blue.
        let blue = DisplayPreferences {
            light_accent: Some(0x007aff),
            dark_accent: Some(0x409cff),
            ..DisplayPreferences::default()
        };
        cx.update(|cx| {
            assert!(apply_display_preferences(blue, cx));
            assert_eq!(cx.theme().ring, hsla(0x007aff));
            assert_eq!(cx.theme().list_active_border, hsla(0x007aff));
            assert_ne!(cx.theme().list_active, hsla(0x007aff));
            assert_eq!(cx.theme().primary, violet_button, "buttons stay violet");
            let brand = super::super::brand::brand(cx);
            assert_eq!(brand.selection_mark, hsla(0x007aff));
            assert_eq!(brand.primary, violet_button);
            // Body text stays at 7:1 on the accent's selection tint, in
            // both modes.
            for dark in [false, true] {
                let roles =
                    super::super::brand::accent_roles(blue.accent(dark), dark, false).unwrap();
                let p = if dark {
                    super::super::palette_gen::DARK
                } else {
                    super::super::palette_gen::LIGHT
                };
                assert!(super::super::brand::contrast(p.foreground, roles.selected) >= 7.0);
            }
            Theme::change(ThemeMode::Dark, None, cx);
            assert_eq!(cx.theme().ring, hsla(0x409cff));
            assert_eq!(cx.theme().list_active_border, hsla(0x409cff));
            assert_eq!(
                super::super::brand::brand(cx).selection_mark,
                hsla(0x409cff)
            );
            Theme::change(ThemeMode::Light, None, cx);
        });
        // A pale yellow can't show focus on the light desk: violet stays.
        let yellow = DisplayPreferences {
            light_accent: Some(0xffe55c),
            ..DisplayPreferences::default()
        };
        cx.update(|cx| {
            assert!(apply_display_preferences(yellow, cx));
            assert_eq!(cx.theme().ring, violet);
            assert!(super::super::brand::accent_roles(Some(0xffe55c), false, false).is_none());
            // Multicolor: no accent.
            assert!(apply_display_preferences(DisplayPreferences::default(), cx));
            assert_eq!(cx.theme().ring, violet);
            assert_eq!(super::super::brand::brand(cx).selection_mark, violet);
        });
    }

    #[gpui::test]
    fn the_accent_preserves_the_more_contrast_ring(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init(cx);
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                Theme::change(mode, None, cx);
                let contrast = DisplayPreferences {
                    increase_contrast: true,
                    ..DisplayPreferences::default()
                };
                apply_display_preferences(contrast, cx);
                let ring = cx.theme().ring;
                let mark = cx.theme().list_active_border;
                let accent = DisplayPreferences {
                    light_accent: Some(0x007aff),
                    dark_accent: Some(0x409cff),
                    ..contrast
                };
                assert!(apply_display_preferences(accent, cx));
                assert_eq!(cx.theme().ring, ring);
                assert_ne!(cx.theme().list_active_border, mark);
            }
        });
    }

    #[gpui::test]
    fn init_switches_component_motion_off(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // GPUI's own default allows motion, so the flag is init's doing.
            assert!(!cx.reduce_motion());
            init(cx);
            let assert_motionless = |cx: &App| {
                assert!(cx.reduce_motion());
                assert_eq!(cx.theme().cursor_blink, CursorBlink::steady());
                assert_eq!(
                    gpui::base::Theme::global(cx).cursor_blink,
                    CursorBlink::steady()
                );
                assert_eq!(cx.theme().scrollbar_mode, ScrollbarMode::Always);
                assert_eq!(
                    gpui::base::Theme::global(cx).scrollbar.mode(),
                    ScrollbarMode::Always
                );
            };
            assert_motionless(cx);
            Theme::change(ThemeMode::Light, None, cx);
            assert_motionless(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            assert_motionless(cx);
            Theme::sync_system_appearance(None, cx);
            assert_motionless(cx);
        });
    }

    #[test]
    fn every_colour_key_is_one_gpui_component_reads() {
        // A misspelt key would be ignored silently and leave a default colour.
        let set: serde_json::Value = serde_json::from_str(THEME_JSON).unwrap();
        let themes = set["themes"].as_array().unwrap();
        assert_eq!(
            themes.len(),
            4,
            "light, dark and their more-contrast variants"
        );
        for theme in themes {
            let config: gpui::component::ThemeConfig =
                serde_json::from_value(theme.clone()).unwrap();
            let known = serde_json::to_value(&config.colors).unwrap();
            let known = known.as_object().unwrap();
            for key in theme["colors"].as_object().unwrap().keys() {
                assert!(
                    known.get(key).is_some_and(|value| !value.is_null()),
                    "{} sets {key}, which gpui-component does not read",
                    config.name
                );
            }
        }
    }

    /// An opaque sRGB colour, each channel in `0.0..=1.0`.
    type Rgb = [f64; 3];

    /// `key`'s colour in `variant`. Contrast needs an opaque `#rrggbb`: a
    /// colour with alpha has no ratio until it is blended over a ground.
    fn colour(variant: &serde_json::Value, key: &str) -> Rgb {
        let name = &variant["name"];
        let hex = variant["colors"][key]
            .as_str()
            .unwrap_or_else(|| panic!("{name} does not set {key}"));
        assert!(
            hex.len() == 7 && hex.starts_with('#'),
            "{name}: {key} is {hex}, not an opaque #rrggbb"
        );
        [1, 3, 5].map(|at| {
            let channel = u8::from_str_radix(&hex[at..at + 2], 16)
                .unwrap_or_else(|_| panic!("{name}: {key} is {hex}, not an opaque #rrggbb"));
            f64::from(channel) / 255.0
        })
    }

    /// WCAG 2.1 relative luminance.
    fn luminance(colour: Rgb) -> f64 {
        let [r, g, b] = colour.map(|c| {
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// WCAG 2.1 contrast ratio, from 1.0 to 21.0.
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// `colour` drawn at `alpha` over `ground`, as GPUI composites it (on
    /// the encoded channels).
    fn blend(colour: Rgb, alpha: f64, ground: Rgb) -> Rgb {
        [0, 1, 2].map(|i| colour[i] * alpha + ground[i] * (1.0 - alpha))
    }

    /// WCAG's minimum for text.
    const TEXT: f64 = 4.5;
    /// WCAG's minimum for edges, marks and the focus indicator.
    const NON_TEXT: f64 = 3.0;

    /// Every variant, by name: light, dark and their more-contrast ones.
    fn variants() -> [(&'static str, serde_json::Value); 4] {
        let set: serde_json::Value = serde_json::from_str(THEME_JSON).unwrap();
        [LIGHT, DARK, LIGHT_CONTRAST, DARK_CONTRAST].map(|name| {
            let variant = set["themes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|theme| theme["name"] == name)
                .unwrap_or_else(|| panic!("the theme lacks {name}"));
            (name, variant.clone())
        })
    }

    #[test]
    fn both_variants_meet_the_contrast_the_design_system_promises() {
        let mut failures = Vec::new();
        for (name, variant) in variants() {
            let of = |key: &str| colour(&variant, key);
            let mut check = |what: String, ratio: f64, needs: f64| {
                // Shown with `--nocapture`, to compare with palette.py --all.
                println!("{name}: {what}: {ratio:.2}:1 (needs {needs}:1)");
                if ratio < needs {
                    failures.push(format!("{name}: {what} is {ratio:.2}:1, needs {needs}:1"));
                }
            };

            // Text on every ground it is set on: the desk (background, title
            // bar), cards, the sidebar, menus and quiet fills.
            for text in ["foreground", "muted.foreground"] {
                for ground in [
                    "background",
                    "group_box.background",
                    "sidebar.background",
                    "sidebar.accent.background",
                    "popover.background",
                    "muted.background",
                    "title_bar.background",
                ] {
                    check(
                        format!("{text} on {ground}"),
                        contrast(of(text), of(ground)),
                        TEXT,
                    );
                }
            }
            // A selected row: its text, and the violet bar and focused edge
            // that mark it beyond its tint.
            check(
                "foreground on list.active.background".into(),
                contrast(of("foreground"), of("list.active.background")),
                7.0,
            );
            check(
                "list.active.border on list.active.background".into(),
                contrast(of("list.active.border"), of("list.active.background")),
                NON_TEXT,
            );

            // Status words, which gpui-component also writes in these colours.
            for tone in ["danger", "success", "warning", "info"] {
                for ground in ["background", "group_box.background", "popover.background"] {
                    check(
                        format!("{tone} on {ground}"),
                        contrast(of(&format!("{tone}.background")), of(ground)),
                        TEXT,
                    );
                }
            }

            // Solid button labels on their fill at rest, under the pointer and
            // pressed: default `button.*` plus every named tone
            // (`button.<tone>.*` and solid `<tone>.*` surfaces).
            let mut button_families = vec!["button".to_string()];
            for tone in [
                "primary",
                "secondary",
                "danger",
                "success",
                "warning",
                "info",
            ] {
                button_families.push(format!("button.{tone}"));
                button_families.push(tone.to_string());
            }
            for keys in button_families {
                let label = of(&format!("{keys}.foreground"));
                for state in ["background", "hover.background", "active.background"] {
                    check(
                        format!("{keys}.foreground on {keys}.{state}"),
                        contrast(label, of(&format!("{keys}.{state}"))),
                        TEXT,
                    );
                }
            }

            // Control edges, the unchecked switch track among them, on the
            // desk and on cards.
            for edge in ["input.border", "switch.background"] {
                for ground in ["background", "group_box.background"] {
                    check(
                        format!("{edge} on {ground}"),
                        contrast(of(edge), of(ground)),
                        NON_TEXT,
                    );
                }
            }

            // The focus ring. gpui-component paints a focused control's border
            // in the full ring colour and adds a halo at half strength outside
            // it, so the solid border carries the 3:1.
            for ground in [
                "background",
                "group_box.background",
                "sidebar.background",
                "popover.background",
            ] {
                check(
                    format!("ring on {ground}"),
                    contrast(of("ring"), of(ground)),
                    NON_TEXT,
                );
            }

            // ACC-32: the more-contrast variants reach AAA for secondary text
            // and draw overlays and drop targets opaque (`colour` refuses
            // alpha), which Reduce Transparency borrows on its own.
            if [LIGHT_CONTRAST, DARK_CONTRAST].contains(&name) {
                for ground in ["background", "group_box.background", "sidebar.background"] {
                    check(
                        format!("muted.foreground on {ground} (more contrast)"),
                        contrast(of("muted.foreground"), of(ground)),
                        7.0,
                    );
                }
                for opaque in ["overlay", "drop_target.background"] {
                    let _ = of(opaque);
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn more_contrast_strengthens_every_edge_and_the_ring() {
        let [light, dark, light_more, dark_more] = variants();
        for ((name, base), (_, more)) in [(light, light_more), (dark, dark_more)] {
            let ground = colour(&base, "background");
            for key in [
                "border",
                "input.border",
                "sidebar.border",
                "title_bar.border",
                "window.border",
                "ring",
                "link",
                "list.active.background",
            ] {
                let before = contrast(colour(&base, key), ground);
                let after = contrast(colour(&more, key), ground);
                assert!(
                    after > before,
                    "{name}: {key} is {after:.2}:1 under more contrast, {before:.2}:1 without"
                );
            }
        }
    }

    #[test]
    fn status_buttons_change_fill_under_the_pointer_and_when_pressed() {
        // SYS-17: each step of a status button raises its label's contrast.
        for (name, variant) in variants() {
            for tone in ["danger", "success", "warning", "info"] {
                for keys in [format!("button.{tone}"), tone.to_string()] {
                    let label = colour(&variant, &format!("{keys}.foreground"));
                    let [rest, hover, active] =
                        ["background", "hover.background", "active.background"]
                            .map(|state| colour(&variant, &format!("{keys}.{state}")));
                    assert!(
                        rest != hover && hover != active && rest != active,
                        "{name}: {keys} has the same fill in two of its states"
                    );
                    let rest_ratio = contrast(label, rest);
                    let hover_ratio = contrast(label, hover);
                    let active_ratio = contrast(label, active);
                    assert!(
                        rest_ratio < hover_ratio && hover_ratio < active_ratio,
                        "{name}: {keys} label contrast must rise rest < hover < active \
                         (got {rest_ratio:.2}, {hover_ratio:.2}, {active_ratio:.2})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_contrast_formula_is_wcags() {
        let (black, white) = ([0.0; 3], [1.0; 3]);
        assert!((contrast(black, white) - 21.0).abs() < 1e-9);
        assert!((contrast(white, white) - 1.0).abs() < 1e-9);
        // #767676 is the lightest grey that passes 4.5:1 on white (4.54:1).
        let grey = [f64::from(0x76_u8) / 255.0; 3];
        assert!((contrast(grey, white) - 4.54).abs() < 0.005);
        // Half black over white is the grey halfway between (3.98:1).
        let halo = blend(black, 0.5, white);
        assert_eq!(halo, [0.5; 3]);
        assert!((contrast(halo, white) - 3.98).abs() < 0.005);
    }
}
