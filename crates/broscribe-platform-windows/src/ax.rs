//! [`AppUiReader`] over Windows UI Automation. Roles and attributes map onto
//! the shared macOS vocabulary ([`broscribe_platform::AX_ROLES`]) with macOS
//! Chromium semantics; traversal, attribute and clipping failures mark the
//! snapshot incomplete instead of passing for a whole tree.
use broscribe_platform::{
    AppUiReader, AxChange, AxChangeCallback, AxIncomplete, AxLimits, AxNode, AxObservation,
    AxScope, AxScopePolicy, AxTree, DocumentContext, Error, Result,
};
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, SAFEARRAY},
            LibraryLoader::GetModuleHandleW,
            Ole::{SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound},
            Variant::VARIANT,
        },
        UI::{Accessibility::*, WindowsAndMessaging::*},
    },
    core::{BSTR, Interface, PCWSTR, Ref, implement},
};
// SDK constants keep their C names; patterns name them through this path,
// since a bare non-uppercase constant in a pattern reads like a binding.
use windows::Win32::UI::Accessibility as uia;

/// Per-call UI Automation timeout for observers.
const OBSERVER_TIMEOUT: Duration = Duration::from_secs(1);
/// Characters of one text attribute kept; longer text is clipped and the
/// snapshot marked incomplete.
const MAX_TEXT: usize = 16384;
const WINDOW_TOKEN: &str = "uia-window-";

pub struct WinAppUiReader;
fn automation(timeout: Duration) -> Result<IUIAutomation> {
    let ms = u32::try_from(timeout.as_millis())
        .unwrap_or(u32::MAX)
        .max(1);
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| crate::error("creating UI Automation client", e))?;
        let bounded: IUIAutomation2 = automation
            .cast()
            .map_err(|e| crate::error("UI Automation timeouts", e))?;
        bounded
            .SetConnectionTimeout(ms)
            .and_then(|_| bounded.SetTransactionTimeout(ms))
            .map_err(|e| crate::error("bounding UI Automation calls", e))?;
        Ok(automation)
    }
}

/// UIA control types (and the ARIA heading role, which Chromium exposes
/// on text elements) onto the shared vocabulary. A pane that supports the
/// Scroll pattern is a scroll view (macOS `AXScrollArea`).
fn role(id: UIA_CONTROLTYPE_ID, aria_role: Option<&str>, scrollable: bool) -> &'static str {
    if aria_role.is_some_and(|r| r.eq_ignore_ascii_case("heading")) {
        return "AXHeading";
    }
    match id {
        uia::UIA_ButtonControlTypeId | uia::UIA_SplitButtonControlTypeId => "AXButton",
        uia::UIA_CheckBoxControlTypeId => "AXCheckBox",
        uia::UIA_RadioButtonControlTypeId | uia::UIA_TabItemControlTypeId => "AXRadioButton",
        uia::UIA_ComboBoxControlTypeId => "AXComboBox",
        uia::UIA_EditControlTypeId | uia::UIA_SpinnerControlTypeId => "AXTextField",
        uia::UIA_TextControlTypeId => "AXStaticText",
        uia::UIA_DocumentControlTypeId => "AXWebArea",
        uia::UIA_WindowControlTypeId => "AXWindow",
        uia::UIA_ListControlTypeId => "AXList",
        uia::UIA_ListItemControlTypeId
        | uia::UIA_DataItemControlTypeId
        | uia::UIA_TreeItemControlTypeId => "AXRow",
        uia::UIA_TableControlTypeId | uia::UIA_DataGridControlTypeId => "AXTable",
        uia::UIA_HeaderItemControlTypeId => "AXColumn",
        uia::UIA_MenuControlTypeId => "AXMenu",
        uia::UIA_MenuItemControlTypeId => "AXMenuItem",
        uia::UIA_MenuBarControlTypeId => "AXMenuBar",
        uia::UIA_TabControlTypeId => "AXTabGroup",
        uia::UIA_ToolBarControlTypeId => "AXToolbar",
        uia::UIA_PaneControlTypeId if scrollable => "AXScrollArea",
        uia::UIA_ScrollBarControlTypeId => "AXScrollBar",
        uia::UIA_SliderControlTypeId => "AXSlider",
        uia::UIA_ImageControlTypeId => "AXImage",
        uia::UIA_HyperlinkControlTypeId => "AXLink",
        uia::UIA_TreeControlTypeId => "AXOutline",
        uia::UIA_SeparatorControlTypeId => "AXSplitter",
        uia::UIA_ProgressBarControlTypeId => "AXProgressIndicator",
        _ => "AXGroup",
    }
}

