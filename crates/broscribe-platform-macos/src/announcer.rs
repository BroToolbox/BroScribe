//! Direct VoiceOver announcements via AppKit (`NSAccessibilityAnnouncementRequested`).
//!
//! AccessKit's macOS adapter only emits live-region announcements from a node's
//! `value`, whereas BroScribe's `Role::Status` and `Role::Alert` nodes carry
//! their text as `label` only (§4.2) so VoiceOver does not read them twice on
//! navigation. Posting `NSAccessibilityAnnouncementRequestedNotification` here
//! keeps that rule intact while ensuring live status and alert changes speak.

use broscribe_platform::{AnnouncementPriority, Announcer};
use objc2::MainThreadMarker;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_app_kit::{
    NSAccessibilityAnnouncementKey, NSAccessibilityAnnouncementRequestedNotification,
    NSAccessibilityPostNotificationWithUserInfo, NSAccessibilityPriorityKey,
    NSAccessibilityPriorityLevel, NSApplication,
};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};

/// Posts `NSAccessibilityAnnouncementRequestedNotification` on the main thread.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacAnnouncer;

fn priority_level(priority: AnnouncementPriority) -> NSAccessibilityPriorityLevel {
    match priority {
        AnnouncementPriority::Medium => NSAccessibilityPriorityLevel::Medium,
        AnnouncementPriority::High => NSAccessibilityPriorityLevel::High,
    }
}

impl Announcer for MacAnnouncer {
    fn announce(&self, text: &str, priority: AnnouncementPriority) {
        if text.trim().is_empty() {
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        let key_window = app.keyWindow();
        let main_window = app.mainWindow();
        let target: &AnyObject = match (&key_window, &main_window) {
            (Some(window), _) => window.as_ref(),
            (None, Some(window)) => window.as_ref(),
            (None, None) => app.as_ref(),
        };

        let user_info = NSMutableDictionary::<_, AnyObject>::new();
        let announcement = NSString::from_str(text);
        let level = NSNumber::new_isize(priority_level(priority).0);
        // SAFETY: the keys are AppKit's documented announcement userInfo keys,
        // `announcement` is an NSString, `level` is an NSNumber of
        // `NSAccessibilityPriorityLevel`, and we are on the main thread.
        unsafe {
            user_info.setObject_forKey(
                &*announcement,
                ProtocolObject::from_ref(NSAccessibilityAnnouncementKey),
            );
            user_info.setObject_forKey(
                &*level,
                ProtocolObject::from_ref(NSAccessibilityPriorityKey),
            );
            NSAccessibilityPostNotificationWithUserInfo(
                target,
                NSAccessibilityAnnouncementRequestedNotification,
                Some(&*user_info),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_announcement_priority_to_appkit_levels() {
        assert_eq!(
            priority_level(AnnouncementPriority::Medium),
            NSAccessibilityPriorityLevel::Medium
        );
        assert_eq!(
            priority_level(AnnouncementPriority::High),
            NSAccessibilityPriorityLevel::High
        );
    }
}
