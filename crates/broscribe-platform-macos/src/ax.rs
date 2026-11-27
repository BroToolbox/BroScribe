//! [`AppUiReader`] over the macOS Accessibility API (design §9): snapshots
//! of an app's `AXUIElement` tree and `AXObserver` notifications on the
//! calling thread's run loop — GPUI's main loop in `broscribe-app`, which
//! holds the Accessibility grant.
//!
//! Needs the Accessibility permission (setup step 2, optional). Reading never
//! raises a window, takes a screenshot or runs OCR, and works while the
//! meeting window sits behind others.
//!
//! Each reader incarnation's [`AxScope`] carries its own element registry
//! (web documents and top-level windows, compared with `CFEqual`), dropped
//! with the scope's last clone: a detached walk of a stopped incarnation can
//! only touch its own registry.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use broscribe_platform::{
    AppUiReader, AxChange, AxChangeCallback, AxIncomplete, AxLimits, AxNode, AxObservation,
    AxScope, AxScopePolicy, AxTree, DocumentContext, Error, Result, ax_transient,
};
use objc2::MainThreadMarker;
use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions,
    AXObserver, AXUIElement, AXValue, AXValueType, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFEqual, CFNull, CFRetained, CFRunLoop, CFRunLoopSource,
    CFString, CFType, CFURL, Type, kCFRunLoopCommonModes,
};

/// Seconds an unresponsive app may block one observer or switch call.
const MESSAGING_TIMEOUT_S: f32 = 1.0;
/// Chromium's switch for building the web accessibility tree without a
/// screen reader (Chromium-based browsers, Electron).
const MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";

/// Attributes read per element, in one `AXUIElementCopyMultipleAttributeValues`
/// call (indices used by [`Walk::node`]).
const ATTRIBUTES: [&str; 11] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXValue",
    "AXDescription",
    "AXIdentifier",
    "AXDOMIdentifier",
    "AXDOMClassList",
    "AXChildren",
    // Web areas only: the document URL (call identity, plan H).
    "AXURL",
    // Windows only: whether the window is minimized.
    "AXMinimized",
];

/// Most elements (documents and windows) one registry tells apart; beyond
/// it a new element gets no identity (it then proves nothing) and the
/// snapshot that met it is marked [`AxIncomplete::identity_cap`]. Tokens are
/// never recycled: a fresh incarnation (a new [`AxScope`]) starts empty.
const MAX_ELEMENTS: usize = 128;

/// The registry refused a new element: it already tells [`MAX_ELEMENTS`]
/// apart.
#[derive(Debug, PartialEq, Eq)]
struct Full;

/// An element kept to recognise the same document or window later.
struct Element(CFRetained<AXUIElement>);
// SAFETY: an AXUIElementRef is an immutable CF reference to a remote
// element; it is only retained, released and compared with `CFEqual` here,
// which Core Foundation allows from any thread.
unsafe impl Send for Element {}

/// One registry of web documents and top-level windows: each distinct
/// element (by `CFEqual`, the header's documented equality for
/// AXUIElementRefs) gets an opaque id from a counter never reused within the
/// registry, so a forgotten or new element never aliases an earlier one.
#[derive(Default)]
struct Elements {
    next: u64,
    known: Vec<(Element, u64)>,
}

impl Elements {
    /// The token of `element`; [`Full`] once the registry is full and the
    /// element is new.
    fn id(&mut self, element: &AXUIElement) -> std::result::Result<String, Full> {
        let probe: &CFType = element;
        if let Some((_, id)) = self.known.iter().find(|(known, _)| {
            let seen: &CFType = &known.0;
            CFEqual(Some(seen), Some(probe))
        }) {
            return Ok(token(*id));
        }
        if self.known.len() >= MAX_ELEMENTS {
            return Err(Full);
        }
        self.next += 1;
        self.known.push((Element(element.retain()), self.next));
        Ok(token(self.next))
    }

    /// The element `token` names in this registry.
    fn find(&self, token_text: &str) -> Option<CFRetained<AXUIElement>> {
        self.known
            .iter()
            .find(|(_, id)| token(*id) == token_text)
            .map(|(element, _)| element.0.retain())
    }
}

fn token(id: u64) -> String {
    format!("mac-el-{id}")
}

/// A scope's registry.
type Registry = Mutex<Elements>;

fn registry(registry: &Registry) -> MutexGuard<'_, Elements> {
    registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A document's URL, from an `AXURL` value (a CFURL, or a string).
fn document_url(value: Option<&CFRetained<CFType>>) -> Option<String> {
    let value = value?;
    let url = match value.downcast_ref::<CFURL>() {
        Some(url) => url.string().to_string(),
        None => value.downcast_ref::<CFString>()?.to_string(),
    };
    (!url.is_empty()).then_some(url)
}

fn notification_name(change: AxChange) -> &'static str {
    match change {
        AxChange::ValueChanged => "AXValueChanged",
        AxChange::TitleChanged => "AXTitleChanged",
        AxChange::ElementCreated => "AXUIElementCreated",
        AxChange::ElementDestroyed => "AXUIElementDestroyed",
        AxChange::WindowCreated => "AXWindowCreated",
        AxChange::FocusedWindowChanged => "AXFocusedWindowChanged",
        AxChange::LayoutChanged => "AXLayoutChanged",
        AxChange::LiveRegionChanged => "AXLiveRegionChanged",
    }
}

fn change_for(name: &str) -> Option<AxChange> {
    AxChange::ALL
        .into_iter()
        .find(|c| notification_name(*c) == name)
}

