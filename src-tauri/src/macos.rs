use anyhow::{Context, Result};
use core_foundation::base::{CFType, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::base::kCFAllocatorDefault;
use core_foundation_sys::base::{Boolean, CFEqual, CFRange, CFRelease, CFRetain, CFTypeRef};
use core_foundation_sys::dictionary::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionaryCreate,
    CFDictionaryRef,
};
use core_foundation_sys::number::kCFBooleanTrue;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use libc::{c_char, c_int, c_void, pid_t};
use serde::Serialize;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use tauri::WebviewWindow;

type AxUiElementRef = *const c_void;
type AxError = i32;

const AX_SUCCESS: AxError = 0;

/// `kAXValueCFRangeType` from `<ApplicationServices/AXValue.h>`.
const AX_VALUE_CF_RANGE_TYPE: u32 = 4;

/// Most UTF-16 units read immediately before the caret when classifying the
/// insertion context.
const INSERTION_PREFIX_UTF16_UNITS: usize = 64;

const COMMAND_KEYCODE: u16 = 55;
const V_KEYCODE: u16 = 9;
const POST_DELAYS_MS: [u64; 3] = [12, 20, 20];

/// How long the application-level fallback keeps polling for a focused element.
const FOCUS_QUERY_TIMEOUT: Duration = Duration::from_millis(2500);
/// Poll interval for the application-level focused element fallback.
const FOCUS_QUERY_POLL: Duration = Duration::from_millis(50);
/// Re-send the accessibility opt-in in the fallback every this many polls.
const FOCUS_REWARM_INTERVAL: usize = 5;
/// Extra grace period granted to a capture that starts with a cold tree.
const CAPTURE_RETRY_TIMEOUT: Duration = Duration::from_millis(1500);

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> Boolean;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXUIElementCreateApplication(pid: pid_t) -> AxUiElementRef;
    fn AXUIElementCreateSystemWide() -> AxUiElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AxUiElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AxError;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: AxUiElementRef,
        parameterized_attribute: CFStringRef,
        parameter: CFTypeRef,
        result: *mut CFTypeRef,
    ) -> AxError;
    fn AXValueCreate(the_type: u32, value: *const c_void) -> CFTypeRef;
    fn AXValueGetValue(value: CFTypeRef, the_type: u32, value_ptr: *mut c_void) -> Boolean;
    fn AXUIElementGetPid(element: AxUiElementRef, pid: *mut pid_t) -> AxError;
    fn AXUIElementIsAttributeSettable(
        element: AxUiElementRef,
        attribute: CFStringRef,
        settable: *mut Boolean,
    ) -> AxError;
    fn AXUIElementSetAttributeValue(
        element: AxUiElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AxError;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

#[link(name = "proc", kind = "dylib")]
unsafe extern "C" {
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffer_size: u32) -> c_int;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetDescription {
    pub pid: i32,
    pub bundle_id: Option<String>,
    pub role: String,
}

/// How the caret sits relative to the surrounding text, used to decide whether
/// a capitalized leading word should be lowered for the current utterance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertionContext {
    /// The surrounding text could not be read.
    Unknown,
    /// Nothing precedes the caret (an empty field or the very start).
    FieldStart,
    /// The caret follows a sentence-ending character.
    SentenceBoundary,
    /// The caret is in the middle of a sentence.
    MidSentence,
}

pub struct FocusedTarget {
    element: AxUiElementRef,
    pub description: TargetDescription,
    pub context: InsertionContext,
}

unsafe impl Send for FocusedTarget {}
unsafe impl Sync for FocusedTarget {}

impl Clone for FocusedTarget {
    fn clone(&self) -> Self {
        unsafe {
            CFRetain(self.element);
        }
        Self {
            element: self.element,
            description: self.description.clone(),
            context: self.context,
        }
    }
}

impl Drop for FocusedTarget {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.element);
        }
    }
}

