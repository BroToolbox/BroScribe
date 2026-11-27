//! UNUserNotificationCenter, initialized/submitted on GPUI's main loop.
//! Apple callbacks only enqueue actions; they never start a UI loop.
use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::ptr::NonNull;
use std::sync::{Arc, LazyLock, Mutex};

use block2::{DynBlock, RcBlock};
use broscribe_platform::system::{
    Notification, NotificationAction, NotificationActionSpec, NotificationAuthorization,
    NotificationDelivery, NotificationPreview,
};
use broscribe_platform::{Error, Notifier, Result};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_foundation::{NSArray, NSError, NSObject, NSSet, NSString};
use objc2_user_notifications::*;

fn bundled() -> bool {
    std::env::current_exe().is_ok_and(|path| {
        path.components()
            .any(|part| part.as_os_str().to_string_lossy().ends_with(".app"))
    })
}

pub(crate) fn authorization_status() -> Option<&'static str> {
    // Unbundled executables have no notification identity.
    if !bundled() {
        return None;
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let completion = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // The notification centre owns settings for this callback.
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        let words = match status {
            UNAuthorizationStatus::NotDetermined => "not_determined",
            UNAuthorizationStatus::Denied => "denied",
            UNAuthorizationStatus::Authorized
            | UNAuthorizationStatus::Provisional
            | UNAuthorizationStatus::Ephemeral => "granted",
            _ => "unknown",
        };
        let _ = sender.try_send(words);
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getNotificationSettingsWithCompletionHandler(&completion);
    receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .ok()
}

type Callback = Arc<dyn Fn(NotificationAction) + Send + Sync>;
/// Keep the most recent callbacks for notifications still in Notification Center.
const MAX_CALLBACKS: usize = 256;

#[derive(Default)]
struct Callbacks(VecDeque<(String, Callback)>);

impl Callbacks {
    fn insert(&mut self, id: String, callback: Callback) {
        self.remove(&id);
        if self.0.len() == MAX_CALLBACKS {
            self.0.pop_front();
        }
        self.0.push_back((id, callback));
    }

    fn get(&self, id: &str) -> Option<&Callback> {
        self.0
            .iter()
            .find_map(|(key, callback)| (key == id).then_some(callback))
    }

    fn remove(&mut self, id: &str) -> Option<Callback> {
        let index = self.0.iter().position(|(key, _)| key == id)?;
        self.0.remove(index).map(|(_, callback)| callback)
    }
}

#[derive(Default)]
struct DelegateIvars {
    callbacks: Mutex<Callbacks>,
}

define_class!(
    // SAFETY: NSObject has no subclass invariants; all ivars are synchronized.
    #[unsafe(super(NSObject))]
    #[name = "BroScribeNotificationDelegate"]
    #[ivars = DelegateIvars]
    struct NotificationDelegate;
    unsafe impl NSObjectProtocol for NotificationDelegate {}
    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn present(
            &self,
            _: &UNUserNotificationCenter,
            _: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn respond(
            &self,
            _: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>,
        ) {
            let id = response.notification().request().identifier().to_string();
            let identifier = response.actionIdentifier().to_string();
            // SAFETY: these constants are provided by the linked framework.
            let (default, dismiss) = unsafe {
                (
                    UNNotificationDefaultActionIdentifier.to_string(),
                    UNNotificationDismissActionIdentifier.to_string(),
                )
            };
            let action = reported_action(&identifier, &default, &dismiss);
            let callback = self
                .ivars()
                .callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(callback) = callback {
                callback(NotificationAction { id, action });
            }
            completion.call(());
        }
    }
);
impl NotificationDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars::default());
        // SAFETY: designated NSObject initializer, no additional invariants.
        unsafe { msg_send![super(this), init] }
    }
}

thread_local! {
    // UNUserNotificationCenter.delegate is weak. Keep it alive on the app loop.
    static DELEGATE: RefCell<Option<Retained<NotificationDelegate>>> = const { RefCell::new(None) };
}
/// The title of an `open` button whose request gives it no label.
const OPEN: &str = "Open";

/// The labels the app's `open` buttons are known to use: "Summary ready",
/// "Final transcript ready" and "Sign-in required". Their categories are
/// registered from the first registration on, so a notification left in
/// Notification Center by an earlier run keeps its button after a relaunch.
const KNOWN_OPEN_LABELS: [&str; 3] = [OPEN, "Open meeting", "Sign in"];

