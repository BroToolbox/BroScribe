use broscribe_platform::{
    MenuItemSpec, Result, StatusClickSender, StatusIcon, StatusItem, TaskbarState,
};
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
};
use windows::{
    Win32::{
        Foundation::{
            ERROR_SUCCESS, GetLastError, HWND, LPARAM, LRESULT, POINT, SetLastError, WPARAM,
        },
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            LibraryLoader::GetModuleHandleW,
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        },
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};
const CALLBACK: u32 = WM_APP + 63;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

/// The status glyph per state, as `.ico` files for a light taskbar (the
/// theme's dark ink) and for a dark one (its light ink); rendered with the
/// app's brand files (crates/broscribe-app/assets/brand,
/// `cargo run -p broscribe-app --example render_brand`).
fn tray_ico(icon: StatusIcon, light_taskbar: bool) -> &'static [u8] {
    macro_rules! ico {
        ($name:literal) => {
            include_bytes!(concat!(
                "../../broscribe-app/assets/brand/windows/",
                $name,
                ".ico"
            ))
        };
    }
    match (icon, light_taskbar) {
        (StatusIcon::Idle, true) => ico!("tray-idle-light"),
        (StatusIcon::Idle, false) => ico!("tray-idle-dark"),
        (StatusIcon::Recording, true) => ico!("tray-recording-light"),
        (StatusIcon::Recording, false) => ico!("tray-recording-dark"),
        (StatusIcon::Paused, true) => ico!("tray-paused-light"),
        (StatusIcon::Paused, false) => ico!("tray-paused-dark"),
    }
}

/// The image in an `.ico` file for `size` pixels: the smallest at least as
/// large, else the largest. `None` when the file is malformed.
fn ico_image(ico: &[u8], size: u32) -> Option<&[u8]> {
    if ico.get(..4)? != [0, 0, 1, 0] {
        return None;
    }
    let count = usize::from(u16::from_le_bytes(ico.get(4..6)?.try_into().ok()?));
    let mut best: Option<(u32, &[u8])> = None;
    for index in 0..count {
        let entry = ico.get(6 + 16 * index..6 + 16 * (index + 1))?;
        let side = match entry[0] {
            0 => 256,
            side => u32::from(side),
        };
        let length = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        let image = ico.get(offset..offset.checked_add(length)?)?;
        let better = match best {
            None => true,
            Some((current, _)) if current >= size => side >= size && side < current,
            Some((current, _)) => side > current,
        };
        if better {
            best = Some((side, image));
        }
    }
    best.map(|(_, image)| image)
}

/// Whether the taskbar is light (Settings › Personalization › Colours ›
/// "Choose your default Windows mode"). Unknown reads as dark, Windows 11's
/// default.
fn light_taskbar() -> bool {
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    status.is_ok() && value != 0
}

/// A notification-area icon handle; ours are destroyed after use (the shell
/// keeps its own copy), the shared system icon never is.
struct Icon {
    handle: HICON,
    owned: bool,
}
impl Icon {
    /// `icon`'s glyph at the small-icon size, in the taskbar's ink, or the
    /// system's application icon if it cannot be made.
    fn status(icon: StatusIcon) -> Self {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let made = ico_image(tray_ico(icon, light_taskbar()), size as u32).and_then(|image| {
            unsafe {
                CreateIconFromResourceEx(image, true, 0x0003_0000, size, size, LR_DEFAULTCOLOR)
            }
            .ok()
        });
        match made {
            Some(handle) => Self {
                handle,
                owned: true,
            },
            None => Self {
                handle: unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default(),
                owned: false,
            },
        }
    }
}
impl Drop for Icon {
    fn drop(&mut self) {
        if self.owned {
            unsafe {
                let _ = DestroyIcon(self.handle);
            }
        }
    }
}

struct TaskbarButton {
    state: Cell<TaskbarState>,
    created: u32,
}

const TASKBAR_SUBCLASS: usize = 1;

