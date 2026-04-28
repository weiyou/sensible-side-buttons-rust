//! Global mouse-button event tap.
//!
//! Direct port of the relevant pieces of `AppDelegate.m:39–63` (the
//! callback) and `AppDelegate.m:318–347` (`startTap:`).
//!
//! Robustness note: like Wowfunhappy's fork (commit 0cafcf5) we re-enable
//! the tap immediately if macOS disables it for exceeding latency budget
//! (`kCGEventTapDisabledByTimeout`). The original SSB does not — over a
//! long session the tap can silently die.
//!
//! Hot-path discipline: the callback reads ONLY atomics from
//! [`TapState`]. No allocation, no locking, no I/O. Anything that
//! touches `Settings` happens on the cold path (menu toggle handlers).

#![allow(non_upper_case_globals, non_snake_case)]

use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopRemoveSource,
    CFRunLoopSourceRef,
};
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Arc;

use crate::gesture::{post_swipe, Direction};

// ---------------------------------------------------------------------------
// FFI surface (CoreGraphics event tap API)
// ---------------------------------------------------------------------------

pub type CGEventTapProxy = *const c_void;
pub type CGEventRef = *mut c_void;
pub type CGEventType = u32;
pub type CGEventMask = u64;
pub type CGEventTapLocation = u32;
pub type CGEventTapPlacement = u32;
pub type CGEventTapOptions = u32;
pub type CGEventField = u32;
pub type CFMachPortRef = *mut c_void;

const kCGHIDEventTap: CGEventTapLocation = 0;
const kCGHeadInsertEventTap: CGEventTapPlacement = 0;
const kCGEventTapOptionDefault: CGEventTapOptions = 0;

/// `CGEventType` for `kCGEventOtherMouseDown` / Up.
/// Values from `<CoreGraphics/CGEventTypes.h>`.
const kCGEventOtherMouseDown: CGEventType = 25;
const kCGEventOtherMouseUp: CGEventType = 26;

/// Special event types delivered to the callback when macOS has disabled
/// our tap. We must call `CGEventTapEnable(true)` to revive it.
const kCGEventTapDisabledByTimeout: CGEventType = 0xFFFF_FFFE;
const kCGEventTapDisabledByUserInput: CGEventType = 0xFFFF_FFFF;

/// Field tag used with `CGEventGetIntegerValueField` to read the mouse
/// button number (`kCGMouseEventButtonNumber` from CGEventTypes.h:208).
const kCGMouseEventButtonNumber: CGEventField = 3;

pub type CGEventTapCallBack = Option<
    unsafe extern "C" fn(
        proxy: CGEventTapProxy,
        ty: CGEventType,
        event: CGEventRef,
        refcon: *mut c_void,
    ) -> CGEventRef,
>;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: CGEventTapLocation,
        place: CGEventTapPlacement,
        options: CGEventTapOptions,
        events_of_interest: CGEventMask,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventTapIsEnabled(tap: CFMachPortRef) -> bool;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: CGEventField) -> i64;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
}

/// Helper exposed for the menu UI — toggles the running tap on/off
/// using the port stored in `TapState`.
pub fn set_tap_running(state: &TapState, on: bool) {
    let port = state.mach_port.load(Ordering::Acquire);
    if !port.is_null() {
        unsafe { CGEventTapEnable(port, on) };
    }
}

const fn event_mask_bit(t: CGEventType) -> CGEventMask {
    1u64 << (t as u64)
}

// ---------------------------------------------------------------------------
// TapState — atomic snapshot of settings used by the callback hot path
// ---------------------------------------------------------------------------

/// Accessed by the CGEventTap callback. **All operations must be lock-free.**
pub struct TapState {
    /// Whether to actively remap. When `false`, the callback returns the
    /// event unchanged (no swallow, no swipe).
    pub enabled: AtomicBool,
    /// `true` = trigger the swipe on mouse-down. `false` = on mouse-up.
    pub mouse_down: AtomicBool,
    /// Swap which physical button maps to back vs forward.
    pub swap_buttons: AtomicBool,
    /// `CFMachPortRef` of the running tap. Stored so the callback (and
    /// the UI) can call `CGEventTapEnable` without a separate handle.
    /// Set after `CGEventTapCreate` succeeds; `null` if no tap exists.
    pub mach_port: AtomicPtr<c_void>,
}

