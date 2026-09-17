//! Enumerates running applications and their windows. The window server gives
//! us ids, z-order and visibility for every app in one call, but it also lists
//! invisible helper windows (omnibox dropdowns, offscreen Electron windows,
//! tooltips). Accessibility (one round-trip per app) knows which windows are
//! real, which are minimized, and their full title (for Chrome that carries
//! the profile). Its answers are gathered per app into an [`AxCache`] that the
//! hub keeps across shows and refreshes in the background, so the app cards
//! count exactly the windows the sub-dock will show.

use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplicationActivationPolicy, NSImage, NSWorkspace};
use objc2_core_foundation::CFArray;
use objc2_core_graphics::{
    kCGWindowAlpha, kCGWindowBounds, kCGWindowIsOnscreen, kCGWindowLayer, kCGWindowName,
    kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID, CGWindowListCopyWindowInfo,
    CGWindowListOption,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use crate::{chrome, focus};

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub id: u32,
    pub pid: i32,
    /// Application name as reported by the window server ("Google Chrome", "Finder", ...).
    pub app: String,
    /// Window title; for Chrome the tab title with the app/profile suffixes stripped.
    pub title: String,
    /// Chrome profile, when known.
    pub profile: Option<String>,
    pub on_screen: bool,
    pub minimized: bool,
    /// False when the window server has the window on no desktop at all:
    /// closed but kept alive by its app (Calendar, Mail viewers), or a
    /// hidden helper window. Such a window is not one the user can switch to.
    pub on_space: bool,
    /// On the desktop currently shown. Off screen yet on the current desktop
    /// means minimized (or the app is hidden).
    pub on_current_space: bool,
    /// Window-server bounds (x, y, width, height) in screen points.
    pub bounds: (f64, f64, f64, f64),
}

/// A running application that would appear in the system ⌘Tab switcher.
#[derive(Clone)]
pub struct AppEntry {
    pub pid: i32,
    pub name: String,
    pub icon: Option<Retained<NSImage>>,
    pub hidden: bool,
    /// Window ids, front to back.
    pub windows: Vec<u32>,
    /// How many of `windows` are not minimized.
    pub active: usize,
    /// Has an un-minimized window on the desktop currently shown.
    pub on_current: bool,
    /// The window whose thumbnail represents the app.
    pub front: Option<u32>,
}

/// Accessibility's view of one app's windows, by window id.
pub type AxMap = HashMap<u32, focus::AxWindow>;
/// Per app (pid). An app is absent when Accessibility could not answer for it.
pub type AxCache = HashMap<i32, AxMap>;

pub struct Snapshot {
    pub apps: Vec<AppEntry>,
    /// Every window of every app in `apps`, front to back (on-screen first),
    /// with `ax` applied.
    pub windows: Vec<WindowInfo>,
    /// The same list straight from the window server, so a fresher `AxCache`
    /// can be applied later without asking the window server again.
    pub raw: Vec<WindowInfo>,
}

/// Lists the switchable apps, most recently used first, and all their windows.
/// `mru` is the app activation history, most recent first; `ax` is the last
/// known Accessibility view (see [`apply_ax`]).
pub fn snapshot(mru: &[i32], ax: &AxCache) -> Snapshot {
    let ws = NSWorkspace::sharedWorkspace();
    let me = std::process::id() as i32;

    let mut apps = Vec::new();
    let running = ws.runningApplications();
    for i in 0..running.count() {
        let a = running.objectAtIndex(i);
        if a.activationPolicy() != NSApplicationActivationPolicy::Regular || a.isTerminated() {
            continue;
        }
        let pid = a.processIdentifier() as i32;
        if pid == me {
            continue;
        }
        apps.push(AppEntry {
            pid,
            name: a.localizedName().map(|s| s.to_string()).unwrap_or_default(),
            icon: a.icon(),
            hidden: a.isHidden(),
            windows: Vec::new(),
            active: 0,
            on_current: false,
            front: None,
        });
    }

    let pids: Vec<i32> = apps.iter().map(|a| a.pid).collect();
    let raw = cg_windows(&pids);
    let mut windows = raw.clone();
    apply_ax(&mut windows, ax, &apps);
    assign_windows(&mut apps, &windows);
    order_apps(&mut apps, &windows, mru);
    Snapshot { apps, windows, raw }
}

