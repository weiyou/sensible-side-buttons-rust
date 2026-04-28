//! `NSMenu` construction + the Obj-C target class that receives
//! menu-item actions and dispatches into the Rust controller logic.
//! Mirrors `AppDelegate.m:160–228` (menu construction) and
//! `:349–389` (toggle handlers).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags, NSMenu,
    NSMenuItem, NSStatusItem,
};
use objc2_foundation::{ns_string, MainThreadMarker, NSObject, NSString};

use crate::tap;
use crate::AppState;

// ---------------------------------------------------------------------------
// Menu target — Obj-C subclass that holds retained menu items and
// dispatches each menu action into the Rust controller logic.
// ---------------------------------------------------------------------------

pub struct MenuTargetIvars {
    pub app_state: &'static AppState,
    pub status_item: Retained<NSStatusItem>,
    pub enabled_item: Retained<NSMenuItem>,
    pub mouse_down_item: Retained<NSMenuItem>,
    pub swap_item: Retained<NSMenuItem>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SSBMenuTarget"]
    #[ivars = MenuTargetIvars]
    pub struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(enabledToggle:))]
        fn enabled_toggle(&self, _sender: Option<&AnyObject>) {
            self.toggle_enabled();
        }

        #[unsafe(method(mouseDownToggle:))]
        fn mouse_down_toggle(&self, _sender: Option<&AnyObject>) {
            self.toggle_mouse_down();
        }

        #[unsafe(method(swapToggle:))]
        fn swap_toggle(&self, _sender: Option<&AnyObject>) {
            self.toggle_swap();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            let app = NSApplication::sharedApplication(self.mtm());
            app.terminate(None);
        }
    }
);

impl MenuTarget {
    fn alloc_init(mtm: MainThreadMarker, ivars: MenuTargetIvars) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    // -----------------------------------------------------------------
    // Toggle handlers — read+flip the relevant atomic, mirror to
    // Settings, persist, and refresh the UI. (`AppDelegate.m:349–389`)
    // -----------------------------------------------------------------

    fn toggle_enabled(&self) {
        let ivars = self.ivars();
        let app_state = ivars.app_state;
        let st = &app_state.tap_state;
        let new_val = !st.enabled.load(Ordering::Relaxed);
        st.enabled.store(new_val, Ordering::Release);

        // Full tap recreate rather than a bare CGEventTapEnable call.
        //
        // After Fast User Switch, the old Mach port is dead and
        // CGEventTapEnable is a silent no-op on it.  The only recovery
        // is a new CGEventTapCreate — which start_tap performs.
        // stop_tap first tears down any existing (possibly dead) port so
        // there is never more than one active tap.
        if new_val {
            tap::start_tap(Arc::clone(st), &app_state.live_tap, true);
        } else {
            tap::stop_tap(&app_state.live_tap);
        }

        // Mirror to settings + persist.
        app_state.settings.lock().enabled = new_val;
        app_state.persist();

        // UI refresh.
        ivars
            .enabled_item
            .setState(if new_val { NSControlStateValueOn } else { NSControlStateValueOff });
        crate::ui::status_item::update_icon(self.mtm(), &ivars.status_item, new_val);
    }

    fn toggle_mouse_down(&self) {
        let ivars = self.ivars();
        let st = &ivars.app_state.tap_state;
        let new_val = !st.mouse_down.load(Ordering::Relaxed);
        st.mouse_down.store(new_val, Ordering::Release);
        ivars.app_state.settings.lock().mouse_down = new_val;
        ivars.app_state.persist();
        ivars
            .mouse_down_item
            .setState(if new_val { NSControlStateValueOn } else { NSControlStateValueOff });
    }

    fn toggle_swap(&self) {
        let ivars = self.ivars();
        let st = &ivars.app_state.tap_state;
        let new_val = !st.swap_buttons.load(Ordering::Relaxed);
        st.swap_buttons.store(new_val, Ordering::Release);
        ivars.app_state.settings.lock().swap_buttons = new_val;
        ivars.app_state.persist();
        ivars
            .swap_item
            .setState(if new_val { NSControlStateValueOn } else { NSControlStateValueOff });
    }
}

// ---------------------------------------------------------------------------
// Public builder
// ---------------------------------------------------------------------------

/// Build the menu, attach it to `status_item`, and return the retained
/// `MenuTarget` so the caller can keep it alive (the menu only weakly
/// references its target via `setTarget:`).
pub fn build(
    mtm: MainThreadMarker,
    app_state: &'static AppState,
    status_item: Retained<NSStatusItem>,
) -> Retained<MenuTarget> {
    let menu: Retained<NSMenu> = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);

    let initial = app_state.settings.lock().clone();

    let enabled_item =
        make_toggle(mtm, ns_string!("Enabled"), sel!(enabledToggle:), initial.enabled);
    menu.addItem(&enabled_item);
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let mouse_down_item = make_toggle(
        mtm,
        ns_string!("Trigger on Mouse Down"),
        sel!(mouseDownToggle:),
        initial.mouse_down,
    );
    menu.addItem(&mouse_down_item);

    let swap_item =
        make_toggle(mtm, ns_string!("Swap Buttons"), sel!(swapToggle:), initial.swap_buttons);
    menu.addItem(&swap_item);

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    // Quit (Cmd-Q).
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            ns_string!("Quit"),
            Some(sel!(quit:)),
            ns_string!("q"),
        )
    };
    quit.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
    menu.addItem(&quit);

    let target = MenuTarget::alloc_init(
        mtm,
        MenuTargetIvars {
            app_state,
            status_item: status_item.clone(),
            enabled_item: enabled_item.clone(),
            mouse_down_item: mouse_down_item.clone(),
            swap_item: swap_item.clone(),
        },
    );

    // Wire each menu item's target so its action selector lands on
    // `MenuTarget`. (`AppDelegate.m:165–225`)
    //
    // We use `msg_send!` directly to side-step the AsRef<AnyObject>
    // ambiguity for Retained<MenuTarget>.
    unsafe {
        let _: () = msg_send![&*enabled_item, setTarget: &*target];
        let _: () = msg_send![&*mouse_down_item, setTarget: &*target];
        let _: () = msg_send![&*swap_item, setTarget: &*target];
        let _: () = msg_send![&*quit, setTarget: &*target];
    }

    status_item.setMenu(Some(&menu));

    // Reflect initial state in the icon.
    crate::ui::status_item::update_icon(mtm, &status_item, initial.enabled);

    target
}

fn make_toggle(
    mtm: MainThreadMarker,
    title: &NSString,
    action: objc2::runtime::Sel,
    on: bool,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            title,
            Some(action),
            ns_string!(""),
        )
    };
    item.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
    item
}
