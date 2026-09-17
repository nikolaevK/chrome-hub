//! ChromeHub — a native macOS app switcher with a window-level drill-down.
//!
//! Menu-bar app (no Dock icon). ⌘Tab replaces the system switcher with one
//! that shows every app with a live thumbnail; ⌘` (or Space) steps into the
//! selected app's windows. ⌥Tab / ⌥` jump straight to the Chrome windows,
//! where profiles are shown. Everything is plain AppKit through `objc2`.

mod capture;
mod chrome;
mod focus;
mod hotkey;
mod tap;
mod ui;
mod windows;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

fn main() {
    let mtm = MainThreadMarker::new().expect("ChromeHub must start on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Trigger the two permission prompts up front so the first hotkey press works.
    focus::request_accessibility();
    capture::request_screen_capture();

    ui::init(mtm);
    app.run();
}
