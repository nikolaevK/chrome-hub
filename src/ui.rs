//! The hub overlay: a non-activating floating panel with a vibrancy background,
//! a grid of cards with live thumbnails, keyboard navigation, search, plus the
//! menu-bar item and the global shortcuts that open it.
//!
//! The hub has two levels. The *Apps* level (⌘Tab) lists every running app
//! like the system switcher, each with a thumbnail of its front window. The
//! *Windows* level is the sub-dock for one app: all of its windows, with the
//! profile badge for Chrome. ⌘` / Space steps down into the selected app,
//! ⌫ / Esc steps back up. ⌥Tab and ⌥` open the Chrome windows directly.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ffi::{c_char, c_void, CString};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSBorderType, NSColor, NSEvent, NSEventModifierFlags,
    NSEventType, NSFont, NSFontWeight, NSFontWeightBold, NSFontWeightMedium, NSFontWeightRegular,
    NSFontWeightSemibold, NSImage, NSImageScaling, NSImageView, NSLineBreakMode, NSMenu,
    NSMenuItem, NSPanel, NSRunningApplication, NSScreen, NSScrollView, NSScrollerStyle,
    NSStatusBar, NSStatusItem, NSTextAlignment, NSTextField, NSTrackingArea,
    NSTrackingAreaOptions, NSVariableStatusItemLength, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSWorkspace, NSWorkspaceDidActivateApplicationNotification,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSOperationQueue, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;

use crate::windows::{self, AppEntry, WindowInfo};
use crate::{capture, chrome, focus, hotkey, tap};

// ---- Layout constants (points) --------------------------------------------

const CARD_W: f64 = 292.0;
const CARD_H: f64 = 240.0;
const THUMB_W: f64 = CARD_W - 24.0;
const THUMB_H: f64 = 168.0;
const GAP: f64 = 14.0;
const PAD: f64 = 22.0;
const HEADER_H: f64 = 82.0;
/// Breathing room between the header line and the first row of cards.
const GRID_GAP: f64 = 18.0;
const FOOTER_H: f64 = 38.0;
const MIN_W: f64 = 560.0;
const EMPTY_H: f64 = 120.0;
const CORNER: f64 = 26.0;

const HOTKEY_SWITCH: u32 = 1; // ⌥Tab — Chrome windows; hold to browse, release to switch
const HOTKEY_BROWSE: u32 = 2; // ⌥`   — Chrome windows; stays open

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Opened with ⌘Tab or ⌥Tab; releasing the held modifier commits the selection.
    Switch(NSEventModifierFlags),
    /// Opened from the menu bar or ⌥`; stays open until Enter/click/Esc.
    Browse,
}

#[derive(Clone, PartialEq, Eq)]
enum Level {
    /// One card per running app.
    Apps,
    /// The windows of the apps with these pids (one app, or every Chrome flavour).
    Windows { pids: Vec<i32>, name: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    Apps,
    Chrome,
}

// ---- Objective-C subclasses -----------------------------------------------

define_class!(
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "ChromeHubPanel"]
    struct HubPanel;

    unsafe impl NSObjectProtocol for HubPanel {}

    impl HubPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            handle_key(event);
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            handle_flags(event);
        }

        #[unsafe(method(resignKeyWindow))]
        fn resign_key_window(&self) {
            let _: () = unsafe { msg_send![super(self), resignKeyWindow] };
            // Clicked somewhere else: dismiss.
            if with_hub(|h| h.visible).unwrap_or(false) {
                hide_panel();
            }
        }
    }
);

struct CardIvars {
    index: Cell<usize>,
    /// The "show windows" affordance inside an app card, rather than the card itself.
    expand: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ChromeHubCard"]
    #[ivars = CardIvars]
    struct CardView;

    unsafe impl NSObjectProtocol for CardView {}

    impl CardView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            let i = self.ivars().index.get();
            with_hub(|h| h.select(i));
            if self.ivars().expand.get() {
                enter_selected();
            } else {
                commit_selection();
            }
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            // A card under a stationary pointer must not steal the selection
            // the instant the hub appears (that breaks the ⌘Tab / ⌥Tab tap).
            let p = NSEvent::mouseLocation();
            let i = self.ivars().index.get();
            with_hub(|h| {
                if (p.x - h.mouse_at_show.x).abs() > 2.0 || (p.y - h.mouse_at_show.y).abs() > 2.0 {
                    h.select(i);
                }
            });
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }
);

