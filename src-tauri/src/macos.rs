use anyhow::{Context, Result};
use core_foundation::base::{CFType, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::base::{Boolean, CFEqual, CFRelease, CFRetain, CFTypeRef};
use core_foundation_sys::base::kCFAllocatorDefault;
use core_foundation_sys::number::kCFBooleanTrue;
use core_foundation_sys::dictionary::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionaryCreate,
    CFDictionaryRef,
};
use libc::{c_char, c_int, c_void, pid_t};
use rdev::{simulate, EventType, Key};
use serde::Serialize;
use tauri::WebviewWindow;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

type AxUiElementRef = *const c_void;
type AxError = i32;

const AX_SUCCESS: AxError = 0;

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
        let element = focused_element()?;
        let mut pid = 0;
        let status = unsafe { AXUIElementGetPid(element, &mut pid) };
        if status != AX_SUCCESS {
            unsafe { CFRelease(element) };
            anyhow::bail!("could not identify the focused application");
        }
        let role = copy_string_attribute(element, "AXRole").unwrap_or_default();
        let subrole = copy_string_attribute(element, "AXSubrole").unwrap_or_default();
        if role.is_empty() {
            unsafe { CFRelease(element) };
            anyhow::bail!("no editable text field is focused");
        }
        if role == "AXSecureTextField" || subrole == "AXSecureTextField" {
            unsafe { CFRelease(element) };
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
            anyhow::bail!("Focus an editable text field before dictating");
        }
        let bundle_id = bundle_identifier(pid);
        if bundle_id.as_ref().is_some_and(|bundle| {
            excluded_apps
                .iter()
                .any(|excluded| excluded.eq_ignore_ascii_case(bundle))
        }) {
            unsafe { CFRelease(element) };
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
    let release_meta =
        simulate(&EventType::KeyRelease(Key::MetaLeft)).context("release Command");
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
    let system = unsafe { AXUIElementCreateSystemWide() };
    if system.is_null() {
        anyhow::bail!("could not access the macOS accessibility system");
    }
    let focused = copy_focused_element(system);
    unsafe { CFRelease(system) };
    if let Some(focused) = focused {
        return Ok(focused);
    }

    let pid = frontmost_application_pid()
        .context("identify the frontmost application for accessibility fallback")?;
    let application = unsafe { AXUIElementCreateApplication(pid) };
    if application.is_null() {
        anyhow::bail!("could not access the frontmost application");
    }

    // Firefox creates its macOS accessibility tree lazily when an assistive
    // client first queries the application role.
    let _ = copy_string_attribute(application, "AXRole");
    for _ in 0..20 {
        if let Some(focused) = copy_focused_element(application) {
            unsafe { CFRelease(application) };
            return Ok(focused);
        }
        thread::sleep(Duration::from_millis(25));
    }
    unsafe { CFRelease(application) };
    anyhow::bail!("no editable text field is focused");
}

fn copy_focused_element(element: AxUiElementRef) -> Option<AxUiElementRef> {
    let attribute = CFString::new("AXFocusedUIElement");
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
fn frontmost_application_pid() -> Result<pid_t> {
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
    let length =
        unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast::<c_void>(), buffer.len() as u32) };
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
}