fn paint_taskbar_state(hwnd: HWND, state: TaskbarState) -> Result<()> {
    let (icon, description) = match state {
        TaskbarState::Recording => (Some(Icon::status(StatusIcon::Recording)), w!("Recording")),
        TaskbarState::Paused => (Some(Icon::status(StatusIcon::Paused)), w!("Paused")),
        TaskbarState::None => (None, PCWSTR::null()),
    };
    // SAFETY: GPUI has already initialized OLE on this UI thread. Explorer
    // copies the glyph during the call, so the temporary HICON can drop here.
    unsafe {
        let taskbar: ITaskbarList3 = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| crate::error("opening taskbar overlay", error))?;
        taskbar
            .HrInit()
            .map_err(|error| crate::error("initializing taskbar overlay", error))?;
        taskbar
            .SetOverlayIcon(
                hwnd,
                icon.as_ref().map_or(HICON::default(), |icon| icon.handle),
                description,
            )
            .map_err(|error| crate::error("setting taskbar overlay", error))
    }
}

unsafe extern "system" fn taskbar_subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    dispatch_window_message(
        || {
            // SAFETY: the subclass owns this allocation from installation
            // until destruction. Copy the state before a call can reenter.
            unsafe {
                if message == WM_NCDESTROY {
                    let _ = RemoveWindowSubclass(hwnd, Some(taskbar_subclass), id);
                    drop(Box::from_raw(data as *mut TaskbarButton));
                } else {
                    let button = &*(data as *const TaskbarButton);
                    if message == button.created {
                        let state = button.state.get();
                        let _ = paint_taskbar_state(hwnd, state);
                    }
                }
            }
            None
        },
        || unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    )
}

fn dispatch_window_message(
    dispatch: impl FnOnce() -> Option<LRESULT>,
    default: impl FnOnce() -> LRESULT,
) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(dispatch)) {
        Ok(Some(result)) => result,
        _ => default(),
    }
}

fn set_taskbar_state(hwnd: HWND, state: TaskbarState) -> Result<()> {
    // SAFETY: registration, updates and destruction all happen on the UI
    // thread that owns this HWND. The callback dispatches to GPUI afterwards.
    unsafe {
        let mut data = 0usize;
        if GetWindowSubclass(
            hwnd,
            Some(taskbar_subclass),
            TASKBAR_SUBCLASS,
            Some(&mut data),
        )
        .as_bool()
        {
            (*(data as *const TaskbarButton)).state.set(state);
        } else {
            let created = RegisterWindowMessageW(w!("TaskbarButtonCreated"));
            if created == 0 {
                return Err(crate::error(
                    "registering taskbar button message",
                    std::io::Error::last_os_error(),
                ));
            }
            let _ = ChangeWindowMessageFilterEx(hwnd, created, MSGFLT_ALLOW, None);
            let button = Box::into_raw(Box::new(TaskbarButton {
                state: Cell::new(state),
                created,
            }));
            if !SetWindowSubclass(
                hwnd,
                Some(taskbar_subclass),
                TASKBAR_SUBCLASS,
                button as usize,
            )
            .as_bool()
            {
                drop(Box::from_raw(button));
                return Err(crate::error(
                    "watching taskbar button",
                    std::io::Error::last_os_error(),
                ));
            }
        }
    }
    // An already visible window may already have received TaskbarButtonCreated.
    paint_taskbar_state(hwnd, state)
}

/// Whether a broadcast means the icon should be drawn again: the light or
/// dark mode changed (`ImmersiveColorSet`), or the display or its scale did.
fn redraws_icon(message: u32, lparam: LPARAM) -> bool {
    match message {
        WM_DISPLAYCHANGE | WM_DPICHANGED => true,
        WM_SETTINGCHANGE if lparam.0 != 0 => unsafe { PCWSTR(lparam.0 as *const u16).to_string() }
            .is_ok_and(|area| area == "ImmersiveColorSet"),
        _ => false,
    }
}

/// State shared by the owner and the window procedure. It is only ever
/// borrowed briefly and shared (`&Tray`): the popup runs a modal loop that
/// can re-enter the procedure and the owner's setters. Keep an owning
/// reference across dispatch and release interior borrows before the loop.
struct Tray {
    clicks: StatusClickSender,
    menu: Menu,
    title: RefCell<String>,
    icon: Cell<StatusIcon>,
    /// `TaskbarCreated`: Explorer (re)started and dropped every icon.
    taskbar_created: u32,
}