impl CardView {
    fn new(mtm: MainThreadMarker, frame: NSRect, index: usize, expand: bool) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(CardIvars { index: Cell::new(index), expand: Cell::new(expand) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ChromeHubGrid"]
    struct GridView;

    unsafe impl NSObjectProtocol for GridView {}

    impl GridView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ChromeHubController"]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    impl Controller {
        #[unsafe(method(statusClicked:))]
        fn status_clicked(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let app = NSApplication::sharedApplication(mtm);
            let secondary = app.currentEvent().map_or(false, |e| {
                matches!(e.r#type(), NSEventType::RightMouseUp | NSEventType::RightMouseDown)
                    || e.modifierFlags().contains(NSEventModifierFlags::Control)
            });
            if secondary {
                show_status_menu();
            } else {
                toggle_panel(Mode::Browse, Start::Apps);
            }
        }

        #[unsafe(method(openHub:))]
        fn open_hub(&self, _sender: Option<&AnyObject>) {
            toggle_panel(Mode::Browse, Start::Apps);
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }
);

// ---- State ------------------------------------------------------------------

struct Card {
    view: Retained<CardView>,
    thumb: Retained<NSImageView>,
    /// App icon shown in the thumbnail area until a capture arrives.
    placeholder: Option<Retained<NSImageView>>,
    window_id: Option<u32>,
}

/// What a card shows, independent of whether it stands for an app or a window.
struct CardModel {
    window_id: Option<u32>,
    title: String,
    subtitle: String,
    icon: Option<Retained<NSImage>>,
    dot: Option<Retained<NSColor>>,
    expandable: bool,
}

struct Hub {
    mtm: MainThreadMarker,
    panel: Retained<HubPanel>,
    root: Retained<NSVisualEffectView>,
    search_icon: Retained<NSImageView>,
    query_label: Retained<NSTextField>,
    count_label: Retained<NSTextField>,
    separator: Retained<NSView>,
    scroll: Retained<NSScrollView>,
    grid: Retained<GridView>,
    empty_label: Retained<NSTextField>,
    hint_label: Retained<NSTextField>,
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    _controller: Retained<Controller>,

    cards: Vec<Card>,
    apps: Vec<AppEntry>,
    /// Every window of every app, with the Accessibility cache applied.
    all_windows: Vec<WindowInfo>,
    /// The same, straight from the window server.
    raw_windows: Vec<WindowInfo>,
    /// Accessibility's last known view of each app's windows. Refreshed in
    /// the background on every show, so app cards count real windows.
    ax_cache: windows::AxCache,
    /// The windows shown at the Windows level, refined through Accessibility.
    scope: Vec<WindowInfo>,
    level: Level,
    /// Opened at the Apps level, so the Windows level can step back up.
    root_is_apps: bool,
    filtered: Vec<usize>,
    selected: usize,
    cols: usize,
    query: String,
    thumbs: HashMap<u32, Retained<NSImage>>,
    mode: Mode,
    mouse_at_show: NSPoint,
    visible: bool,
    generation: u64,
    screen_visible: NSRect,
}

thread_local! {
    static HUB: RefCell<Option<Hub>> = const { RefCell::new(None) };
    /// App activation history (pids), most recent first.
    static MRU: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

/// Mirror of `Hub::visible` for the tap thread, which must not touch `HUB`.
static VISIBLE: AtomicBool = AtomicBool::new(false);

fn with_hub<R>(f: impl FnOnce(&mut Hub) -> R) -> Option<R> {
    HUB.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

fn note_activation(pid: i32) {
    MRU.with(|m| {
        let mut m = m.borrow_mut();
        m.retain(|&p| p != pid);
        m.insert(0, pid);
        m.truncate(64);
    });
}

// ---- Public entry points ----------------------------------------------------

pub fn init(mtm: MainThreadMarker) {
    let hub = Hub::new(mtm);
    HUB.with(|cell| *cell.borrow_mut() = Some(hub));

    tap::install(on_tap_key, on_cmd_up);
    hotkey::install(on_hotkey);
    if !hotkey::register(HOTKEY_SWITCH, hotkey::KEY_TAB, hotkey::MOD_OPTION) {
        eprintln!("chromehub: could not register ⌥Tab");
    }
    if !hotkey::register(HOTKEY_BROWSE, hotkey::KEY_GRAVE, hotkey::MOD_OPTION) {
        eprintln!("chromehub: could not register ⌥`");
    }
    observe_activations();
    register_notify_trigger();
}

/// Keeps the most-recently-used order that ⌘Tab presents apps in.
fn observe_activations() {
    let ws = NSWorkspace::sharedWorkspace();
    if let Some(front) = ws.frontmostApplication() {
        note_activation(front.processIdentifier() as i32);
    }
    let block = RcBlock::new(|_n: NonNull<NSNotification>| {
        if let Some(app) = NSWorkspace::sharedWorkspace().frontmostApplication() {
            note_activation(app.processIdentifier() as i32);
        }
    });
    let observer = unsafe {
        ws.notificationCenter().addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidActivateApplicationNotification),
            None,
            Some(&NSOperationQueue::mainQueue()), // MRU is main-thread state
            &block,
        )
    };
    std::mem::forget(observer); // observes for the whole process
}

/// Scriptable trigger: `notifyutil -p com.konstantin.chromehub.toggle` opens or
/// closes the hub (handy for Raycast/BetterTouchTool/Automator bindings).
fn register_notify_trigger() {
    extern "C" {
        fn notify_register_dispatch(name: *const c_char, out_token: *mut i32, queue: *mut c_void, handler: *const c_void) -> u32;
    }
    let name = CString::new("com.konstantin.chromehub.toggle").unwrap();
    let block = RcBlock::new(|_token: i32| toggle_panel(Mode::Browse, Start::Apps));
    let mut token = 0i32;
    unsafe {
        notify_register_dispatch(name.as_ptr(), &mut token, DispatchQueue::main() as *const DispatchQueue as *mut c_void, &*block as *const _ as *const c_void);
    }
    std::mem::forget(block); // lives for the whole process
}

/// ⌥Tab / ⌥` (Carbon hotkeys): straight into the Chrome windows.
fn on_hotkey(id: u32) {
    let visible = with_hub(|h| h.visible).unwrap_or(false);
    match (id, visible) {
        (HOTKEY_SWITCH, true) => {
            let shift = NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Shift);
            with_hub(|h| h.step(if shift { -1 } else { 1 }));
        }
        (HOTKEY_SWITCH, false) => show_panel(Mode::Switch(NSEventModifierFlags::Option), Start::Chrome),
        (HOTKEY_BROWSE, true) => hide_panel(),
        (HOTKEY_BROWSE, false) => show_panel(Mode::Browse, Start::Chrome),
        _ => {}
    }
}

/// ⌘Tab / ⌘` from the event tap. Runs on the tap thread, so all real work
/// is handed to the main queue; the return value decides whether the key is
/// swallowed and must be known synchronously, hence the atomic.
fn on_tap_key(key: tap::Key, shift: bool) -> bool {
    let visible = VISIBLE.load(Ordering::Relaxed);
    match key {
        tap::Key::Tab => {
            DispatchQueue::main().exec_async(move || {
                if with_hub(|h| h.visible).unwrap_or(false) {
                    with_hub(|h| {
                        // ⌘Tab on an open browse hub turns it into a switcher: release ⌘ to switch.
                        h.mode = Mode::Switch(NSEventModifierFlags::Command);
                        h.step(if shift { -1 } else { 1 });
                    });
                } else {
                    show_panel(Mode::Switch(NSEventModifierFlags::Command), Start::Apps);
                }
            });
            true
        }
        tap::Key::Grave => {
            if !visible {
                return false; // ⌘` keeps its normal meaning while the hub is closed
            }
            DispatchQueue::main().exec_async(move || cycle_windows(shift));
            true
        }
    }
}

fn on_cmd_up() {
    DispatchQueue::main().exec_async(|| {
        if with_hub(|h| h.visible && h.mode == Mode::Switch(NSEventModifierFlags::Command)).unwrap_or(false) {
            commit_selection();
        }
    });
}

fn toggle_panel(mode: Mode, start: Start) {
    if with_hub(|h| h.visible).unwrap_or(false) {
        hide_panel();
    } else {
        show_panel(mode, start);
    }
}

fn show_panel(mode: Mode, start: Start) {
    let Some(panel) = with_hub(|h| h.prepare_show(mode, start)) else { return };
    panel.makeKeyAndOrderFront(None);

    // Tapped and released before we appeared: behave like a ⌘Tab tap.
    if let Mode::Switch(held) = mode {
        if !NSEvent::modifierFlags_class().contains(held) {
            commit_selection();
            return;
        }
    }
    start_capture();
    start_ax_refresh();
}

fn hide_panel() {
    if let Some(panel) = with_hub(|h| {
        h.set_visible(false);
        h.generation += 1;
        h.panel.clone()
    }) {
        panel.orderOut(None);
    }
}

/// ⌘` / Space: at the Apps level opens the selected app's windows, at the
/// Windows level moves to the next (or previous) window.
fn cycle_windows(backwards: bool) {
    let entered = with_hub(|h| match h.level {
        Level::Apps => h.enter_selected(),
        Level::Windows { .. } => {
            h.step(if backwards { -1 } else { 1 });
            false
        }
    })
    .unwrap_or(false);
    if entered {
        start_capture();
    }
}

fn enter_selected() {
    if with_hub(|h| h.enter_selected()).unwrap_or(false) {
        start_capture();
    }
}

fn go_back() -> bool {
    let went = with_hub(|h| h.back()).unwrap_or(false);
    if went {
        start_capture();
    }
    went
}

enum Action {
    App(i32),
    Window(WindowInfo),
    None,
}

fn commit_selection() {
    let target = with_hub(|h| {
        if !h.visible {
            return None;
        }
        let idx = h.filtered.get(h.selected).copied();
        h.set_visible(false);
        h.generation += 1;
        let action = match (&h.level, idx) {
            (_, None) => Action::None,
            (Level::Apps, Some(i)) => Action::App(h.apps[i].pid),
            (Level::Windows { .. }, Some(i)) => Action::Window(h.scope[i].clone()),
        };
        Some((h.panel.clone(), action))
    })
    .flatten();
    let Some((panel, action)) = target else { return };
    panel.orderOut(None);
    match action {
        Action::App(pid) => focus::activate_app(pid),
        Action::Window(w) => {
            if !focus::focus_window(w.pid, w.id, &w.title) {
                // Accessibility not granted (yet): at least bring the app forward.
                if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(w.pid) {
                    app.activateWithOptions(objc2_app_kit::NSApplicationActivationOptions::ActivateAllWindows);
                }
            }
        }
        Action::None => {}
    }
}

/// Refreshes the Accessibility view of every listed app off the main thread,
/// then corrects the app cards if their window lists changed. Keeps ⌘Tab
/// instant: the cards open with the last known counts and settle a moment
/// later, like the thumbnails.
fn start_ax_refresh() {
    let Some((pids, generation)) = with_hub(|h| (h.apps.iter().map(|a| a.pid).collect::<Vec<_>>(), h.generation)) else { return };
    let debug = std::env::var_os("CHROMEHUB_DEBUG").is_some();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let ax = windows::ax_lookup(&pids);
        if debug {
            eprintln!("ax refresh: {} of {} apps answered in {:?}", ax.len(), pids.len(), started.elapsed());
        }
        DispatchQueue::main().exec_async(move || {
            let changed = with_hub(|h| {
                h.ax_cache.retain(|pid, _| pids.contains(pid)); // forget quit apps
                h.ax_cache.extend(ax);
                let changed = h.refresh_from_ax();
                if changed && h.visible && h.generation == generation && h.level == Level::Apps {
                    h.rebuild();
                    return true;
                }
                false
            })
            .unwrap_or(false);
            if changed {
                start_capture(); // a card's representative window may have changed
            }
        });
    });
}

