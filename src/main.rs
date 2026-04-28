//! Menu-bar application entry point. Replaces
//! `SideButtonFixer/main.m` + the bootstrap in
//! `AppDelegate.m:applicationDidFinishLaunching`.

use std::sync::Arc;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;

use ssb::settings::Settings;
use ssb::tap::{Tap, TapState};
use ssb::{permissions, ui, AppState};

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
    // 4. Create the event tap (needs Accessibility). Leak it so the
    //    run loop owns it for the rest of the process.
    // -----------------------------------------------------------------
    let tap = match Tap::new(Arc::clone(&tap_state)) {
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
    // The tap must outlive `main`; NSApplication.run() never returns.
    // Leak it deliberately.
    if let Some(t) = tap {
        Box::leak(Box::new(t));
    }

    // -----------------------------------------------------------------
    // 5. Build app state and UI.
    // -----------------------------------------------------------------
    let app_state: &'static AppState = Box::leak(Box::new(AppState {
        settings: parking_lot::Mutex::new(settings),
        tap_state,
    }));

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
