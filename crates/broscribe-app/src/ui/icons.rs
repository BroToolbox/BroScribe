#[cfg(test)]
use gpui::AssetSource as _;
use gpui::assets::IconName;
use gpui::component::Icon;
use gpui::{Styled as _, rems};

pub(crate) const DISCLOSURE_CLOSED: IconName = IconName::ChevronRight;
pub(crate) const DISCLOSURE_OPEN: IconName = IconName::ChevronDown;
pub(crate) const DISMISS: IconName = IconName::Close;
pub(crate) const DONE: IconName = IconName::Check;
pub(crate) const SEARCH: IconName = IconName::Search;
pub(crate) const PLAY: IconName = IconName::Play;
pub(crate) const PAUSE: IconName = IconName::Pause;
pub(crate) const STOP: IconName = IconName::Square;
pub(crate) const ADD: IconName = IconName::Plus;

pub(crate) const LIBRARY: IconName = IconName::BookOpen;
pub(crate) const REVIEW_QUEUE: IconName = IconName::Inbox;
pub(crate) const PEOPLE: IconName = IconName::Users;
pub(crate) const SIDEBAR: IconName = IconName::PanelLeft;
pub(crate) const MAIN_MENU: IconName = IconName::Menu;

pub(crate) fn icon(name: IconName) -> Icon {
    Icon::new(name).size(rems(1.)).flex_none()
}

#[cfg(test)]
#[test]
fn every_action_icon_has_a_bundled_lucide_asset() {
    for name in [
        DISCLOSURE_CLOSED,
        DISCLOSURE_OPEN,
        DISMISS,
        DONE,
        SEARCH,
        PLAY,
        PAUSE,
        STOP,
        ADD,
        LIBRARY,
        REVIEW_QUEUE,
        PEOPLE,
        SIDEBAR,
        MAIN_MENU,
    ] {
        let path = name.path();
        assert!(
            super::assets::Assets.load(&path).unwrap().is_some(),
            "{path}"
        );
    }
}