impl FocusedTarget {
    pub fn capture(excluded_apps: &[String]) -> Result<Self> {
        if !accessibility_trusted() {
            anyhow::bail!("Accessibility permission is required");
        }
        let element = match focused_element() {
            Ok(element) => element,
            Err(error) => {
                // A browser may only build its accessibility tree after the
                // first opt-in request; give the frontmost application a short
                // grace period before surfacing the failure. A capture that
                // succeeds immediately never pays this extra latency.
                match focused_element_with_timeout(CAPTURE_RETRY_TIMEOUT) {
                    Ok(element) => element,
                    Err(retry_error) => {
                        let pid = frontmost_application_pid().unwrap_or_default();
                        log::warn!(
                            "focus capture failed for pid={pid}, bundle_id={:?}, role=\"\", error=\"{error}\" (retry error: \"{retry_error}\")",
                            bundle_identifier(pid)
                        );
                        return Err(error);
                    }
                }
            }
        };
        let mut pid = 0;
        let status = unsafe { AXUIElementGetPid(element, &mut pid) };
        if status != AX_SUCCESS {
            unsafe { CFRelease(element) };
            log_capture_failure(0, "", None, "could not identify the focused application");
            anyhow::bail!("could not identify the focused application");
        }
        let role = copy_string_attribute(element, "AXRole").unwrap_or_default();
        let subrole = copy_string_attribute(element, "AXSubrole").unwrap_or_default();
        let bundle_id = bundle_identifier(pid);
        if role.is_empty() {
            unsafe { CFRelease(element) };
            log_capture_failure(
                pid,
                &role,
                bundle_id.as_deref(),
                "no editable text field is focused",
            );
            anyhow::bail!("no editable text field is focused");
        }
        if role == "AXSecureTextField" || subrole == "AXSecureTextField" {
            unsafe { CFRelease(element) };
            log_capture_failure(
                pid,
                &role,
                bundle_id.as_deref(),
                "Dictation is unavailable in secure text fields",
            );
            anyhow::bail!("Dictation is unavailable in secure text fields");
        }
        let mut settable: Boolean = 0;
        let value_attribute = CFString::new("AXValue");
        let status = unsafe {
            AXUIElementIsAttributeSettable(
                element,
                value_attribute.as_concrete_TypeRef(),
                &mut settable,
            )
        };
        let editable_role = matches!(
            role.as_str(),
            "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField"
        );
        if status != AX_SUCCESS || (settable == 0 && !editable_role) {
            unsafe { CFRelease(element) };
            log_capture_failure(
                pid,
                &role,
                bundle_id.as_deref(),
                "Focus an editable text field before dictating",
            );
            anyhow::bail!("Focus an editable text field before dictating");
        }
        if bundle_id.as_ref().is_some_and(|bundle| {
            excluded_apps
                .iter()
                .any(|excluded| excluded.eq_ignore_ascii_case(bundle))
        }) {
            unsafe { CFRelease(element) };
            log_capture_failure(
                pid,
                &role,
                bundle_id.as_deref(),
                "VoxType is disabled for this application",
            );
            anyhow::bail!("VoxType is disabled for this application");
        }
        let context = insertion_context(element);
        Ok(Self {
            element,
            context,
            description: TargetDescription {
                pid,
                bundle_id,
                role,
            },
        })
    }

    pub fn is_still_focused(&self) -> bool {
        let Ok(current) = focused_element() else {
            return false;
        };
        let same = unsafe { CFEqual(self.element, current) != 0 };
        unsafe { CFRelease(current) };
        same
    }

    pub fn restore_focus(&self) -> Result<()> {
        activate_application(self.description.pid)?;
        let attribute = CFString::new("AXFocused");
        let mut last_status = AX_SUCCESS;
        for _ in 0..10 {
            if self.is_still_focused() {
                return Ok(());
            }
            last_status = unsafe {
                AXUIElementSetAttributeValue(
                    self.element,
                    attribute.as_concrete_TypeRef(),
                    kCFBooleanTrue.cast(),
                )
            };
            thread::sleep(Duration::from_millis(20));
        }
        if last_status != AX_SUCCESS {
            anyhow::bail!("could not refocus the original text field (AX error {last_status})");
        }
        anyhow::bail!("the original text field did not regain focus");
    }
}

pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

pub fn request_accessibility() -> bool {
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt.cast::<c_void>()];
        let values = [kCFBooleanTrue.cast::<c_void>()];
        let options = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        if options.is_null() {
            return false;
        }
        let trusted = AXIsProcessTrustedWithOptions(options) != 0;
        CFRelease(options.cast());
        trusted
    }
}

pub fn input_monitoring_trusted() -> bool {
    unsafe { CGPreflightListenEventAccess() }
}