/// Orders apps the way the hub lists them: apps with a window on the current
/// desktop first, then apps whose windows are all on other desktops; within
/// each group the frontmost app, then most recently used, then front to back.
pub fn order_apps(apps: &mut [AppEntry], windows: &[WindowInfo], mru: &[i32]) {
    let front_pid = NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier() as i32);
    let mut z_rank: HashMap<i32, usize> = HashMap::new();
    for (z, w) in windows.iter().enumerate() {
        if w.on_screen {
            z_rank.entry(w.pid).or_insert(z);
        }
    }
    apps.sort_by_cached_key(|a| {
        let recency = if Some(a.pid) == front_pid {
            0
        } else {
            mru.iter().position(|&p| p == a.pid).map_or(usize::MAX, |p| p + 1)
        };
        (!a.on_current, recency, z_rank.get(&a.pid).copied().unwrap_or(usize::MAX), a.name.to_lowercase())
    });
}

/// Gives each app its window ids (front to back) and the window whose
/// thumbnail represents it.
pub fn assign_windows(apps: &mut [AppEntry], windows: &[WindowInfo]) {
    for app in apps.iter_mut() {
        let mine: Vec<&WindowInfo> = windows.iter().filter(|w| w.pid == app.pid).collect();
        app.windows = mine.iter().map(|w| w.id).collect();
        app.active = mine.iter().filter(|w| !w.minimized).count();
        app.on_current = mine.iter().any(|w| !w.minimized && w.on_current_space);
        // Prefer a visible, titled window: untitled ones tend to be helpers.
        app.front = mine
            .iter()
            .find(|w| w.on_screen && !w.minimized && !w.title.is_empty())
            .or_else(|| mine.iter().find(|w| !w.minimized && !w.title.is_empty()))
            .or_else(|| mine.iter().find(|w| !w.title.is_empty()))
            .or_else(|| mine.first())
            .map(|w| w.id);
    }
}

/// Asks Accessibility about each app's windows. Safe to call off the main
/// thread; a hung app costs at most the messaging timeout in `focus`.
pub fn ax_lookup(pids: &[i32]) -> AxCache {
    let debug = std::env::var_os("CHROMEHUB_DEBUG").is_some();
    let mut out = AxCache::new();
    for &pid in pids {
        let started = std::time::Instant::now();
        if let Some(list) = focus::ax_windows(pid) {
            out.insert(pid, list.into_iter().map(|w| (w.id, w)).collect());
        }
        let took = started.elapsed();
        if debug && took.as_millis() >= 50 {
            eprintln!("  slow accessibility reply: pid {pid} took {took:?}");
        }
    }
    out
}

/// Whether a window the user can switch to. Accessibility lists the windows
/// on the current Space (plus minimized ones): of those, a standard window
/// counts — or any window for apps whose windows carry no standard subrole
/// (some Electron/Qt apps). A window Accessibility has never heard of is on
/// another desktop if it is titled and on some Space; on-screen unknowns are
/// helpers (omnibox dropdowns, tooltips), untitled ones hidden helpers, and
/// titled ones on no Space are closed windows the app keeps alive.
fn is_real(m: &AxMap, w: &WindowInfo) -> bool {
    match m.get(&w.id) {
        Some(a) => a.standard || !m.values().any(|x| x.standard),
        None => !w.on_screen && other_desktop(w),
    }
}

/// The window-server-only test for a window on another desktop, also used
/// for apps Accessibility has not answered for yet.
fn other_desktop(w: &WindowInfo) -> bool {
    !w.title.is_empty() && w.on_space
}

/// Applies Accessibility's view to the windows of every app present in `ax`:
/// drops the ones that are not real (see [`is_real`]) and fills in
/// `minimized` and the full title (for Chrome: the profile). For apps absent
/// from `ax`, only closed-but-kept windows are dropped. Windows Accessibility
/// does not list get `minimized` from the window server: off screen yet on
/// the current desktop, unless the app is hidden.
pub fn apply_ax(windows: &mut Vec<WindowInfo>, ax: &AxCache, apps: &[AppEntry]) {
    windows.retain(|w| ax.get(&w.pid).map_or(w.on_screen || other_desktop(w), |m| is_real(m, w)));
    for w in windows.iter_mut() {
        let hidden = apps.iter().any(|a| a.pid == w.pid && a.hidden);
        w.minimized = !w.on_screen && w.on_current_space && !hidden;
        let Some(info) = ax.get(&w.pid).and_then(|m| m.get(&w.id)) else { continue };
        w.minimized = info.minimized;
        let (ax_title, ax_profile) =
            if chrome::is_chrome(&w.app) { chrome::split_title(&info.title, &w.app) } else { (info.title.clone(), None) };
        if w.title.is_empty() {
            w.title = ax_title;
        }
        if w.profile.is_none() {
            w.profile = ax_profile;
        }
    }
}

