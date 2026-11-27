//! Spike: one GPUI window plus an `NSStatusItem` menu, both on GPUI's main loop.
//!
//! The status item and its menu are plain AppKit objects created inside
//! `Application::run`'s launch callback, so AppKit delivers their clicks on the
//! run loop GPUI already drives. The menu target forwards each click into an
//! unbounded channel; a task on GPUI's foreground executor receives it and
//! updates GPUI state. Nothing renders until that state changes.
//!
//! `SPIKE_SELFTEST=1` triggers the three menu items through
//! `NSMenu::performActionForItemAtIndex` (same target-action path as a click)
//! after short delays, so the plumbing can be checked without a human.
//! `SPIKE_ACCESSORY=1` runs as a menu bar utility (no Dock icon).

use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _, Render,
    Styled as _, Window, WindowBounds, WindowHandle, WindowOptions, div, px, rgb, size,
};

/// A command sent from the status-item menu to GPUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuCommand {
    ShowWindow,
    Increment,
    Quit,
}

impl MenuCommand {
    const ALL: [MenuCommand; 3] = [Self::ShowWindow, Self::Increment, Self::Quit];

    fn title(self) -> &'static str {
        match self {
            Self::ShowWindow => "Show window",
            Self::Increment => "Increment",
            Self::Quit => "Quit",
        }
    }

    fn tag(self) -> isize {
        match self {
            Self::ShowWindow => 1,
            Self::Increment => 2,
            Self::Quit => 3,
        }
    }

    fn from_tag(tag: isize) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.tag() == tag)
    }
}

struct Counter {
    value: u64,
}

struct SpikeView {
    counter: Entity<Counter>,
    renders: u64,
}

impl SpikeView {
    fn new(counter: Entity<Counter>, cx: &mut Context<Self>) -> Self {
        // Repaint only when the counter changes.
        cx.observe(&counter, |_, _, cx| cx.notify()).detach();
        Self {
            counter,
            renders: 0,
        }
    }
}

impl Render for SpikeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        let value = self.counter.read(cx).value;
        log(&format!("render #{} (counter = {value})", self.renders));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .size_full()
            .justify_center()
            .items_center()
            .bg(rgb(0x202124))
            .text_color(rgb(0xe8eaed))
            .child("BroScribe status-item spike")
            .child(div().text_xl().child(format!("Counter: {value}")))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x9aa0a6))
                    .child("Use the BS menu-bar item to change it."),
            )
    }
}

fn log(message: &str) {
    use std::sync::LazyLock;
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    eprintln!("[{:>8.3}s] {message}", START.elapsed().as_secs_f64());
}

fn open_window(counter: &Entity<Counter>, cx: &mut App) -> WindowHandle<SpikeView> {
    let bounds = Bounds::centered(None, size(px(360.), px(160.)), cx);
    let counter = counter.clone();
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| SpikeView::new(counter, cx)),
        )
        .expect("open window");
    log("window opened");
    handle
}

/// Brings the window forward, reopening it if the user closed it.
fn show_window(window: &mut WindowHandle<SpikeView>, counter: &Entity<Counter>, cx: &mut App) {
    cx.activate(true);
    let reused = window
        .update(cx, |_, window, _| window.activate_window())
        .is_ok();
    if reused {
        log("window re-activated");
    } else {
        *window = open_window(counter, cx);
    }
}