fn start_capture() {
    let Some((ids, generation)) = with_hub(|h| (h.capture_ids(), h.generation)) else { return };
    let cb: capture::Callback = Arc::new(move |id: u32, image: CFRetained<CGImage>| {
        DispatchQueue::main().exec_async(move || {
            let _mtm = MainThreadMarker::new().expect("main queue");
            let ns_image = NSImage::initWithCGImage_size(NSImage::alloc(), &image, NSSize::ZERO);
            with_hub(|h| {
                if h.generation == generation {
                    h.set_thumbnail(id, ns_image);
                }
            });
        });
    });
    capture::capture_windows(ids, cb);
}

fn show_status_menu() {
    if let Some((item, menu, mtm)) = with_hub(|h| (h.status_item.clone(), h.menu.clone(), h.mtm)) {
        item.setMenu(Some(&menu));
        if let Some(button) = item.button(mtm) {
            unsafe { button.performClick(None) };
        }
        item.setMenu(None);
    }
}

// ---- Keyboard ---------------------------------------------------------------

const KEY_TAB: u16 = 48;
const KEY_SPACE: u16 = 49;
const KEY_GRAVE: u16 = 50;
const KEY_DELETE: u16 = 51;
const KEY_ESC: u16 = 53;
const KEY_RETURN: u16 = 36;
const KEY_ENTER: u16 = 76;
const KEY_LEFT: u16 = 123;
const KEY_RIGHT: u16 = 124;
const KEY_DOWN: u16 = 125;
const KEY_UP: u16 = 126;