/// Menu items plus whether a popup of them is open.
#[derive(Default)]
struct Menu {
    items: RefCell<Vec<MenuItemSpec>>,
    open: Cell<bool>,
}
impl Menu {
    /// A snapshot to show, unless a popup is already open (re-entrant
    /// request). Pair with [`Menu::close`].
    fn open(&self) -> Option<Vec<MenuItemSpec>> {
        (!self.open.replace(true)).then(|| self.items.borrow().clone())
    }
    fn close(&self) {
        self.open.set(false);
    }
    /// Safe while a popup is open: it keeps its own snapshot.
    fn replace(&self, items: Vec<MenuItemSpec>) {
        self.items.replace(items);
    }
}

/// The menu item chosen by `TrackPopupMenu` (1-based command), if usable.
fn chosen(items: &[MenuItemSpec], selected: usize) -> Option<String> {
    let item = items.get(selected.checked_sub(1)?)?;
    (item.enabled && !item.is_separator()).then(|| item.id.clone())
}

/// The 1-based command id of the enabled default menu item, if any.
fn default_command(items: &[MenuItemSpec]) -> Option<u32> {
    let target = broscribe_platform::ui::default_menu_item(items)?;
    items
        .iter()
        .position(|item| std::ptr::eq(item, target))
        .and_then(|index| u32::try_from(index + 1).ok())
}

fn activate(tray: &Tray) {
    let id = {
        let items = tray.menu.items.borrow();
        broscribe_platform::ui::default_menu_item(&items).map(|item| item.id.clone())
    };
    if let Some(id) = id {
        let _ = tray.clicks.unbounded_send(id);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    dispatch_window_message(
        || {
            if message == WM_NCCREATE {
                // SAFETY: CreateWindowExW synchronously passes the caller's live Rc.
                let owner = unsafe {
                    let create = &*(lparam.0 as *const CREATESTRUCTW);
                    &*create.lpCreateParams.cast::<Rc<Tray>>()
                };
                let state = Rc::into_raw(owner.clone());
                // Zero is also the expected previous value for a new window.
                unsafe {
                    SetLastError(ERROR_SUCCESS);
                    if SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) == 0
                        && GetLastError() != ERROR_SUCCESS
                    {
                        drop(Rc::from_raw(state));
                        return Some(LRESULT(0));
                    }
                }
            }
            if message == WM_NCDESTROY {
                // SAFETY: USERDATA owns one reference. Clear it before releasing
                // it to prevent reentrant use.
                let state = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *const Tray;
                if !state.is_null() {
                    unsafe { drop(Rc::from_raw(state)) };
                }
            } else if let Some(tray) = tray_for_dispatch(hwnd) {
                if message == CALLBACK {
                    let event = lparam.0 as u32 & 0xffff;
                    if event == WM_CONTEXTMENU {
                        show_menu(hwnd, &tray);
                    } else if event == NIN_SELECT || event == NIN_KEYSELECT {
                        activate(&tray);
                    }
                    return Some(LRESULT(0));
                }
                if redraws_icon(message, lparam) {
                    let _ = notify(hwnd, &tray, NIM_MODIFY);
                }
                if message == tray.taskbar_created && tray.taskbar_created != 0 {
                    let _ = add_icon(hwnd, &tray);
                    return Some(LRESULT(0));
                }
            }
            None
        },
        || unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    )
}

fn tray_for_dispatch(hwnd: HWND) -> Option<Rc<Tray>> {
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Tray;
    if state.is_null() {
        return None;
    }
    // SAFETY: USERDATA owns the reference. Both messages execute on this
    // thread, so WM_NCDESTROY can only release it after this clone.
    unsafe {
        Rc::increment_strong_count(state);
        Some(Rc::from_raw(state))
    }
}