fn ax_error(what: &str, pid: u32, err: AXError) -> Error {
    match err {
        AXError::APIDisabled => {
            Error::Unsupported("the Accessibility permission is not granted".into())
        }
        AXError::InvalidUIElement => Error::NotFound(format!("{what}: pid {pid} has no UI")),
        AXError::AttributeUnsupported | AXError::NotificationUnsupported | AXError::NoValue => {
            Error::Unsupported(format!("{what}: not supported by pid {pid}"))
        }
        // Busy or past the messaging timeout: worth retrying later.
        AXError::CannotComplete => {
            ax_transient(format!("{what}: pid {pid} did not answer (busy or quit)"))
        }
        other => Error::Internal(format!("{what}: AX error {} for pid {pid}", other.0)),
    }
}

fn application(pid: u32, timeout_s: f32) -> Result<CFRetained<AXUIElement>> {
    let raw = libc::pid_t::try_from(pid)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| Error::InvalidInput(format!("invalid pid {pid}")))?;
    // SAFETY: any positive pid is accepted; an element for a missing process
    // only makes later calls fail with `InvalidUIElement`.
    let app = unsafe { AXUIElement::new_application(raw) };
    set_timeout(&app, timeout_s);
    Ok(app)
}

/// Bounds how long one unresponsive element may block a call on it.
fn set_timeout(element: &AXUIElement, timeout_s: f32) {
    // SAFETY: plain setter on a valid element; a timeout set on one element
    // applies to that element only, so every element read gets its own.
    let _ = unsafe { element.set_messaging_timeout(timeout_s) };
}

/// One attribute of `element`; `Err` carries the AX error.
fn copy_attribute(
    element: &AXUIElement,
    name: &str,
) -> std::result::Result<CFRetained<CFType>, AXError> {
    let name = CFString::from_str(name);
    let mut value: *const CFType = ptr::null();
    // SAFETY: `value` is a valid out-pointer; on success it holds a +1
    // reference that `from_raw` takes over.
    let err = unsafe { element.copy_attribute_value(&name, NonNull::from(&mut value)) };
    if err != AXError::Success {
        return Err(err);
    }
    NonNull::new(value.cast_mut())
        // SAFETY: a +1 reference from a Copy function.
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or(AXError::NoValue)
}

/// Bulk reads can succeed while individual slots contain AX error values.
fn copy_attributes(
    element: &AXUIElement,
    names: &CFArray<CFString>,
) -> std::result::Result<CFRetained<CFArray<CFType>>, AXError> {
    let mut values: *const CFArray = ptr::null();
    // SAFETY: `names` is an array of CFStrings and `values` a valid
    // out-pointer receiving a +1 array.
    let err = unsafe {
        element.copy_multiple_attribute_values(
            names.as_opaque(),
            AXCopyMultipleAttributeOptions::empty(),
            NonNull::from(&mut values),
        )
    };
    if err != AXError::Success {
        return Err(err);
    }
    let values = NonNull::new(values.cast_mut()).ok_or(AXError::NoValue)?;
    // SAFETY: the documented result is a +1 array of CF objects, including
    // AXValue error placeholders and CFNull for missing attributes.
    Ok(unsafe { CFRetained::cast_unchecked(CFRetained::from_raw(values)) })
}

fn attribute_error(value: &CFType) -> Option<AXError> {
    let value = value.downcast_ref::<AXValue>()?;
    let mut error = AXError::Failure;
    // SAFETY: the output points to an AXError. AXValueGetValue checks the
    // requested type and writes nothing if this AXValue wraps another type.
    unsafe { value.value(AXValueType::AXError, NonNull::from(&mut error).cast()) }.then_some(error)
}

/// A bulk-read slot holding a real read failure: the element did not
/// answer in time or vanished. Unsupported and absent attributes are not
/// failures.
fn slot_failed(value: Option<&CFRetained<CFType>>) -> bool {
    value
        .and_then(|v| attribute_error(v))
        .is_some_and(|e| matches!(e, AXError::CannotComplete | AXError::InvalidUIElement))
}

fn children(
    value: Option<&CFRetained<CFType>>,
) -> std::result::Result<Vec<CFRetained<CFType>>, AXError> {
    let value = value.ok_or(AXError::NoValue)?;
    if value.downcast_ref::<CFArray>().is_some() {
        return Ok(array_items(Some(value)));
    }
    match attribute_error(value) {
        // A leaf may not implement AXChildren, or have no value for it.
        Some(AXError::AttributeUnsupported | AXError::NoValue) => Ok(Vec::new()),
        Some(error) => Err(error),
        None if value.downcast_ref::<CFNull>().is_some() => Ok(Vec::new()),
        // An unexpected type is not evidence that traversal reached a leaf.
        None => Err(AXError::Failure),
    }
}

fn text(value: Option<&CFRetained<CFType>>) -> Option<String> {
    let s = value?.downcast_ref::<CFString>()?.to_string();
    (!s.is_empty()).then_some(s)
}

fn array_items(value: Option<&CFRetained<CFType>>) -> Vec<CFRetained<CFType>> {
    let Some(array) = value.and_then(|v| v.downcast_ref::<CFArray>()) else {
        return Vec::new();
    };
    // SAFETY: every CFArray holds CF objects.
    let array: &CFArray<CFType> = unsafe { array.cast_unchecked() };
    array.iter().collect()
}

type Read<'r> = dyn Fn(
        &AXUIElement,
        &CFArray<CFString>,
    ) -> std::result::Result<CFRetained<CFArray<CFType>>, AXError>
    + 'r;