/// What one UIA element says, before mapping.
#[derive(Default)]
struct Fields {
    name: Option<String>,
    aria_role: Option<String>,
    automation_id: Option<String>,
    class_name: Option<String>,
    /// The Value pattern's value (a document's URL for web documents).
    value: Option<String>,
    /// A pane supporting the Scroll pattern (read for panes only).
    scrollable: bool,
}

/// One element onto an [`AxNode`] with macOS Chromium semantics: the
/// single accessible name is both `title` and `description` (macOS splits
/// it by where it came from), static text without a value carries its name
/// as `value`, the automation id (DOM `id`) is the identifier and the class
/// name (DOM `class`) the class list. UIA help text (macOS `AXHelp`, never
/// read there) is left out. A document's value is its URL: returned apart,
/// never a value.
fn mapped(control_type: UIA_CONTROLTYPE_ID, fields: Fields) -> (AxNode, Option<String>) {
    let role = role(control_type, fields.aria_role.as_deref(), fields.scrollable);
    let (value, url) = if role == "AXWebArea" {
        (None, fields.value)
    } else {
        let value = fields.value.or_else(|| {
            (role == "AXStaticText")
                .then(|| fields.name.clone())
                .flatten()
        });
        (value, None)
    };
    let node = AxNode {
        role: role.into(),
        title: fields.name.clone(),
        description: fields.name,
        value,
        identifier: fields.automation_id,
        classes: fields
            .class_name
            .map(|c| c.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        ..AxNode::default()
    };
    (node, url)
}

/// One element placed in a snapshot.
struct Placed {
    node: AxNode,
    /// The minimized state its subtree inherits.
    hidden: Option<bool>,
    /// Whether its contents may be read: not a document of another origin.
    open: bool,
}

/// Maps one element ([`mapped`]) and places it in the snapshot. `window`:
/// a top-level window's token and minimized state (`None`: unknown, which
/// never counts as visible). A top-level element is a window whatever
/// control type its provider reports (a Chromium-hosted one may report a
/// pane): the application's children are its windows on every platform.
/// `document_id`: the element's runtime id, read for documents only.
fn place(
    control_type: UIA_CONTROLTYPE_ID,
    fields: Fields,
    window: Option<(String, Option<bool>)>,
    hidden: Option<bool>,
    document_id: impl FnOnce() -> Option<String>,
    scope: &AxScope,
) -> Placed {
    let (mut node, url) = mapped(control_type, fields);
    if let Some((token, minimized)) = window {
        let hidden = minimized.unwrap_or(true);
        if node.role != "AXWindow" {
            node.role = "AXWindow".into();
        }
        node.document = Some(DocumentContext::new(None, Some(token), hidden));
        return Placed {
            node,
            hidden: Some(hidden),
            open: true,
        };
    }
    if node.role != "AXWebArea" {
        return Placed {
            node,
            hidden,
            open: true,
        };
    }
    let context = DocumentContext::new(url, document_id(), hidden.unwrap_or(true));
    let open = scope.permits(context.url());
    node.document = Some(context);
    Placed { node, hidden, open }
}

/// A text property: failures and clipping mark the snapshot incomplete.
fn text(value: windows::core::Result<BSTR>, incomplete: &mut AxIncomplete) -> Option<String> {
    let value = match value {
        Ok(value) => value.to_string(),
        Err(_) => {
            incomplete.read_failed = true;
            return None;
        }
    };
    if value.chars().count() > MAX_TEXT {
        incomplete.clipped = true;
        return Some(value.chars().take(MAX_TEXT).collect());
    }
    (!value.is_empty()).then_some(value)
}

fn window_token(hwnd: HWND, owner: u32) -> String {
    format!("{WINDOW_TOKEN}{}-{owner}", hwnd.0 as usize)
}

fn parse_window_token(token: &str) -> Option<(usize, u32)> {
    let (hwnd, owner) = token.strip_prefix(WINDOW_TOKEN)?.split_once('-')?;
    Some((hwnd.parse().ok()?, owner.parse().ok()?))
}

/// The UI Automation runtime id of `element`, unique while it exists.
fn runtime_id(element: &IUIAutomationElement) -> Option<String> {
    let array = unsafe { element.GetRuntimeId() }.ok()?;
    if array.is_null() {
        return None;
    }
    let array: *const SAFEARRAY = array;
    let ids = (|| {
        let lower = unsafe { SafeArrayGetLBound(array, 1) }?;
        let upper = unsafe { SafeArrayGetUBound(array, 1) }?;
        let mut ids = Vec::new();
        for index in lower..=upper {
            let mut id = 0i32;
            unsafe { SafeArrayGetElement(array, &index, (&mut id as *mut i32).cast()) }?;
            ids.push(id.to_string());
        }
        windows::core::Result::Ok(ids)
    })();
    // SAFETY: GetRuntimeId hands over the array; it is destroyed once.
    let _ = unsafe { SafeArrayDestroy(array) };
    ids.ok()
        .filter(|ids| !ids.is_empty())
        .map(|ids| format!("uia-doc-{}", ids.join(".")))
}

struct Walk<'a> {
    walker: &'a IUIAutomationTreeWalker,
    scope: &'a AxScope,
    limits: AxLimits,
    deadline: Instant,
    count: usize,
    incomplete: AxIncomplete,
}

