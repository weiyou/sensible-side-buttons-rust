//! Accessibility-permission helpers. Wraps
//! `AXIsProcessTrustedWithOptions` from `ApplicationServices.framework`.
//!
//! Mirrors `updateMenuMode:` in `AppDelegate.m:240–256`.

use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::dictionary::{CFDictionaryCreate, CFDictionaryRef};
use core_foundation_sys::number::{kCFBooleanFalse, kCFBooleanTrue};
use core_foundation_sys::string::CFStringRef;
use std::os::raw::c_void;
use std::ptr;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// Key for the accessibility-prompt option. Defined in
    /// `<ApplicationServices/AXUIElement.h>` as
    /// `kAXTrustedCheckOptionPrompt`.
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
}

/// Returns whether this process is trusted by the Accessibility API.
/// If `prompt` is `true` and we are NOT trusted, macOS shows its own
/// system prompt directing the user to System Settings.
pub fn check_accessibility(prompt: bool) -> bool {
    unsafe {
        let key = kAXTrustedCheckOptionPrompt as *const c_void;
        let value = if prompt {
            kCFBooleanTrue as *const c_void
        } else {
            kCFBooleanFalse as *const c_void
        };
        let dict: CFDictionaryRef = CFDictionaryCreate(
            ptr::null(),
            &key as *const _,
            &value as *const _,
            1,
            ptr::null(),
            ptr::null(),
        );
        let trusted = AXIsProcessTrustedWithOptions(dict);
        CFRelease(dict as CFTypeRef);
        trusted
    }
}

/// Open the Privacy → Accessibility pane of System Settings via
/// `NSWorkspace`. Helper for the no-permission path.
pub fn open_accessibility_settings() {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSString, NSURL};

    let url_str = NSString::from_str(
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
    );
    if let Some(url) = NSURL::URLWithString(&url_str) {
        let workspace = NSWorkspace::sharedWorkspace();
        workspace.openURL(&url);
    }
}