fn handle_key(event: &NSEvent) {
    let code = event.keyCode();
    let flags = event.modifierFlags();
    let shift = flags.contains(NSEventModifierFlags::Shift);
    match code {
        KEY_ESC => {
            let cleared = with_hub(|h| {
                if h.query.is_empty() {
                    false
                } else {
                    h.query.clear();
                    h.rebuild();
                    true
                }
            })
            .unwrap_or(false);
            if !cleared && !go_back() {
                hide_panel();
            }
        }
        KEY_RETURN | KEY_ENTER => commit_selection(),
        KEY_SPACE => {
            let (typing, at_apps) = with_hub(|h| (!h.query.is_empty(), h.level == Level::Apps)).unwrap_or((false, false));
            if typing {
                with_hub(|h| h.type_char(' '));
            } else if at_apps {
                enter_selected();
            } else {
                commit_selection();
            }
        }
        KEY_GRAVE => cycle_windows(shift),
        KEY_TAB => {
            with_hub(|h| h.step(if shift { -1 } else { 1 }));
        }
        KEY_LEFT => {
            with_hub(|h| h.step(-1));
        }
        KEY_RIGHT => {
            with_hub(|h| h.step(1));
        }
        KEY_UP => {
            with_hub(|h| h.step(-(h.cols as isize)));
        }
        KEY_DOWN => {
            with_hub(|h| h.step(h.cols as isize));
        }
        KEY_DELETE => {
            let deleted = with_hub(|h| {
                let popped = h.query.pop().is_some();
                if popped {
                    h.rebuild();
                }
                popped
            })
            .unwrap_or(false);
            if !deleted {
                go_back();
            }
        }
        _ => {
            if flags.intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Control) {
                return;
            }
            let holding = with_hub(|h| matches!(h.mode, Mode::Switch(m) if flags.contains(m))).unwrap_or(false);
            if holding {
                return; // holding the switcher modifier; don't type accented characters
            }
            let Some(chars) = event.characters().map(|s| s.to_string()) else { return };
            for c in chars.chars() {
                if c.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&c) {
                    continue; // arrows, F-keys and other special keys
                }
                let jumped = with_hub(|h| {
                    if h.query.is_empty() && c.is_ascii_digit() && c != '0' {
                        let i = c as usize - '1' as usize;
                        if i < h.filtered.len() {
                            h.select(i);
                            return true;
                        }
                    }
                    h.type_char(c);
                    false
                })
                .unwrap_or(false);
                if jumped {
                    commit_selection();
                    return;
                }
            }
        }
    }
}

fn handle_flags(event: &NSEvent) {
    let flags = event.modifierFlags();
    let should_commit = with_hub(|h| h.visible && matches!(h.mode, Mode::Switch(m) if !flags.contains(m))).unwrap_or(false);
    if should_commit {
        commit_selection();
    }
}

// ---- Hub implementation -----------------------------------------------------

fn weight(w: &'static NSFontWeight) -> NSFontWeight {
    *w
}

fn label(mtm: MainThreadMarker, text: &str, size: f64, w: NSFontWeight, color: &NSColor) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(&NSFont::systemFontOfSize_weight(size, w)));
    l.setTextColor(Some(color));
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l.setUsesSingleLineMode(true);
    l
}

fn layer(view: &NSView) -> Retained<CALayer> {
    view.setWantsLayer(true);
    view.layer().expect("layer-backed view")
}

fn symbol(name: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)
}

fn profile_color(name: &str) -> Retained<NSColor> {
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    match h.finish() % 8 {
        0 => NSColor::systemBlueColor(),
        1 => NSColor::systemGreenColor(),
        2 => NSColor::systemOrangeColor(),
        3 => NSColor::systemPinkColor(),
        4 => NSColor::systemPurpleColor(),
        5 => NSColor::systemTealColor(),
        6 => NSColor::systemIndigoColor(),
        _ => NSColor::systemYellowColor(),
    }
}

fn screen_under_mouse(mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    let p = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    for i in 0..screens.count() {
        let s = screens.objectAtIndex(i);
        let f = s.frame();
        if p.x >= f.origin.x && p.x < f.origin.x + f.size.width && p.y >= f.origin.y && p.y < f.origin.y + f.size.height {
            return Some(s);
        }
    }
    NSScreen::mainScreen(mtm)
}

fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier() as i32)
}

