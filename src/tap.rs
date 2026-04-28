//! Global mouse-button event tap.
//!
//! Direct port of the relevant pieces of `AppDelegate.m:39–63` (the
//! callback) and `AppDelegate.m:318–347` (`startTap:`).
//!
//! Robustness:
//!
//! * Like Wowfunhappy's fork we immediately re-enable the tap on
//!   `kCGEventTapDisabledByTimeout` / `kCGEventTapDisabledByUserInput`.
//!
//! * **Stronger**: we call `CGEventTapIsEnabled` after the re-enable call
//!   to detect a truly dead Mach port (the common case after Fast User
//!   Switch). If the port is dead we schedule a full tap recreation via
//!   `dispatch_async_f` on the main queue. Crucially this happens
//!   *asynchronously* — the GCD block runs after the current callback
//!   returns, avoiding any reentrancy hazard.
//!
//! * The "Enabled" menu toggle calls `stop_tap` + `start_tap` (full
//!   recreate) instead of a bare `CGEventTapEnable`. This makes the toggle
//!   a reliable recovery mechanism even when the Mach port is invalid.
//!
//! Hot-path discipline: the callback reads ONLY atomics from [`TapState`].
//! No allocation, no locking, no I/O. All cold-path work happens in menu
//! toggle handlers or the GCD trampoline.

#![allow(non_upper_case_globals, non_snake_case)]

use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopRemoveSource,
    CFRunLoopSourceRef,
};
use parking_lot::Mutex;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::gesture::{post_swipe, Direction};

// ---------------------------------------------------------------------------
// FFI surface (CoreGraphics event tap API + GCD)
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

const kCGEventOtherMouseDown: CGEventType = 25;
const kCGEventOtherMouseUp: CGEventType = 26;

/// Delivered to the callback when macOS disables the tap for exceeding
/// latency budget or at a user-session boundary (Fast User Switch).
const kCGEventTapDisabledByTimeout: CGEventType = 0xFFFF_FFFE;
const kCGEventTapDisabledByUserInput: CGEventType = 0xFFFF_FFFF;

/// `kCGMouseEventButtonNumber` (CGEventTypes.h:208).
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

// GCD — libdispatch symbols are in libSystem, which is always linked.
//
// `dispatch_get_main_queue()` is DISPATCH_ALWAYS_INLINE in modern Apple SDKs
// and has no external symbol. The underlying global is `_dispatch_main_q`.
// `dispatch_async_f` is a real exported function and links normally.
extern "C" {
    // The underlying queue object for the main queue.  Cast its address to a
    // pointer to pass to dispatch_async_f.
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}

// ---------------------------------------------------------------------------
// Recreation hook — registered once from main.rs
// ---------------------------------------------------------------------------

/// Signature of the GCD trampoline that recreates the tap.
pub type RecreateFn = unsafe extern "C" fn(*mut c_void);

// We store the function pointer as a `usize` so we can use `AtomicUsize`
// (Rust has no `AtomicFnPtr`). Transmuting to/from `usize` is sound on all
// platforms where function pointers and data pointers are the same width —
// which is true on every Apple target.
static RECREATE_CTX: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static RECREATE_FN: AtomicUsize = AtomicUsize::new(0);