/// `label` spelt in hex: every label gives a valid identifier, the same
/// label always the same one and two labels never the same one.
fn hex(label: &str) -> String {
    label.bytes().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// The native action identifier of the `open` button titled `label`.
/// Notification Center wants every action identifier unique among all the
/// app's categories: "Open" keeps `open`, any other title gets
/// `open-<hex>`, with the same hex as its category.
fn open_action_id(label: &str) -> String {
    if label == OPEN {
        "open".into()
    } else {
        format!("open-{}", hex(label))
    }
}

/// Whether `identifier` is the native identifier of an `open` button.
fn is_open_action_id(identifier: &str) -> bool {
    identifier == "open"
        || identifier.strip_prefix("open-").is_some_and(|hex| {
            !hex.is_empty()
                && hex.len() % 2 == 0
                && hex
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// The action a response reports, from its native action identifier and
/// Notification Center's identifiers for a body click (`default`) and a
/// dismissal (`dismiss`). A body click and every `open` button report
/// `open`, a dismissal `ignore`; Record and Ignore report themselves.
fn reported_action(identifier: &str, default: &str, dismiss: &str) -> String {
    if identifier == default || is_open_action_id(identifier) {
        "open".into()
    } else if identifier == dismiss {
        "ignore".into()
    } else {
        identifier.into()
    }
}

/// The buttons of a notification, registered with Notification Center as
/// one category. Its identifier is stable for its buttons.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    /// Record and Ignore, for a detected meeting; dismissing it is
    /// reported as Ignore.
    Record,
    /// One `open` button with this title.
    Open(String),
    Notice(NotificationPreview),
}

/// A button a category registers.
#[derive(Debug, PartialEq, Eq)]
struct Button {
    /// Its native action identifier, unique among every button the app
    /// registers.
    id: String,
    /// The action reported when it is clicked.
    action: &'static str,
    title: String,
    /// Clicking it brings the app forward.
    foreground: bool,
}

impl Category {
    fn of(actions: &[NotificationActionSpec], preview: NotificationPreview) -> Self {
        if actions.iter().any(|action| action.id == "record") {
            return Self::Record;
        }
        match actions.iter().find(|action| action.id == "open") {
            Some(action) if !action.label.trim().is_empty() => {
                Self::Open(action.label.trim().to_owned())
            }
            Some(_) => Self::Open(OPEN.into()),
            None => Self::Notice(preview),
        }
    }

    fn preview(&self) -> &'static str {
        match self {
            Self::Record | Self::Notice(NotificationPreview::Detection) => "Meeting detected",
            Self::Notice(NotificationPreview::Recording) => "Recording update",
            Self::Notice(NotificationPreview::Meeting) => "Meeting update",
            Self::Open(_) | Self::Notice(NotificationPreview::App) => "BroScribe update",
        }
    }

    /// Its identifier. "Open" keeps `broscribe-open`; any other title gets
    /// its own, spelt in hex, so every title is a valid identifier, the same
    /// title always gets the same one and two titles never share one.
    fn identifier(&self) -> String {
        match self {
            Self::Record => "broscribe-record".into(),
            Self::Open(label) if label == OPEN => "broscribe-open".into(),
            Self::Open(label) => format!("broscribe-open-{}", hex(label)),
            Self::Notice(preview) => match preview {
                NotificationPreview::Detection => "broscribe-notice-detection",
                NotificationPreview::Recording => "broscribe-notice-recording",
                NotificationPreview::Meeting => "broscribe-notice-meeting",
                NotificationPreview::App => "broscribe-notice-app",
            }
            .into(),
        }
    }

    fn buttons(&self) -> Vec<Button> {
        match self {
            Self::Record => vec![
                Button {
                    id: "record".into(),
                    action: "record",
                    title: "Record".into(),
                    foreground: false,
                },
                Button {
                    id: "ignore".into(),
                    action: "ignore",
                    title: "Ignore".into(),
                    foreground: false,
                },
            ],
            Self::Open(label) => vec![Button {
                id: open_action_id(label),
                action: "open",
                title: label.clone(),
                foreground: true,
            }],
            Self::Notice(_) => Vec::new(),
        }
    }

    /// Dismissing the notification is reported (as `ignore`).
    fn reports_dismiss(&self) -> bool {
        matches!(self, Self::Record)
    }
}

/// Every category this process registered: Record and those of the known
/// labels from the start, then one for each new label.
static CATEGORIES: LazyLock<Mutex<BTreeSet<Category>>> = LazyLock::new(|| {
    let known = KNOWN_OPEN_LABELS
        .into_iter()
        .map(|label| Category::Open(label.into()));
    let notices = [
        NotificationPreview::App,
        NotificationPreview::Detection,
        NotificationPreview::Recording,
        NotificationPreview::Meeting,
    ]
    .into_iter()
    .map(Category::Notice);
    Mutex::new(
        std::iter::once(Category::Record)
            .chain(known)
            .chain(notices)
            .collect(),
    )
});

/// Adds `category` to those this process registered and returns them all,
/// as the next registration lists them.
fn register(category: &Category) -> Vec<Category> {
    let mut registered = CATEGORIES.lock().unwrap_or_else(|e| e.into_inner());
    registered.insert(category.clone());
    registered.iter().cloned().collect()
}

/// `category` as Notification Center registers it.
fn un_category(category: &Category) -> Retained<UNNotificationCategory> {
    let buttons: Vec<_> = category
        .buttons()
        .into_iter()
        .map(|button| {
            UNNotificationAction::actionWithIdentifier_title_options(
                &NSString::from_str(&button.id),
                &NSString::from_str(&button.title),
                if button.foreground {
                    UNNotificationActionOptions::Foreground
                } else {
                    UNNotificationActionOptions::empty()
                },
            )
        })
        .collect();
    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_hiddenPreviewsBodyPlaceholder_options(
        &NSString::from_str(&category.identifier()),
        &NSArray::from_retained_slice(&buttons),
        &NSArray::<NSString>::new(),
        &NSString::from_str(category.preview()),
        if category.reports_dismiss() {
            UNNotificationCategoryOptions::CustomDismissAction
        } else {
            UNNotificationCategoryOptions::empty()
        },
    )
}

fn is_current(id: &str, callback: &Callback) -> bool {
    DELEGATE.with(|slot| {
        slot.borrow().as_ref().is_some_and(|delegate| {
            delegate
                .ivars()
                .callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .is_some_and(|registered| Arc::ptr_eq(registered, callback))
        })
    })
}

fn post_authorized(notification: Notification, callback: Callback) {
    let center = UNUserNotificationCenter::currentNotificationCenter();
    // One stable category per set of buttons. Registering replaces every
    // category, so all those this process registered, and those of the
    // known labels, are kept and an earlier notification keeps its
    // buttons.
    let category = Category::of(&notification.actions, notification.preview);
    let registered: Vec<_> = register(&category).iter().map(un_category).collect();
    center.setNotificationCategories(&NSSet::from_retained_slice(&registered));
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notification.title));
    content.setBody(&NSString::from_str(&notification.body));
    content.setCategoryIdentifier(&NSString::from_str(&category.identifier()));
    // A result of a newer run replaces the earlier row in Notification Center.
    let identifiers = NSArray::from_retained_slice(&[NSString::from_str(&notification.id)]);
    center.removeDeliveredNotificationsWithIdentifiers(&identifiers);
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&notification.id),
        &content,
        None,
    );
    let id = notification.id;
    let block = RcBlock::new(move |error: *mut NSError| {
        callback(NotificationAction {
            id: id.clone(),
            action: if error.is_null() {
                "__delivered"
            } else {
                "__denied"
            }
            .into(),
        });
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&block));
}

