//! System display accessibility settings (ACC-32): a Windows contrast theme
//! from `SystemParametersInfoW(SPI_GETHIGHCONTRAST)`. Windows has no separate
//! Increase Contrast or Reduce Transparency setting BroScribe follows; a
//! contrast theme turns on the more-contrast variant and opaque fills.

use broscribe_platform::DisplayAccessibility;
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

/// Reads whether a contrast theme is on. A failed read reports off.
#[derive(Clone, Copy, Debug, Default)]
pub struct WinDisplayAccessibility;

impl DisplayAccessibility for WinDisplayAccessibility {
    fn increase_contrast(&self) -> bool {
        false
    }

    fn reduce_transparency(&self) -> bool {
        false
    }

    fn forced_colors(&self) -> bool {
        let mut contrast = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        // SAFETY: `contrast` is a live, correctly sized HIGHCONTRASTW for the
        // duration of the call, as SPI_GETHIGHCONTRAST requires.
        let read = unsafe {
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                contrast.cbSize,
                Some((&mut contrast as *mut HIGHCONTRASTW).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        read.is_ok() && contrast.dwFlags.contains(HCF_HIGHCONTRASTON)
    }
}