/// Register the GCD trampoline that will be called (on the main queue,
/// asynchronously) when the event tap needs full recreation.
///
/// `ctx` must remain valid for the lifetime of the process — in practice
/// it is a `&'static AppState` cast to `*mut c_void`.
/// Call this once before `NSApplication::run`.
pub fn set_recreate_callback(ctx: *mut c_void, f: RecreateFn) {
    RECREATE_CTX.store(ctx, Ordering::Release);
    RECREATE_FN.store(f as usize, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fn event_mask_bit(t: CGEventType) -> CGEventMask {
    1u64 << (t as u64)
}

// ---------------------------------------------------------------------------
// TapState — atomic snapshot of settings used by the callback hot path
// ---------------------------------------------------------------------------

/// All fields are atomics so the CGEventTap callback can read them without
/// locking. The only writes come from menu toggle handlers on the main thread.
pub struct TapState {
    /// Whether to actively remap side buttons. When `false` the callback
    /// passes events through unchanged.
    pub enabled: AtomicBool,
    /// `true` = fire the swipe on mouse-down; `false` = on mouse-up.
    pub mouse_down: AtomicBool,
    /// Swap which physical button maps to back vs. forward.
    pub swap_buttons: AtomicBool,
    /// `CFMachPortRef` of the live tap, or null when no tap exists.
    /// Written by `Tap::new` / `Tap::drop`; read by the callback to
    /// call `CGEventTapEnable` on itself.
    pub mach_port: AtomicPtr<c_void>,
    /// Set by the callback when `CGEventTapEnable` fails to revive a dead
    /// port (e.g. after Fast User Switch). Cleared by `start_tap`.
    /// Used as a one-shot flag: the callback swaps it from false→true and
    /// only dispatches the GCD recreation once per incident.
    pub needs_recreate: AtomicBool,
}

impl TapState {
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        Self {
            enabled: AtomicBool::new(s.enabled),
            mouse_down: AtomicBool::new(s.mouse_down),
            swap_buttons: AtomicBool::new(s.swap_buttons),
            mach_port: AtomicPtr::new(ptr::null_mut()),
            needs_recreate: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------------------
// Tap — RAII wrapper around a CGEventTap Mach port + run-loop source
// ---------------------------------------------------------------------------

pub struct Tap {
    /// Keeps the pointer used as `refcon` alive for the tap's lifetime.
    state: Arc<TapState>,
    mach_port: CFMachPortRef,
    run_loop_source: CFRunLoopSourceRef,
}

// Raw pointers are not auto-Send/Sync. The tap is always used on the main
// thread; we mark it Send so `Mutex<Option<Tap>>` in AppState compiles.
unsafe impl Send for Tap {}
unsafe impl Sync for Tap {}

impl Tap {
    /// Create a new CGEventTap. Returns `Err` if `CGEventTapCreate` returns
    /// null (Accessibility permission not granted, or session boundary).
    ///
    /// The tap is created **disabled**; callers must call `set_enabled(true)`
    /// when ready.
    pub fn new(state: Arc<TapState>) -> Result<Self, &'static str> {
        let events_of_interest =
            event_mask_bit(kCGEventOtherMouseDown) | event_mask_bit(kCGEventOtherMouseUp);

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

            // Created disabled; caller enables via set_enabled.
            CGEventTapEnable(port, false);

            // Publish the port so the callback can reach it without holding
            // a reference to `Tap`. See Drop::compare_exchange note below.
            state.mach_port.store(port, Ordering::Release);

            Ok(Tap { state, mach_port: port, run_loop_source: source })
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        unsafe { CGEventTapEnable(self.mach_port, enabled) };
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
        // Only clear `mach_port` if it still points to *our* port. When
        // `start_tap` creates a new Tap before dropping the old one, the new
        // Tap's port is already stored here. A plain `store(null)` would
        // clobber it; `compare_exchange` leaves it intact.
        let _ = self.state.mach_port.compare_exchange(
            self.mach_port,
            ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
}

// ---------------------------------------------------------------------------
// Public tap lifecycle functions
// ---------------------------------------------------------------------------

/// Destroy the current event tap, releasing the Mach port and removing
/// the run-loop source. The `enabled` atomic is left unchanged; only the
/// hardware plumbing is torn down.
pub fn stop_tap(live_tap: &Mutex<Option<Tap>>) {
    *live_tap.lock() = None;
}

/// Create a fresh event tap and install it, replacing any existing tap.
///
/// This is the only correct way to recover after a Fast User Switch: a tap
/// whose Mach port has been invalidated by the OS cannot be revived with
/// `CGEventTapEnable` alone — it must be fully recreated.
///
/// Returns `true` on success. On failure (Accessibility not granted) the
/// `live_tap` slot is cleared and `false` is returned.
pub fn start_tap(state: Arc<TapState>, live_tap: &Mutex<Option<Tap>>, enabled: bool) -> bool {
    // Clear the recreation flag before doing anything so a concurrent
    // (GCD-dispatched) trampoline doesn't re-trigger us.
    state.needs_recreate.store(false, Ordering::Release);

    match Tap::new(Arc::clone(&state)) {
        Ok(tap) => {
            if enabled {
                tap.set_enabled(true);
            }
            // `*lock() = Some(tap)` drops the old Tap first (its Drop uses
            // compare_exchange so it won't clobber the new port), then
            // stores the new one.
            *live_tap.lock() = Some(tap);
            true
        }
        Err(e) => {
            eprintln!("[ssb] failed to (re)create event tap: {e}");
            *live_tap.lock() = None;
            false
        }
    }
}

// ---------------------------------------------------------------------------
// The callback — mirrors `SBFMouseCallback` (AppDelegate.m:39–63)
// ---------------------------------------------------------------------------

unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: CGEventRef,
    refcon: *mut c_void,
) -> CGEventRef {
    // ------------------------------------------------------------------
    // Robustness: macOS sends these pseudo-events when it disables our tap.
    // ------------------------------------------------------------------
    if ty == kCGEventTapDisabledByTimeout || ty == kCGEventTapDisabledByUserInput {
        if refcon.is_null() {
            return event;
        }
        let state = &*(refcon as *const TapState);

        // Don't attempt recovery if the user intentionally turned the tap off.
        if !state.enabled.load(Ordering::Relaxed) {
            return event;
        }

        let port = state.mach_port.load(Ordering::Acquire);

        // Attempt a lightweight re-enable first (Wowfunhappy strategy).
        let revived = if !port.is_null() {
            CGEventTapEnable(port, true);
            // Check whether the re-enable actually worked. After a Fast User
            // Switch the Mach port is invalidated and `CGEventTapEnable` is a
            // silent no-op — `CGEventTapIsEnabled` will still return false.
            CGEventTapIsEnabled(port)
        } else {
            false
        };

        if !revived {
            // Port is dead. Schedule a full tap recreation on the main queue.
            // We use `swap` as a test-and-set: if `needs_recreate` was already
            // true a GCD block is already in flight — don't enqueue another.
            if !state.needs_recreate.swap(true, Ordering::AcqRel) {
                let ctx = RECREATE_CTX.load(Ordering::Acquire);
                let fn_usize = RECREATE_FN.load(Ordering::Acquire);
                if !ctx.is_null() && fn_usize != 0 {
                    // Safety: we stored a valid `RecreateFn` via set_recreate_callback.
                    let f: RecreateFn = std::mem::transmute(fn_usize);
                    // _dispatch_main_q is the main-queue object; take its
                    // address and cast — this is the standard Rust/GCD idiom
                    // since dispatch_get_main_queue() is always-inlined.
                    let main_q = &_dispatch_main_q as *const c_void;
                    dispatch_async_f(main_q, ctx, f);
                }
            }
        }

        return event;
    }

    // ------------------------------------------------------------------
    // Normal mouse-button events
    // ------------------------------------------------------------------
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

    // AppDelegate.m:46–62: swallow both edges; fire the swipe only on the
    // configured edge (down or up).
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
