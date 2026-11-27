//! The taskbar stores command words in its links so an older task never
//! changes meaning when the list is rebuilt.
use std::path::Path;

use broscribe_platform::shell::{DockItem, JumpList};
use broscribe_platform::{Error, Result, plain_verbatim};
use windows::Win32::{
    Foundation::PROPERTYKEY,
    System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, StructuredStorage::PROPVARIANT},
    UI::Shell::{
        Common::{IObjectArray, IObjectCollection},
        DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW,
        PropertiesSystem::IPropertyStore,
        SetCurrentProcessExplicitAppUserModelID, ShellLink,
    },
};
use windows::core::{GUID, HSTRING, Interface};

use crate::shell::profile_app_id;
use crate::{Com, error, install_location::quote};

const TITLE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xf29f85e0_4ff9_1068_ab91_08002b27b3d9),
    pid: 2,
};

pub struct WinJumpList {
    app_id: HSTRING,
    executable: HSTRING,
    overrides: String,
}

impl WinJumpList {
    pub fn new(data_dir: Option<&Path>, library: Option<&Path>) -> Result<Self> {
        let app_id = HSTRING::from(profile_app_id(data_dir));
        let exe = std::env::current_exe()?;
        let executable = HSTRING::from(plain_verbatim(&exe).as_os_str());
        let mut overrides = String::new();
        for (flag, value) in [("--dock-data-dir", data_dir), ("--library", library)] {
            if let Some(value) = value {
                let path = value.to_str().and_then(quote).ok_or_else(|| {
                    Error::InvalidInput("A jump-list path cannot be passed to the app".into())
                })?;
                overrides.push_str(&format!(" {flag} {path}"));
            }
        }
        // Do this on the application thread before opening its first window.
        unsafe { SetCurrentProcessExplicitAppUserModelID(&app_id) }
            .map_err(|e| error("setting BroScribe taskbar identity", e))?;
        Ok(Self {
            app_id,
            executable,
            overrides,
        })
    }

    fn task(&self, item: &DockItem) -> windows::core::Result<IShellLinkW> {
        let args = HSTRING::from(format!(
            "--dock-action {}{}",
            item.command.word(),
            self.overrides
        ));
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&self.executable)?;
            link.SetIconLocation(&self.executable, 0)?;
            link.SetArguments(&args)?;
            let properties: IPropertyStore = link.cast()?;
            properties.SetValue(&TITLE, &PROPVARIANT::from(item.label))?;
            properties.Commit()?;
            Ok(link)
        }
    }

    fn publish(&self, items: &[DockItem]) -> windows::core::Result<()> {
        unsafe {
            let list: ICustomDestinationList =
                CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
            list.SetAppID(&self.app_id)?;
            let tasks: IObjectCollection =
                CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
            for item in items {
                tasks.AddObject(&self.task(item)?)?;
            }
            let mut slots = 0;
            let _: IObjectArray = list.BeginList(&mut slots)?;
            if let Err(error) = list.AddUserTasks(&tasks).and_then(|_| list.CommitList()) {
                let _ = list.AbortList();
                return Err(error);
            }
            Ok(())
        }
    }
}

impl JumpList for WinJumpList {
    fn replace(&self, items: &[DockItem]) -> Result<()> {
        let _com = Com::new()?;
        self.publish(items)
            .map_err(|e| error("publishing the BroScribe jump list", e))
    }
}
