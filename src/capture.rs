//! Window thumbnails via ScreenCaptureKit (the only supported capture path on
//! macOS 15+). Captures run on ScreenCaptureKit's own queues; results are
//! handed back through `cb`, which may be invoked on any thread.

use std::ptr::NonNull;
use std::sync::Arc;

use block2::RcBlock;
use objc2::AnyThread;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCScreenshotManager, SCShareableContent,
    SCStreamConfiguration,
};

pub type Callback = Arc<dyn Fn(u32, CFRetained<CGImage>) + Send + Sync>;

/// Longest edge of a captured thumbnail, in pixels (2x for Retina cards).
pub const MAX_PX: f64 = 640.0;

extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

pub fn request_screen_capture() {
    unsafe {
        if !CGPreflightScreenCaptureAccess() {
            CGRequestScreenCaptureAccess();
        }
    }
}

/// Captures every window in `ids` (that ScreenCaptureKit can see) and calls
/// `cb(window_id, image)` once per successful capture.
pub fn capture_windows(ids: Vec<u32>, cb: Callback) {
    if ids.is_empty() {
        return;
    }
    let handler = RcBlock::new(move |content: *mut SCShareableContent, _err: *mut NSError| {
        if content.is_null() {
            return;
        }
        let content = unsafe { &*content };
        let windows = unsafe { content.windows() };
        for i in 0..windows.count() {
            let w = windows.objectAtIndex(i);
            let id = unsafe { w.windowID() };
            if !ids.contains(&id) {
                continue;
            }
            let frame = unsafe { w.frame() };
            let (fw, fh) = (frame.size.width, frame.size.height);
            if fw < 1.0 || fh < 1.0 {
                continue;
            }
            let scale = (MAX_PX / fw).min(MAX_PX / fh).min(2.0);
            let (pw, ph) = ((fw * scale).round() as usize, (fh * scale).round() as usize);

            let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w) };
            let cfg = unsafe { SCStreamConfiguration::new() };
            unsafe {
                cfg.setWidth(pw.max(1));
                cfg.setHeight(ph.max(1));
                cfg.setShowsCursor(false);
                cfg.setScalesToFit(true);
                cfg.setIgnoreShadowsSingleWindow(true);
                cfg.setCaptureResolution(SCCaptureResolutionType::Automatic);
            }
            let cb = cb.clone();
            let done = RcBlock::new(move |img: *mut CGImage, _err: *mut NSError| {
                if let Some(ptr) = NonNull::new(img) {
                    let image = unsafe { CFRetained::retain(ptr) };
                    cb(id, image);
                }
            });
            unsafe {
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(&filter, &cfg, Some(&done));
            }
        }
    });
    unsafe {
        // Include off-screen (minimized / other Space) windows too.
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, false, &handler,
        );
    }
}

/// True when the Screen Recording grant is in effect for this process.
pub fn has_screen_capture() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}