impl Hub {
    fn new(mtm: MainThreadMarker) -> Self {
        // Panel ------------------------------------------------------------
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(MIN_W, 300.0));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let this = mtm.alloc::<HubPanel>().set_ivars(());
        let panel: Retained<HubPanel> = unsafe {
            msg_send![super(this), initWithContentRect: rect, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        panel.setLevel(101); // kCGPopUpMenuWindowLevel: above full-screen apps too
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setHidesOnDeactivate(false);
        panel.setMovable(false);
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(false);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Transient
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );

        // Vibrancy root ------------------------------------------------------
        let root = NSVisualEffectView::initWithFrame(mtm.alloc(), rect);
        root.setMaterial(NSVisualEffectMaterial::Popover);
        root.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        root.setState(NSVisualEffectState::Active);
        {
            let l = layer(&root);
            l.setCornerRadius(CORNER);
            l.setMasksToBounds(true);
            l.setBorderWidth(1.0);
            l.setBorderColor(Some(&NSColor::colorWithWhite_alpha(1.0, 0.16).CGColor()));
        }
        panel.setContentView(Some(&root));

        // Header -------------------------------------------------------------
        let search_icon = NSImageView::initWithFrame(mtm.alloc(), NSRect::ZERO);
        search_icon.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        search_icon.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
        root.addSubview(&search_icon);

        let query_label = label(mtm, "", 22.0, weight(unsafe { &NSFontWeightRegular }), &NSColor::labelColor());
        root.addSubview(&query_label);

        let count_label = label(mtm, "", 13.0, weight(unsafe { &NSFontWeightMedium }), &NSColor::secondaryLabelColor());
        count_label.setAlignment(NSTextAlignment::Right);
        root.addSubview(&count_label);

        let separator = NSView::initWithFrame(mtm.alloc(), NSRect::ZERO);
        layer(&separator);
        root.addSubview(&separator);

        // Grid ---------------------------------------------------------------
        let scroll = NSScrollView::initWithFrame(mtm.alloc(), NSRect::ZERO);
        scroll.setDrawsBackground(false);
        scroll.setBorderType(NSBorderType::NoBorder);
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
        scroll.setAutohidesScrollers(true);
        scroll.setScrollerStyle(NSScrollerStyle::Overlay);
        let grid: Retained<GridView> = unsafe { msg_send![super(mtm.alloc::<GridView>().set_ivars(())), initWithFrame: NSRect::ZERO] };
        scroll.setDocumentView(Some(&grid));
        root.addSubview(&scroll);

        let empty_label = label(mtm, "", 15.0, weight(unsafe { &NSFontWeightMedium }), &NSColor::tertiaryLabelColor());
        empty_label.setAlignment(NSTextAlignment::Center);
        empty_label.setHidden(true);
        root.addSubview(&empty_label);

        // Footer -------------------------------------------------------------
        let hint_label = label(mtm, "", 11.0, weight(unsafe { &NSFontWeightRegular }), &NSColor::tertiaryLabelColor());
        hint_label.setAlignment(NSTextAlignment::Center);
        root.addSubview(&hint_label);

        // Menu bar -----------------------------------------------------------
        let controller: Retained<Controller> = unsafe { msg_send![super(mtm.alloc::<Controller>().set_ivars(())), init] };
        let status_item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = status_item.button(mtm) {
            if let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str("macwindow.on.rectangle"),
                Some(&NSString::from_str("Chrome Hub")),
            ) {
                img.setTemplate(true);
                button.setImage(Some(&img));
            }
            button.setToolTip(Some(&NSString::from_str("Chrome Hub — ⌘Tab apps, ⌥Tab Chrome windows, ⌥` browse")));
            unsafe {
                button.setTarget(Some(&controller));
                button.setAction(Some(sel!(statusClicked:)));
                button.sendActionOn(objc2_app_kit::NSEventMask::LeftMouseUp | objc2_app_kit::NSEventMask::RightMouseUp);
            }
        }
        let menu = NSMenu::new(mtm);
        let open = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str("Open Chrome Hub"), Some(sel!(openHub:)), &NSString::from_str(""))
        };
        unsafe { open.setTarget(Some(&controller)) };
        menu.addItem(&open);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str("Quit Chrome Hub"), Some(sel!(quit:)), &NSString::from_str("q"))
        };
        unsafe { quit.setTarget(Some(&controller)) };
        menu.addItem(&quit);

        Hub {
            mtm,
            panel,
            root,
            search_icon,
            query_label,
            count_label,
            separator,
            scroll,
            grid,
            empty_label,
            hint_label,
            status_item,
            menu,
            _controller: controller,
            cards: Vec::new(),
            apps: Vec::new(),
            all_windows: Vec::new(),
            raw_windows: Vec::new(),
            ax_cache: windows::AxCache::new(),
            scope: Vec::new(),
            level: Level::Apps,
            root_is_apps: true,
            filtered: Vec::new(),
            selected: 0,
            cols: 1,
            query: String::new(),
            thumbs: HashMap::new(),
            mode: Mode::Browse,
            mouse_at_show: NSPoint::ZERO,
            visible: false,
            generation: 0,
            screen_visible: NSRect::ZERO,
        }
    }

    /// Refreshes the app and window lists, lays everything out and returns
    /// the panel so the caller can order it front outside the state borrow.
    fn prepare_show(&mut self, mode: Mode, start: Start) -> Retained<HubPanel> {
        let mru = MRU.with(|m| m.borrow().clone());
        let snap = windows::snapshot(&mru, &self.ax_cache);
        self.apps = snap.apps;
        self.all_windows = snap.windows;
        self.raw_windows = snap.raw;
        if std::env::var_os("CHROMEHUB_DEBUG").is_some() {
            eprintln!(
                "screen-recording={} accessibility={} apps={} windows={}",
                capture::has_screen_capture(),
                focus::is_trusted(),
                self.apps.len(),
                self.all_windows.len(),
            );
        }
        self.mouse_at_show = NSEvent::mouseLocation();
        self.thumbs.retain(|id, _| self.all_windows.iter().any(|w| w.id == *id));
        self.mode = mode;
        self.set_visible(true);
        self.generation += 1;
        self.query.clear();
        if let Some(screen) = screen_under_mouse(self.mtm) {
            self.screen_visible = screen.visibleFrame();
        }

        match start {
            Start::Apps => {
                self.root_is_apps = true;
                self.level = Level::Apps;
                // Like ⌘Tab: when the current app heads the list, preselect the
                // previous one. (An app without windows, or a menu-bar app, in
                // front is not listed, so the head is then already the app to
                // go back to.)
                let front_first = self.apps.iter().find(|a| self.listed(a)).map_or(false, |a| Some(a.pid) == frontmost_pid());
                self.selected = if matches!(mode, Mode::Switch(_)) && front_first { 1 } else { 0 };
                self.rebuild();
            }
            Start::Chrome => {
                self.root_is_apps = false;
                let pids = self.apps.iter().filter(|a| chrome::is_chrome(&a.name)).map(|a| a.pid).collect();
                self.open_windows(pids, "Chrome".to_string());
            }
        }
        self.panel.clone()
    }

    /// Switches to the Windows level for `pids`, consulting Accessibility for
    /// the real window list, minimized state and full titles.
    fn open_windows(&mut self, pids: Vec<i32>, name: String) {
        // Ask Accessibility now for an exact list; keep the answer so the app
        // cards agree with it when stepping back up.
        let ax = windows::ax_lookup(&pids);
        let mut scope: Vec<WindowInfo> = self.raw_windows.iter().filter(|w| pids.contains(&w.pid)).cloned().collect();
        if ax.is_empty() {
            scope.retain(|w| !w.title.is_empty()); // no Accessibility: titled windows only
        } else {
            windows::apply_ax(&mut scope, &ax, &self.apps);
            self.ax_cache.extend(ax);
            self.refresh_from_ax();
        }
        // Like ⌘Tab: if this app is already in front, preselect its *previous* window.
        let in_front = frontmost_pid().map_or(false, |p| pids.contains(&p));
        self.selected = if matches!(self.mode, Mode::Switch(_)) && in_front && scope.len() > 1 { 1 } else { 0 };
        self.scope = scope;
        self.level = Level::Windows { pids, name };
        self.query.clear();
        self.rebuild();
    }

    /// Opens the selected app's windows. False when there is nothing to open.
    fn enter_selected(&mut self) -> bool {
        if self.level != Level::Apps {
            return false;
        }
        let Some(&i) = self.filtered.get(self.selected) else { return false };
        let app = &self.apps[i];
        if app.windows.is_empty() {
            return false;
        }
        let (pid, name) = (app.pid, app.name.clone());
        self.open_windows(vec![pid], name);
        true
    }

    /// Back up from the Windows level to the Apps level it was entered from.
    fn back(&mut self) -> bool {
        let Level::Windows { pids, .. } = &self.level else { return false };
        if !self.root_is_apps {
            return false;
        }
        let pid = pids.first().copied();
        self.level = Level::Apps;
        self.query.clear();
        self.selected = 0;
        self.rebuild();
        if let Some(pos) = pid.and_then(|p| self.filtered.iter().position(|&i| self.apps[i].pid == p)) {
            self.select(pos);
        }
        true
    }

    /// Apps with no window on screen anywhere (Finder idling, Calendar with
    /// its window closed or minimized) are running but not open: the hub
    /// leaves them out. Their minimized windows stay reachable through the
    /// Chrome shortcuts and the sub-dock of an app that is open.
    fn listed(&self, a: &AppEntry) -> bool {
        a.active > 0
    }

    /// Re-derives the app window lists from the window-server list and the
    /// current Accessibility cache. True when any app's windows changed.
    fn refresh_from_ax(&mut self) -> bool {
        let before: Vec<(i32, Vec<u32>)> = self.apps.iter().map(|a| (a.pid, a.windows.clone())).collect();
        let mut w = self.raw_windows.clone();
        windows::apply_ax(&mut w, &self.ax_cache, &self.apps);
        windows::assign_windows(&mut self.apps, &w);
        let mru = MRU.with(|m| m.borrow().clone());
        windows::order_apps(&mut self.apps, &w, &mru);
        if std::env::var_os("CHROMEHUB_DEBUG").is_some() {
            eprintln!("{}", windows::ax_summary(&self.raw_windows, &w, &self.apps, &self.ax_cache));
        }
        self.all_windows = w;
        self.apps.iter().map(|a| (a.pid, a.windows.clone())).ne(before.iter().cloned())
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        VISIBLE.store(visible, Ordering::Relaxed);
    }

    /// Items the current level shows before the search filter.
    fn item_count(&self) -> usize {
        match self.level {
            Level::Apps => self.apps.iter().filter(|a| self.listed(a)).count(),
            Level::Windows { .. } => self.scope.len(),
        }
    }

    fn matches(&self, i: usize) -> bool {
        if matches!(self.level, Level::Apps) && !self.listed(&self.apps[i]) {
            return false;
        }
        if self.query.is_empty() {
            return true;
        }
        let hay = match self.level {
            Level::Apps => self.apps[i].name.to_lowercase(),
            Level::Windows { .. } => {
                let w = &self.scope[i];
                format!("{} {}", w.title, w.profile.as_deref().unwrap_or("")).to_lowercase()
            }
        };
        self.query.to_lowercase().split_whitespace().all(|t| hay.contains(t))
    }

    fn card_model(&self, i: usize) -> CardModel {
        match self.level {
            Level::Apps => {
                let a = &self.apps[i];
                let n = a.windows.len();
                let mut subtitle = match n {
                    0 => "no windows".to_string(),
                    1 => "1 window".to_string(),
                    n => format!("{n} windows"),
                };
                let minimized = n - a.active;
                if minimized > 0 {
                    subtitle.push_str(&format!("  ·  {minimized} minimized"));
                }
                if !a.on_current {
                    subtitle.push_str("  ·  other desktop");
                }
                if a.hidden {
                    subtitle.push_str("  ·  hidden");
                }
                CardModel { window_id: a.front, title: a.name.clone(), subtitle, icon: a.icon.clone(), dot: None, expandable: n > 0 }
            }
            Level::Windows { .. } => {
                let w = &self.scope[i];
                let title = if w.title.is_empty() { w.app.clone() } else { w.title.clone() };
                let group = w.profile.clone().unwrap_or_else(|| w.app.clone());
                let mut subtitle = group.clone();
                if w.minimized {
                    subtitle.push_str("  ·  minimized");
                } else if !w.on_screen {
                    subtitle.push_str("  ·  other desktop");
                }
                CardModel { window_id: Some(w.id), title, subtitle, icon: None, dot: Some(profile_color(&group)), expandable: false }
            }
        }
    }

    /// Windows whose thumbnails the current level shows.
    fn capture_ids(&self) -> Vec<u32> {
        match self.level {
            Level::Apps => self.apps.iter().filter_map(|a| a.front).collect(),
            Level::Windows { .. } => self.scope.iter().map(|w| w.id).collect(),
        }
    }

    fn type_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let mtm = self.mtm;
        for c in self.cards.drain(..) {
            c.view.removeFromSuperview();
        }
        let items = match self.level {
            Level::Apps => self.apps.len(),
            Level::Windows { .. } => self.scope.len(),
        };
        self.filtered = (0..items).filter(|&i| self.matches(i)).collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }

        // Geometry ------------------------------------------------------------
        let n = self.filtered.len();
        let vis = self.screen_visible;
        let max_cols = (((vis.size.width * 0.92) - 2.0 * PAD + GAP) / (CARD_W + GAP)).floor().max(1.0) as usize;
        let want = ((n as f64 * 1.6).sqrt().ceil() as usize).max(1);
        let cols = want.clamp(1, max_cols).min(n.max(1)).max(2.min(n.max(1)));
        self.cols = cols;
        let rows = if n == 0 { 0 } else { (n + cols - 1) / cols };
        let grid_w = cols as f64 * CARD_W + (cols as f64 - 1.0) * GAP;
        let grid_h = if rows == 0 { EMPTY_H } else { rows as f64 * CARD_H + (rows as f64 - 1.0) * GAP };
        let max_grid_h = (vis.size.height * 0.86 - HEADER_H - GRID_GAP - FOOTER_H).max(CARD_H);
        let vis_grid_h = grid_h.min(max_grid_h);
        let panel_w = (grid_w + 2.0 * PAD).max(MIN_W);
        let panel_h = HEADER_H + GRID_GAP + vis_grid_h + FOOTER_H;
        let x = vis.origin.x + (vis.size.width - panel_w) / 2.0;
        let y = vis.origin.y + (vis.size.height - panel_h) / 2.0 + vis.size.height * 0.03;
        let frame = NSRect::new(NSPoint::new(x.round(), y.round()), NSSize::new(panel_w, panel_h));
        self.panel.setFrame_display(frame, true);
        self.root.setFrame(NSRect::new(NSPoint::ZERO, frame.size));

        // Header --------------------------------------------------------------
        // The search row sits slightly above the header's middle so that its
        // optical weight (a 22pt line) balances against the line below it.
        let header_mid = panel_h - HEADER_H / 2.0 + 2.0;
        self.search_icon.setFrame(NSRect::new(NSPoint::new(PAD + 2.0, header_mid - 11.0), NSSize::new(22.0, 22.0)));
        let count_w = 140.0;
        self.query_label.setFrame(NSRect::new(
            NSPoint::new(PAD + 36.0, header_mid - 14.0),
            NSSize::new(panel_w - 2.0 * PAD - 36.0 - count_w - 8.0, 28.0),
        ));
        self.count_label.setFrame(NSRect::new(NSPoint::new(panel_w - PAD - count_w, header_mid - 8.0), NSSize::new(count_w, 17.0)));
        self.separator.setFrame(NSRect::new(NSPoint::new(PAD, panel_h - HEADER_H), NSSize::new(panel_w - 2.0 * PAD, 1.0)));
        layer(&self.separator).setBackgroundColor(Some(&NSColor::labelColor().colorWithAlphaComponent(0.10).CGColor()));

        let (noun, placeholder, empty_msg, header_icon, hint) = match &self.level {
            Level::Apps => (
                "app",
                "Search apps".to_string(),
                "No apps are running".to_string(),
                symbol("magnifyingglass"),
                "↑ ↓ ← →  navigate     ⏎  switch     `  windows     1–9  jump     type to filter     esc  close",
            ),
            Level::Windows { pids, name } => (
                "window",
                format!("Search {name} windows"),
                format!("No {name} windows are open"),
                pids.first().and_then(|p| self.apps.iter().find(|a| a.pid == *p)).and_then(|a| a.icon.clone()).or_else(|| symbol("magnifyingglass")),
                if self.root_is_apps {
                    "↑ ↓ ← →  navigate     ⏎  open     `  next     1–9  jump     type to filter     ⌫  back"
                } else {
                    "↑ ↓ ← →  navigate     ⏎  open     1–9  jump     type to filter     esc  close"
                },
            ),
        };
        self.search_icon.setImage(header_icon.as_deref());
        if self.query.is_empty() {
            self.query_label.setStringValue(&NSString::from_str(&placeholder));
            self.query_label.setTextColor(Some(&NSColor::tertiaryLabelColor()));
        } else {
            self.query_label.setStringValue(&NSString::from_str(&self.query));
            self.query_label.setTextColor(Some(&NSColor::labelColor()));
        }
        let total = self.item_count();
        let count_text = if n == total {
            format!("{total} {noun}{}", if total == 1 { "" } else { "s" })
        } else {
            format!("{n} of {total}")
        };
        self.count_label.setStringValue(&NSString::from_str(&count_text));
        self.hint_label.setStringValue(&NSString::from_str(hint));

        // Grid ----------------------------------------------------------------
        let grid_x = (panel_w - grid_w) / 2.0;
        self.scroll.setFrame(NSRect::new(NSPoint::new(grid_x, FOOTER_H), NSSize::new(grid_w, vis_grid_h)));
        self.grid.setFrame(NSRect::new(NSPoint::ZERO, NSSize::new(grid_w, grid_h)));
        self.empty_label.setHidden(n != 0);
        self.empty_label.setFrame(NSRect::new(NSPoint::new(0.0, FOOTER_H + vis_grid_h / 2.0 - 10.0), NSSize::new(panel_w, 20.0)));
        if n == 0 {
            let msg = if total == 0 { empty_msg } else { format!("No {noun}s match") };
            self.empty_label.setStringValue(&NSString::from_str(&msg));
        }
        self.hint_label.setFrame(NSRect::new(NSPoint::new(0.0, 12.0), NSSize::new(panel_w, 16.0)));

        for (pos, &i) in self.filtered.iter().enumerate() {
            let col = pos % cols;
            let row = pos / cols;
            let frame = NSRect::new(
                NSPoint::new(col as f64 * (CARD_W + GAP), row as f64 * (CARD_H + GAP)),
                NSSize::new(CARD_W, CARD_H),
            );
            let model = self.card_model(i);
            let thumb = model.window_id.and_then(|id| self.thumbs.get(&id));
            let card = make_card(mtm, pos, &model, frame, thumb);
            self.grid.addSubview(&card.view);
            self.cards.push(card);
        }
        self.apply_selection();
        self.scroll.contentView().scrollToPoint(NSPoint::ZERO);
        self.reveal_selected();
    }

    fn select(&mut self, i: usize) {
        if i < self.filtered.len() && i != self.selected {
            self.selected = i;
            self.apply_selection();
            self.reveal_selected();
        }
    }

    fn step(&mut self, delta: isize) {
        let n = self.filtered.len() as isize;
        if n == 0 {
            return;
        }
        let next = (self.selected as isize + delta).rem_euclid(n) as usize;
        self.select(next);
    }

    fn apply_selection(&self) {
        let accent = NSColor::controlAccentColor().CGColor();
        let idle_bg = NSColor::labelColor().colorWithAlphaComponent(0.05).CGColor();
        let active_bg = NSColor::labelColor().colorWithAlphaComponent(0.13).CGColor();
        for (i, card) in self.cards.iter().enumerate() {
            let l = layer(&card.view);
            if i == self.selected {
                l.setBackgroundColor(Some(&active_bg));
                l.setBorderWidth(2.0);
                l.setBorderColor(Some(&accent));
            } else {
                l.setBackgroundColor(Some(&idle_bg));
                l.setBorderWidth(0.0);
            }
        }
    }

    fn reveal_selected(&self) {
        if let Some(card) = self.cards.get(self.selected) {
            let mut f = card.view.frame();
            f.origin.y -= GAP;
            f.size.height += 2.0 * GAP;
            self.grid.scrollRectToVisible(f);
        }
    }

    fn set_thumbnail(&mut self, id: u32, image: Retained<NSImage>) {
        for card in &self.cards {
            if card.window_id == Some(id) {
                card.thumb.setImage(Some(&image));
                if let Some(p) = &card.placeholder {
                    p.setHidden(true);
                }
            }
        }
        self.thumbs.insert(id, image);
    }
}

