//! Menu-bar application entry point. Replaces
//! `SideButtonFixer/main.m` + the bootstrap in
//! `AppDelegate.m:applicationDidFinishLaunching`.

use std::sync::Arc;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;

use ssb::settings::Settings;
use ssb::tap::{Tap, TapState};
use ssb::{permissions, ui, AppState};

// ---------------------------------------------------------------------------
// GCD trampoline for auto-recovery after Fast User Switch
// ---------------------------------------------------------------------------

/// Called on the main dispatch queue (asynchronously, after the tap
/// callback returns) when the callback detects that the Mach port is dead.
///
/// After a Fast User Switch back to this user, macOS invalidates the old
/// `CGEventTap` port. `CGEventTapEnable` on a dead port is a no-op.
/// The only recovery is a full `CGEventTapCreate` call — which is exactly
/// what `start_tap` does.
unsafe extern "C" fn recreate_trampoline(ctx: *mut std::os::raw::c_void) {
    use std::sync::atomic::Ordering;
    let app_state = &*(ctx as *const AppState);
    if app_state.tap_state.enabled.load(Ordering::Relaxed) {
        ssb::tap::start_tap(Arc::clone(&app_state.tap_state), &app_state.live_tap, true);
    } else {
        // User intentionally disabled the tap. Clear the flag so a future
        // menu re-enable starts fresh rather than finding needs_recreate=true.
        app_state.tap_state.needs_recreate.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let mtm = MainThreadMarker::new()
        .expect("sensible-side-buttons must run on the main thread");

    // -----------------------------------------------------------------
    // 1. NSApplication setup. `Accessory` policy = menu-bar only,
    //    no Dock icon, no Cmd-Tab presence (matches `LSUIElement = true`
    //    in the original `Info.plist`).
    // -----------------------------------------------------------------
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // -----------------------------------------------------------------
    // 2. Load persisted settings & build atomic mirror.
    // -----------------------------------------------------------------
    let settings = Settings::load();
    let tap_state = Arc::new(TapState::from_settings(&settings));

    // -----------------------------------------------------------------
    // 3. Accessibility check. macOS shows its own prompt when the
    //    process isn't trusted; we additionally open the Settings pane
    //    on the no-trust path so the user lands in the right place.
    // -----------------------------------------------------------------
    let trusted = permissions::check_accessibility(true);
    if !trusted {
        eprintln!(
            "[ssb] Accessibility permission required. Add this binary to \
             System Settings → Privacy & Security → Accessibility, \
             then relaunch."
        );
        permissions::open_accessibility_settings();
    }

    // -----------------------------------------------------------------
    // 4. Create the initial event tap and store it in AppState.
    //    We keep a live `Option<Tap>` in a Mutex so it can be torn down
    //    and recreated by the menu toggle and the GCD trampoline.
    // -----------------------------------------------------------------
    let initial_tap = match Tap::new(Arc::clone(&tap_state)) {
        Ok(t) => {
            if settings.enabled {
                t.set_enabled(true);
            }
            Some(t)
        }
        Err(e) => {
            eprintln!("[ssb] could not create event tap: {e}");
            None
        }
    };

    // -----------------------------------------------------------------
    // 5. Build app state and UI.
    // -----------------------------------------------------------------
    let app_state: &'static AppState = Box::leak(Box::new(AppState {
        settings: parking_lot::Mutex::new(settings),
        tap_state,
        live_tap: parking_lot::Mutex::new(initial_tap),
    }));

    // Register the GCD trampoline so the tap callback can schedule a
    // full recreation when it detects a dead Mach port.
    ssb::tap::set_recreate_callback(
        app_state as *const AppState as *mut std::os::raw::c_void,
        recreate_trampoline,
    );

    let status_item = ui::status_item::create(mtm);
    let menu_target = ui::menu::build(mtm, app_state, status_item);

    // The menu's items hold the target only weakly (target/action
    // pattern), so we must keep the `MenuTarget` alive ourselves.
    Box::leak(Box::new(menu_target));

    // -----------------------------------------------------------------
    // 6. Run.
    // -----------------------------------------------------------------
    app.run();
}