pub fn request_input_monitoring() -> bool {
    unsafe { CGRequestListenEventAccess() }
}

pub fn synthesize_paste() -> Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| anyhow::anyhow!("create a HID event source"))
        .context("synthesize Command-V")?;
    prime_event_pipeline(&source).context("prime the event pipeline")?;
    let events = build_command_v_events(&source);
    if events.len() != paste_key_plan().len() {
        anyhow::bail!("could not create the Command-V keyboard events");
    }
    for (index, event) in events.iter().enumerate() {
        event.post(CGEventTapLocation::HID);
        if let Some(delay) = POST_DELAYS_MS.get(index) {
            thread::sleep(Duration::from_millis(*delay));
        }
    }
    Ok(())
}

/// The Command-down/V-down/V-up/Command-up sequence posted for a paste.
fn paste_key_plan() -> [(u16, bool); 4] {
    [
        (COMMAND_KEYCODE, true),
        (V_KEYCODE, true),
        (V_KEYCODE, false),
        (COMMAND_KEYCODE, false),
    ]
}

/// Builds the keyboard events for a paste, forcing the Command flag onto each
/// one so the sequence can never arrive without Command held.
fn build_command_v_events(source: &CGEventSource) -> Vec<CGEvent> {
    paste_key_plan()
        .into_iter()
        .filter_map(|(keycode, keydown)| {
            let event = CGEvent::new_keyboard_event(source.clone(), keycode, keydown).ok()?;
            event.set_flags(CGEventFlags::CGEventFlagCommand);
            Some(event)
        })
        .collect()
}

/// Posts a harmless no-op once per process so a dropped first posted event can
/// never be the Command-down. The mouse-move event targets the current pointer
/// location, so the cursor does not visibly move.
fn prime_event_pipeline(source: &CGEventSource) -> Result<()> {
    static PRIMED: OnceLock<()> = OnceLock::new();
    if PRIMED.get().is_some() {
        return Ok(());
    }
    let location = CGEvent::new(source.clone())
        .map_err(|()| anyhow::anyhow!("read the pointer location"))?
        .location();
    let event = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::MouseMoved,
        location,
        CGMouseButton::Left,
    )
    .map_err(|()| anyhow::anyhow!("create the priming event"))?;
    event.post(CGEventTapLocation::HID);
    thread::sleep(Duration::from_millis(12));
    let _ = PRIMED.set(());
    Ok(())
}

/// `NSWindowCollectionBehaviorCanJoinAllSpaces` from `<AppKit/NSWindow.h>`
/// (value 1 << 0): the window is shown on every Space rather than only the
/// Space that was active when it was ordered front.
const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: usize = 1 << 0;

/// `NSWindowCollectionBehaviorStationary` from `<AppKit/NSWindow.h>`
/// (value 1 << 4): the window does not move during Exposé/Space switches.
const NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY: usize = 1 << 4;

/// `NSWindowCollectionBehaviorFullScreenAuxiliary` from `<AppKit/NSWindow.h>`
/// (value 1 << 8): the window may be shown alongside another app's native
/// fullscreen window on that app's dedicated fullscreen Space.
const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: usize = 1 << 8;

/// `NSStatusWindowLevel` from `<AppKit/NSWindow.h>`: above normal and floating
/// windows, so the overlay stays visible over fullscreen apps.
const NS_STATUS_WINDOW_LEVEL: isize = 25;

/// Collection behavior that lets the recording HUD join every Space, including
/// the dedicated Space another app creates when it enters native fullscreen.
fn overlay_collection_behavior() -> usize {
    NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
        | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY
        | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY
}

#[allow(unexpected_cfgs)]
pub fn show_without_activation(window: &WebviewWindow) -> Result<()> {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};

    let ns_window = window.ns_window().context("get native overlay window")?;
    let behavior = overlay_collection_behavior();
    let level = NS_STATUS_WINDOW_LEVEL;
    unsafe {
        let ns_window = ns_window.cast::<Object>();
        let _: () = msg_send![ns_window, setCollectionBehavior: behavior];
        let _: () = msg_send![ns_window, setLevel: level];
        let _: () = msg_send![ns_window, orderFrontRegardless];
    }
    Ok(())
}