struct Walk<'a> {
    names: CFRetained<CFArray<CFString>>,
    limits: AxLimits,
    deadline: Instant,
    nodes: usize,
    incomplete: AxIncomplete,
    scope: &'a AxScope,
    registry: &'a Registry,
}

impl Walk<'_> {
    /// The incarnation's token of a window or document. A full registry
    /// leaves it without one and records why: the snapshot can no longer
    /// tell that element apart, so it is not whole.
    fn identity(&mut self, element: &AXUIElement) -> Option<String> {
        match registry(self.registry).id(element) {
            Ok(id) => Some(id),
            Err(Full) => {
                self.incomplete.identity_cap = true;
                None
            }
        }
    }

    /// Reads `element` and, within the limits, its subtree. `hidden`: the
    /// ancestor window's state, or `None` before any window has been read.
    fn node(
        &mut self,
        element: &AXUIElement,
        depth: usize,
        hidden: Option<bool>,
        read: &Read<'_>,
    ) -> std::result::Result<AxNode, AXError> {
        let values = read(element, &self.names)?;
        let v: Vec<CFRetained<CFType>> = values.iter().collect();
        let at = |i: usize| v.get(i);
        self.nodes += 1;
        // A slot that timed out or whose element vanished is a read
        // failure: what it would have said is unknown.
        if (0..ATTRIBUTES.len()).any(|i| i != 8 && slot_failed(at(i))) {
            self.incomplete.read_failed = true;
        }
        let mut node = AxNode {
            role: text(at(0)).unwrap_or_default(),
            subrole: text(at(1)),
            title: text(at(2)),
            value: text(at(3)),
            description: text(at(4)),
            identifier: text(at(5)).or_else(|| text(at(6))),
            classes: array_items(at(7))
                .iter()
                .filter_map(|c| c.downcast_ref::<CFString>().map(|s| s.to_string()))
                .collect(),
            document: None,
            children: Vec::new(),
        };
        if node.role.is_empty() {
            // An element that cannot say what it is cannot be matched.
            self.incomplete.read_failed = true;
        }
        let hidden = if node.role == "AXWindow" {
            // Only an actual false boolean establishes a usable window.
            // Missing, unsupported and failed visibility must never turn an
            // unknown window into a visible post-call surface. Keep its
            // controls readable so they can still veto an automatic stop.
            Some(
                hidden.unwrap_or(false)
                    || at(10)
                        .and_then(|v| v.downcast_ref::<CFBoolean>())
                        .map(CFBoolean::as_bool)
                        .unwrap_or(true),
            )
        } else {
            hidden
        };
        // Top-level windows carry their element identity (window-closed
        // call ends); web documents their URL and element identity.
        if node.role == "AXWindow" && depth == 1 {
            let id = self.identity(element);
            node.document = Some(DocumentContext::new(None, id, hidden.unwrap_or(true)));
        }
        if node.role == "AXWebArea" {
            let id = self.identity(element);
            let context = DocumentContext::new(document_url(at(9)), id, hidden.unwrap_or(true));
            if !self.scope.policy().document_text {
                node.value = None;
            }
            let permitted = self.scope.permits(context.url());
            node.document = Some(context);
            if !permitted {
                // Another origin: identity only, nothing inside is read.
                return Ok(node);
            }
        }
        let children = match children(at(8)) {
            Ok(children) => children,
            Err(_) => {
                self.incomplete.read_failed = true;
                Vec::new()
            }
        };
        for child in children {
            if depth + 1 > self.limits.max_depth {
                self.incomplete.depth_cap = true;
                break;
            }
            if self.nodes >= self.limits.max_nodes {
                self.incomplete.node_cap = true;
                break;
            }
            if Instant::now() >= self.deadline {
                self.incomplete.time_budget = true;
                break;
            }
            // A child that is no element, or that could not be read (it
            // vanished between the two calls, or did not answer), leaves the
            // snapshot partial: it is still returned, but marked incomplete
            // so nothing treats it as a whole tree.
            let Some(child) = child.downcast_ref::<AXUIElement>() else {
                self.incomplete.read_failed = true;
                continue;
            };
            match self.node(child, depth + 1, hidden, read) {
                Ok(n) => node.children.push(n),
                Err(_) => self.incomplete.read_failed = true,
            }
        }
        Ok(node)
    }
}

/// The notification target handed to `AXObserverAddNotification` as refcon.
struct ObserverState {
    callback: AxChangeCallback,
}

unsafe extern "C-unwind" fn observer_callback(
    _observer: NonNull<AXObserver>,
    _element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    refcon: *mut c_void,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: refcon is the `ObserverState` owned by a live
    // `MacAxObservation`, which removes every notification (on this thread)
    // before freeing it.
    let state = unsafe { &*refcon.cast::<ObserverState>() };
    // SAFETY: the notification name is a valid CFString for the call.
    let name = unsafe { notification.as_ref() }.to_string();
    if let Some(change) = change_for(&name) {
        // Never unwind into the run loop.
        let _ = catch_unwind(AssertUnwindSafe(|| (state.callback)(change)));
    }
}

/// An attached `AXObserver`. Main-thread only; dropping it detaches the
/// observer and frees the callback.
struct MacAxObservation {
    observer: CFRetained<AXObserver>,
    app: CFRetained<AXUIElement>,
    names: Vec<CFRetained<CFString>>,
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
    state: *mut ObserverState,
}

impl AxObservation for MacAxObservation {}

