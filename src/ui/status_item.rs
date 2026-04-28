//! `NSStatusItem` creation + visual state. Replaces the
//! `MenuIcon` / `MenuIconDisabled` image assets used by the original
//! (`AppDelegate.m:308–315`) with simple unicode glyphs so the binary
//! has no asset dependencies.

use objc2::rc::Retained;
use objc2_app_kit::{NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
use objc2_foundation::{MainThreadMarker, NSString};

const TITLE_ENABLED: &str = "⇆";
const TITLE_DISABLED: &str = "↮";

pub fn create(mtm: MainThreadMarker) -> Retained<NSStatusItem> {
    let bar = NSStatusBar::systemStatusBar();
    let item = bar.statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str(TITLE_ENABLED));
    }
    item
}

pub fn update_icon(mtm: MainThreadMarker, item: &NSStatusItem, enabled: bool) {
    if let Some(button) = item.button(mtm) {
        let title = NSString::from_str(if enabled { TITLE_ENABLED } else { TITLE_DISABLED });
        button.setTitle(&title);
    }
}