#[allow(unexpected_cfgs)]
fn activate_application(pid: i32) -> Result<()> {
    use objc::runtime::{Object, BOOL, NO};
    use objc::{class, msg_send, sel, sel_impl};

    const ACTIVATE_IGNORING_OTHER_APPS: usize = 1 << 1;

    unsafe {
        let application: *mut Object =
            msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid];
        if application.is_null() {
            anyhow::bail!("the original application is no longer running");
        }
        let activated: BOOL =
            msg_send![application, activateWithOptions: ACTIVATE_IGNORING_OTHER_APPS];
        if activated == NO {
            anyhow::bail!("the original application could not be activated");
        }
    }
    Ok(())
}

fn focused_element() -> Result<AxUiElementRef> {
    focused_element_with_timeout(FOCUS_QUERY_TIMEOUT)
}

fn focused_element_with_timeout(timeout: Duration) -> Result<AxUiElementRef> {
    let system = unsafe { AXUIElementCreateSystemWide() };
    if system.is_null() {
        anyhow::bail!("could not access the macOS accessibility system");
    }
    let focused = copy_focused_element(system);
    unsafe { CFRelease(system) };
    if let Some(focused) = focused {
        return Ok(focused);
    }

    // Firefox (Gecko) and Chromium keep their macOS accessibility tree
    // disabled until an assistive client explicitly opts in, so enable the
    // frontmost application before querying its element tree.
    warm_up_frontmost_application();

    let pid = frontmost_application_pid()
        .context("identify the frontmost application for accessibility fallback")?;
    let application = unsafe { AXUIElementCreateApplication(pid) };
    if application.is_null() {
        anyhow::bail!("could not access the frontmost application");
    }

    let polls = warmup_poll_count(timeout, FOCUS_QUERY_POLL);
    let deadline = Instant::now() + timeout;
    for attempt in 0..polls {
        if let Some(focused) = focused_element_for_application(application) {
            unsafe { CFRelease(application) };
            return Ok(focused);
        }
        // The application may only build its tree after it has processed the
        // opt-in request, so keep re-warming while we wait.
        if attempt % FOCUS_REWARM_INTERVAL == 0 {
            warm_up_accessibility(pid);
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(FOCUS_QUERY_POLL);
    }
    unsafe { CFRelease(application) };
    anyhow::bail!("no editable text field is focused");
}

/// Query an application element for its focused element, falling back to the
/// application's focused window when the application level query is empty.
///
/// Firefox exposes the focused URL/search field through the focused window
/// rather than directly on the application element once accessibility has just
/// been enabled, so the window level query is tried as well.
fn focused_element_for_application(application: AxUiElementRef) -> Option<AxUiElementRef> {
    if let Some(focused) = copy_focused_element(application) {
        return Some(focused);
    }
    let window = copy_element_attribute(application, "AXFocusedWindow")?;
    let focused = copy_focused_element(window);
    unsafe { CFRelease(window) };
    focused
}

/// Enable the target application's macOS accessibility tree.
///
/// Firefox (Gecko) and Chromium-based applications keep their accessibility
/// tree disabled until an assistive client explicitly opts in. Without this
/// they return no `AXFocusedUIElement` (including for the URL/search bar), so
/// dictation never captures the focused field. `AXEnhancedUserInterface` is
/// the Firefox/WebKit opt-in and `AXManualAccessibility` the Chromium/Electron
/// one; an application that does not support a given attribute returns an
/// error that is expected and ignored.
fn warm_up_accessibility(pid: i32) {
    let application = unsafe { AXUIElementCreateApplication(pid) };
    if application.is_null() {
        return;
    }
    for attribute in ["AXEnhancedUserInterface", "AXManualAccessibility"] {
        let name = CFString::new(attribute);
        let status = unsafe {
            AXUIElementSetAttributeValue(
                application,
                name.as_concrete_TypeRef(),
                kCFBooleanTrue.cast(),
            )
        };
        log::debug!("accessibility warm-up pid={pid} attribute={attribute} status={status}");
    }
    unsafe { CFRelease(application) };
}

/// Enable accessibility for the frontmost application, ignoring any failure.
///
/// Exposed so the privacy watchdog can warm an application up as soon as it
/// becomes frontmost, before the user reaches for the dictation hotkey.
pub fn warm_up_frontmost_application() {
    if let Ok(pid) = frontmost_application_pid() {
        warm_up_accessibility(pid);
    }
}

/// Number of polls to attempt before `timeout` elapses. Extracted so the retry
/// timing can be unit-tested without sleeping.
fn warmup_poll_count(timeout: Duration, poll: Duration) -> usize {
    if poll.is_zero() {
        return 1;
    }
    let polls = timeout.as_nanos() / poll.as_nanos();
    usize::try_from(polls).unwrap_or(usize::MAX).max(1)
}

fn log_capture_failure(pid: i32, role: &str, bundle_id: Option<&str>, error: &str) {
    log::debug!(
        "focus capture failed: pid={pid}, role=\"{role}\", bundle_id={bundle_id:?}, error=\"{error}\""
    );
}

fn copy_focused_element(element: AxUiElementRef) -> Option<AxUiElementRef> {
    copy_element_attribute(element, "AXFocusedUIElement")
}

fn copy_element_attribute(element: AxUiElementRef, name: &str) -> Option<AxUiElementRef> {
    let attribute = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyAttributeValue(
            element,
            attribute.as_concrete_TypeRef(),
            &mut value as *mut CFTypeRef,
        )
    };
    if status != AX_SUCCESS || value.is_null() {
        return None;
    }
    Some(value.cast())
}

