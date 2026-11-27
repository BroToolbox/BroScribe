//! System display accessibility settings (ACC-32): the XDG Settings
//! portal's `org.freedesktop.appearance` `contrast` key, where 1 asks for
//! higher contrast and 0 states no preference. Linux desktops have no shared
//! Reduce Transparency or forced-colours setting, so those stay off.

use std::time::Duration;

use broscribe_platform::DisplayAccessibility;
use zbus::zvariant::Value;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "contrast";
/// A portal that doesn't answer leaves the ordinary theme.
const METHOD_TIMEOUT: Duration = Duration::from_secs(2);

/// Reads the portal's contrast setting over the session bus. Without a
/// session bus, a portal or the key, the setting reads as off.
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxDisplayAccessibility;

impl DisplayAccessibility for LinuxDisplayAccessibility {
    fn increase_contrast(&self) -> bool {
        futures::executor::block_on(read_contrast()).is_ok_and(|value| higher_contrast(&value))
    }

    fn reduce_transparency(&self) -> bool {
        false
    }

    fn forced_colors(&self) -> bool {
        false
    }
}

/// `ReadOne` (Settings version 2), else the older `Read`, whose reply wraps
/// the value in one more variant.
async fn read_contrast() -> zbus::Result<zbus::zvariant::OwnedValue> {
    let connection = zbus::connection::Builder::session()?
        .method_timeout(METHOD_TIMEOUT)
        .build()
        .await?;
    let proxy = zbus::Proxy::new(&connection, PORTAL, PORTAL_PATH, SETTINGS).await?;
    match proxy.call("ReadOne", &(NAMESPACE, KEY)).await {
        Ok(value) => Ok(value),
        Err(_) => proxy.call("Read", &(NAMESPACE, KEY)).await,
    }
}

/// Whether a portal reply asks for higher contrast, through any number of
/// variant wrappers.
fn higher_contrast(value: &Value<'_>) -> bool {
    match value {
        Value::U32(contrast) => *contrast == 1,
        Value::Value(inner) => higher_contrast(inner),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_higher_contrast_reply_turns_contrast_on() {
        assert!(higher_contrast(&Value::U32(1)));
        assert!(!higher_contrast(&Value::U32(0)));
        assert!(!higher_contrast(&Value::U32(7)));
        let wrapped = Value::Value(Box::new(Value::Value(Box::new(Value::U32(1)))));
        assert!(higher_contrast(&wrapped), "the older Read reply");
        assert!(!higher_contrast(&Value::Str("1".into())));
    }
}
