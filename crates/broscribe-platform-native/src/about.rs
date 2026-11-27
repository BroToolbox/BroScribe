//! The About reader (SH24) for the current OS.

use std::sync::Arc;

use broscribe_platform::about::PlatformAboutReader;

use crate::{NativePlatform, imp};

impl NativePlatform {
    /// Reads the running build's About facts: version, build, architecture,
    /// packaging, code signature and shipped licences. `app_version` is the
    /// calling executable's `env!("CARGO_PKG_VERSION")`. Unlike
    /// [`NativePlatform::updater`] it inspects any location, including
    /// source builds and temporary or translocated bundles. Blocking: call
    /// [`PlatformAboutReader::read`] off the UI loop.
    pub fn about_reader(app_version: impl Into<String>) -> Arc<dyn PlatformAboutReader> {
        #[cfg(target_os = "macos")]
        {
            Arc::new(imp::MacAboutReader::new(app_version))
        }
        #[cfg(target_os = "linux")]
        {
            Arc::new(imp::LinuxAboutReader::new(app_version))
        }
        #[cfg(target_os = "windows")]
        {
            Arc::new(imp::WinAboutReader::new(app_version))
        }
    }
}