fn key(s: &objc2_core_foundation::CFString) -> &NSString {
    // CFString and NSString are toll-free bridged.
    unsafe { &*(s as *const objc2_core_foundation::CFString as *const NSString) }
}

fn number(dict: &NSDictionary<NSString, AnyObject>, k: &NSString) -> Option<f64> {
    let obj = dict.objectForKey(k)?;
    let n = obj.downcast_ref::<NSNumber>()?;
    Some(n.doubleValue())
}

/// Window-server view of the windows owned by `pids`: visible ones first in
/// z-order, then windows on other desktops or minimized. Fast; no Accessibility.
fn cg_windows(pids: &[i32]) -> Vec<WindowInfo> {
    let Some(list) = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements,
        0,
    ) else {
        return vec![];
    };
    let list: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(&*list as *const CFArray as *const NSArray<NSDictionary<NSString, AnyObject>>) };

    let (k_num, k_layer, k_bounds, k_alpha, k_pid, k_owner, k_name, k_onscreen) = unsafe {
        (
            key(kCGWindowNumber),
            key(kCGWindowLayer),
            key(kCGWindowBounds),
            key(kCGWindowAlpha),
            key(kCGWindowOwnerPID),
            key(kCGWindowOwnerName),
            key(kCGWindowName),
            key(kCGWindowIsOnscreen),
        )
    };
    let (k_width, k_height) = (NSString::from_str("Width"), NSString::from_str("Height"));
    let (k_x, k_y) = (NSString::from_str("X"), NSString::from_str("Y"));

    let mut out = Vec::new();
    for i in 0..list.count() {
        let d = list.objectAtIndex(i);
        let Some(pid) = number(&d, k_pid) else { continue };
        let pid = pid as i32;
        if !pids.contains(&pid) {
            continue;
        }
        if number(&d, k_layer).unwrap_or(1.0) != 0.0 || number(&d, k_alpha).unwrap_or(1.0) <= 0.0 {
            continue;
        }
        let Some(bounds) = d.objectForKey(k_bounds).and_then(|b| b.downcast::<NSDictionary>().ok()) else { continue };
        let bounds: &NSDictionary<NSString, AnyObject> = unsafe { &*(&*bounds as *const NSDictionary as *const _) };
        let (width, height) = (number(bounds, &k_width).unwrap_or(0.0), number(bounds, &k_height).unwrap_or(0.0));
        let (bx, by) = (number(bounds, &k_x).unwrap_or(0.0), number(bounds, &k_y).unwrap_or(0.0));
        // Apps keep small invisible helper windows (tab-strip hover targets etc.).
        if width < 200.0 || height < 120.0 {
            continue;
        }
        let Some(id) = number(&d, k_num) else { continue };
        let owner = d.objectForKey(k_owner).and_then(|o| o.downcast::<NSString>().ok()).map(|s| s.to_string()).unwrap_or_default();
        let cg_title = d
            .objectForKey(k_name)
            .and_then(|o| o.downcast::<NSString>().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let on_screen = number(&d, k_onscreen).unwrap_or(0.0) != 0.0;
        let on_current_space = on_screen || spaces::count(id as u32, spaces::CURRENT).map_or(false, |n| n > 0);
        let on_space = on_current_space || spaces::count(id as u32, spaces::ALL).map_or(true, |n| n > 0);
        let (title, profile) = if chrome::is_chrome(&owner) { chrome::split_title(&cg_title, &owner) } else { (cg_title, None) };
        out.push(WindowInfo {
            id: id as u32,
            pid,
            app: owner,
            title,
            profile,
            on_screen,
            minimized: false,
            on_space,
            on_current_space,
            bounds: (bx, by, width, height),
        });
    }
    out.sort_by_key(|w| !w.on_screen);
    out
}

