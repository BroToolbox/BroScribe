//! BroScribe's text styles, expressed in rems so they follow reading size.

use std::sync::{Arc, LazyLock};

use gpui::{FontFeatures, FontWeight, Styled, rems};

use super::fonts;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRole {
    Display,
    Title,
    Section,
    Reading,
    Body,
    Label,
    Caption,
    Micro,
    Visor,
    /// The caller supplies the theme's platform monospace family.
    Mono,
}

impl TextRole {
    fn metrics(self) -> (f32, f32, FontWeight, Option<&'static str>) {
        match self {
            Self::Display => (2.25, 2.5, FontWeight::SEMIBOLD, Some(fonts::DISPLAY)),
            Self::Title => (1.375, 1.75, FontWeight::SEMIBOLD, Some(fonts::DISPLAY)),
            Self::Section => (1., 1.375, FontWeight::SEMIBOLD, Some(fonts::SANS)),
            Self::Reading => (0.9375, 1.375, FontWeight::NORMAL, Some(fonts::SANS)),
            Self::Body => (0.875, 1.25, FontWeight::NORMAL, Some(fonts::SANS)),
            Self::Label => (0.875, 1.25, FontWeight::MEDIUM, Some(fonts::SANS)),
            Self::Caption => (0.75, 1., FontWeight::NORMAL, Some(fonts::SANS)),
            // 12 px at Standard keeps Small (87.5%) above the 10 px floor.
            Self::Micro => (0.75, 0.875, FontWeight::MEDIUM, Some(fonts::SANS)),
            Self::Visor => (0.8125, 1., FontWeight::SEMIBOLD, Some(fonts::SANS)),
            Self::Mono => (0.8125, 1.125, FontWeight::NORMAL, None),
        }
    }
}

pub trait TextRoleExt: Styled + Sized {
    fn text_role(self, role: TextRole) -> Self {
        let (size, line_height, weight, family) = role.metrics();
        let element = self
            .text_size(rems(size))
            .line_height(rems(line_height))
            .font_weight(weight);
        match family {
            Some(family) => element.font_family(family),
            None => element,
        }
    }
}

impl<T: Styled> TextRoleExt for T {}

/// Align changing numbers without switching them to a different family.
pub(crate) fn tabular<T: Styled>(element: T) -> T {
    static FEATURES: LazyLock<FontFeatures> =
        LazyLock::new(|| FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
    element.font_features(FEATURES.clone())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn assert_tabular(element: &mut impl Styled) {
        assert_eq!(
            element
                .text_style()
                .font_features
                .as_ref()
                .unwrap()
                .0
                .as_ref(),
            &[("tnum".into(), 1)]
        );
    }

    #[test]
    fn desktop_ramp_uses_bundled_weights_and_scalable_metrics() {
        let cases = [
            (
                TextRole::Display,
                36.,
                40.,
                FontWeight::SEMIBOLD,
                Some(fonts::DISPLAY),
            ),
            (
                TextRole::Title,
                22.,
                28.,
                FontWeight::SEMIBOLD,
                Some(fonts::DISPLAY),
            ),
            (
                TextRole::Section,
                16.,
                22.,
                FontWeight::SEMIBOLD,
                Some(fonts::SANS),
            ),
            (
                TextRole::Reading,
                15.,
                22.,
                FontWeight::NORMAL,
                Some(fonts::SANS),
            ),
            (
                TextRole::Body,
                14.,
                20.,
                FontWeight::NORMAL,
                Some(fonts::SANS),
            ),
            (
                TextRole::Label,
                14.,
                20.,
                FontWeight::MEDIUM,
                Some(fonts::SANS),
            ),
            (
                TextRole::Caption,
                12.,
                16.,
                FontWeight::NORMAL,
                Some(fonts::SANS),
            ),
            (
                TextRole::Micro,
                12.,
                14.,
                FontWeight::MEDIUM,
                Some(fonts::SANS),
            ),
            (
                TextRole::Visor,
                13.,
                16.,
                FontWeight::SEMIBOLD,
                Some(fonts::SANS),
            ),
            (TextRole::Mono, 13., 18., FontWeight::NORMAL, None),
        ];
        for (role, size, line, weight, family) in cases {
            let (actual_size, actual_line, actual_weight, actual_family) = role.metrics();
            assert_eq!(
                (
                    actual_size * 16.,
                    actual_line * 16.,
                    actual_weight,
                    actual_family
                ),
                (size, line, weight, family),
                "{role:?}"
            );
            assert!(actual_size * 14. >= 10., "{role:?} at Small");
        }
    }
}
