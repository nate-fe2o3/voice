use crate::controller::Controller;
use crate::macos;
use crate::settings::Hotkey;
use libc::c_void;
use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::Duration;

type CFMachPortRef = *const c_void;
type CFRunLoopRef = *const c_void;
type CFRunLoopSourceRef = *const c_void;
type CFRunLoopMode = *const c_void;
type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;

const EVENT_KEY_DOWN: u32 = 10;
const EVENT_KEY_UP: u32 = 11;
const EVENT_FLAGS_CHANGED: u32 = 12;
const EVENT_MOUSE_MOVED: u32 = 5;
const EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const KEYBOARD_EVENT_KEYCODE: u32 = 9;

#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: unsafe extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
    fn CFRunLoopRun();
    fn CFRelease(value: *const c_void);
    static kCFRunLoopCommonModes: CFRunLoopMode;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Begin,
    Finish,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum NativeEvent {
    KeyDown(ShortcutKey),
    KeyUp(ShortcutKey),
    ModifierChanged(ShortcutKey),
    PointerMoved { x: f64, y: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ShortcutKey {
    RightOption,
    F8,
    Escape,
    Space,
    MetaLeft,
    MetaRight,
    ShiftLeft,
    ShiftRight,
}

#[derive(Default)]
struct ShortcutState {
    pressed: HashSet<ShortcutKey>,
    trigger: Option<ShortcutKey>,
}

impl ShortcutState {
    fn handle(&mut self, event: NativeEvent, configured: &Hotkey) -> Option<Intent> {
        match event {
            NativeEvent::KeyDown(key) => self.key_down(key, configured),
            NativeEvent::KeyUp(key) => self.key_up(key),
            NativeEvent::ModifierChanged(key) => {
                if self.pressed.contains(&key) {
                    self.key_up(key)
                } else {
                    self.key_down(key, configured)
                }
            }
            NativeEvent::PointerMoved { .. } => None,
        }
    }

    fn key_down(&mut self, key: ShortcutKey, configured: &Hotkey) -> Option<Intent> {
        if !self.pressed.insert(key) {
            return None;
        }
        if key == ShortcutKey::Escape {
            self.trigger = None;
            return Some(Intent::Cancel);
        }
        if is_configured_trigger(configured, key, &self.pressed) {
            self.trigger = Some(key);
            Some(Intent::Begin)
        } else if self.trigger == Some(ShortcutKey::RightOption) {
            self.trigger = None;
            Some(Intent::Cancel)
        } else {
            None
        }
    }

    fn key_up(&mut self, key: ShortcutKey) -> Option<Intent> {
        self.pressed.remove(&key);
        if self.trigger == Some(key) {
            self.trigger = None;
            Some(Intent::Finish)
        } else {
            None
        }
    }
}

pub fn start(controller: &Arc<Controller>) {
    let weak = Arc::downgrade(controller);
    std::thread::Builder::new()
        .name("voxtype-hotkey-supervisor".into())
        .spawn(move || {
            while let Some(controller) = weak.upgrade() {
                if let Err(error) = run_listener(weak.clone()) {
                    controller.set_hotkey_listener_status(false, Some(error));
                }
                drop(controller);
                if weak.upgrade().is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn global hotkey listener");
}

fn run_listener(controller: Weak<Controller>) -> Result<(), String> {
    if let Some(error) = listener_permission_error(macos::input_monitoring_trusted()) {
        return Err(error.into());
    }

    let (intent_sender, intent_receiver) = mpsc::channel();
    let intent_controller = controller.clone();
    std::thread::Builder::new()
        .name("voxtype-hotkey-actions".into())
        .spawn(move || {
            while let Ok(intent) = intent_receiver.recv() {
                let Some(controller) = intent_controller.upgrade() else {
                    break;
                };
                match intent {
                    Intent::Begin => controller.begin_recording(),
                    Intent::Finish => controller.finish_recording(),
                    Intent::Cancel => controller.cancel_recording(),
                }
            }
        })
        .map_err(|error| format!("start shortcut action worker: {error}"))?;

    let (event_sender, event_receiver) = mpsc::channel();
    let event_controller = controller.clone();
    std::thread::Builder::new()
        .name("voxtype-hotkey-events".into())
        .spawn(move || {
            let mut state = ShortcutState::default();
            while let Ok(event) = event_receiver.recv() {
                let Some(controller) = event_controller.upgrade() else {
                    break;
                };
                if let NativeEvent::PointerMoved { x, y } = event {
                    controller.try_update_pointer(x, y);
                    continue;
                }
                if let Some(intent) = state.handle(event, &controller.settings().hotkey) {
                    let _ = intent_sender.send(intent);
                }
            }
        })
        .map_err(|error| format!("start shortcut event worker: {error}"))?;

    native_listen(event_sender, || {
        if let Some(controller) = controller.upgrade() {
            controller.set_hotkey_listener_status(true, None);
        }
    })
}

fn native_listen(
    event_sender: mpsc::Sender<NativeEvent>,
    on_started: impl FnOnce(),
) -> Result<(), String> {
    let sender = Box::into_raw(Box::new(event_sender));
    let event_mask = [
        EVENT_KEY_DOWN,
        EVENT_KEY_UP,
        EVENT_FLAGS_CHANGED,
        EVENT_MOUSE_MOVED,
        EVENT_LEFT_MOUSE_DRAGGED,
        EVENT_RIGHT_MOUSE_DRAGGED,
    ]
    .into_iter()
    .fold(0, |mask, event| mask | (1_u64 << event));

    let tap = unsafe { CGEventTapCreate(0, 0, 1, event_mask, event_tap_callback, sender.cast()) };
    if tap.is_null() {
        unsafe {
            drop(Box::from_raw(sender));
        }
        return Err("Global shortcut listener could not create an event tap. Re-enable VoxType in System Settings > Privacy & Security > Input Monitoring.".into());
    }

    let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0) };
    if source.is_null() {
        unsafe {
            CFRelease(tap);
            drop(Box::from_raw(sender));
        }
        return Err("Global shortcut listener could not create its run loop.".into());
    }

    unsafe {
        let run_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        on_started();
        CFRunLoopRun();
        CFRelease(source);
        CFRelease(tap);
        drop(Box::from_raw(sender));
    }
    Ok(())
}

unsafe extern "C" fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let sender = unsafe { &*user_info.cast::<mpsc::Sender<NativeEvent>>() };
    let native_event = match event_type {
        EVENT_KEY_DOWN | EVENT_KEY_UP | EVENT_FLAGS_CHANGED => {
            let code = unsafe { CGEventGetIntegerValueField(event, KEYBOARD_EVENT_KEYCODE) };
            key_from_code(code).map(|key| match event_type {
                EVENT_KEY_DOWN => NativeEvent::KeyDown(key),
                EVENT_KEY_UP => NativeEvent::KeyUp(key),
                _ => NativeEvent::ModifierChanged(key),
            })
        }
        EVENT_MOUSE_MOVED | EVENT_LEFT_MOUSE_DRAGGED | EVENT_RIGHT_MOUSE_DRAGGED => {
            let point = unsafe { CGEventGetLocation(event) };
            Some(NativeEvent::PointerMoved {
                x: point.x,
                y: point.y,
            })
        }
        _ => None,
    };
    if let Some(native_event) = native_event {
        let _ = sender.send(native_event);
    }
    event
}

fn key_from_code(code: i64) -> Option<ShortcutKey> {
    match code {
        61 => Some(ShortcutKey::RightOption),
        100 => Some(ShortcutKey::F8),
        53 => Some(ShortcutKey::Escape),
        49 => Some(ShortcutKey::Space),
        55 => Some(ShortcutKey::MetaLeft),
        54 => Some(ShortcutKey::MetaRight),
        56 => Some(ShortcutKey::ShiftLeft),
        60 => Some(ShortcutKey::ShiftRight),
        _ => None,
    }
}

fn listener_permission_error(trusted: bool) -> Option<&'static str> {
    (!trusted).then_some(
        "Input Monitoring permission is required for global push-to-talk. Enable VoxType in System Settings > Privacy & Security > Input Monitoring.",
    )
}

fn is_configured_trigger(
    configured: &Hotkey,
    key: ShortcutKey,
    pressed: &HashSet<ShortcutKey>,
) -> bool {
    match configured {
        Hotkey::RightOption => key == ShortcutKey::RightOption,
        Hotkey::F8 => key == ShortcutKey::F8,
        Hotkey::CommandShiftSpace => {
            key == ShortcutKey::Space
                && pressed
                    .iter()
                    .any(|key| matches!(key, ShortcutKey::MetaLeft | ShortcutKey::MetaRight))
                && pressed
                    .iter()
                    .any(|key| matches!(key, ShortcutKey::ShiftLeft | ShortcutKey::ShiftRight))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_requires_input_monitoring() {
        assert!(listener_permission_error(false).is_some());
        assert!(listener_permission_error(true).is_none());
    }

    #[test]
    fn maps_only_shortcut_keycodes_without_text_conversion() {
        assert_eq!(key_from_code(100), Some(ShortcutKey::F8));
        assert_eq!(key_from_code(61), Some(ShortcutKey::RightOption));
        assert_eq!(key_from_code(0), None);
    }

    #[test]
    fn f8_press_and_release_emit_one_recording_cycle() {
        let mut state = ShortcutState::default();

        assert_eq!(
            state.handle(NativeEvent::KeyDown(ShortcutKey::F8), &Hotkey::F8),
            Some(Intent::Begin)
        );
        assert_eq!(
            state.handle(NativeEvent::KeyDown(ShortcutKey::F8), &Hotkey::F8),
            None
        );
        assert_eq!(
            state.handle(NativeEvent::KeyUp(ShortcutKey::F8), &Hotkey::F8),
            Some(Intent::Finish)
        );
    }

    #[test]
    fn modifier_events_drive_command_shift_space() {
        let mut state = ShortcutState::default();
        let configured = Hotkey::CommandShiftSpace;

        assert_eq!(
            state.handle(
                NativeEvent::ModifierChanged(ShortcutKey::MetaLeft),
                &configured
            ),
            None
        );
        assert_eq!(
            state.handle(
                NativeEvent::ModifierChanged(ShortcutKey::ShiftLeft),
                &configured
            ),
            None
        );
        assert_eq!(
            state.handle(NativeEvent::KeyDown(ShortcutKey::Space), &configured),
            Some(Intent::Begin)
        );
        assert_eq!(
            state.handle(NativeEvent::KeyUp(ShortcutKey::Space), &configured),
            Some(Intent::Finish)
        );
    }

    #[test]
    #[ignore = "requires a macOS GUI session with Input Monitoring permission"]
    fn native_listener_receives_synthetic_f8() {
        let (event_sender, event_receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            native_listen(event_sender, || ready_sender.send(()).unwrap()).unwrap();
        });
        ready_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("native listener did not start");

        rdev::simulate(&rdev::EventType::KeyPress(rdev::Key::F8)).unwrap();
        rdev::simulate(&rdev::EventType::KeyRelease(rdev::Key::F8)).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if event_receiver.recv_timeout(Duration::from_millis(100))
                == Ok(NativeEvent::KeyDown(ShortcutKey::F8))
            {
                return;
            }
        }
        panic!("native listener did not receive synthetic F8");
    }
}