fn show_menu(hwnd: HWND, tray: &Tray) {
    // A snapshot: the setters may replace the items while the menu is open.
    let Some(items) = tray.menu.open() else {
        return;
    };
    let selected = unsafe { track(hwnd, &items) };
    tray.menu.close();
    if let Some(id) = selected.and_then(|selected| chosen(&items, selected)) {
        let _ = tray.clicks.unbounded_send(id);
    }
}

/// Shows the popup for `items`; the chosen 1-based command, if any.
unsafe fn track(hwnd: HWND, items: &[MenuItemSpec]) -> Option<usize> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (index, item) in items.iter().enumerate() {
            let text = crate::wide(&item.title);
            let flags = if item.is_separator() {
                MF_SEPARATOR
            } else if item.enabled {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            if AppendMenuW(menu, flags, index + 1, PCWSTR(text.as_ptr())).is_err() {
                let _ = DestroyMenu(menu);
                return None;
            }
        }
        if let Some(default) = default_command(items) {
            let _ = SetMenuDefaultItem(menu, default, 0);
        }
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let selected = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        Some(selected)
    }
}

/// The icon's data and the icon handle it points to, which must outlive
/// the `Shell_NotifyIconW` call.
fn icon_data(hwnd: HWND, tray: &Tray) -> Result<(NOTIFYICONDATAW, Icon)> {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: CALLBACK,
        ..Default::default()
    };
    let icon = Icon::status(tray.icon.get());
    data.hIcon = icon.handle;
    let tip = crate::wide(tray.title.borrow().as_str());
    let len = tip.len().saturating_sub(1).min(data.szTip.len() - 1);
    data.szTip[..len].copy_from_slice(&tip[..len]);
    Ok((data, icon))
}

fn notify(hwnd: HWND, tray: &Tray, operation: NOTIFY_ICON_MESSAGE) -> Result<()> {
    let (data, _icon) = icon_data(hwnd, tray)?;
    if unsafe { Shell_NotifyIconW(operation, &data) }.as_bool() {
        Ok(())
    } else {
        Err(crate::error(
            "updating the notification area item",
            std::io::Error::last_os_error(),
        ))
    }
}

/// Adds the icon with version-4 callbacks (also after Explorer restarts).
fn add_icon(hwnd: HWND, tray: &Tray) -> Result<()> {
    notify(hwnd, tray, NIM_ADD)?;
    let (mut data, _icon) = icon_data(hwnd, tray)?;
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    if unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
        Ok(())
    } else {
        Err(crate::error(
            "setting tray callback version",
            std::io::Error::last_os_error(),
        ))
    }
}

/// Create and mutate on GPUI's UI thread. Its existing Win32 pump dispatches
/// this hidden window's messages; this type never starts a thread or loop.
/// The window is a hidden top-level window (not message-only) so it also
/// receives the `TaskbarCreated` broadcast and restores the icon itself.
pub struct WinStatusItem {
    hwnd: HWND,
    tray: Rc<Tray>,
    _main_thread: PhantomData<Rc<()>>,
}
fn create_tray_window(tray: &Rc<Tray>) -> Result<HWND> {
    let class = crate::wide("BroScribe.Platform.Tray");
    unsafe {
        let module = GetModuleHandleW(None).map_err(|e| crate::error("tray module", e))?;
        let window_class = WNDCLASSW {
            hInstance: module.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            lpfnWndProc: Some(window_proc),
            ..Default::default()
        };
        if RegisterClassW(&window_class) == 0
            && windows::Win32::Foundation::GetLastError()
                != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS
        {
            return Err(crate::error(
                "registering tray window class",
                std::io::Error::last_os_error(),
            ));
        }
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            Some(std::ptr::from_ref(tray).cast()),
        )
        .map_err(|e| crate::error("creating tray window", e))
    }
}
impl WinStatusItem {
    pub fn new(clicks: StatusClickSender) -> Result<Self> {
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let tray = Rc::new(Tray {
            clicks,
            menu: Menu::default(),
            title: RefCell::new("BroScribe".into()),
            icon: Cell::new(StatusIcon::Idle),
            taskbar_created,
        });
        let hwnd = create_tray_window(&tray)?;
        if taskbar_created != 0 {
            // An elevated process still hears Explorer's restart.
            let _ =
                unsafe { ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, None) };
        }
        let item = Self {
            hwnd,
            tray,
            _main_thread: PhantomData,
        };
        add_icon(item.hwnd, &item.tray)?;
        Ok(item)
    }
}
impl StatusItem for WinStatusItem {
    fn set_title(&mut self, title: &str) -> Result<()> {
        self.tray.title.replace(title.into());
        notify(self.hwnd, &self.tray, NIM_MODIFY)
    }
    /// The waveform glyph for `icon`; the tooltip says the state in words.
    fn set_icon(&mut self, icon: StatusIcon) -> Result<()> {
        if self.tray.icon.replace(icon) == icon {
            return Ok(());
        }
        notify(self.hwnd, &self.tray, NIM_MODIFY)
    }
    fn set_menu(&mut self, items: Vec<MenuItemSpec>) -> Result<()> {
        // An open popup keeps its snapshot; the next one shows these.
        self.tray.menu.replace(items);
        Ok(())
    }
    fn set_badge_count(&mut self, _n: usize) -> Result<()> {
        Ok(())
    }