/// One line per app for `CHROMEHUB_DEBUG`: how many windows the window server
/// lists, how many survive, and how many of those are on other desktops.
/// App names only, never titles.
pub fn ax_summary(raw: &[WindowInfo], kept: &[WindowInfo], apps: &[AppEntry], ax: &AxCache) -> String {
    apps.iter()
        .map(|a| {
            let raw_n = raw.iter().filter(|w| w.pid == a.pid).count();
            let kept_n = kept.iter().filter(|w| w.pid == a.pid).count();
            let other = kept.iter().filter(|w| w.pid == a.pid && ax.get(&a.pid).map_or(false, |m| !m.contains_key(&w.id))).count();
            let ax_n = ax.get(&a.pid).map_or("-".to_string(), |m| m.len().to_string());
            let dropped: Vec<&WindowInfo> = raw.iter().filter(|w| w.pid == a.pid && !kept.iter().any(|k| k.id == w.id)).collect();
            let (mut on_t, mut on_u, mut off_u, mut closed, mut ax_nonstd) = (0, 0, 0, 0, 0);
            let mut geo = String::new();
            for w in dropped {
                let (x, y, wd, ht) = w.bounds;
                geo.push_str(&format!(" [{x:.0},{y:.0} {wd:.0}x{ht:.0}{}]", if w.on_space { "" } else { " no-space" }));
                let known = ax.get(&a.pid).map_or(false, |m| m.contains_key(&w.id));
                match (known, w.on_screen, w.title.is_empty(), w.on_space) {
                    (true, _, _, _) => ax_nonstd += 1,
                    (false, true, false, _) => on_t += 1,
                    (false, true, true, _) => on_u += 1,
                    (false, false, true, _) => off_u += 1,
                    (false, false, false, _) => closed += 1,
                }
            }
            let kept_geo: String = kept
                .iter()
                .filter(|w| w.pid == a.pid)
                .map(|w| {
                    let (x, y, wd, ht) = w.bounds;
                    format!(
                        " ({x:.0},{y:.0} {wd:.0}x{ht:.0}{}{}{})",
                        if w.on_screen { " on" } else { " off" },
                        if w.on_current_space { " cur" } else { "" },
                        if w.minimized { " min" } else { "" }
                    )
                })
                .collect();
            format!(
                "  {}{}{}: cg={raw_n} ax={ax_n} shown={kept_n}{kept_geo} other-desktop={other} dropped[ax-nonstandard={ax_nonstd} onscreen-titled={on_t} onscreen-untitled={on_u} offscreen-untitled={off_u} closed={closed}]{geo}",
                a.name,
                if a.hidden { " (hidden)" } else { "" },
                if a.on_current { "" } else { " (elsewhere)" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Which desktops (Spaces) a window is on, through SkyLight's private
/// `SLSCopySpacesForWindows` — the call AltTab and yabai use for the same
/// question. It is resolved at runtime so a macOS without it degrades to
/// "unknown" rather than failing to launch.
mod spaces {
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    use objc2_foundation::{NSArray, NSNumber};

    type MainConnection = unsafe extern "C" fn() -> i32;
    type CopySpaces = unsafe extern "C" fn(i32, i32, *const c_void) -> *const c_void;

    extern "C" {
        fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFRelease(cf: *const c_void);
    }

    struct Api {
        connection: i32,
        copy_spaces: CopySpaces,
    }

    static API: OnceLock<Option<Api>> = OnceLock::new();

    fn load() -> Option<Api> {
        unsafe {
            let lib = dlopen(c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight".as_ptr(), 1 /* RTLD_LAZY */);
            if lib.is_null() {
                return None;
            }
            let main = dlsym(lib, c"SLSMainConnectionID".as_ptr());
            let copy = dlsym(lib, c"SLSCopySpacesForWindows".as_ptr());
            if main.is_null() || copy.is_null() {
                return None;
            }
            let main: MainConnection = std::mem::transmute(main);
            Some(Api { connection: main(), copy_spaces: std::mem::transmute(copy) })
        }
    }

    /// Space masks for `count`.
    pub const CURRENT: i32 = 5;
    pub const ALL: i32 = 7;

    /// Number of Spaces (matching `mask`) the window is on, or None when the
    /// API is unavailable.
    pub fn count(window_id: u32, mask: i32) -> Option<usize> {
        let api = API.get_or_init(load).as_ref()?;
        let ids = NSArray::from_retained_slice(&[NSNumber::new_u32(window_id)]);
        let arr = unsafe { (api.copy_spaces)(api.connection, mask, &*ids as *const NSArray<NSNumber> as *const c_void) };
        if arr.is_null() {
            return Some(0);
        }
        let n = unsafe { CFArrayGetCount(arr) };
        unsafe { CFRelease(arr) };
        Some(n.max(0) as usize)
    }
}