impl Drop for MacAxObservation {
    fn drop(&mut self) {
        for name in &self.names {
            // SAFETY: removing a registered notification from a valid observer.
            unsafe { self.observer.remove_notification(&self.app, name) };
        }
        // SAFETY: reading an immutable CF constant.
        let mode = unsafe { kCFRunLoopCommonModes };
        self.run_loop.remove_source(Some(&self.source), mode);
        // SAFETY: created by `Box::into_raw` in `observe`; no callback can
        // run any more (notifications removed, source detached, same thread).
        drop(unsafe { Box::from_raw(self.state) });
    }
}

/// The macOS [`AppUiReader`].
#[derive(Default)]
pub struct MacAppUiReader {}

impl MacAppUiReader {
    fn walk(&self, scope: &AxScope, registry: &Registry, limits: AxLimits) -> Result<AxTree> {
        limits.validate()?;
        let pid = scope.pid();
        let timeout_s = limits.element_timeout.as_secs_f32();
        let app = application(pid, timeout_s)?;
        let names: Vec<CFRetained<CFString>> =
            ATTRIBUTES.iter().map(|a| CFString::from_str(a)).collect();
        let mut walk = Walk {
            names: CFArray::from_retained_objects(&names),
            limits,
            deadline: Instant::now() + limits.budget,
            nodes: 0,
            incomplete: AxIncomplete::default(),
            scope,
            registry,
        };
        let read = |element: &AXUIElement, names: &CFArray<CFString>| {
            set_timeout(element, timeout_s);
            copy_attributes(element, names)
        };
        let root = walk
            .node(&app, 0, None, &read)
            .map_err(|e| ax_error("accessibility snapshot", pid, e))?;
        Ok(AxTree {
            root,
            incomplete: walk.incomplete,
        })
    }
}

impl AppUiReader for MacAppUiReader {
    fn is_trusted(&self) -> bool {
        // SAFETY: no arguments; never prompts.
        unsafe { AXIsProcessTrusted() }
    }