#[allow(unexpected_cfgs)]
pub fn frontmost_application_pid() -> Result<pid_t> {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            anyhow::bail!("could not access the macOS workspace");
        }
        let application: *mut Object = msg_send![workspace, frontmostApplication];
        if application.is_null() {
            anyhow::bail!("no application is frontmost");
        }
        let pid: pid_t = msg_send![application, processIdentifier];
        if pid <= 0 {
            anyhow::bail!("the frontmost application has no process identifier");
        }
        Ok(pid)
    }
}

fn copy_string_attribute(element: AxUiElementRef, name: &str) -> Option<String> {
    let attribute = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyAttributeValue(
            element,
            attribute.as_concrete_TypeRef(),
            &mut value as *mut CFTypeRef,
        )
    };
    if status != AX_SUCCESS || value.is_null() {
        return None;
    }
    let value = unsafe { CFType::wrap_under_create_rule(value) };
    value
        .downcast::<CFString>()
        .map(|string| string.to_string())
}

/// Reads the text immediately before the caret in `element` and classifies it
/// as the insertion context for capitalization.
///
/// Every accessibility failure is folded into `InsertionContext::Unknown`,
/// which leaves the transcript untouched.
fn insertion_context(element: AxUiElementRef) -> InsertionContext {
    match text_before_caret(element) {
        Some(prefix) => insertion_context_from_prefix(&prefix),
        None => InsertionContext::Unknown,
    }
}

fn text_before_caret(element: AxUiElementRef) -> Option<String> {
    let caret = caret_offset(element)?;
    if caret == 0 {
        return Some(String::new());
    }
    let start = caret.saturating_sub(INSERTION_PREFIX_UTF16_UNITS);
    let length = caret - start;
    if let Some(prefix) = string_for_range(element, start, length) {
        return Some(prefix);
    }
    string_prefix(element, caret)
}

/// Reads the caret offset, in UTF-16 units from the start of the field, from
/// `AXSelectedTextRange`, which is an `AXValue` wrapping a `CFRange`.
fn caret_offset(element: AxUiElementRef) -> Option<usize> {
    let attribute = CFString::new("AXSelectedTextRange");
    let mut value: CFTypeRef = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyAttributeValue(
            element,
            attribute.as_concrete_TypeRef(),
            &mut value as *mut CFTypeRef,
        )
    };
    if status != AX_SUCCESS || value.is_null() {
        return None;
    }
    let value = unsafe { CFType::wrap_under_create_rule(value) };
    let mut range = CFRange {
        location: 0,
        length: 0,
    };
    let ok = unsafe {
        AXValueGetValue(
            value.as_CFTypeRef(),
            AX_VALUE_CF_RANGE_TYPE,
            &mut range as *mut CFRange as *mut c_void,
        )
    };
    if ok == 0 {
        return None;
    }
    usize::try_from(range.location).ok()
}

