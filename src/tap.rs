//! Takes over ⌘Tab (and ⌘` while the hub is open) with a session-level event
//! tap, the way AltTab, Contexts and Hammerspoon do. Carbon hotkeys cannot
//! claim ⌘Tab because the Dock owns it; an active tap inserted at the head of
//! the session sees the key first and swallows it. Requires Accessibility.
//!
//! The tap lives on its own thread with its own run loop. An active tap holds
//! up keyboard delivery for the whole system until its callback returns, so
//! it must never wait on the hub's main-thread work (Accessibility queries,
//! layout); a stalled tap also gets switched off by the system.
//!
//! Installation retries until Accessibility is granted, so ⌘Tab starts
//! working the moment the grant is made — no relaunch needed.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::time::Duration;

use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

use crate::focus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Tab,
    Grave,
}

struct Handlers {
    /// ⌘+key pressed (`shift` for the reverse direction). Return true to
    /// swallow the key so the Dock / frontmost app never sees it.
    on_key: fn(Key, bool) -> bool,
    /// ⌘ released.
    on_cmd_up: fn(),
}

static HANDLERS: OnceLock<Handlers> = OnceLock::new();

thread_local! {
    // Lives on the tap thread, where the callback runs.
    static TAP: RefCell<Option<CFRetained<CFMachPort>>> = const { RefCell::new(None) };
}

const KEY_TAB: i64 = 48;
const KEY_GRAVE: i64 = 50;

unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    _user: *mut c_void,
) -> *mut CGEvent {
    let ev = unsafe { event.as_ref() };
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        // The system switches a slow tap off; switch it back on.
        TAP.with(|t| {
            if let Some(port) = &*t.borrow() {
                CGEvent::tap_enable(port, true);
            }
        });
        return event.as_ptr();
    }
    let Some(h) = HANDLERS.get() else { return event.as_ptr() };
    let flags = CGEvent::flags(Some(ev));
    if kind == CGEventType::KeyDown {
        if flags.contains(CGEventFlags::MaskCommand)
            && !flags.intersects(CGEventFlags::MaskControl | CGEventFlags::MaskAlternate)
        {
            let key = match CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) {
                KEY_TAB => Some(Key::Tab),
                KEY_GRAVE => Some(Key::Grave),
                _ => None,
            };
            if let Some(key) = key {
                if (h.on_key)(key, flags.contains(CGEventFlags::MaskShift)) {
                    return std::ptr::null_mut();
                }
            }
        }
    } else if kind == CGEventType::FlagsChanged && !flags.contains(CGEventFlags::MaskCommand) {
        (h.on_cmd_up)();
    }
    event.as_ptr()
}

fn create() -> Option<CFRetained<CFMachPort>> {
    let mask: CGEventMask = (1 << CGEventType::KeyDown.0) | (1 << CGEventType::FlagsChanged.0);
    unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(callback),
            std::ptr::null_mut(),
        )
    }
}

/// Starts the tap thread. Both handlers run on that thread and must return
/// immediately (hand real work to `DispatchQueue::main`); they must not touch
/// main-thread state.
pub fn install(on_key: fn(Key, bool) -> bool, on_cmd_up: fn()) {
    let _ = HANDLERS.set(Handlers { on_key, on_cmd_up });
    let debug = std::env::var_os("CHROMEHUB_DEBUG").is_some();
    let spawned = std::thread::Builder::new().name("cmd-tab-tap".into()).spawn(move || {
        let (mut waited, mut failed) = (false, false);
        let port = loop {
            // Only try once trusted: an untrusted attempt can trigger the
            // Input Monitoring prompt, which we neither need nor want.
            if focus::is_trusted() {
                if let Some(port) = create() {
                    break port;
                }
                // Trusted but refused: never hammer the system (each attempt
                // could raise a prompt). Retry slowly in case it was transient.
                if !failed {
                    failed = true;
                    eprintln!("chromehub: Accessibility is granted but the ⌘Tab tap was refused; retrying every minute");
                }
                std::thread::sleep(Duration::from_secs(60));
                continue;
            } else if !waited {
                waited = true;
                eprintln!("chromehub: ⌘Tab waits for the Accessibility grant (the system switcher stays until then)");
            }
            std::thread::sleep(Duration::from_secs(2));
        };
        let (Some(source), Some(run_loop)) = (CFMachPort::new_run_loop_source(None, Some(&port), 0), CFRunLoop::current()) else {
            eprintln!("chromehub: could not attach the ⌘Tab tap to a run loop");
            return;
        };
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        CGEvent::tap_enable(&port, true);
        TAP.with(|t| *t.borrow_mut() = Some(port));
        if debug {
            eprintln!("chromehub: ⌘Tab tap installed");
        }
        CFRunLoop::run(); // forever
    });
    if spawned.is_err() {
        eprintln!("chromehub: could not start the ⌘Tab tap thread; the system switcher stays");
    }
}