    fn request_trust(&self) -> bool {
        // SAFETY: reading an immutable CF constant.
        let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
        let value: &CFBoolean = CFBoolean::new(true);
        let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[value]);
        // SAFETY: the documented option dictionary (CFString → CFBoolean).
        unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) }
    }

    fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
        // A one-off registry: its tokens mean nothing after this call.
        let scope = AxScope::new(pid, AxScopePolicy::default());
        self.walk(&scope, &Registry::default(), limits)
    }

    fn scope(&self, pid: u32, policy: AxScopePolicy) -> AxScope {
        AxScope::with_state(pid, policy, Arc::new(Registry::default()))
    }

    fn snapshot_in(&self, scope: &AxScope, limits: AxLimits) -> Result<AxTree> {
        match scope.state::<Registry>() {
            Some(state) => self.walk(scope, state, limits),
            None => self.walk(scope, &Registry::default(), limits),
        }
    }

    /// An element is destroyed when the element itself answers
    /// `kAXErrorInvalidUIElement`; a window on another Space, minimized or
    /// hidden still answers, and an unanswered request proves nothing.
    fn destroyed(&self, scope: &AxScope, elements: &[String]) -> Result<Vec<String>> {
        let Some(state) = scope.state::<Registry>() else {
            return Ok(Vec::new());
        };
        let known: Vec<(String, CFRetained<AXUIElement>)> = {
            let state = registry(state);
            elements
                .iter()
                .filter_map(|t| state.find(t).map(|element| (t.clone(), element)))
                .collect()
        };
        Ok(known
            .into_iter()
            .filter(|(_, element)| {
                set_timeout(element, MESSAGING_TIMEOUT_S);
                matches!(
                    copy_attribute(element, "AXRole"),
                    Err(AXError::InvalidUIElement)
                )
            })
            .map(|(token, _)| token)
            .collect())
    }

    fn observe(
        &self,
        pid: u32,
        changes: &[AxChange],
        callback: AxChangeCallback,
    ) -> Result<Box<dyn AxObservation>> {
        if MainThreadMarker::new().is_none() {
            return Err(Error::Internal(
                "accessibility observers run on the UI loop's main thread".into(),
            ));
        }
        let app = application(pid, MESSAGING_TIMEOUT_S)?;
        let raw_pid = libc::pid_t::try_from(pid)
            .map_err(|_| Error::InvalidInput(format!("invalid pid {pid}")))?;
        let mut raw: *mut AXObserver = ptr::null_mut();
        // SAFETY: valid callback and out-pointer; success yields a +1 observer.
        let err = unsafe {
            AXObserver::create(raw_pid, Some(observer_callback), NonNull::from(&mut raw))
        };
        if err != AXError::Success {
            return Err(ax_error("accessibility observer", pid, err));
        }
        let observer = NonNull::new(raw)
            // SAFETY: +1 reference from a Create function.
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| Error::Internal("AXObserverCreate returned no observer".into()))?;
        let run_loop = CFRunLoop::current()
            .ok_or_else(|| Error::Internal("no run loop on this thread".into()))?;
        // SAFETY: the observer is valid; the source lives as long as it does.
        let source = unsafe { observer.run_loop_source() };
        let state = Box::into_raw(Box::new(ObserverState { callback }));
        // From here on `Drop` cleans up whatever was registered.
        let mut observation = MacAxObservation {
            observer,
            app,
            names: Vec::new(),
            run_loop,
            source,
            state,
        };
        let mut last_error = None;
        for change in changes {
            let name = CFString::from_str(notification_name(*change));
            // SAFETY: refcon points to `state`, freed only after removal.
            let err = unsafe {
                observation
                    .observer
                    .add_notification(&observation.app, &name, state.cast())
            };
            match err {
                AXError::Success | AXError::NotificationAlreadyRegistered => {
                    observation.names.push(name);
                }
                // Apps implement subsets; a missing notification is fine.
                AXError::NotificationUnsupported => {}
                other => last_error = Some(other),
            }
        }
        if observation.names.is_empty() {
            let err = last_error.unwrap_or(AXError::NotificationUnsupported);
            return Err(ax_error("accessibility observer", pid, err));
        }
        // SAFETY: reading an immutable CF constant.
        let mode = unsafe { kCFRunLoopCommonModes };
        observation
            .run_loop
            .add_source(Some(&observation.source), mode);
        Ok(Box::new(observation))
    }

    fn manual_accessibility(&self, pid: u32) -> Result<Option<bool>> {
        let app = application(pid, MESSAGING_TIMEOUT_S)?;
        match copy_attribute(&app, MANUAL_ACCESSIBILITY) {
            Ok(v) => Ok(v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)),
            Err(AXError::AttributeUnsupported | AXError::NoValue) => Ok(None),
            Err(e) => Err(ax_error("AXManualAccessibility", pid, e)),
        }
    }

    fn set_manual_accessibility(&self, pid: u32, enabled: bool) -> Result<()> {
        let app = application(pid, MESSAGING_TIMEOUT_S)?;
        let name = CFString::from_str(MANUAL_ACCESSIBILITY);
        let value: &CFType = CFBoolean::new(enabled);
        // SAFETY: Chromium documents a CFBoolean value for this attribute.
        let err = unsafe { app.set_attribute_value(&name, value) };
        if err == AXError::Success {
            Ok(())
        } else {
            Err(ax_error("AXManualAccessibility", pid, err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_readers::rules::conformance::{self, Element, Kind, Mapper, Os};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn notification_names_round_trip() {
        for change in AxChange::ALL {
            assert_eq!(change_for(notification_name(change)), Some(change));
        }
        assert_eq!(change_for("AXSelectedTextChanged"), None);
    }

    #[test]
    fn invalid_pid_and_limits_are_rejected() {
        let reader = MacAppUiReader::default();
        assert!(reader.snapshot(0, AxLimits::default()).is_err());
        let no_time = AxLimits {
            budget: Duration::ZERO,
            ..AxLimits::default()
        };
        assert!(matches!(
            reader.snapshot(1, no_time),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn busy_apps_are_transient_and_vanished_ones_are_not() {
        assert!(broscribe_platform::is_ax_transient(&ax_error(
            "x",
            1,
            AXError::CannotComplete
        )));
        assert!(!broscribe_platform::is_ax_transient(&ax_error(
            "x",
            1,
            AXError::InvalidUIElement
        )));
    }

    fn error_value(mut error: AXError) -> CFRetained<CFType> {
        // SAFETY: AXValueCreate copies an AXError from a valid pointer of
        // precisely the type selected by AXValueType::AXError.
        let value = unsafe { AXValue::new(AXValueType::AXError, NonNull::from(&mut error).cast()) }
            .expect("AX error placeholder");
        let value: &CFType = &value;
        value.retain()
    }

    fn fixture_attributes(
        role: &str,
        children: &CFType,
        minimized: &CFType,
    ) -> Vec<CFRetained<CFType>> {
        let mut values = vec![error_value(AXError::AttributeUnsupported); ATTRIBUTES.len()];
        let role: &CFType = &CFString::from_str(role);
        values[0] = role.retain();
        values[8] = children.retain();
        values[10] = minimized.retain();
        values
    }

    fn string(text: &str) -> CFRetained<CFType> {
        let text: &CFType = &CFString::from_str(text);
        text.retain()
    }

    /// Native bulk-read values for app → window → web area → button,
    /// without messaging an app or needing Accessibility permission.
    struct Fixture {
        elements: [CFRetained<AXUIElement>; 4],
        values: [Vec<CFRetained<CFType>>; 4],
    }

    impl Fixture {
        fn new(window_children: Option<&CFType>, minimized: &CFType, leaf: &CFType) -> Self {
            // SAFETY: creating references for arbitrary positive pids does
            // not contact those processes; the fixture supplies every value.
            let elements =
                unsafe { [100, 101, 102, 103].map(|pid| AXUIElement::new_application(pid)) };
            let list = |i: usize| {
                let child: &CFType = &elements[i];
                CFArray::from_retained_objects(&[child.retain()])
            };
            let (windows, webs, buttons) = (list(1), list(2), list(3));
            let mut app = fixture_attributes("AXApplication", &windows, minimized);
            app[10] = error_value(AXError::AttributeUnsupported);
            let window =
                fixture_attributes("AXWindow", window_children.unwrap_or(&webs), minimized);
            let mut web = fixture_attributes("AXWebArea", &buttons, minimized);
            web[3] = string("Whole private document text");
            web[9] = string("https://meet.google.com/abc-defg-hij");
            let mut button = fixture_attributes("AXButton", leaf, minimized);
            button[2] = string("Leave call");
            Self {
                elements,
                values: [app, window, web, button],
            }
        }

        fn set(&mut self, element: usize, slot: usize, value: CFRetained<CFType>) {
            self.values[element][slot] = value;
        }

        fn walk(&self, scope: &AxScope, registry: &Registry, limits: AxLimits) -> AxTree {
            let mut walk = Walk {
                names: CFArray::from_retained_objects(&[]),
                limits,
                deadline: Instant::now() + limits.budget,
                nodes: 0,
                incomplete: AxIncomplete::default(),
                scope,
                registry,
            };
            let read = |element: &AXUIElement, _: &CFArray<CFString>| {
                let element: &CFType = element;
                self.elements
                    .iter()
                    .position(|known| {
                        let known: &CFType = known;
                        CFEqual(Some(element), Some(known))
                    })
                    .map(|i| CFArray::from_retained_objects(&self.values[i]))
                    .ok_or(AXError::InvalidUIElement)
            };
            let root = walk
                .node(&self.elements[0], 0, None, &read)
                .expect("native fixture decodes");
            AxTree {
                root,
                incomplete: walk.incomplete,
            }
        }

        fn tree(&self) -> AxTree {
            self.walk(
                &AxScope::new(1, AxScopePolicy::default()),
                &Registry::default(),
                AxLimits::default(),
            )
        }
    }

    fn empty() -> CFRetained<CFArray<CFType>> {
        CFArray::<CFType>::from_retained_objects(&[])
    }

    fn document(tree: &AxTree) -> &AxNode {
        &tree.root.children[0].children[0]
    }

    #[test]
    fn failed_children_make_snapshot_incomplete() {
        for error in [
            AXError::CannotComplete,
            AXError::Failure,
            AXError::InvalidUIElement,
            AXError::APIDisabled,
            AXError::NotImplemented,
        ] {
            let tree =
                Fixture::new(Some(&error_value(error)), CFBoolean::new(false), &empty()).tree();
            assert!(tree.incomplete.read_failed, "AXChildren error {}", error.0);
            assert!(tree.root.children[0].children.is_empty());
            let tree = Fixture::new(None, CFBoolean::new(false), &error_value(error)).tree();
            assert!(
                tree.incomplete.read_failed,
                "descendant AXChildren error {}",
                error.0
            );
            assert_eq!(tree.root.count(), 4);
        }
    }

    #[test]
    fn failed_attribute_slots_make_snapshot_incomplete() {
        for (slot, error, failed) in [
            (2, AXError::CannotComplete, true),
            (4, AXError::InvalidUIElement, true),
            (3, AXError::NoValue, false),
            (5, AXError::AttributeUnsupported, false),
        ] {
            let mut fixture = Fixture::new(None, CFBoolean::new(false), &empty());
            fixture.set(3, slot, error_value(error));
            assert_eq!(fixture.tree().incomplete.read_failed, failed, "{slot}");
        }
        // An element without a role cannot be matched.
        let mut fixture = Fixture::new(None, CFBoolean::new(false), &empty());
        fixture.set(3, 0, error_value(AXError::AttributeUnsupported));
        assert!(fixture.tree().incomplete.read_failed);
    }

    #[test]
    fn unknown_minimized_state_cannot_prove_visible_end() {
        // SAFETY: reading the immutable Core Foundation null singleton.
        let null: &CFType = unsafe { objc2_core_foundation::kCFNull }.unwrap();
        let wrong_type: &CFType = &CFString::from_str("not a minimized boolean");
        for value in [
            error_value(AXError::AttributeUnsupported),
            error_value(AXError::NoValue),
            null.retain(),
            wrong_type.retain(),
        ] {
            let tree = Fixture::new(None, &value, &empty()).tree();
            assert!(!tree.truncated());
            let doc = document(&tree);
            assert!(doc.document.as_ref().unwrap().hidden());
            assert!(tree.root.children[0].document.as_ref().unwrap().hidden());
            // Visibility uncertainty must not discard a readable active
            // control: it can still conservatively veto an automatic stop.
            assert_eq!(doc.children[0].title.as_deref(), Some("Leave call"));
        }
        // A minimized state that did not answer is a read failure as well.
        let tree = Fixture::new(None, &error_value(AXError::CannotComplete), &empty()).tree();
        assert!(tree.incomplete.read_failed);
        assert!(document(&tree).document.as_ref().unwrap().hidden());
    }

    #[test]
    fn known_minimized_state_and_child_arrays_remain_usable() {
        for minimized in [false, true] {
            let tree = Fixture::new(None, CFBoolean::new(minimized), &empty()).tree();
            assert!(!tree.truncated());
            assert_eq!(tree.root.count(), 4);
            assert_eq!(
                document(&tree).document.as_ref().unwrap().hidden(),
                minimized,
            );
        }
    }

    #[test]
    fn benign_leaf_absence_keeps_snapshot_complete() {
        // SAFETY: reading the immutable Core Foundation null singleton.
        let null: &CFType = unsafe { objc2_core_foundation::kCFNull }.unwrap();
        for value in [
            error_value(AXError::AttributeUnsupported),
            error_value(AXError::NoValue),
            null.retain(),
        ] {
            let tree = Fixture::new(None, CFBoolean::new(false), &value).tree();
            assert!(!tree.truncated());
            let doc = document(&tree);
            assert!(!doc.document.as_ref().unwrap().hidden());
            assert_eq!(doc.children[0].title.as_deref(), Some("Leave call"));
        }
    }

    /// The window's and the document's identity tokens.
    fn ids(tree: &AxTree) -> (Option<String>, Option<String>) {
        (
            tree.root.children[0]
                .document
                .as_ref()
                .and_then(|d| d.element().map(str::to_owned)),
            document(tree)
                .document
                .as_ref()
                .and_then(|d| d.element().map(str::to_owned)),
        )
    }

    /// Fills `elements` with distinct new elements up to [`MAX_ELEMENTS`].
    fn fill(elements: &mut Elements) {
        for pid in 200..200 + (MAX_ELEMENTS - elements.known.len()) as i32 {
            // SAFETY: as in the fixture, no process is contacted.
            let element = unsafe { AXUIElement::new_application(pid) };
            assert!(elements.id(&element).is_ok());
        }
        assert_eq!(elements.known.len(), MAX_ELEMENTS);
    }

    #[test]
    fn windows_and_documents_get_tokens_stable_within_one_registry_only() {
        let fixture = Fixture::new(None, CFBoolean::new(false), &empty());
        let scope = AxScope::new(1, AxScopePolicy::default());
        let incarnation = Registry::default();
        let first = ids(&fixture.walk(&scope, &incarnation, AxLimits::default()));
        let again = ids(&fixture.walk(&scope, &incarnation, AxLimits::default()));
        assert!(first.0.is_some() && first.1.is_some() && first.0 != first.1);
        assert_eq!(first, again);
        assert!(
            registry(&incarnation)
                .find(first.0.as_deref().unwrap())
                .is_some()
        );
        // Only top-level windows and documents carry identities.
        let tree = fixture.walk(&scope, &incarnation, AxLimits::default());
        assert!(tree.root.document.is_none());
        assert!(document(&tree).children[0].document.is_none());
    }

    #[test]
    fn a_full_registry_marks_the_snapshot_and_a_fresh_incarnation_clears_it() {
        let reader = MacAppUiReader::default();
        let fixture = Fixture::new(None, CFBoolean::new(false), &empty());
        let (window, web) = (&fixture.elements[1], &fixture.elements[2]);
        let limits = AxLimits::default();

        // A full registry that already knows both elements refuses nothing.
        let known = reader.scope(1, AxScopePolicy::default());
        let state = known.state::<Registry>().expect("incarnation registry");
        let tokens = {
            let mut elements = registry(state);
            let tokens = (elements.id(window).ok(), elements.id(web).ok());
            fill(&mut elements);
            tokens
        };
        let tree = fixture.walk(&known, state, limits);
        assert_eq!(tree.incomplete, AxIncomplete::default());
        assert_eq!(ids(&tree), tokens);

        // Full before the document was met: the registry refuses it by
        // capacity, never by recycling a token, and the snapshot says so.
        let incarnation = reader.scope(1, AxScopePolicy::default());
        let state = incarnation
            .state::<Registry>()
            .expect("incarnation registry");
        let window_token = {
            let mut elements = registry(state);
            let token = elements.id(window).expect("room for the window");
            fill(&mut elements);
            assert_eq!(elements.id(web), Err(Full));
            assert_eq!(elements.id(window), Ok(token.clone()));
            token
        };
        for _ in 0..2 {
            let capped = fixture.walk(&incarnation, state, limits);
            assert_eq!(
                capped.incomplete,
                AxIncomplete {
                    identity_cap: true,
                    ..AxIncomplete::default()
                }
            );
            assert!(capped.truncated(), "a refused identity is not a whole tree");
            assert_eq!(ids(&capped), (Some(window_token.clone()), None));
            // The contents are still read; only the identity is missing.
            assert_eq!(
                document(&capped).children[0].title.as_deref(),
                Some("Leave call")
            );
            assert_eq!(registry(state).known.len(), MAX_ELEMENTS);
        }

        // A fresh incarnation starts with an empty registry.
        let fresh = reader.scope(1, AxScopePolicy::default());
        let state = fresh.state::<Registry>().expect("incarnation registry");
        let tree = fixture.walk(&fresh, state, limits);
        assert_eq!(tree.incomplete, AxIncomplete::default());
        let (window_id, web_id) = ids(&tree);
        assert!(window_id.is_some() && web_id.is_some());
    }

    #[test]
    fn scoped_snapshots_skip_other_origins_and_document_text() {
        let fixture = Fixture::new(None, CFBoolean::new(false), &empty());
        let scope = |origins: &[&str], document_text| {
            AxScope::new(
                1,
                AxScopePolicy {
                    origins: origins.iter().map(|o| (*o).to_owned()).collect(),
                    document_text,
                },
            )
        };
        let walk = |scope: &AxScope| fixture.walk(scope, &Registry::default(), AxLimits::default());
        let permitted = walk(&scope(&["https://meet.google.com"], false));
        assert_eq!(document(&permitted).children.len(), 1);
        assert_eq!(document(&permitted).value, None, "no document text");
        let other = walk(&scope(&["https://other.example"], false));
        assert!(document(&other).children.is_empty());
        assert!(!other.truncated(), "a policy skip is not a cut");
        assert!(
            document(&other)
                .document
                .as_ref()
                .unwrap()
                .element()
                .is_some()
        );
        let with_text = walk(&scope(&[], true));
        assert_eq!(
            document(&with_text).value.as_deref(),
            Some("Whole private document text")
        );
    }

    #[test]
    fn caps_and_budget_are_reported_separately() {
        let fixture = Fixture::new(None, CFBoolean::new(false), &empty());
        let scope = AxScope::new(1, AxScopePolicy::default());
        let walk = |limits| fixture.walk(&scope, &Registry::default(), limits);
        let depth = walk(AxLimits {
            max_depth: 1,
            ..AxLimits::default()
        });
        assert!(depth.incomplete.depth_cap && !depth.incomplete.node_cap);
        let nodes = walk(AxLimits {
            max_nodes: 2,
            ..AxLimits::default()
        });
        assert!(nodes.incomplete.node_cap && !nodes.incomplete.depth_cap);
        let mut walker = Walk {
            names: CFArray::from_retained_objects(&[]),
            limits: AxLimits::default(),
            // Already spent.
            deadline: Instant::now(),
            nodes: 0,
            incomplete: AxIncomplete::default(),
            scope: &scope,
            registry: &Registry::default(),
        };
        let read = |_: &AXUIElement, _: &CFArray<CFString>| {
            Ok::<_, AXError>(CFArray::from_retained_objects(&fixture.values[0]))
        };
        let root = walker.node(&fixture.elements[0], 0, None, &read).unwrap();
        assert!(root.children.is_empty());
        assert!(walker.incomplete.time_budget);
    }

    /// Native AX values per element of one synthetic snapshot.
    type Values = Vec<(CFRetained<AXUIElement>, Vec<CFRetained<CFType>>)>;

    /// Synthetic bulk-read answers for conformance elements, through
    /// [`Walk::node`] with one registry for the mapper's life: native AX
    /// roles, an author label as `AXDescription` and a name from contents
    /// as `AXTitle`, static and document text as `AXValue`, DOM ids as
    /// `AXDOMIdentifier` (native ones `AXIdentifier`), classes as
    /// `AXDOMClassList`, a document's URL as `AXURL`, and one element
    /// reference per keyed element.
    #[derive(Default)]
    struct NativeAx {
        registry: Registry,
        keyed: RefCell<HashMap<String, i32>>,
        next: Cell<i32>,
    }

    impl NativeAx {
        /// A distinct element reference, the same for the same key.
        fn reference(&self, key: Option<&str>) -> CFRetained<AXUIElement> {
            let fresh = || {
                let pid = 10_000 + self.next.get();
                self.next.set(self.next.get() + 1);
                pid
            };
            let pid = match key {
                Some(key) => *self
                    .keyed
                    .borrow_mut()
                    .entry(key.to_owned())
                    .or_insert_with(fresh),
                None => fresh(),
            };
            // SAFETY: as in the fixture, no process is contacted.
            unsafe { AXUIElement::new_application(pid) }
        }

        fn element(&self, element: &Element, web: bool, values: &mut Values) -> CFRetained<CFType> {
            let web = web || element.kind == Kind::Document;
            let children: Vec<CFRetained<CFType>> = element
                .children
                .iter()
                .map(|child| self.element(child, web, values))
                .collect();
            let role = match element.kind {
                Kind::Window => "AXWindow",
                Kind::Document => "AXWebArea",
                Kind::Group => "AXGroup",
                Kind::Outline => "AXOutline",
                Kind::Row => "AXRow",
                Kind::Button => "AXButton",
                Kind::Text => "AXStaticText",
                Kind::Heading => "AXHeading",
            };
            let minimized = match element.minimized {
                Some(minimized) if element.kind == Kind::Window => {
                    let state: &CFType = CFBoolean::new(minimized);
                    state.retain()
                }
                _ => error_value(AXError::AttributeUnsupported),
            };
            let mut slots =
                fixture_attributes(role, &CFArray::from_retained_objects(&children), &minimized);
            if let Some(name) = &element.name {
                slots[if element.labelled { 4 } else { 2 }] = string(name);
            }
            if let Some(text) = &element.text {
                slots[3] = string(text);
            }
            if let Some(url) = &element.url {
                slots[9] = string(url);
            }
            if let Some(id) = &element.id {
                slots[if web { 6 } else { 5 }] = string(id);
            }
            if !element.classes.is_empty() {
                let classes: Vec<CFRetained<CFType>> =
                    element.classes.iter().map(|c| string(c)).collect();
                let list = CFArray::from_retained_objects(&classes);
                let list_type: &CFType = &list;
                slots[7] = list_type.retain();
            }
            if element.unreadable {
                slots[2] = error_value(AXError::CannotComplete);
            }
            let reference = self.reference(element.key.as_deref());
            let handle: &CFType = &reference;
            let handle = handle.retain();
            values.push((reference, slots));
            handle
        }
    }

    impl Mapper for NativeAx {
        fn os(&self) -> Os {
            Os::MacOs
        }

        fn snapshot(&self, windows: &[Element], policy: &AxScopePolicy) -> AxTree {
            let mut values = Values::new();
            let children: Vec<CFRetained<CFType>> = windows
                .iter()
                .map(|window| self.element(window, false, &mut values))
                .collect();
            let application = self.reference(None);
            let slots = fixture_attributes(
                "AXApplication",
                &CFArray::from_retained_objects(&children),
                &error_value(AXError::AttributeUnsupported),
            );
            let root_element: &AXUIElement = &application;
            values.push((root_element.retain(), slots));
            let scope = AxScope::new(1, policy.clone());
            let limits = AxLimits::default();
            let mut walk = Walk {
                names: CFArray::from_retained_objects(&[]),
                limits,
                deadline: Instant::now() + limits.budget,
                nodes: 0,
                incomplete: AxIncomplete::default(),
                scope: &scope,
                registry: &self.registry,
            };
            let read = |element: &AXUIElement, _: &CFArray<CFString>| {
                let element: &CFType = element;
                values
                    .iter()
                    .find(|(known, _)| {
                        let known: &CFType = known;
                        CFEqual(Some(element), Some(known))
                    })
                    .map(|(_, slots)| CFArray::from_retained_objects(slots))
                    .ok_or(AXError::InvalidUIElement)
            };
            let root = walk
                .node(&application, 0, None, &read)
                .expect("synthetic snapshot decodes");
            AxTree {
                root,
                incomplete: walk.incomplete,
            }
        }
    }

    #[test]
    fn every_shipped_rule_reads_through_the_ax_mapping() {
        conformance::check(&NativeAx::default());
    }
}