/// Reads `length` UTF-16 units starting at `location` through the parameterized
/// `AXStringForRange` attribute. Applications that do not implement it return
/// an error, which the caller handles by falling back to the full value.
fn string_for_range(element: AxUiElementRef, location: usize, length: usize) -> Option<String> {
    let range = CFRange {
        location: isize::try_from(location).ok()?,
        length: isize::try_from(length).ok()?,
    };
    let range_value = unsafe {
        AXValueCreate(
            AX_VALUE_CF_RANGE_TYPE,
            &range as *const CFRange as *const c_void,
        )
    };
    if range_value.is_null() {
        return None;
    }
    let attribute = CFString::new("AXStringForRange");
    let mut result: CFTypeRef = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyParameterizedAttributeValue(
            element,
            attribute.as_concrete_TypeRef(),
            range_value,
            &mut result as *mut CFTypeRef,
        )
    };
    unsafe { CFRelease(range_value) };
    if status != AX_SUCCESS || result.is_null() {
        return None;
    }
    let result = unsafe { CFType::wrap_under_create_rule(result) };
    result
        .downcast::<CFString>()
        .map(|string| string.to_string())
}

/// Fallback used when `AXStringForRange` is unavailable: slice the full
/// `AXValue` string by the caret's UTF-16 offset. AX offsets are UTF-16 code
/// units, so the string is never sliced by character index.
fn string_prefix(element: AxUiElementRef, caret: usize) -> Option<String> {
    let value = copy_string_attribute(element, "AXValue")?;
    let utf16: Vec<u16> = value.encode_utf16().collect();
    if caret > utf16.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&utf16[..caret]))
}

/// Classifies the text immediately before the caret.
pub fn insertion_context_from_prefix(prefix: &str) -> InsertionContext {
    let mut candidate = trim_trailing_inline_whitespace(prefix);
    while let Some(last) = candidate.chars().last() {
        if is_closing_punctuation(last) {
            candidate = &candidate[..candidate.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    match candidate.chars().last() {
        None => InsertionContext::FieldStart,
        Some(last) if is_sentence_boundary(last) => InsertionContext::SentenceBoundary,
        Some(_) => InsertionContext::MidSentence,
    }
}

/// Trims trailing spaces and tabs, but deliberately keeps a trailing newline so
/// text typed at the start of a new line is still a sentence boundary.
fn trim_trailing_inline_whitespace(text: &str) -> &str {
    text.trim_end_matches(|character: char| {
        character.is_whitespace() && !matches!(character, '\n' | '\r')
    })
}

fn is_closing_punctuation(character: char) -> bool {
    matches!(
        character,
        ')' | ']' | '}' | '"' | '\'' | '\u{2019}' | '\u{201d}'
    )
}

fn is_sentence_boundary(character: char) -> bool {
    matches!(character, '.' | '!' | '?' | '\n' | '\r' | '\u{2026}')
}

/// Lowercases the leading word of a mid-sentence dictation only when it is safe
/// to do so. Every other insertion context returns `text` unchanged.
pub fn adjust_leading_capitalization(
    text: &str,
    context: InsertionContext,
    preferred_terms: &[String],
) -> String {
    if context != InsertionContext::MidSentence {
        return text.to_string();
    }
    let Some((letter_index, letter)) = leading_capital_letter(text) else {
        return text.to_string();
    };
    let word = word_containing(text, letter_index);
    if preserves_capitalization(word, preferred_terms) {
        return text.to_string();
    }
    let mut adjusted = String::with_capacity(text.len());
    adjusted.push_str(&text[..letter_index]);
    adjusted.push(letter.to_ascii_lowercase());
    adjusted.push_str(&text[letter_index + letter.len_utf8()..]);
    adjusted
}

/// Byte index and character of the first ASCII uppercase letter after any
/// leading whitespace and opening punctuation. Digits, sentence punctuation and
/// non-ASCII letters all stop the search without a candidate.
fn leading_capital_letter(text: &str) -> Option<(usize, char)> {
    for (index, character) in text.char_indices() {
        if character.is_whitespace() || is_opening_punctuation(character) {
            continue;
        }
        return character.is_ascii_uppercase().then_some((index, character));
    }
    None
}

fn is_opening_punctuation(character: char) -> bool {
    matches!(
        character,
        '(' | '[' | '{' | '"' | '\'' | '\u{2018}' | '\u{201c}'
    )
}

/// The word that starts at `start`, up to the first character that is neither
/// alphanumeric nor an apostrophe.
fn word_containing(text: &str, start: usize) -> &str {
    let rest = &text[start..];
    let end = rest
        .char_indices()
        .find(|(_, character)| !is_word_character(*character))
        .map(|(index, _)| index)
        .unwrap_or(rest.len());
    &rest[..end]
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '\'' | '\u{2019}')
}

fn preserves_capitalization(word: &str, preferred_terms: &[String]) -> bool {
    is_first_person_pronoun(word)
        || has_internal_uppercase(word)
        || is_entirely_uppercase(word)
        || preferred_terms
            .iter()
            .any(|term| term.eq_ignore_ascii_case(word))
}

/// The English first-person pronoun and its contractions keep their capital.
fn is_first_person_pronoun(word: &str) -> bool {
    if word == "I" {
        return true;
    }
    let Some(rest) = word.strip_prefix('I') else {
        return false;
    };
    let Some(contraction) = rest.strip_prefix(['\'', '\u{2019}']) else {
        return false;
    };
    matches!(contraction, "m" | "ve" | "ll" | "d")
}

/// Acronyms and camel-case words carry an uppercase letter after the first.
fn has_internal_uppercase(word: &str) -> bool {
    word.chars().skip(1).any(char::is_uppercase)
}

fn is_entirely_uppercase(word: &str) -> bool {
    word.chars().count() > 1 && !word.chars().any(char::is_lowercase)
}

fn bundle_identifier(pid: pid_t) -> Option<String> {
    let executable = process_path(pid)?;
    let app = app_bundle_path(&executable)?;
    let info = app.join("Contents").join("Info.plist");
    let value = plist::Value::from_file(info).ok()?;
    value
        .as_dictionary()?
        .get("CFBundleIdentifier")?
        .as_string()
        .map(str::to_string)
}

fn process_path(pid: pid_t) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; 4096];
    let length = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<c_void>(),
            buffer.len() as u32,
        )
    };
    if length <= 0 {
        return None;
    }
    let path = unsafe { CStr::from_ptr(buffer.as_ptr().cast::<c_char>()) };
    Some(PathBuf::from(path.to_string_lossy().as_ref()))
}

