use anyhow::{Context, Result};
use core_foundation::base::{CFType, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::base::kCFAllocatorDefault;
use core_foundation_sys::base::{Boolean, CFEqual, CFRelease, CFRetain, CFTypeRef};
use core_foundation_sys::dictionary::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionaryCreate,
    CFDictionaryRef,
};
use core_foundation_sys::number::kCFBooleanTrue;
use libc::{c_char, c_int, c_void, pid_t};
use rdev::{simulate, EventType, Key};
use serde::Serialize;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use tauri::WebviewWindow;

type AxUiElementRef = *const c_void;
type AxError = i32;

const AX_SUCCESS: AxError = 0;

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

pub struct FocusedTarget {
    element: AxUiElementRef,
    pub description: TargetDescription,
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
        Ok(Self {
            element,
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
    simulate(&EventType::KeyPress(Key::MetaLeft)).context("press Command")?;
    thread::sleep(Duration::from_millis(8));
    let press = simulate(&EventType::KeyPress(Key::KeyV)).context("press V");
    let release_v = simulate(&EventType::KeyRelease(Key::KeyV)).context("release V");
    let release_meta = simulate(&EventType::KeyRelease(Key::MetaLeft)).context("release Command");
    press.and(release_v).and(release_meta)
}

#[allow(unexpected_cfgs)]
pub fn show_without_activation(window: &WebviewWindow) -> Result<()> {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};

    let ns_window = window.ns_window().context("get native overlay window")?;
    unsafe {
        let _: () = msg_send![ns_window.cast::<Object>(), orderFrontRegardless];
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
}
