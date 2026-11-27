use broscribe_platform::{
    Error, Result,
    system::{Notification, NotificationAction, NotificationAuthorization, NotificationDelivery},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use windows::{
    Data::Xml::Dom::XmlDocument,
    Foundation::TypedEventHandler,
    UI::Notifications::{
        NotificationSetting, ToastActivatedEventArgs, ToastDismissedEventArgs,
        ToastFailedEventArgs, ToastNotification, ToastNotificationManager,
    },
    Win32::{
        Foundation::ERROR_NOT_FOUND,
        System::WinRT::{
            RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize,
        },
    },
    core::{HSTRING, IInspectable, Interface},
};

pub(crate) struct WinRt;
impl WinRt {
    pub(crate) fn new() -> Result<Self> {
        unsafe {
            match RoInitialize(RO_INIT_MULTITHREADED) {
                Ok(()) => {}
                Err(e) if e.code().0 == 0x80010106u32 as i32 => {
                    RoInitialize(RO_INIT_SINGLETHREADED)
                        .map_err(|e| crate::error("initializing WinRT STA", e))?;
                }
                Err(e) => return Err(crate::error("initializing WinRT", e)),
            }
        }
        Ok(Self)
    }
}
impl Drop for WinRt {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
pub(crate) fn authorization(app_id: &HSTRING) -> NotificationAuthorization {
    let Ok(_runtime) = WinRt::new() else {
        return NotificationAuthorization::Unknown;
    };
    let status =
        ToastNotificationManager::CreateToastNotifierWithId(app_id).and_then(|n| n.Setting());
    match status {
        Ok(NotificationSetting::Enabled) => NotificationAuthorization::Granted,
        Ok(_) => NotificationAuthorization::Denied,
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => {
            NotificationAuthorization::NotDetermined
        }
        Err(_) => NotificationAuthorization::Unknown,
    }
}

static NEXT: AtomicU64 = AtomicU64::new(1);
struct ActiveToast {
    app_id: HSTRING,
    id: String,
    toast: ToastNotification,
    done: Arc<AtomicBool>,
}
static ACTIVE: LazyLock<Mutex<HashMap<u64, ActiveToast>>> = LazyLock::new(Default::default);
fn remove(key: u64) {
    let removed = ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    drop(removed);
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn toast_tag(id: &str) -> HSTRING {
    if id.encode_utf16().count() <= 64 {
        HSTRING::from(id)
    } else {
        // A template result names both its meeting and its template. Keep
        // the full key for callbacks and shorten only Windows' native tag.
        HSTRING::from(format!("long-{:016x}", crate::secret_chunks::checksum(id)))
    }
}

pub(crate) fn withdraw(app_id: &HSTRING, id: &str) -> Result<()> {
    let _runtime = WinRt::new()?;
    let records: Vec<_> = ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extract_if(|_, record| &record.app_id == app_id && record.id == id)
        .map(|(_, record)| {
            record.done.store(true, Ordering::Release);
            record
        })
        .collect();
    let mut failure = None;
    if !records.is_empty() {
        match ToastNotificationManager::CreateToastNotifierWithId(app_id) {
            Ok(native) => {
                for record in records {
                    if let Err(e) = when_present(native.Hide(&record.toast)) {
                        failure = Some(crate::error("hiding withdrawn toast", e));
                    }
                }
            }
            Err(e) if e.code() != ERROR_NOT_FOUND.to_hresult() => {
                failure = Some(crate::error("withdrawing toast notifier", e));
            }
            Err(_) => {}
        }
    }
    if let Err(e) = when_present(ToastNotificationManager::History().and_then(|h| {
        h.RemoveGroupedTagWithId(&toast_tag(id), &HSTRING::from("BroScribe"), app_id)
    })) {
        failure = Some(crate::error("removing toast history", e));
    }
    if let Some(e) = failure {
        Err(e)
    } else {
        Ok(())
    }
}

fn when_present(removal: windows::core::Result<()>) -> windows::core::Result<()> {
    match removal {
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
        other => other,
    }
}

fn prepare_replacement(
    setting: windows::core::Result<NotificationSetting>,
    withdraw: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    match setting {
        Ok(NotificationSetting::Enabled) => {}
        Ok(_) => return Ok(false),
        // A newly registered desktop identity may not have a setting yet.
        // Let Show and the failure callback report whether delivery works.
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => {}
        Err(error) => return Err(crate::error("querying toast permission", error)),
    }
    // A removal failure cannot prevent delivery of its replacement.
    let _ = withdraw();
    Ok(true)
}

pub(crate) fn show(
    notifier: &crate::WinNotifier,
    notification: Notification,
    callback: Arc<dyn Fn(NotificationAction) + Send + Sync>,
) -> Result<NotificationDelivery> {
    let _runtime = WinRt::new()?;
    if notification.id.is_empty() {
        return Err(Error::InvalidInput(
            "Windows toast IDs must be nonempty".into(),
        ));
    }
    if notification.actions.len() > 5 {
        return Err(Error::InvalidInput(
            "Windows permits at most five toast actions".into(),
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for action in &notification.actions {
        if action.id.is_empty() || action.id.starts_with("__") || !ids.insert(action.id.clone()) {
            return Err(Error::InvalidInput(
                "toast action IDs must be unique, nonempty and non-reserved".into(),
            ));
        }
    }
    notifier.register_shortcut()?;
    let app_id = notifier.app_id();
    let native = ToastNotificationManager::CreateToastNotifierWithId(&app_id)
        .map_err(|e| crate::error("creating actionable toast notifier", e))?;
    if !prepare_replacement(native.Setting(), || withdraw(&app_id, &notification.id))? {
        return Ok(NotificationDelivery::Denied);
    }
    let actions: String = notification
        .actions
        .iter()
        .map(|action| {
            format!(
                "<action content=\"{}\" arguments=\"{}\" activationType=\"foreground\"/>",
                escape(&action.label),
                escape(&action.id)
            )
        })
        .collect();
    let document =
        XmlDocument::new().map_err(|e| crate::error("creating actionable toast XML", e))?;
    let launch = if notification.opens_on_click {
        ids.insert("open".into());
        " launch=\"open\""
    } else {
        ""
    };
    document.LoadXml(&HSTRING::from(format!("<toast{launch}><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><actions>{actions}</actions></toast>", escape(&notification.title), escape(&notification.body)))).map_err(|e| crate::error("loading actionable toast XML", e))?;
    let toast = ToastNotification::CreateToastNotification(&document)
        .map_err(|e| crate::error("creating actionable toast", e))?;
    let key = NEXT.fetch_add(1, Ordering::Relaxed);
    let done = Arc::new(AtomicBool::new(false));
    toast
        .SetTag(&toast_tag(&notification.id))
        .and_then(|_| toast.SetGroup(&HSTRING::from("BroScribe")))
        .map_err(|e| crate::error("setting toast identity", e))?;
    let id = notification.id.clone();
    let activated_callback = callback.clone();
    let activated_done = done.clone();
    let activated = TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, args| {
        if let Some(args) = args.as_ref()
            && let Ok(args) = args.cast::<ToastActivatedEventArgs>()
            && let Ok(action) = args.Arguments()
        {
            let action = action.to_string();
            if ids.contains(&action) && !activated_done.swap(true, Ordering::AcqRel) {
                activated_callback(NotificationAction {
                    id: id.clone(),
                    action,
                });
            }
        }
        remove(key);
        Ok(())
    });
    toast
        .Activated(&activated)
        .map_err(|e| crate::error("registering toast activation", e))?;
    let dismissed =
        TypedEventHandler::<ToastNotification, ToastDismissedEventArgs>::new(move |_, args| {
            if args.as_ref().and_then(|a| a.Reason().ok())
                == Some(windows::UI::Notifications::ToastDismissalReason::UserCanceled)
            {
                remove(key);
            }
            Ok(())
        });
    toast
        .Dismissed(&dismissed)
        .map_err(|e| crate::error("registering toast dismissal", e))?;
    let failed_done = done.clone();
    let failed_id = notification.id.clone();
    let failed = TypedEventHandler::<ToastNotification, ToastFailedEventArgs>::new(move |_, _| {
        if !failed_done.swap(true, Ordering::AcqRel) {
            callback(NotificationAction {
                id: failed_id.clone(),
                action: "__denied".into(),
            });
        }
        remove(key);
        Ok(())
    });
    toast
        .Failed(&failed)
        .map_err(|e| crate::error("registering toast failure", e))?;
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).insert(
        key,
        ActiveToast {
            app_id,
            id: notification.id,
            toast: toast.clone(),
            done,
        },
    );
    if let Err(e) = native.Show(&toast) {
        remove(key);
        return Err(crate::error("showing actionable toast", e));
    }
    Ok(NotificationDelivery::Delivered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_toasts_stay_denied_and_missing_history_is_harmless() {
        let missing = || windows::core::Error::from_hresult(ERROR_NOT_FOUND.to_hresult());
        assert!(
            !prepare_replacement(Ok(NotificationSetting::DisabledForApplication), || {
                panic!("a disabled identity must not be asked to remove its history")
            })
            .unwrap()
        );
        assert!(
            prepare_replacement(Ok(NotificationSetting::Enabled), || {
                Err(Error::Unsupported("synthetic history failure".into()))
            })
            .unwrap()
        );
        assert!(prepare_replacement(Err(missing()), || Ok(())).unwrap());
        assert!(when_present(Err(missing())).is_ok());
    }

    #[test]
    fn stable_generation_ids_fit_the_native_tag_limit() {
        let id = format!("generation-m-000001-template-{}", "a".repeat(40));
        assert!(id.len() > 64);
        let tag = toast_tag(&id);
        assert!(!tag.is_empty() && tag.len() <= 64);
        assert_eq!(tag, toast_tag(&id));
        assert_ne!(tag, toast_tag(&format!("{id}b")));
        assert_eq!(toast_tag("final-m-000001").to_string(), "final-m-000001");
    }
}
