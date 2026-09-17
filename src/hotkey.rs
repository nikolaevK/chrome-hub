//! Global hotkeys via Carbon's RegisterEventHotKey — no Accessibility
//! permission needed and it works while any app is frontmost.

use std::ffi::c_void;
use std::sync::OnceLock;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type Handler = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: Handler,
        num_types: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: *mut c_void,
        options: u32,
        out_ref: *mut *mut c_void,
    ) -> i32;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: u32,
        actual_size: *mut u32,
        data: *mut c_void,
    ) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}
const K_EVENT_CLASS_KEYBOARD: u32 = fourcc(b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 6;
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = fourcc(b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = fourcc(b"hkid");
const SIGNATURE: u32 = fourcc(b"CHUB");

// Carbon modifier bit for ⌥ (cmdKey = 1<<8, shiftKey = 1<<9, controlKey = 1<<12).
pub const MOD_OPTION: u32 = 1 << 11;

// Virtual key codes (kVK_*).
pub const KEY_TAB: u32 = 48;
pub const KEY_GRAVE: u32 = 50;

static HANDLER: OnceLock<fn(u32)> = OnceLock::new();

extern "C" fn on_hotkey(_call: EventHandlerCallRef, event: EventRef, _user: *mut c_void) -> i32 {
    let mut id = EventHotKeyID { signature: 0, id: 0 };
    let err = unsafe {
        GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>() as u32,
            std::ptr::null_mut(),
            &mut id as *mut EventHotKeyID as *mut c_void,
        )
    };
    if err == 0 && id.signature == SIGNATURE {
        if let Some(f) = HANDLER.get() {
            f(id.id);
        }
    }
    0
}

/// Installs the Carbon event handler once. `f` runs on the main thread with
/// the hotkey id that was pressed.
pub fn install(f: fn(u32)) {
    let _ = HANDLER.set(f);
    let spec = EventTypeSpec { event_class: K_EVENT_CLASS_KEYBOARD, event_kind: K_EVENT_HOT_KEY_PRESSED };
    let mut handler_ref = std::ptr::null_mut();
    unsafe {
        InstallEventHandler(GetApplicationEventTarget(), on_hotkey, 1, &spec, std::ptr::null_mut(), &mut handler_ref);
    }
}

pub fn register(id: u32, key_code: u32, modifiers: u32) -> bool {
    let mut hk = std::ptr::null_mut();
    let hk_id = EventHotKeyID { signature: SIGNATURE, id };
    unsafe { RegisterEventHotKey(key_code, modifiers, hk_id, GetApplicationEventTarget(), 0, &mut hk) == 0 }
}
