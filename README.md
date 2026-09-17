# Chrome Hub

A native macOS menu-bar app switcher with a window-level drill-down, written
in Rust (AppKit + ScreenCaptureKit through `objc2`, Apple Silicon release
profile).

`⌘ Tab` replaces the system switcher with one that shows every open app
with a live thumbnail of its front window. Open means having a window
somewhere: apps idling without one (Finder), with their window closed but
kept alive (Calendar), or with every window minimized are left out. Apps with
a window on the current desktop come first, then apps whose windows are all
on other desktops. Step into any app and you get its
*sub-dock*: all of its windows, each with a thumbnail and title. For Google
Chrome the sub-dock also shows a colour-coded profile badge per window, so you
can jump to the right profile no matter how many windows are open.

## Shortcuts

| Keys | What it does |
| --- | --- |
| `⌘ Tab` | App switcher: hold `⌘`, tap `Tab` to cycle apps (most recently used first), release `⌘` to switch. A quick tap jumps to the previous app. |
| `⌘ \`` / `Space` (hub open) | Steps into the selected app's windows. Further `⌘ \`` presses cycle through them; release `⌘` to focus the selected window. |
| `⌥ Tab` | Straight into the Chrome sub-dock in switcher mode: hold `⌥`, tap `Tab` to cycle, release `⌥` to switch. |
| `⌥ \`` | Chrome sub-dock in browse mode: stays open, type to filter by tab title or profile. |
| Menu-bar icon | Opens browse mode at the app level. Right-click for the menu (Quit). |
| `← → ↑ ↓` / `Tab` | Move the selection |
| `1` – `9` | Jump straight to that app or window |
| `⏎` / click | Activate the selected app / focus the selected window (un-minimizes it if needed). The `›` button on an app card opens its windows. |
| `⌫` / `Esc` | Delete a search character / clear search / back up to the apps / close |

`⌘ Tab` is taken over with an event tap (`src/tap.rs`); the `⌥` hotkeys are
constants at the top of `src/ui.rs` (`init`) and `src/hotkey.rs` (key codes).

## Build and run

```sh
./make_app.sh --run
```

This builds `target/release/chromehub`, wraps it as `dist/ChromeHub.app`
(no Dock icon), signs it and launches it.

The first build creates a self-signed "ChromeHub Dev Signing" certificate in
a dedicated keychain (`~/Library/Keychains/ChromeHubSigning.keychain-db`, no
dialogs, nothing added to your trust settings) and every build signs with it.
That matters: macOS ties the permission grants below to the app's signing
requirement, and with ad-hoc signing that requirement changed on every build,
so each rebuild silently revoked the grants and the app asked again. With the
stable identity the grants survive rebuilds.

On first launch macOS asks for two permissions. Both are required:

- **Screen Recording** – window titles and thumbnails (ScreenCaptureKit).
- **Accessibility** – taking over `⌘ Tab`, listing an app's real windows and
  raising a specific window instead of the whole app. Until it is granted
  `⌘ Tab` stays the system switcher; the app checks every two seconds and
  takes `⌘ Tab` over as soon as the grant appears, no relaunch needed.

Grant them in System Settings → Privacy & Security (relaunch after granting
Screen Recording).
To start it at login, add `dist/ChromeHub.app` under
System Settings → General → Login Items.

## Privacy and security

The app asks for two sensitive permissions, so here is exactly what it does
with them:

- **Screen Recording** is used only to capture thumbnails of Chrome windows,
  one window at a time through `SCContentFilter(desktopIndependentWindow:)`,
  and only for the window ids found in the Chrome window list. Nothing else on
  screen is ever captured. Thumbnails live in memory for as long as the window
  exists and are never written to disk.
- **Accessibility** is used to read an app's window list and titles and to
  raise the chosen window or app, and to install the `⌘ Tab` event tap. The
  tap receives key-down and modifier events system-wide but looks only at the
  key code and modifier bits, acts only on `⌘ Tab` and (while the hub is
  open) `⌘ \``, and passes everything else through untouched. Nothing is
  logged, stored or injected.
- The app has no network code and no dependencies beyond Apple's frameworks
  via `objc2`.
- `CHROMEHUB_DEBUG=1` prints permission state, per-app window counts and
  geometry, Accessibility timings and when the `⌘ Tab` tap is installed to
  stderr, never titles or captures.
- `notifyutil -p com.konstantin.chromehub.toggle` toggles the hub. Any local
  process can post that notification; it only opens or closes the overlay.
- `_AXUIElementGetWindow` is a private Accessibility function (the standard
  way to map an AX window to a window id). If a future macOS removes it the
  app degrades to title matching instead of failing. `SLSCopySpacesForWindows`
  is a private SkyLight function loaded at runtime; without it every
  off-screen window is assumed to be on some desktop.

## How it works

- `src/windows.rs` – lists the apps that would appear in the system `⌘ Tab`
  (regular activation policy), most recently used first, and their windows
  with one `CGWindowListCopyWindowInfo` call. The window server also lists
  invisible helper windows (Chrome's tab-strip hover targets and omnibox
  dropdown, Electron's offscreen windows) and windows an app closed but keeps
  alive. Accessibility is asked which windows are real, which are minimized,
  and for the full title; SkyLight's private `SLSCopySpacesForWindows` (the
  call AltTab and yabai use, resolved at runtime) says which desktops a window
  is on. A window on no desktop is closed or a helper; one off screen on the
  current desktop is minimized; one Accessibility has never heard of but that
  sits titled on some desktop is on another desktop, since Accessibility only
  reports the current one. Accessibility answers are cached per app and
  refreshed on a background thread every time the hub opens, so `⌘ Tab`
  appears instantly and the app cards count exactly the windows their
  sub-dock shows.
- `src/chrome.rs` – parses `Tab - Google Chrome - Profile (email)` titles.
  The profile suffix is only in the Accessibility title, not the
  window-server one.
- `src/capture.rs` – captures each window with `SCScreenshotManager` on a
  background queue; thumbnails are cached per window id so the hub opens
  instantly and refreshes a moment later.
- `src/focus.rs` – Accessibility: lists an app's windows, matches a window id
  to its element to un-minimize, raise and focus it, and activates apps the
  way `⌘ Tab` does (restoring a window when all are minimized).
- `src/tap.rs` – the session event tap that claims `⌘ Tab` and `⌘ \``. It
  runs on its own thread: an active tap holds up keyboard delivery for the
  whole system until its callback returns, so it must never wait on the hub's
  main-thread work. It only reads an atomic "hub visible" flag and hands the
  key to the main queue.
- `src/hotkey.rs` – Carbon `RegisterEventHotKey` for the `⌥` shortcuts.
- `src/ui.rs` – the non-activating `NSPanel` with an `NSVisualEffectView`
  background, the two-level card grid (apps, then one app's windows),
  keyboard handling, the most-recently-used order and the status item.