fn main() {
    log("starting");
    let selftest = std::env::var_os("SPIKE_SELFTEST").is_some();
    let accessory = std::env::var_os("SPIKE_ACCESSORY").is_some();

    gpui_platform::application().run(move |cx: &mut App| {
        log(&format!(
            "launched on thread {:?}",
            std::thread::current().id()
        ));
        if accessory {
            cx.set_activation_policy(gpui::ActivationPolicy::Accessory);
        }

        let counter = cx.new(|_| Counter { value: 0 });
        let mut window = open_window(&counter, cx);
        cx.activate(true);

        let (tx, mut rx) = mpsc::unbounded::<MenuCommand>();
        #[cfg(target_os = "macos")]
        let status_item = status_item::StatusItem::install(tx);
        #[cfg(not(target_os = "macos"))]
        drop(tx);

        #[cfg(target_os = "macos")]
        if selftest {
            let menu = status_item.menu();
            cx.spawn(async move |cx| {
                for command in [
                    MenuCommand::Increment,
                    MenuCommand::Increment,
                    MenuCommand::ShowWindow,
                    MenuCommand::Increment,
                    MenuCommand::Quit,
                ] {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    log(&format!(
                        "selftest: performing menu item {:?}",
                        command.title()
                    ));
                    status_item::perform(&menu, command);
                }
            })
            .detach();
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (selftest, Duration::ZERO);

        // The receiving end runs on GPUI's foreground executor: the same thread
        // and run loop that delivered the click.
        cx.spawn(async move |cx| {
            #[cfg(target_os = "macos")]
            let _status_item = status_item; // Keep the item and its target alive.
            while let Some(command) = rx.next().await {
                log(&format!("GPUI received {command:?}"));
                cx.update(|cx| match command {
                    MenuCommand::ShowWindow => show_window(&mut window, &counter, cx),
                    MenuCommand::Increment => counter.update(cx, |counter, cx| {
                        counter.value += 1;
                        cx.notify();
                    }),
                    MenuCommand::Quit => cx.quit(),
                });
            }
        })
        .detach();
    });
    log("run loop returned");
}

#[cfg(target_os = "macos")]
mod status_item {
    use futures::channel::mpsc::UnboundedSender;
    use objc2::rc::Retained;
    use objc2::runtime::NSObjectProtocol;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    };
    use objc2_foundation::{NSObject, NSString};

    use super::{MenuCommand, log};

    pub struct TargetIvars {
        tx: UnboundedSender<MenuCommand>,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; MenuTarget adds no Drop.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "BroScribeSpikeMenuTarget"]
        #[ivars = TargetIvars]
        struct MenuTarget;

        unsafe impl NSObjectProtocol for MenuTarget {}

        impl MenuTarget {
            #[unsafe(method(menuItemClicked:))]
            fn menu_item_clicked(&self, sender: &NSMenuItem) {
                let on_main = MainThreadMarker::new().is_some();
                let Some(command) = MenuCommand::from_tag(sender.tag()) else {
                    return;
                };
                log(&format!(
                    "AppKit action {command:?} (main thread: {on_main}, thread {:?})",
                    std::thread::current().id()
                ));
                // Fails only once GPUI's receiver is gone (the app is quitting).
                let _ = self.ivars().tx.unbounded_send(command);
            }
        }
    );

    impl MenuTarget {
        fn new(mtm: MainThreadMarker, tx: UnboundedSender<MenuCommand>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(TargetIvars { tx });
            // SAFETY: NSObject's designated initialiser.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Owns the status item and the menu target. `NSMenuItem.target` is weak, so the
    /// target must outlive the menu; dropping this removes the item from the menu bar.
    pub struct StatusItem {
        item: Retained<NSStatusItem>,
        _target: Retained<MenuTarget>,
    }

    impl StatusItem {
        pub fn install(tx: UnboundedSender<MenuCommand>) -> Self {
            let mtm =
                MainThreadMarker::new().expect("status item must be created on the main thread");
            let target = MenuTarget::new(mtm, tx);

            let item =
                NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            if let Some(button) = item.button(mtm) {
                button.setTitle(&NSString::from_str("BS"));
            }

            let menu = NSMenu::new(mtm);
            for command in MenuCommand::ALL {
                // SAFETY: `menuItemClicked:` is implemented by `MenuTarget` with the
                // `(id sender)` signature AppKit uses for menu actions.
                let menu_item = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(command.title()),
                        Some(sel!(menuItemClicked:)),
                        &NSString::from_str(""),
                    )
                };
                // SAFETY: the target is retained by `StatusItem` for as long as the menu exists.
                unsafe { menu_item.setTarget(Some(&target)) };
                menu_item.setTag(command.tag());
                menu.addItem(&menu_item);
            }
            item.setMenu(Some(&menu));
            log("status item installed");

            Self {
                item,
                _target: target,
            }
        }

        pub fn menu(&self) -> Retained<NSMenu> {
            let mtm = MainThreadMarker::new().expect("main thread");
            self.item.menu(mtm).expect("status item has a menu")
        }
    }

    impl Drop for StatusItem {
        fn drop(&mut self) {
            NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
        }
    }

    /// Fires a menu item through AppKit's own target-action dispatch.
    pub fn perform(menu: &NSMenu, command: MenuCommand) {
        let index = MenuCommand::ALL
            .iter()
            .position(|candidate| *candidate == command)
            .expect("known command");
        menu.performActionForItemAtIndex(index as isize);
    }
}