impl Walk<'_> {
    /// The next element of a traversal: a null result ends it, any other
    /// failure ends it as a read failure.
    fn next(
        &mut self,
        result: windows::core::Result<IUIAutomationElement>,
    ) -> Option<IUIAutomationElement> {
        match result {
            Ok(element) => Some(element),
            Err(e) if e.code() == windows::core::Error::empty().code() => None,
            Err(_) => {
                self.incomplete.read_failed = true;
                None
            }
        }
    }

    /// Reads `element` and, within the limits, its subtree. `hidden`: the
    /// top-level window's minimized state; `window`: its token, for the
    /// top-level window itself.
    fn node(
        &mut self,
        element: &IUIAutomationElement,
        depth: usize,
        hidden: Option<bool>,
        window: Option<String>,
    ) -> windows::core::Result<AxNode> {
        self.count += 1;
        let control_type = unsafe { element.CurrentControlType() }?;
        let incomplete = &mut self.incomplete;
        let fields = Fields {
            name: text(unsafe { element.CurrentName() }, incomplete),
            aria_role: text(unsafe { element.CurrentAriaRole() }, incomplete),
            automation_id: text(unsafe { element.CurrentAutomationId() }, incomplete),
            class_name: text(unsafe { element.CurrentClassName() }, incomplete),
            value: unsafe {
                element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            }
            .ok()
            .and_then(|pattern| text(unsafe { pattern.CurrentValue() }, incomplete)),
            scrollable: window.is_none()
                && control_type == UIA_PaneControlTypeId
                && unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
                }
                .is_ok(),
        };
        let window = window.map(|token| {
            // Only a known, not-minimized state makes a window visible.
            let minimized = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationWindowPattern>(UIA_WindowPatternId)
            }
            .ok()
            .and_then(|pattern| unsafe { pattern.CurrentWindowVisualState() }.ok())
            .map(|state| state == WindowVisualState_Minimized);
            (token, minimized)
        });
        let Placed {
            mut node,
            hidden,
            open,
        } = place(
            control_type,
            fields,
            window,
            hidden,
            || runtime_id(element),
            self.scope,
        );
        if !open {
            // Another origin: identity only, nothing inside is read.
            return Ok(node);
        }
        if node.role == "AXWebArea" && self.scope.policy().document_text {
            let document = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            }
            .ok()
            .and_then(|pattern| unsafe { pattern.DocumentRange() }.ok())
            .map(|range| unsafe { range.GetText(MAX_TEXT as i32 + 1) });
            if let Some(document) = document {
                node.value = text(document, &mut self.incomplete);
            }
        }
        let walker = self.walker;
        let mut child = self.next(unsafe { walker.GetFirstChildElement(element) });
        if depth >= self.limits.max_depth {
            if child.is_some() {
                self.incomplete.depth_cap = true;
            }
            return Ok(node);
        }
        while let Some(current) = child {
            if self.count >= self.limits.max_nodes {
                self.incomplete.node_cap = true;
                break;
            }
            if Instant::now() >= self.deadline {
                self.incomplete.time_budget = true;
                break;
            }
            match self.node(&current, depth + 1, hidden, None) {
                Ok(n) => node.children.push(n),
                // The element vanished or did not answer: partial.
                Err(_) => self.incomplete.read_failed = true,
            }
            child = self.next(unsafe { walker.GetNextSiblingElement(&current) });
        }
        Ok(node)
    }
}