impl TapState {
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        Self {
            enabled: AtomicBool::new(s.enabled),
            mouse_down: AtomicBool::new(s.mouse_down),
            swap_buttons: AtomicBool::new(s.swap_buttons),
            mach_port: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tap — RAII wrapper around the CGEventTap mach port + run-loop source
// ---------------------------------------------------------------------------

pub struct Tap {
    /// The Arc keeps the inner pointer (used as `refcon`) valid for the
    /// tap's lifetime; clones may exist elsewhere (e.g. inside `AppState`).
    state: Arc<TapState>,
    mach_port: CFMachPortRef,
    run_loop_source: CFRunLoopSourceRef,
}

// `CFMachPortRef` and `CFRunLoopSourceRef` are raw pointers, so the
// auto-derived !Send/!Sync would be wrong. The tap is used only on the
// main thread, but `Tap` may live inside structures we'd like to leak as
// `&'static`; mark explicitly.
unsafe impl Send for Tap {}
unsafe impl Sync for Tap {}

impl Tap {
    /// Create the event tap. Fails (returns `Err`) if `CGEventTapCreate`
    /// returns null — typically because Accessibility permission isn't
    /// granted to the running process.
    pub fn new(state: Arc<TapState>) -> Result<Self, &'static str> {
        let events_of_interest =
            event_mask_bit(kCGEventOtherMouseDown) | event_mask_bit(kCGEventOtherMouseUp);

        // `Arc::as_ptr` stays valid as long as any clone of the Arc
        // (specifically `self.state`) lives — i.e. for the lifetime of
        // this `Tap`.
        let user_info = Arc::as_ptr(&state) as *mut c_void;

        unsafe {
            let port = CGEventTapCreate(
                kCGHIDEventTap,
                kCGHeadInsertEventTap,
                kCGEventTapOptionDefault,
                events_of_interest,
                Some(tap_callback),
                user_info,
            );
            if port.is_null() {
                return Err("CGEventTapCreate returned NULL (Accessibility not granted?)");
            }

            let source = CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
            CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);

            // Start disabled — caller flips via `set_enabled`. Matches
            // `startTap:`'s explicit `CGEventTapEnable(self.tap, true)`
            // step at the end (`AppDelegate.m:333`).
            CGEventTapEnable(port, false);

            // Publish the port so the menu UI / robustness code can reach
            // it without holding a reference to `Tap`.
            state.mach_port.store(port, Ordering::Release);

            Ok(Tap {
                state,
                mach_port: port,
                run_loop_source: source,
            })
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        unsafe { CGEventTapEnable(self.mach_port, enabled) };
    }

    pub fn is_enabled(&self) -> bool {
        unsafe { CGEventTapIsEnabled(self.mach_port) }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            CGEventTapEnable(self.mach_port, false);
            CFRunLoopRemoveSource(
                CFRunLoopGetMain(),
                self.run_loop_source,
                kCFRunLoopCommonModes,
            );
            CFRelease(self.run_loop_source as CFTypeRef);
            CFRelease(self.mach_port as CFTypeRef);
        }
        // Clear the published port so any late callback bails.
        self.state
            .mach_port
            .store(ptr::null_mut(), Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// The callback. Mirrors `SBFMouseCallback` (AppDelegate.m:39–63).
// ---------------------------------------------------------------------------

unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: CGEventRef,
    refcon: *mut c_void,
) -> CGEventRef {
    // ----- robustness: tap-disabled events (Wowfunhappy commit 0cafcf5) ---
    if ty == kCGEventTapDisabledByTimeout || ty == kCGEventTapDisabledByUserInput {
        if !refcon.is_null() {
            let state = &*(refcon as *const TapState);
            let port = state.mach_port.load(Ordering::Acquire);
            if !port.is_null() {
                CGEventTapEnable(port, true);
            }
        }
        return event;
    }

    if refcon.is_null() {
        return event;
    }
    let state = &*(refcon as *const TapState);

    if !state.enabled.load(Ordering::Relaxed) {
        return event;
    }

    let button = CGEventGetIntegerValueField(event, kCGMouseEventButtonNumber);
    let is_down = ty == kCGEventOtherMouseDown;

    let swap = state.swap_buttons.load(Ordering::Relaxed);
    let on_down = state.mouse_down.load(Ordering::Relaxed);

    // Original semantics (`AppDelegate.m:46–62`):
    //   if mouseDown setting → fire on the down edge; if mouseUp setting
    //   → fire on the up edge. The other edge is *swallowed* (return NULL)
    //   regardless, so the OS never sees the click.
    let back_button = if swap { 4 } else { 3 } as i64;
    let forward_button = if swap { 3 } else { 4 } as i64;

    let trigger = (on_down && is_down) || (!on_down && !is_down);

    if button == back_button {
        if trigger {
            post_swipe(Direction::Left);
        }
        return ptr::null_mut(); // swallow
    }
    if button == forward_button {
        if trigger {
            post_swipe(Direction::Right);
        }
        return ptr::null_mut(); // swallow
    }

    event
}
