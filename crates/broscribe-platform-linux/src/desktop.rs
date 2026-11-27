use broscribe_platform::{Error, Result, WindowInfo, WindowInspector};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

#[derive(Default)]
pub struct LinuxWindowInspector;
impl LinuxWindowInspector {
    pub fn titles_available() -> bool {
        !is_wayland() && has_x11_display()
    }
}

fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|s| s.eq_ignore_ascii_case("wayland"))
}

fn has_x11_display() -> bool {
    std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
}

impl WindowInspector for LinuxWindowInspector {
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        if is_wayland() {
            return Err(Error::Unsupported("Wayland does not expose other applications' window titles; detection uses microphone usage and app identity".into()));
        }
        // A missing display is a capability gap (headless, console, service
        // without a session); a configured display that fails is reported.
        if !has_x11_display() {
            return Err(Error::Unsupported(
                "no X11 display in this session; detection uses microphone usage and app identity"
                    .into(),
            ));
        }
        let (connection, screen) =
            x11rb::connect(None).map_err(|e| crate::platform_error("connecting to X11", e))?;
        let atom = |name: &[u8]| -> Result<u32> {
            Ok(connection
                .intern_atom(false, name)
                .map_err(|e| crate::platform_error("interning X11 atom", e))?
                .reply()
                .map_err(|e| crate::platform_error("reading X11 atom", e))?
                .atom)
        };
        let clients = atom(b"_NET_CLIENT_LIST")?;
        let pid_atom = atom(b"_NET_WM_PID")?;
        let name_atom = atom(b"_NET_WM_NAME")?;
        let utf8 = atom(b"UTF8_STRING")?;
        let list = connection
            .get_property(
                false,
                connection.setup().roots[screen].root,
                clients,
                AtomEnum::WINDOW,
                0,
                65536,
            )
            .map_err(|e| crate::platform_error("listing X11 windows", e))?
            .reply()
            .map_err(|e| crate::platform_error("reading X11 windows", e))?;
        let mut windows = Vec::new();
        for window in list.value32().into_iter().flatten() {
            // Windows may disappear between these requests. That window is
            // skipped, while a connection/list failure above is reported.
            let Ok(pid_cookie) =
                connection.get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)
            else {
                continue;
            };
            let Ok(pid_reply) = pid_cookie.reply() else {
                continue;
            };
            let Some(pid) = pid_reply.value32().and_then(|mut v| v.next()) else {
                continue;
            };
            let title = connection
                .get_property(false, window, name_atom, utf8, 0, 16384)
                .ok()
                .and_then(|c| c.reply().ok())
                .filter(|r| r.type_ == utf8)
                .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                .or_else(|| {
                    connection
                        .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 16384)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                });
            let class = connection
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 4096)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| {
                    r.value
                        .split(|b| *b == 0)
                        .rfind(|v| !v.is_empty())
                        .map(|v| String::from_utf8_lossy(v).into_owned())
                });
            let app = class
                .clone()
                .or_else(|| {
                    std::fs::read_to_string(format!("/proc/{pid}/comm"))
                        .ok()
                        .map(|s| s.trim().to_owned())
                })
                .unwrap_or_default();
            windows.push(WindowInfo {
                pid,
                app,
                bundle_id: class,
                title,
            });
        }
        Ok(windows)
    }
}