impl WinAppUiReader {
    fn read_tree(&self, scope: &AxScope, limits: AxLimits) -> Result<AxTree> {
        limits.validate()?;
        let _com = crate::Com::new()?;
        let automation = automation(limits.element_timeout)?;
        let walker = unsafe { automation.ControlViewWalker() }
            .map_err(|e| crate::error("UI Automation tree walker", e))?;
        let mut walk = Walk {
            walker: &walker,
            scope,
            limits,
            deadline: Instant::now() + limits.budget,
            count: 1,
            incomplete: AxIncomplete::default(),
        };
        let mut root = AxNode::new("AXApplication");
        for (hwnd, owner) in super::top_windows(Some(scope.pid()))? {
            if limits.max_depth == 0 {
                walk.incomplete.depth_cap = true;
                break;
            }
            if walk.count >= limits.max_nodes {
                walk.incomplete.node_cap = true;
                break;
            }
            if Instant::now() >= walk.deadline {
                walk.incomplete.time_budget = true;
                break;
            }
            let element = match unsafe { automation.ElementFromHandle(hwnd) } {
                Ok(element) => element,
                // Closed between enumeration and reading, or unanswered.
                Err(_) => {
                    walk.incomplete.read_failed = true;
                    continue;
                }
            };
            match walk.node(&element, 1, None, Some(window_token(hwnd, owner))) {
                Ok(node) => root.children.push(node),
                Err(_) => walk.incomplete.read_failed = true,
            }
        }
        Ok(AxTree {
            root,
            incomplete: walk.incomplete,
        })
    }
}