pub struct MacNotifier;
impl Notifier for MacNotifier {
    fn authorization(&self) -> NotificationAuthorization {
        match authorization_status() {
            Some("not_determined") => NotificationAuthorization::NotDetermined,
            Some("denied") => NotificationAuthorization::Denied,
            Some("granted") => NotificationAuthorization::Granted,
            _ if bundled() => NotificationAuthorization::Unknown,
            _ => NotificationAuthorization::Unsupported,
        }
    }

    fn request_authorization(&self) -> NotificationAuthorization {
        let status = self.authorization();
        if status != NotificationAuthorization::NotDetermined {
            return status;
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            let result = if !error.is_null() {
                NotificationAuthorization::Unknown
            } else if granted.as_bool() {
                NotificationAuthorization::Granted
            } else {
                NotificationAuthorization::Denied
            };
            let _ = sender.try_send(result);
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert
                    | UNAuthorizationOptions::Badge
                    | UNAuthorizationOptions::Sound,
                &completion,
            );
        receiver
            .recv_timeout(std::time::Duration::from_secs(120))
            .unwrap_or(NotificationAuthorization::Unknown)
    }

    fn notify(&self, title: &str, body: &str) -> Result<()> {
        let notification = Notification {
            preview: NotificationPreview::App,
            opens_on_click: false,
            id: format!("broscribe-{}", std::process::id()),
            title: title.into(),
            body: body.into(),
            actions: Vec::new(),
        };
        self.notify_actionable(notification, Arc::new(|_| {}))
            .map(|_| ())
    }