fn app_bundle_path(executable: &Path) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for component in executable.components() {
        path.push(component);
        if path.extension().is_some_and(|extension| extension == "app") {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_app_bundle_from_executable() {
        let path = Path::new("/Applications/TextEdit.app/Contents/MacOS/TextEdit");
        assert_eq!(
            app_bundle_path(path).unwrap(),
            Path::new("/Applications/TextEdit.app")
        );
    }

    #[test]
    fn paste_key_plan_wraps_v_with_command() {
        assert_eq!(
            paste_key_plan(),
            [(55, true), (9, true), (9, false), (55, false)]
        );
    }

    #[test]
    fn built_command_v_events_all_carry_command_flag() {
        let source = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
            Ok(source) => source,
            Err(()) => return,
        };
        let events = build_command_v_events(&source);
        assert_eq!(events.len(), 4);
        for event in &events {
            assert!(event.get_flags().contains(CGEventFlags::CGEventFlagCommand));
        }
    }

    #[test]
    fn overlay_collection_behavior_matches_named_options() {
        assert_eq!(overlay_collection_behavior(), 273);
        assert_eq!(NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES, 1 << 0);
        assert_eq!(NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY, 1 << 4);
        assert_eq!(NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY, 1 << 8);
        let behavior = overlay_collection_behavior();
        assert_eq!(
            behavior & NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES,
            NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
        );
        assert_eq!(
            behavior & NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY,
            NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY
        );
        assert_eq!(
            behavior & NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY,
            NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY
        );
    }

    #[test]
    fn poll_count_rounds_timeout_down_to_polls() {
        assert_eq!(
            warmup_poll_count(Duration::from_millis(2500), Duration::from_millis(50)),
            50
        );
        assert_eq!(
            warmup_poll_count(Duration::from_millis(1500), Duration::from_millis(50)),
            30
        );
    }

    #[test]
    fn poll_count_is_at_least_one() {
        assert_eq!(
            warmup_poll_count(Duration::from_millis(10), Duration::from_millis(50)),
            1
        );
        assert_eq!(
            warmup_poll_count(Duration::ZERO, Duration::from_millis(50)),
            1
        );
    }

    #[test]
    fn poll_count_handles_zero_poll_interval() {
        assert_eq!(
            warmup_poll_count(Duration::from_millis(2500), Duration::ZERO),
            1
        );
    }

    #[test]
    fn empty_prefix_is_the_start_of_the_field() {
        assert_eq!(
            insertion_context_from_prefix(""),
            InsertionContext::FieldStart
        );
        assert_eq!(
            insertion_context_from_prefix("   "),
            InsertionContext::FieldStart
        );
    }

    #[test]
    fn sentence_boundary_follows_terminators() {
        for prefix in ["Hello.", "Done!", "Hello! ", "Hello? ", "Hello\u{2026}"] {
            assert_eq!(
                insertion_context_from_prefix(prefix),
                InsertionContext::SentenceBoundary,
                "prefix {prefix:?}"
            );
        }
    }

    #[test]
    fn trailing_whitespace_before_the_caret_stays_mid_sentence() {
        assert_eq!(
            insertion_context_from_prefix("I went to the store and "),
            InsertionContext::MidSentence
        );
        assert_eq!(
            insertion_context_from_prefix("Hello"),
            InsertionContext::MidSentence
        );
    }

    #[test]
    fn newline_is_a_sentence_boundary() {
        assert_eq!(
            insertion_context_from_prefix("Hello\n"),
            InsertionContext::SentenceBoundary
        );
        assert_eq!(
            insertion_context_from_prefix("Hello\r"),
            InsertionContext::SentenceBoundary
        );
    }

    #[test]
    fn closing_punctuation_is_ignored_when_classifying() {
        assert_eq!(
            insertion_context_from_prefix("Well (that is fine)"),
            InsertionContext::MidSentence
        );
        assert_eq!(
            insertion_context_from_prefix("He said \"go.\""),
            InsertionContext::SentenceBoundary
        );
    }

    #[test]
    fn mid_sentence_capital_is_lowered() {
        assert_eq!(
            adjust_leading_capitalization("Hello", InsertionContext::MidSentence, &[]),
            "hello"
        );
        assert_eq!(
            adjust_leading_capitalization("Hello world ", InsertionContext::MidSentence, &[]),
            "hello world "
        );
    }

    #[test]
    fn other_contexts_are_left_untouched() {
        for context in [
            InsertionContext::FieldStart,
            InsertionContext::SentenceBoundary,
            InsertionContext::Unknown,
        ] {
            assert_eq!(
                adjust_leading_capitalization("Hello there ", context, &[]),
                "Hello there "
            );
        }
    }

    #[test]
    fn first_person_pronoun_keeps_its_capital() {
        assert_eq!(
            adjust_leading_capitalization("I think so ", InsertionContext::MidSentence, &[]),
            "I think so "
        );
        assert_eq!(
            adjust_leading_capitalization("I'm ready ", InsertionContext::MidSentence, &[]),
            "I'm ready "
        );
        assert_eq!(
            adjust_leading_capitalization("I\u{2019}ve tried ", InsertionContext::MidSentence, &[]),
            "I\u{2019}ve tried "
        );
    }

    #[test]
    fn acronyms_and_camel_case_keep_their_capital() {
        assert_eq!(
            adjust_leading_capitalization("VoxType is here ", InsertionContext::MidSentence, &[]),
            "VoxType is here "
        );
        assert_eq!(
            adjust_leading_capitalization("ASR works ", InsertionContext::MidSentence, &[]),
            "ASR works "
        );
    }

    #[test]
    fn preferred_terms_keep_their_capital() {
        let terms = vec!["Qwen".to_string()];
        assert_eq!(
            adjust_leading_capitalization("Qwen runs ", InsertionContext::MidSentence, &terms),
            "Qwen runs "
        );
    }

    #[test]
    fn leading_quote_is_skipped_before_lowering() {
        assert_eq!(
            adjust_leading_capitalization("\"Hello", InsertionContext::MidSentence, &[]),
            "\"hello"
        );
        assert_eq!(
            adjust_leading_capitalization("\u{201c}Hello", InsertionContext::MidSentence, &[]),
            "\u{201c}hello"
        );
    }

    #[test]
    fn non_letter_leading_characters_are_left_untouched() {
        assert_eq!(
            adjust_leading_capitalization("1. Hello ", InsertionContext::MidSentence, &[]),
            "1. Hello "
        );
        assert_eq!(
            adjust_leading_capitalization("\u{c9}lan ", InsertionContext::MidSentence, &[]),
            "\u{c9}lan "
        );
    }
}