const CHANGED: u32 = WM_APP + 64;
struct Callback {
    callback: AxChangeCallback,
    changes: Vec<AxChange>,
    active: Arc<AtomicBool>,
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }
    if message == CHANGED {
        let callback = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Callback;
        if !callback.is_null() {
            let callback = unsafe { &*callback };
            if callback.active.load(Ordering::Acquire)
                && let Some(change) = AxChange::ALL.get(wparam.0)
                && callback.changes.contains(change)
            {
                // Never unwind into the message loop.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (callback.callback)(*change)
                }));
            }
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
#[derive(Clone)]
struct Target {
    hwnd: usize,
    pid: u32,
    active: Arc<AtomicBool>,
}
impl Target {
    fn post(&self, sender: Ref<IUIAutomationElement>, change: AxChange) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let Some(sender) = sender.as_ref() else {
            return;
        };
        if unsafe { sender.CurrentProcessId() }.ok() != Some(self.pid as i32) {
            return;
        }
        if let Some(index) = AxChange::ALL.iter().position(|c| *c == change) {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    CHANGED,
                    WPARAM(index),
                    LPARAM(0),
                );
            }
        }
    }
}
#[implement(IUIAutomationEventHandler)]
struct Events(Target);
impl IUIAutomationEventHandler_Impl for Events_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: Ref<IUIAutomationElement>,
        event: UIA_EVENT_ID,
    ) -> windows::core::Result<()> {
        let change = match event {
            uia::UIA_Window_WindowOpenedEventId => AxChange::WindowCreated,
            uia::UIA_Window_WindowClosedEventId => AxChange::ElementDestroyed,
            uia::UIA_LiveRegionChangedEventId => AxChange::LiveRegionChanged,
            _ => AxChange::LayoutChanged,
        };
        self.0.post(sender, change);
        Ok(())
    }
}
#[implement(IUIAutomationPropertyChangedEventHandler)]
struct Properties(Target);
impl IUIAutomationPropertyChangedEventHandler_Impl for Properties_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: Ref<IUIAutomationElement>,
        property: UIA_PROPERTY_ID,
        _value: &VARIANT,
    ) -> windows::core::Result<()> {
        self.0.post(
            sender,
            if property == UIA_NamePropertyId {
                AxChange::TitleChanged
            } else {
                AxChange::ValueChanged
            },
        );
        Ok(())
    }
}
#[implement(IUIAutomationStructureChangedEventHandler)]
struct Structure(Target);
impl IUIAutomationStructureChangedEventHandler_Impl for Structure_Impl {
    fn HandleStructureChangedEvent(
        &self,
        sender: Ref<IUIAutomationElement>,
        change: StructureChangeType,
        _runtime: *const SAFEARRAY,
    ) -> windows::core::Result<()> {
        let change = match change {
            uia::StructureChangeType_ChildAdded | uia::StructureChangeType_ChildrenBulkAdded => {
                AxChange::ElementCreated
            }
            uia::StructureChangeType_ChildRemoved
            | uia::StructureChangeType_ChildrenBulkRemoved => AxChange::ElementDestroyed,
            _ => AxChange::LayoutChanged,
        };
        self.0.post(sender, change);
        Ok(())
    }
}
#[implement(IUIAutomationFocusChangedEventHandler)]
struct Focus(Target);
impl IUIAutomationFocusChangedEventHandler_Impl for Focus_Impl {
    fn HandleFocusChangedEvent(
        &self,
        sender: Ref<IUIAutomationElement>,
    ) -> windows::core::Result<()> {
        self.0.post(sender, AxChange::FocusedWindowChanged);
        Ok(())
    }
}
struct Observation {
    hwnd: HWND,
    callback: Box<Callback>,
    automation: IUIAutomation,
    root: IUIAutomationElement,
    events: IUIAutomationEventHandler,
    properties: IUIAutomationPropertyChangedEventHandler,
    structure: IUIAutomationStructureChangedEventHandler,
    focus: IUIAutomationFocusChangedEventHandler,
    registered_events: Vec<UIA_EVENT_ID>,
    properties_added: bool,
    structure_added: bool,
    focus_added: bool,
    _main_thread: PhantomData<Rc<()>>,
    _com: crate::Com,
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.callback.active.store(false, Ordering::Release);
        unsafe {
            for event in &self.registered_events {
                let _ =
                    self.automation
                        .RemoveAutomationEventHandler(*event, &self.root, &self.events);
            }
            if self.properties_added {
                let _ = self
                    .automation
                    .RemovePropertyChangedEventHandler(&self.root, &self.properties);
            }
            if self.structure_added {
                let _ = self
                    .automation
                    .RemoveStructureChangedEventHandler(&self.root, &self.structure);
            }
            if self.focus_added {
                let _ = self.automation.RemoveFocusChangedEventHandler(&self.focus);
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
impl AxObservation for Observation {}
impl AppUiReader for WinAppUiReader {
    fn is_trusted(&self) -> bool {
        match crate::Com::new() {
            Ok(_com) => automation(OBSERVER_TIMEOUT).is_ok(),
            Err(_) => false,
        }
    }
    fn request_trust(&self) -> bool {
        self.is_trusted()
    }
    fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
        self.read_tree(&AxScope::new(pid, AxScopePolicy::default()), limits)
    }
    /// Tokens are self-describing (window handle and owner, runtime ids):
    /// no per-incarnation state. Documents of other origins are skipped
    /// while walking.
    fn snapshot_in(&self, scope: &AxScope, limits: AxLimits) -> Result<AxTree> {
        self.read_tree(scope, limits)
    }
    /// A top-level window is destroyed when its handle no longer names a
    /// window of the scope's process. Hidden, minimized or unenumerated
    /// windows keep their handle and are never listed; documents are never
    /// verified.
    fn destroyed(&self, scope: &AxScope, elements: &[String]) -> Result<Vec<String>> {
        Ok(elements
            .iter()
            .filter(|token| {
                parse_window_token(token).is_some_and(|(hwnd, owner)| {
                    owner == scope.pid() && {
                        let hwnd = HWND(hwnd as *mut _);
                        let mut current = 0;
                        // SAFETY: both calls accept any handle value.
                        let exists = unsafe { IsWindow(Some(hwnd)) }.as_bool();
                        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut current)) };
                        !exists || current != owner
                    }
                })
            })
            .cloned()
            .collect())
    }
    fn observe(
        &self,
        pid: u32,
        changes: &[AxChange],
        callback: AxChangeCallback,
    ) -> Result<Box<dyn AxObservation>> {
        let com = crate::Com::new()?;
        let automation = automation(OBSERVER_TIMEOUT)?;
        let root = unsafe { automation.GetRootElement() }
            .map_err(|e| crate::error("UI Automation desktop root", e))?;
        let active = Arc::new(AtomicBool::new(true));
        let mut callback = Box::new(Callback {
            callback,
            changes: changes.to_vec(),
            active: active.clone(),
        });
        let class = crate::wide("BroScribe.Platform.UIAutomation");
        let hwnd = unsafe {
            let module =
                GetModuleHandleW(None).map_err(|e| crate::error("UI Automation module", e))?;
            let definition = WNDCLASSW {
                hInstance: module.into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                lpfnWndProc: Some(window_proc),
                ..Default::default()
            };
            if RegisterClassW(&definition) == 0
                && windows::Win32::Foundation::GetLastError()
                    != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS
            {
                return Err(crate::error(
                    "registering UI Automation message window",
                    std::io::Error::last_os_error(),
                ));
            }
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(module.into()),
                Some((&mut *callback as *mut Callback).cast()),
            )
            .map_err(|e| crate::error("UI Automation message-only window", e))?
        };
        let target = Target {
            hwnd: hwnd.0 as usize,
            pid,
            active,
        };
        let mut observation = Observation {
            hwnd,
            callback,
            automation,
            root,
            events: Events(target.clone()).into(),
            properties: Properties(target.clone()).into(),
            structure: Structure(target.clone()).into(),
            focus: Focus(target).into(),
            registered_events: Vec::new(),
            properties_added: false,
            structure_added: false,
            focus_added: false,
            _main_thread: PhantomData,
            _com: com,
        };
        unsafe {
            for (change, event) in [
                (AxChange::WindowCreated, UIA_Window_WindowOpenedEventId),
                (AxChange::ElementDestroyed, UIA_Window_WindowClosedEventId),
                (AxChange::LayoutChanged, UIA_LayoutInvalidatedEventId),
                (AxChange::LiveRegionChanged, UIA_LiveRegionChangedEventId),
            ] {
                if changes.contains(&change) {
                    observation
                        .automation
                        .AddAutomationEventHandler(
                            event,
                            &observation.root,
                            TreeScope_Subtree,
                            None,
                            &observation.events,
                        )
                        .map_err(|e| crate::error("registering UI Automation event", e))?;
                    observation.registered_events.push(event);
                }
            }
            let mut properties = Vec::new();
            if changes.contains(&AxChange::TitleChanged) {
                properties.push(UIA_NamePropertyId);
            }
            if changes.contains(&AxChange::ValueChanged) {
                properties.push(UIA_ValueValuePropertyId);
            }
            if !properties.is_empty() {
                observation
                    .automation
                    .AddPropertyChangedEventHandlerNativeArray(
                        &observation.root,
                        TreeScope_Subtree,
                        None,
                        &observation.properties,
                        &properties,
                    )
                    .map_err(|e| crate::error("registering UI Automation properties", e))?;
                observation.properties_added = true;
            }
            if changes.iter().any(|c| {
                matches!(
                    c,
                    AxChange::ElementCreated | AxChange::ElementDestroyed | AxChange::LayoutChanged
                )
            }) {
                observation
                    .automation
                    .AddStructureChangedEventHandler(
                        &observation.root,
                        TreeScope_Subtree,
                        None,
                        &observation.structure,
                    )
                    .map_err(|e| crate::error("registering UI Automation structure events", e))?;
                observation.structure_added = true;
            }
            if changes.contains(&AxChange::FocusedWindowChanged) {
                observation
                    .automation
                    .AddFocusChangedEventHandler(None, &observation.focus)
                    .map_err(|e| crate::error("registering UI Automation focus events", e))?;
                observation.focus_added = true;
            }
        }
        Ok(Box::new(observation))
    }
    fn manual_accessibility(&self, _pid: u32) -> Result<Option<bool>> {
        Ok(None)
    }
    fn set_manual_accessibility(&self, _pid: u32, _enabled: bool) -> Result<()> {
        Err(Error::Unsupported("AXManualAccessibility is a macOS-only attribute; Windows Chromium exposes its tree through UI Automation".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::AX_ROLES;
    use broscribe_readers::rules::conformance::{self, Element, Kind, Mapper, Os};
    use std::{cell::RefCell, collections::HashSet};
    use windows::Win32::Foundation::E_FAIL;

    #[test]
    fn control_types_map_into_and_cover_the_shared_vocabulary() {
        // Every UIA control type, as each element the mapper tells apart.
        let mut emitted: HashSet<&str> = (UIA_ButtonControlTypeId.0..=UIA_AppBarControlTypeId.0)
            .map(UIA_CONTROLTYPE_ID)
            .flat_map(|id| {
                [
                    role(id, None, false),
                    role(id, None, true),
                    role(id, Some("heading"), false),
                ]
            })
            .collect();
        for ax in &emitted {
            assert!(AX_ROLES.contains(ax), "{ax}");
        }
        // The snapshot root, and top-level elements whatever their type.
        emitted.extend(["AXApplication", "AXWindow"]);
        for ax in AX_ROLES {
            assert!(emitted.contains(ax), "no UIA element maps to {ax}");
        }
        assert_eq!(role(UIA_PaneControlTypeId, None, true), "AXScrollArea");
        assert_eq!(role(UIA_PaneControlTypeId, None, false), "AXGroup");
        assert_eq!(
            role(UIA_GroupControlTypeId, Some("region"), false),
            "AXGroup"
        );
    }

    #[test]
    fn fields_follow_macos_chromium_semantics() {
        let (button, _) = mapped(
            UIA_ButtonControlTypeId,
            Fields {
                name: Some("Leave call".into()),
                automation_id: Some("hangup-button".into()),
                class_name: Some("control  leave".into()),
                ..Fields::default()
            },
        );
        assert_eq!(button.role, "AXButton");
        assert_eq!(button.title.as_deref(), Some("Leave call"));
        assert_eq!(button.description.as_deref(), Some("Leave call"));
        assert_eq!(button.identifier.as_deref(), Some("hangup-button"));
        assert_eq!(button.classes, ["control", "leave"]);
        let (text, _) = mapped(
            UIA_TextControlTypeId,
            Fields {
                name: Some("You left the meeting".into()),
                ..Fields::default()
            },
        );
        assert_eq!(text.value.as_deref(), Some("You left the meeting"));
        // A document's Value is its URL: never exposed as a value.
        let (document, url) = mapped(
            UIA_DocumentControlTypeId,
            Fields {
                value: Some("https://meet.google.com/abc-defg-hij".into()),
                ..Fields::default()
            },
        );
        assert_eq!(document.value, None);
        assert_eq!(url.as_deref(), Some("https://meet.google.com/abc-defg-hij"));
    }

    const PID: u32 = 4242;

    /// Synthetic UIA answers for conformance elements, through [`place`]:
    /// a Chromium-hosted top-level window reports a pane, static text names
    /// its text, a heading is text with the ARIA heading role, a document's
    /// URL is its Value, a window handle and a runtime id stand for each
    /// keyed element. Document text patterns are not modelled.
    #[derive(Default)]
    struct Uia {
        keys: RefCell<Vec<String>>,
    }

    impl Uia {
        /// A stable native number for `key`.
        fn number(&self, key: &str) -> usize {
            let mut keys = self.keys.borrow_mut();
            let index = match keys.iter().position(|k| k == key) {
                Some(index) => index,
                None => {
                    keys.push(key.to_owned());
                    keys.len() - 1
                }
            };
            index + 1
        }

        fn node(
            &self,
            element: &Element,
            top: bool,
            hidden: Option<bool>,
            scope: &AxScope,
            incomplete: &mut AxIncomplete,
        ) -> AxNode {
            let hosts_web = element.children.iter().any(|c| c.kind == Kind::Document);
            let (control_type, aria_role) = match element.kind {
                Kind::Window if hosts_web => (UIA_PaneControlTypeId, None),
                Kind::Window => (UIA_WindowControlTypeId, None),
                Kind::Document => (UIA_DocumentControlTypeId, None),
                Kind::Group => (UIA_GroupControlTypeId, None),
                Kind::Outline => (UIA_TreeControlTypeId, None),
                Kind::Row => (UIA_TreeItemControlTypeId, None),
                Kind::Button => (UIA_ButtonControlTypeId, None),
                Kind::Text => (UIA_TextControlTypeId, None),
                Kind::Heading => (UIA_TextControlTypeId, Some("heading")),
            };
            let answer = |value: Option<&str>| {
                windows::core::Result::Ok(BSTR::from(value.unwrap_or_default()))
            };
            let name = if element.kind == Kind::Text {
                element.text.as_deref()
            } else {
                element.name.as_deref()
            };
            let classes = element.classes.join(" ");
            let fields = Fields {
                name: text(
                    if element.unreadable {
                        Err(E_FAIL.into())
                    } else {
                        answer(name)
                    },
                    incomplete,
                ),
                aria_role: text(answer(aria_role), incomplete),
                automation_id: text(answer(element.id.as_deref()), incomplete),
                class_name: text(answer(Some(classes.as_str())), incomplete),
                value: (element.kind == Kind::Document)
                    .then(|| text(answer(element.url.as_deref()), incomplete))
                    .flatten(),
                scrollable: false,
            };
            let key = element.key.as_deref();
            let window = top.then(|| {
                let handle = self.number(key.expect("windows have keys"));
                (window_token(HWND(handle as *mut _), PID), element.minimized)
            });
            let placed = place(
                control_type,
                fields,
                window,
                hidden,
                || key.map(|k| format!("uia-doc-{}", self.number(k))),
                scope,
            );
            let mut node = placed.node;
            if placed.open {
                node.children = element
                    .children
                    .iter()
                    .map(|child| self.node(child, false, placed.hidden, scope, incomplete))
                    .collect();
            }
            node
        }
    }

    impl Mapper for Uia {
        fn os(&self) -> Os {
            Os::Windows
        }

        fn snapshot(&self, windows: &[Element], policy: &AxScopePolicy) -> AxTree {
            let scope = AxScope::new(PID, policy.clone());
            let mut incomplete = AxIncomplete::default();
            let mut root = AxNode::new("AXApplication");
            root.children = windows
                .iter()
                .map(|window| self.node(window, true, None, &scope, &mut incomplete))
                .collect();
            AxTree { root, incomplete }
        }
    }

    #[test]
    fn every_shipped_rule_reads_through_the_uia_mapping() {
        conformance::check(&Uia::default());
    }

    #[test]
    fn window_tokens_round_trip_and_reject_others() {
        let token = window_token(HWND(0x1234 as *mut _), 77);
        assert_eq!(parse_window_token(&token), Some((0x1234, 77)));
        for other in [
            "uia-doc-42.7",
            "mac-el-1",
            "uia-window-x-1",
            "uia-window-12",
        ] {
            assert_eq!(parse_window_token(other), None, "{other}");
        }
        // Tokens of another process are never verified destroyed here.
        let scope = AxScope::new(78, AxScopePolicy::default());
        assert!(
            WinAppUiReader
                .destroyed(&scope, &[token])
                .unwrap()
                .is_empty()
        );
    }
}