    fn notify_actionable(
        &self,
        notification: Notification,
        callback: Callback,
    ) -> Result<NotificationDelivery> {
        if MainThreadMarker::new().is_none() {
            return Err(Error::Unsupported(
                "notifications must run on the application UI loop".into(),
            ));
        }
        // An unbundled command-line executable has no notification identity;
        // currentNotificationCenter may raise an Objective-C exception there.
        if !std::env::current_exe().is_ok_and(|path| {
            path.components()
                .any(|part| part.as_os_str().to_string_lossy().ends_with(".app"))
        }) {
            return Ok(NotificationDelivery::Denied);
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        DELEGATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let delegate = slot.get_or_insert_with(NotificationDelegate::new);
            delegate
                .ivars()
                .callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(notification.id.clone(), callback.clone());
            center.setDelegate(Some(ProtocolObject::from_ref(&**delegate)));
        });
        let id = notification.id.clone();
        let settings_center = center.clone();
        let block = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            if !granted.as_bool() || !error.is_null() {
                callback(NotificationAction {
                    id: id.clone(),
                    action: "__denied".into(),
                });
                return;
            }
            let notification = notification.clone();
            let callback = callback.clone();
            let settings = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
                // SAFETY: Apple guarantees this settings object during the callback.
                let settings = unsafe { settings.as_ref() };
                let visible = settings.authorizationStatus() == UNAuthorizationStatus::Authorized
                    && settings.alertSetting() == UNNotificationSetting::Enabled;
                if !visible {
                    callback(NotificationAction {
                        id: notification.id.clone(),
                        action: "__denied".into(),
                    });
                    return;
                }
                let notification = notification.clone();
                let callback = callback.clone();
                dispatch2::DispatchQueue::main().exec_async(move || {
                    if is_current(&notification.id, &callback) {
                        post_authorized(notification, callback);
                    }
                });
            });
            settings_center.getNotificationSettingsWithCompletionHandler(&settings);
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert
                | UNAuthorizationOptions::Badge
                | UNAuthorizationOptions::Sound,
            &block,
        );
        Ok(NotificationDelivery::Pending)
    }

    fn withdraw(&self, id: &str) -> Result<()> {
        if MainThreadMarker::new().is_none() {
            return Err(Error::Unsupported(
                "notifications must run on the application UI loop".into(),
            ));
        }
        DELEGATE.with(|slot| {
            if let Some(delegate) = slot.borrow().as_ref() {
                delegate
                    .ivars()
                    .callbacks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(id);
                let center = UNUserNotificationCenter::currentNotificationCenter();
                let identifiers = NSArray::from_retained_slice(&[NSString::from_str(id)]);
                center.removePendingNotificationRequestsWithIdentifiers(&identifiers);
                center.removeDeliveredNotificationsWithIdentifiers(&identifiers);
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_responses_consume_one_entry_and_capacity_evicts_the_oldest() {
        let mut callbacks = Callbacks::default();
        let callback: Callback = Arc::new(|_| {});
        let replacement: Callback = Arc::new(|_| {});
        for ordinal in 0..MAX_CALLBACKS {
            callbacks.insert(format!("n-{ordinal}"), callback.clone());
        }
        callbacks.insert("n-0".into(), replacement.clone());
        callbacks.insert("new".into(), callback.clone());
        assert_eq!(callbacks.0.len(), MAX_CALLBACKS);
        assert!(callbacks.get("n-1").is_none());
        assert!(Arc::ptr_eq(&callbacks.remove("n-0").unwrap(), &replacement));
        assert!(callbacks.remove("n-0").is_none());
        assert!(callbacks.get("new").is_some());
    }

    fn action(id: &str, label: &str) -> NotificationActionSpec {
        NotificationActionSpec {
            id: id.into(),
            label: label.into(),
        }
    }

    fn open(label: &str) -> Category {
        Category::of(&[action("open", label)], NotificationPreview::App)
    }

    /// The one button an `open` category registers, which reports `open`:
    /// its native id and title.
    fn open_button(category: &Category) -> (String, String) {
        let buttons = category.buttons();
        assert_eq!(buttons.len(), 1, "{category:?}");
        assert!(buttons[0].foreground, "{category:?}");
        assert_eq!(buttons[0].action, "open", "{category:?}");
        (buttons[0].id.clone(), buttons[0].title.clone())
    }

    /// The action a response with this native identifier reports, with
    /// Notification Center's own body-click and dismiss identifiers.
    fn reported(identifier: &str) -> String {
        // SAFETY: these constants are provided by the linked framework.
        let (default, dismiss) = unsafe {
            (
                UNNotificationDefaultActionIdentifier.to_string(),
                UNNotificationDismissActionIdentifier.to_string(),
            )
        };
        reported_action(identifier, &default, &dismiss)
    }

    /// WRT-06: the button shows the label the request gives, as on Windows
    /// and Linux, and still reports `open`.
    #[test]
    fn an_open_button_shows_the_label_its_request_gives() {
        // "Final transcript ready" (broscribed's scheduler).
        let ready = open("Open meeting");
        assert_eq!(ready, Category::Open("Open meeting".into()));
        assert_eq!(
            open_button(&ready),
            (
                "open-4f70656e206d656574696e67".into(),
                "Open meeting".into()
            )
        );
        assert!(!ready.reports_dismiss());
        // "Sign-in required" (broscribed's integrations).
        assert_eq!(
            open_button(&open("Sign in")),
            ("open-5369676e20696e".into(), "Sign in".into())
        );
        // A finished summary (the app's generation notice) asks for "Open":
        // the category and action it always had.
        assert_eq!(open("Open").identifier(), "broscribe-open");
        assert_eq!(open_button(&open("Open")), ("open".into(), "Open".into()));
        // Space around a label is not shown.
        assert_eq!(open(" Open meeting\n"), ready);
        // An explicitly requested button with an empty label keeps "Open".
        for actions in [vec![action("open", "")], vec![action("open", "  ")]] {
            let category = Category::of(&actions, NotificationPreview::App);
            assert_eq!(category.identifier(), "broscribe-open", "{actions:?}");
            assert_eq!(open_button(&category), ("open".into(), "Open".into()));
        }
    }

    #[test]
    fn hidden_previews_use_generic_text_and_actionless_categories_have_no_open_button() {
        for (category, expected) in [
            (Category::Record, "Meeting detected"),
            (
                Category::Notice(NotificationPreview::Recording),
                "Recording update",
            ),
            (
                Category::Notice(NotificationPreview::Meeting),
                "Meeting update",
            ),
        ] {
            assert_eq!(
                un_category(&category)
                    .hiddenPreviewsBodyPlaceholder()
                    .to_string(),
                expected
            );
        }
        let category = Category::of(&[], NotificationPreview::Meeting);
        assert!(un_category(&category).actions().is_empty());
    }

    /// A meeting prompt keeps its own Record and Ignore buttons and
    /// category, whatever else it carries.
    #[test]
    fn a_meeting_prompt_keeps_record_and_ignore() {
        for actions in [
            vec![action("record", "Record"), action("ignore", "Ignore")],
            vec![action("open", "Open meeting"), action("record", "Record")],
        ] {
            let prompt = Category::of(&actions, NotificationPreview::App);
            assert_eq!(prompt, Category::Record, "{actions:?}");
            assert_eq!(prompt.identifier(), "broscribe-record");
            assert!(prompt.reports_dismiss());
            assert_eq!(
                prompt.buttons(),
                vec![
                    Button {
                        id: "record".into(),
                        action: "record",
                        title: "Record".into(),
                        foreground: false,
                    },
                    Button {
                        id: "ignore".into(),
                        action: "ignore",
                        title: "Ignore".into(),
                        foreground: false,
                    },
                ]
            );
        }
    }

    /// Record, Ignore, a body click and a dismissal report what they
    /// always did.
    #[test]
    fn record_ignore_body_click_and_dismiss_report_as_before() {
        assert_eq!(reported("record"), "record");
        assert_eq!(reported("ignore"), "ignore");
        // SAFETY: these constants are provided by the linked framework.
        let (default, dismiss) = unsafe {
            (
                UNNotificationDefaultActionIdentifier.to_string(),
                UNNotificationDismissActionIdentifier.to_string(),
            )
        };
        assert_eq!(reported(&default), "open");
        assert_eq!(reported(&dismiss), "ignore");
        // Only an `open` button's own identifier is read as `open`.
        for other in ["open-", "open-4", "open-4F", "open-zz", "opener", "Open"] {
            assert_eq!(reported(other), other);
        }
    }

    /// Notification Center wants every action identifier unique among all
    /// the app's categories: every button registered has its own, and each
    /// reports the action it stands for (every `open` button `open`).
    #[test]
    fn every_registered_button_has_its_own_native_id() {
        let mut registered = Vec::new();
        for title in [
            "Synthetic button",
            "open",
            "Open-meeting",
            "Open meeting…",
            "Reunião aberta",
            "Record",
            "Ignore",
        ] {
            registered = register(&open(title));
        }
        let mut ids = BTreeSet::new();
        for category in &registered {
            for button in category.buttons() {
                assert_eq!(reported(&button.id), button.action, "{button:?}");
                assert!(button.id.is_ascii(), "{button:?}");
                assert!(ids.insert(button.id.clone()), "{button:?} shares its id");
            }
        }
        for id in ["record", "ignore", "open", "open-4f70656e206d656574696e67"] {
            assert!(ids.contains(id), "{id}");
        }
    }

    /// The same title always gets the same identifier, and two titles
    /// never share one, with each other or with the fixed categories.
    #[test]
    fn each_title_has_one_stable_category() {
        assert_eq!(
            open("Open meeting").identifier(),
            "broscribe-open-4f70656e206d656574696e67"
        );
        let titles = [
            "Open",
            "Open meeting",
            "Sign in",
            "open",
            "Open-meeting",
            "Open meeting…",
            "Reunião aberta",
            "broscribe-open",
            "Record",
        ];
        let mut identifiers = BTreeSet::from([Category::Record.identifier()]);
        let mut natives = BTreeSet::from(["record".to_owned(), "ignore".to_owned()]);
        for title in titles {
            let identifier = open(title).identifier();
            assert_eq!(identifier, open(title).identifier(), "{title}");
            assert!(identifier.starts_with("broscribe-open"), "{identifier}");
            assert!(identifier.is_ascii(), "{identifier}");
            assert!(identifiers.insert(identifier), "{title} shares one");
            // Its button's native id too: stable, its own, and `open`.
            let (native, _) = open_button(&open(title));
            assert_eq!(native, open_button(&open(title)).0, "{title}");
            assert_eq!(reported(&native), "open", "{title}");
            assert!(natives.insert(native), "{title} shares its action id");
        }
    }

    /// The first registration already holds the labels the app is known to
    /// use, so a notification an earlier run left keeps its button after a
    /// relaunch; registering a new title keeps those registered before it,
    /// so an earlier notification keeps its button.
    #[test]
    fn registering_a_title_keeps_the_earlier_ones() {
        let first = register(&open("Synthetic first"));
        for category in [
            Category::Record,
            open("Open"),
            open("Open meeting"),
            open("Sign in"),
        ] {
            assert!(first.contains(&category), "{category:?}");
        }
        let second = register(&open("Synthetic second"));
        for category in [
            Category::Record,
            open("Open"),
            open("Synthetic first"),
            open("Synthetic second"),
        ] {
            assert!(second.contains(&category), "{category:?}");
        }
        // A title registered again is still registered once.
        let again = register(&open("Synthetic first"));
        assert_eq!(
            again
                .iter()
                .filter(|category| **category == open("Synthetic first"))
                .count(),
            1
        );
    }
}
