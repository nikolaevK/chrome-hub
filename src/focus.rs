//! Accessibility (AX) access to applications: listing real windows and
//! bringing a specific window or app to the front.

use std::ffi::c_void;

use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

type AXUIElementRef = *const c_void;
type CFTypeRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(el: AXUIElementRef, attr: CFTypeRef, out: *mut CFTypeRef) -> i32;
    fn AXUIElementSetAttributeValue(el: AXUIElementRef, attr: CFTypeRef, value: CFTypeRef) -> i32;
    fn AXUIElementPerformAction(el: AXUIElementRef, action: CFTypeRef) -> i32;
    fn AXUIElementSetMessagingTimeout(el: AXUIElementRef, timeout_seconds: f32) -> i32;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    /// Private but stable since 10.x; used by every window manager on macOS.
    /// If it ever disappears, `list_windows` falls back to titled windows and
    /// `focus_window` to matching by title.
    fn _AXUIElementGetWindow(el: AXUIElementRef, out: *mut u32) -> i32;
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetCount(arr: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(arr: CFTypeRef, idx: isize) -> CFTypeRef;
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
}

/// A +1 CoreFoundation reference released on drop.
struct Owned(CFTypeRef);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

fn cfstr(s: &str) -> objc2::rc::Retained<NSString> {
    NSString::from_str(s)
}
fn raw(s: &NSString) -> CFTypeRef {
    s as *const NSString as CFTypeRef
}

/// Shows the system Accessibility prompt once if we are not yet trusted.
pub fn request_accessibility() -> bool {
    unsafe {
        let key: &NSString = &*(kAXTrustedCheckOptionPrompt as *const NSString);
        let opts = NSDictionary::from_slices(&[key], &[&*NSNumber::new_bool(true)]);
        AXIsProcessTrustedWithOptions(&*opts as *const NSDictionary<NSString, NSNumber> as CFTypeRef)
    }
}

/// True when the Accessibility grant is in effect for this process.
pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) }
}

/// The AX element for `pid`, with a short reply timeout so a hung app cannot
/// stall the hub (the default is several seconds).
fn app_element(pid: i32) -> Option<Owned> {
    let app = Owned(unsafe { AXUIElementCreateApplication(pid) });
    if app.0.is_null() {
        return None;
    }
    unsafe { AXUIElementSetMessagingTimeout(app.0, 0.3) };
    Some(app)
}

fn copy_attr(el: AXUIElementRef, name: &str) -> Option<Owned> {
    let mut out: CFTypeRef = std::ptr::null();
    let attr = cfstr(name);
    let err = unsafe { AXUIElementCopyAttributeValue(el, raw(&attr), &mut out) };
    (err == 0 && !out.is_null()).then_some(Owned(out))
}

fn attr_string(el: AXUIElementRef, name: &str) -> Option<String> {
    let v = copy_attr(el, name)?;
    if unsafe { CFGetTypeID(v.0) != CFStringGetTypeID() } {
        return None;
    }
    let s: &NSString = unsafe { &*(v.0 as *const NSString) };
    Some(s.to_string())
}

fn window_id(el: AXUIElementRef) -> Option<u32> {
    let mut id = 0u32;
    (unsafe { _AXUIElementGetWindow(el, &mut id) } == 0).then_some(id)
}

pub struct AxWindow {
    pub id: u32,
    pub minimized: bool,
    /// AXStandardWindow: a real browser window (not a popup, dropdown or dialog).
    pub standard: bool,
    /// Full title, e.g. `Inbox - Google Chrome - Work (me@example.com)`.
    pub title: String,
}

/// All Accessibility windows of `pid`, or `None` when Accessibility is not
/// granted.
pub fn ax_windows(pid: i32) -> Option<Vec<AxWindow>> {
    if !is_trusted() {
        return None;
    }
    let app = app_element(pid)?;
    let windows = copy_attr(app.0, "AXWindows")?;
    let mut out = Vec::new();
    for i in 0..unsafe { CFArrayGetCount(windows.0) } {
        let w = unsafe { CFArrayGetValueAtIndex(windows.0, i) };
        let Some(id) = window_id(w) else { continue };
        out.push(AxWindow {
            id,
            minimized: copy_attr(w, "AXMinimized").map_or(false, |v| v.0 == unsafe { kCFBooleanTrue }),
            standard: attr_string(w, "AXSubrole").as_deref() == Some("AXStandardWindow"),
            title: attr_string(w, "AXTitle").unwrap_or_default(),
        });
    }
    Some(out)
}

/// Un-minimizes, raises and focuses the window, then activates its app. Falls
/// back to matching by title when the window-id lookup is unavailable.
pub fn focus_window(pid: i32, id: u32, title: &str) -> bool {
    let Some(app) = app_element(pid) else { return false };
    let Some(windows) = copy_attr(app.0, "AXWindows") else { return false };

    let mut target: AXUIElementRef = std::ptr::null();
    let mut by_title: AXUIElementRef = std::ptr::null();
    for i in 0..unsafe { CFArrayGetCount(windows.0) } {
        let w = unsafe { CFArrayGetValueAtIndex(windows.0, i) };
        if window_id(w) == Some(id) {
            target = w;
            break;
        }
        if by_title.is_null() && !title.is_empty() && attr_string(w, "AXTitle").map_or(false, |t| t.starts_with(title)) {
            by_title = w;
        }
    }
    if target.is_null() {
        target = by_title;
    }
    if target.is_null() {
        return false;
    }

    unsafe {
        AXUIElementSetAttributeValue(target, raw(&cfstr("AXMinimized")), kCFBooleanFalse);
        AXUIElementPerformAction(target, raw(&cfstr("AXRaise")));
        AXUIElementSetAttributeValue(target, raw(&cfstr("AXMain")), kCFBooleanTrue);
        AXUIElementSetAttributeValue(target, raw(&cfstr("AXFocused")), kCFBooleanTrue);
        AXUIElementSetAttributeValue(app.0, raw(&cfstr("AXFrontmost")), kCFBooleanTrue);
    }
    if let Some(running) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        running.activateWithOptions(NSApplicationActivationOptions::empty());
    }
    true
}

/// Activates an application the way ⌘Tab does: un-hides it, brings all its
/// windows forward, and restores a window if every one of them is minimized.
pub fn activate_app(pid: i32) {
    if let Some(running) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        running.unhide();
        running.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    }
    if let Some(windows) = ax_windows(pid) {
        let standard: Vec<&AxWindow> = windows.iter().filter(|w| w.standard).collect();
        if let Some(first) = standard.first() {
            if standard.iter().all(|w| w.minimized) {
                focus_window(pid, first.id, &first.title);
            }
        }
    }
}