    fn set_taskbar_state(
        &mut self,
        window: &dyn broscribe_platform::overlay::HasWindowHandle,
        state: TaskbarState,
    ) -> Result<()> {
        set_taskbar_state(crate::overlay::hwnd(window)?, state)
    }
}
impl Drop for WinStatusItem {
    fn drop(&mut self) {
        if let Ok((data, _icon)) = icon_data(self.hwnd, &self.tray) {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            }
        }
        // WM_NCDESTROY releases the window's reference; active dispatches
        // retain their own reference until they return.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_dispatch_falls_through_after_a_panic() {
        let result = dispatch_window_message(|| panic!("synthetic callback panic"), || LRESULT(9));
        assert_eq!(result, LRESULT(9));
    }

    #[test]
    fn dispatch_keeps_the_tray_alive_after_reentrant_window_destruction() {
        let (clicks, _receiver) = futures::channel::mpsc::unbounded();
        let owner = Rc::new(Tray {
            clicks,
            menu: Menu::default(),
            title: RefCell::new("Synthetic tray".into()),
            icon: Cell::new(StatusIcon::Idle),
            taskbar_created: 0,
        });
        let weak = Rc::downgrade(&owner);
        let hwnd = create_tray_window(&owner).unwrap();
        assert_eq!(Rc::strong_count(&owner), 2);
        let dispatch = tray_for_dispatch(hwnd).unwrap();
        assert_eq!(Rc::strong_count(&owner), 3);
        assert!(dispatch.menu.open().is_some());

        drop(owner);
        // Simulate what can happen inside TrackPopupMenu's nested pump.
        unsafe { DestroyWindow(hwnd) }.unwrap();
        assert_eq!(Rc::strong_count(&dispatch), 1);
        dispatch.menu.close();
        drop(dispatch);
        assert!(weak.upgrade().is_none());
    }

    /// (side, image) of every entry in an `.ico` file.
    fn entries(ico: &[u8]) -> Vec<(u32, &[u8])> {
        let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
        (0..count)
            .map(|index| {
                let entry = &ico[6 + 16 * index..6 + 16 * (index + 1)];
                let length = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
                let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
                (u32::from(entry[0]), &ico[offset..offset + length])
            })
            .collect()
    }

    #[test]
    fn every_state_has_a_glyph_per_taskbar_at_each_small_icon_size() {
        for icon in [StatusIcon::Idle, StatusIcon::Recording, StatusIcon::Paused] {
            let light = tray_ico(icon, true);
            let dark = tray_ico(icon, false);
            assert_ne!(light, dark, "{icon:?}: one ink for both taskbars");
            for ico in [light, dark] {
                let sides: Vec<u32> = entries(ico).iter().map(|(side, _)| *side).collect();
                assert_eq!(sides, [16, 20, 24, 32, 40, 48], "{icon:?}");
                for (side, image) in entries(ico) {
                    // A 32-bit BITMAPINFOHEADER image, mask height included.
                    assert_eq!(&image[..4], &40u32.to_le_bytes());
                    assert_eq!(
                        i32::from_le_bytes(image[4..8].try_into().unwrap()),
                        side as i32
                    );
                    assert_eq!(
                        i32::from_le_bytes(image[8..12].try_into().unwrap()),
                        2 * side as i32
                    );
                    assert_eq!(u16::from_le_bytes([image[14], image[15]]), 32);
                }
            }
        }
        assert_ne!(
            tray_ico(StatusIcon::Idle, false),
            tray_ico(StatusIcon::Recording, false)
        );
    }

    #[test]
    fn the_closest_image_at_least_as_large_is_chosen() {
        let ico = tray_ico(StatusIcon::Idle, false);
        let side = |size| {
            let image = ico_image(ico, size).unwrap();
            u32::from_le_bytes(image[4..8].try_into().unwrap())
        };
        assert_eq!(side(16), 16);
        assert_eq!(side(18), 20);
        assert_eq!(side(24), 24);
        assert_eq!(side(30), 32);
        assert_eq!(side(96), 48);
        assert_eq!(ico_image(b"not an icon", 16), None);
        assert_eq!(ico_image(&ico[..30], 16), None);
    }

    #[test]
    fn only_enabled_real_items_are_clicked() {
        let mut disabled = MenuItemSpec::new("off", "Off");
        disabled.enabled = false;
        let items = vec![
            MenuItemSpec::new("open", "Open"),
            MenuItemSpec::separator(),
            disabled,
        ];
        assert_eq!(chosen(&items, 1).as_deref(), Some("open"));
        assert_eq!(chosen(&items, 0), None);
        assert_eq!(chosen(&items, 2), None);
        assert_eq!(chosen(&items, 3), None);
        assert_eq!(chosen(&items, 4), None);
    }

    #[test]
    fn a_menu_replaced_while_open_keeps_the_open_snapshot_and_refuses_nesting() {
        let menu = Menu::default();
        menu.replace(vec![MenuItemSpec::new("first", "First")]);
        let snapshot = menu.open().unwrap();
        // Re-entrant setter and a nested open request during the modal loop.
        menu.replace(vec![MenuItemSpec::new("second", "Second")]);
        assert!(menu.open().is_none());
        menu.close();
        assert_eq!(chosen(&snapshot, 1).as_deref(), Some("first"));
        assert_eq!(menu.open().unwrap()[0].id, "second");
    }

    #[test]
    fn activation_and_default_command_use_only_the_enabled_default_item() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let tray = Tray {
            clicks: tx,
            menu: Menu::default(),
            title: RefCell::new("BroScribe".into()),
            icon: Cell::new(StatusIcon::Idle),
            taskbar_created: 0,
        };

        // Without an enabled default item, nothing is activated or marked default.
        let no_default = vec![
            MenuItemSpec::new("record-start", "Start recording"),
            MenuItemSpec::new("open-panel", "Open recording panel"),
        ];
        assert_eq!(default_command(&no_default), None);
        tray.menu.replace(no_default);
        activate(&tray);
        assert!(rx.try_recv().is_err());

        let mut open = MenuItemSpec::new("open-app", "Open BroScribe");
        open.default = true;
        let items = vec![
            MenuItemSpec::new("record-start", "Start recording"),
            MenuItemSpec::separator(),
            open,
        ];
        assert_eq!(default_command(&items), Some(3));
        tray.menu.replace(items);
        activate(&tray);
        assert_eq!(rx.try_recv().unwrap(), "open-app");
        assert!(rx.try_recv().is_err());

        let mut disabled_default = MenuItemSpec::new("open-app", "Open BroScribe");
        disabled_default.default = true;
        disabled_default.enabled = false;
        let disabled = vec![
            MenuItemSpec::new("record-stop", "Stop and save recording"),
            disabled_default,
        ];
        assert_eq!(default_command(&disabled), None);
        tray.menu.replace(disabled);
        activate(&tray);
        assert!(rx.try_recv().is_err());
    }
}