/// A rounded dark pill with `text` centred in it. The label is sized to its
/// text and placed by hand: a label stretched to the pill's frame would draw
/// its text top-aligned.
fn badge(mtm: MainThreadMarker, text: &str) -> Retained<NSView> {
    let l = label(mtm, text, 11.0, weight(unsafe { &NSFontWeightBold }), &NSColor::whiteColor());
    l.setAlignment(NSTextAlignment::Center);
    l.sizeToFit();
    let text_size = l.frame().size;
    let (w, h) = ((text_size.width + 10.0).max(22.0).round(), 20.0);
    let pill = NSView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::ZERO, NSSize::new(w, h)));
    {
        let layer = layer(&pill);
        layer.setCornerRadius(6.0);
        layer.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.55).CGColor()));
    }
    l.setFrameOrigin(NSPoint::new(((w - text_size.width) / 2.0).round(), ((h - text_size.height) / 2.0).round()));
    pill.addSubview(&l);
    pill
}

fn make_card(mtm: MainThreadMarker, pos: usize, m: &CardModel, frame: NSRect, thumb: Option<&Retained<NSImage>>) -> Card {
    let view = CardView::new(mtm, frame, pos, false);
    {
        let l = layer(&view);
        l.setCornerRadius(16.0);
        l.setMasksToBounds(true);
    }
    let tracking = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            mtm.alloc(),
            NSRect::ZERO,
            NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::ActiveAlways | NSTrackingAreaOptions::InVisibleRect,
            Some(&view),
            None,
        )
    };
    view.addTrackingArea(&tracking);

    // Thumbnail --------------------------------------------------------------
    let thumb_frame = NSRect::new(NSPoint::new(12.0, CARD_H - 12.0 - THUMB_H), NSSize::new(THUMB_W, THUMB_H));
    let thumb_bg = NSView::initWithFrame(mtm.alloc(), thumb_frame);
    {
        let l = layer(&thumb_bg);
        l.setCornerRadius(10.0);
        l.setMasksToBounds(true);
        l.setBackgroundColor(Some(&NSColor::labelColor().colorWithAlphaComponent(0.07).CGColor()));
    }
    view.addSubview(&thumb_bg);
    let thumb_view = NSImageView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::ZERO, thumb_frame.size));
    thumb_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    if let Some(img) = thumb {
        thumb_view.setImage(Some(img));
    }
    thumb_bg.addSubview(&thumb_view);

    // App icon in place of the thumbnail until a capture arrives (or for good
    // when the app has no windows).
    let placeholder = m.icon.as_ref().map(|icon| {
        let size = 72.0;
        let p = NSImageView::initWithFrame(
            mtm.alloc(),
            NSRect::new(NSPoint::new((THUMB_W - size) / 2.0, (THUMB_H - size) / 2.0), NSSize::new(size, size)),
        );
        p.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        p.setImage(Some(icon));
        p.setHidden(thumb.is_some());
        thumb_bg.addSubview(&p);
        p
    });

    // Number badge (1–9) -----------------------------------------------------
    if pos < 9 {
        let pill = badge(mtm, &(pos + 1).to_string());
        pill.setFrameOrigin(NSPoint::new(8.0, THUMB_H - 8.0 - pill.frame().size.height));
        thumb_bg.addSubview(&pill);
    }

    // Title ------------------------------------------------------------------
    let title = label(mtm, &m.title, 13.0, weight(unsafe { &NSFontWeightSemibold }), &NSColor::labelColor());
    title.setFrame(NSRect::new(NSPoint::new(14.0, 32.0), NSSize::new(CARD_W - 28.0, 18.0)));
    view.addSubview(&title);

    // Subtitle row: app icon or profile dot, text, and the expand chevron ----
    let mut text_x = 29.0;
    if let Some(icon) = &m.icon {
        let iv = NSImageView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::new(13.0, 11.0), NSSize::new(18.0, 18.0)));
        iv.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        iv.setImage(Some(icon));
        view.addSubview(&iv);
        text_x = 36.0;
    } else if let Some(color) = &m.dot {
        let dot = NSView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::new(15.0, 16.0), NSSize::new(8.0, 8.0)));
        let l = layer(&dot);
        l.setCornerRadius(4.0);
        l.setBackgroundColor(Some(&color.CGColor()));
        view.addSubview(&dot);
    }
    let mut text_w = CARD_W - text_x - 14.0;
    if m.expandable {
        let (bw, bh) = (30.0, 22.0);
        let button = CardView::new(mtm, NSRect::new(NSPoint::new(CARD_W - 12.0 - bw, 9.0), NSSize::new(bw, bh)), pos, true);
        {
            let l = layer(&button);
            l.setCornerRadius(7.0);
            l.setBackgroundColor(Some(&NSColor::labelColor().colorWithAlphaComponent(0.09).CGColor()));
        }
        let chevron = NSImageView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::new((bw - 12.0) / 2.0, (bh - 12.0) / 2.0), NSSize::new(12.0, 12.0)));
        chevron.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        chevron.setImage(symbol("chevron.right").as_deref());
        chevron.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
        button.addSubview(&chevron);
        button.setToolTip(Some(&NSString::from_str("Show windows  (` or space)")));
        view.addSubview(&button);
        text_w -= bw + 8.0;
    }
    let subtitle = label(mtm, &m.subtitle, 12.0, weight(unsafe { &NSFontWeightMedium }), &NSColor::secondaryLabelColor());
    subtitle.setFrame(NSRect::new(NSPoint::new(text_x, 12.0), NSSize::new(text_w, 16.0)));
    view.addSubview(&subtitle);

    Card { view, thumb: thumb_view, placeholder, window_id: m.window_id }
}
