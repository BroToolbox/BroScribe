//! Opens web links with `NSWorkspace` (http and https only).

use broscribe_platform::system::check_web_url;
use broscribe_platform::{
    BrowserLauncher, Error, HostPlatform, Result, SettingsPane, SystemSettingsOpener,
};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};

pub struct MacBrowserLauncher;

impl BrowserLauncher for MacBrowserLauncher {
    fn open(&self, url: &str) -> Result<()> {
        check_web_url(url)?;
        let ns_url = NSURL::URLWithString(&NSString::from_str(url))
            .ok_or_else(|| Error::InvalidInput(format!("malformed URL {url}")))?;
        // Re-check what Foundation parsed, not only the input text.
        let scheme = ns_url.scheme().map(|s| s.to_string()).unwrap_or_default();
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Err(Error::InvalidInput(format!("refusing to open {url}")));
        }
        if NSWorkspace::sharedWorkspace().openURL(&ns_url) {
            Ok(())
        } else {
            Err(Error::Internal(format!("the system could not open {url}")))
        }
    }
}

pub struct MacSystemSettings;

fn settings_url(pane: SettingsPane) -> Option<&'static str> {
    Some(match pane {
        SettingsPane::Microphone => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        }
        SettingsPane::SystemAudio => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        SettingsPane::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
        SettingsPane::Notifications => {
            "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
        }
        SettingsPane::LoginItems => {
            "x-apple.systempreferences:com.apple.LoginItems-Settings.extension"
        }
        SettingsPane::Taskbar => return None,
    })
}

impl SystemSettingsOpener for MacSystemSettings {
    fn host_platform(&self) -> HostPlatform {
        HostPlatform::MacOs
    }

    fn supports(&self, pane: SettingsPane) -> bool {
        settings_url(pane).is_some()
    }

    fn notifications_permission(&self) -> Option<&'static str> {
        crate::notifier::authorization_status()
    }

    fn open(&self, pane: SettingsPane) -> Result<()> {
        let url = settings_url(pane)
            .ok_or_else(|| Error::Unsupported("system settings pane unavailable".into()))?;
        let url = NSURL::URLWithString(&NSString::from_str(url))
            .ok_or_else(|| Error::Internal("invalid System Settings destination".into()))?;
        if NSWorkspace::sharedWorkspace().openURL(&url) {
            Ok(())
        } else {
            Err(Error::Internal("couldn't open System Settings".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_non_web_urls_without_opening_anything() {
        for bad in [
            "file:///etc/hosts",
            "javascript:alert(1)",
            "x-apple.systempreferences:",
        ] {
            assert!(MacBrowserLauncher.open(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_use_fixed_privacy_destinations() {
        assert_eq!(
            settings_url(SettingsPane::Microphone),
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        );
        assert_eq!(
            settings_url(SettingsPane::SystemAudio),
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        );
        assert_eq!(
            settings_url(SettingsPane::Accessibility),
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        );
        assert_eq!(
            settings_url(SettingsPane::Notifications),
            Some("x-apple.systempreferences:com.apple.Notifications-Settings.extension")
        );
        assert_eq!(
            settings_url(SettingsPane::LoginItems),
            Some("x-apple.systempreferences:com.apple.LoginItems-Settings.extension")
        );
        assert!(!MacSystemSettings.supports(SettingsPane::Taskbar));
    }
}
