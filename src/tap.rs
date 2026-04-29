//! Global mouse-button event tap.
//!
//! Robustness layers (strongest to weakest):
//!
//! 1. **Immediate re-enable** (`tap_callback`): on `kCGEventTapDisabledByTimeout`
//!    or `kCGEventTapDisabledByUserInput`, call `CGEventTapEnable` right away.
//!    Handles the common latency-budget case with zero latency.
//!
//! 2. **GCD async recreate** (`tap_callback`): if `CGEventTapIsEnabled` still
//!    returns false after the re-enable call, the Mach port is dead.  Schedule
//!    a full `CGEventTapCreate` on the main queue via `dispatch_async_f`.
//!    Runs asynchronously so there is no reentrancy hazard.
//!
//! 3. **Periodic health check** (`health_check_callback`, every 20 s):
//!    - **Session guard**: uses `CGSessionCopyCurrentDictionary` +
//!      `kCGSessionOnConsoleKey` to detect Fast User Switch.  When this
//!      session is not the active console, stop the tap cleanly.  When
//!      it becomes active again (and `enabled == true`), recreate it.
//!    - **Tap liveness**: checks `CGEventTapIsEnabled` and `needs_recreate`.
//!      If either indicates the tap is dead, recreate.
//!
//! Hot-path discipline: `tap_callback` reads ONLY atomics from `TapState`.
//! No allocation, no locking, no I/O.

#![allow(non_upper_case_globals, non_snake_case)]

use core_foundation_sys::base::{CFAllocatorRef, CFIndex, CFOptionFlags, CFRelease, CFTypeRef};
use core_foundation_sys::date::CFAbsoluteTime;
use core_foundation_sys::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopAddTimer, CFRunLoopGetMain,
    CFRunLoopRemoveSource, CFRunLoopSourceRef, CFRunLoopTimerContext, CFRunLoopTimerCreate,
    CFRunLoopTimerRef,
};
use parking_lot::Mutex;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::gesture::{post_swipe, Direction};

// ---------------------------------------------------------------------------
// FFI — CoreGraphics event tap API + CGSession
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
/// the latency budget (`Timeout`) or at a user-session boundary (`UserInput`).
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

    // CGSession.h -------------------------------------------------------
    // Returns a CFDictionary describing the calling process's GUI login
    // session.  The caller owns the result and must CFRelease it.
    // Returns NULL when no session info is available (treat as active).
    fn CGSessionCopyCurrentDictionary() -> *mut c_void; // CFDictionaryRef
}

// ---------------------------------------------------------------------------
// FFI — CoreFoundation dictionary + boolean helpers
// ---------------------------------------------------------------------------

// CoreFoundation is always linked transitively through CoreGraphics.
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    // CFBooleanRef → primitive bool.  CFBoolean is a 1-byte value.
    fn CFBooleanGetValue(boolean: *const c_void) -> bool;
    fn CFAbsoluteTimeGetCurrent() -> CFAbsoluteTime;
    // Create a CFString from a null-terminated C string.
    // kCGSessionOnConsoleKey is a C macro (CFSTR), not an exported symbol,
    // so we recreate the key string at runtime with this call.
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const std::os::raw::c_char,
        encoding: u32,
    ) -> *const c_void; // CFStringRef
}

// ---------------------------------------------------------------------------
// FFI — GCD (libSystem, always linked)
// ---------------------------------------------------------------------------

// `dispatch_get_main_queue()` is DISPATCH_ALWAYS_INLINE in modern Apple SDKs
// and has no exported symbol.  The underlying global is `_dispatch_main_q`.
// `dispatch_async_f` is a real exported function and links normally.
extern "C" {
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

// Function pointer stored as `usize` for `AtomicUsize` (no `AtomicFnPtr` in std).
// Transmuting is sound on Apple targets where fn ptr == data ptr width.
static RECREATE_CTX: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static RECREATE_FN: AtomicUsize = AtomicUsize::new(0);

/// Register the GCD trampoline called (on the main queue, asynchronously)
/// when the event tap needs full recreation.  `ctx` must stay valid for the
/// process lifetime — pass a `&'static AppState` cast to `*mut c_void`.
/// Call once before `NSApplication::run`.
pub fn set_recreate_callback(ctx: *mut c_void, f: RecreateFn) {
    RECREATE_CTX.store(ctx, Ordering::Release);
    RECREATE_FN.store(f as usize, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Rate-limit state
// ---------------------------------------------------------------------------

// Seconds (CFAbsoluteTime truncated) of the most recent recreation attempt.
// Shared between the GCD path and the health-check timer; both run on the
// main thread, but the atomic keeps reads/writes race-free across the two.
static LAST_RECOVERY_SECS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fn event_mask_bit(t: CGEventType) -> CGEventMask {
    1u64 << (t as u64)
}

/// Record now as the last recovery attempt timestamp.
fn stamp_recovery_time() {
    let secs = unsafe { CFAbsoluteTimeGetCurrent() } as u64;
    LAST_RECOVERY_SECS.store(secs, Ordering::Relaxed);
}

/// True if at least 10 seconds have elapsed since the last recovery attempt.
fn recovery_allowed() -> bool {
    let now = unsafe { CFAbsoluteTimeGetCurrent() } as u64;
    let last = LAST_RECOVERY_SECS.load(Ordering::Relaxed);
    now.saturating_sub(last) >= 10
}

/// True if this process's GUI session is the active console session.
///
/// Uses `CGSessionCopyCurrentDictionary` + the `kCGSSessionOnConsoleKey`
/// string.  `kCGSessionOnConsoleKey` is a C macro (`CFSTR(...)`) with no
/// exported linker symbol, so we build the key string at runtime.
///
/// Returns `true` when the dictionary is unavailable (fail-open: keep
/// the tap running in environments without a proper GUI session).
fn is_on_active_console() -> bool {
    // kCFStringEncodingASCII = 0x0600.  The key string is pure ASCII.
    const CF_ENCODING_ASCII: u32 = 0x0600;

    unsafe {
        let dict = CGSessionCopyCurrentDictionary();
        if dict.is_null() {
            return true; // no session info — assume active
        }
        // kCGSessionOnConsoleKey expands to CFSTR("kCGSSessionOnConsoleKey").
        let key = CFStringCreateWithCString(
            ptr::null(), // kCFAllocatorDefault
            b"kCGSSessionOnConsoleKey\0".as_ptr() as *const _,
            CF_ENCODING_ASCII,
        );
        let on_console = if key.is_null() {
            true // allocation failure — fail-open
        } else {
            let val = CFDictionaryGetValue(dict, key);
            let result = !val.is_null() && CFBooleanGetValue(val);
            CFRelease(key as CFTypeRef);
            result
        };
        CFRelease(dict as CFTypeRef);
        on_console
    }
}

// ---------------------------------------------------------------------------
// TapState — atomic snapshot read by the callback hot path
// ---------------------------------------------------------------------------

/// All fields are atomics; the CGEventTap callback can read them without
/// locking.  All writes come from menu toggle handlers on the main thread.
pub struct TapState {
    /// Whether to actively remap side buttons.
    pub enabled: AtomicBool,
    /// `true` = fire swipe on mouse-down, `false` = on mouse-up.
    pub mouse_down: AtomicBool,
    /// Swap which physical button maps to back vs. forward.
    pub swap_buttons: AtomicBool,
    /// `CFMachPortRef` of the running tap, or null.
    /// Written by `Tap::new` / `Tap::drop`; read by the callback.
    pub mach_port: AtomicPtr<c_void>,
    /// Set by the callback when the port is dead and needs recreation.
    /// Cleared by `start_tap`.  Used as a one-shot flag via `swap`.
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
    state: Arc<TapState>,
    mach_port: CFMachPortRef,
    run_loop_source: CFRunLoopSourceRef,
}

// Used on the main thread only; mark Send so `Mutex<Option<Tap>>` compiles.
unsafe impl Send for Tap {}
unsafe impl Sync for Tap {}

impl Tap {
    /// Create a new (disabled) CGEventTap.  Returns `Err` when
    /// `CGEventTapCreate` returns null (Accessibility not granted, or
    /// this is not the active console session).
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
            CGEventTapEnable(port, false); // created disabled; caller enables

            // Publish the port so the callback can reach it without a `Tap` ref.
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
        // Only clear `mach_port` if it still points to our port.
        // `start_tap` installs the new port before dropping the old Tap;
        // compare_exchange prevents clobbering the freshly-stored new port.
        let _ = self.state.mach_port.compare_exchange(
            self.mach_port,
            ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
}

// ---------------------------------------------------------------------------
// Public lifecycle functions
// ---------------------------------------------------------------------------

/// Destroy the current tap: disable, remove run-loop source, release port.
/// `enabled` is left unchanged — the hardware plumbing is torn down but
/// the user's preference is preserved.
pub fn stop_tap(live_tap: &Mutex<Option<Tap>>) {
    *live_tap.lock() = None;
}

/// Create a fresh event tap, replacing any existing one.
///
/// This is the only correct recovery path after Fast User Switch: a tap
/// whose Mach port has been invalidated cannot be revived with
/// `CGEventTapEnable` — a new `CGEventTapCreate` is required.
///
/// Returns `true` on success; `false` if `CGEventTapCreate` fails
/// (Accessibility not granted, or session is not the active console).
pub fn start_tap(state: Arc<TapState>, live_tap: &Mutex<Option<Tap>>, enabled: bool) -> bool {
    state.needs_recreate.store(false, Ordering::Release);
    // Stamp time so a pending GCD dispatch or the next health-check tick
    // doesn't immediately pile on after this call.
    stamp_recovery_time();

    match Tap::new(Arc::clone(&state)) {
        Ok(tap) => {
            if enabled {
                tap.set_enabled(true);
            }
            // Assignment drops the old Tap first (compare_exchange in its Drop
            // avoids clobbering the new port already stored by Tap::new).
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
// Periodic health-check timer
// ---------------------------------------------------------------------------

/// Context passed as `info` to the CFRunLoopTimer callback.
/// Both pointers are into the leaked `AppState` and are valid forever.
struct HealthCtx {
    state: *const TapState,
    live_tap: *const Mutex<Option<Tap>>,
}

// All access is on the main thread; safe to mark Send + Sync.
unsafe impl Send for HealthCtx {}
unsafe impl Sync for HealthCtx {}

/// CFRunLoopTimer callback — fires every 20 seconds on the main run loop.
///
/// **Stage 1 — Session guard**: reads `CGSessionCopyCurrentDictionary` /
/// `kCGSessionOnConsoleKey`.  When this session is not the active console
/// (another user switched in via Fast User Switch), the tap is stopped so it
/// doesn't fight with the other user's session.  When the session becomes
/// active again (next tick or later) the tap is recreated automatically.
///
/// **Stage 2 — Tap health**: when on the active console, checks
/// `CGEventTapIsEnabled` and the `needs_recreate` flag.  If either is bad,
/// recreates the tap via the registered trampoline.
extern "C" fn health_check_callback(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    // SAFETY: `info` is a `&'static HealthCtx` allocated by
    // `install_health_check_timer` and never freed.
    let hctx = unsafe { &*(info as *const HealthCtx) };
    let state = unsafe { &*hctx.state };
    let live_tap = unsafe { &*hctx.live_tap };

    // ------------------------------------------------------------------
    // Stage 1: session guard
    // ------------------------------------------------------------------
    if !is_on_active_console() {
        // This session is in the background — another user is active.
        // Stop the tap so it doesn't interfere.  It will be recreated
        // automatically on the next tick after this session returns.
        if live_tap.lock().is_some() {
            eprintln!("[ssb] health check: session inactive — stopping tap");
            stop_tap(live_tap);
        }
        return;
    }

    // ------------------------------------------------------------------
    // Stage 2: tap health
    // ------------------------------------------------------------------
    if !state.enabled.load(Ordering::Relaxed) {
        return; // user has intentionally disabled remapping
    }

    let port = state.mach_port.load(Ordering::Acquire);
    let tap_alive = !port.is_null() && unsafe { CGEventTapIsEnabled(port) };
    let flagged = state.needs_recreate.load(Ordering::Acquire);

    if tap_alive && !flagged {
        return; // tap is healthy
    }

    // Rate limit: at most one recreation attempt per 10 seconds.
    if !recovery_allowed() {
        return;
    }

    eprintln!("[ssb] health check: tap dead (alive={tap_alive} flagged={flagged}) — recreating");
    stamp_recovery_time();

    // Call the trampoline directly — the CFRunLoopTimer fires on the same
    // main thread, so no GCD dispatch is needed and there is no reentrancy.
    let recreate_ctx = RECREATE_CTX.load(Ordering::Acquire);
    let fn_usize = RECREATE_FN.load(Ordering::Acquire);
    if !recreate_ctx.is_null() && fn_usize != 0 {
        // SAFETY: stored via `set_recreate_callback` as a valid `RecreateFn`.
        let f: RecreateFn = unsafe { std::mem::transmute(fn_usize) };
        unsafe { f(recreate_ctx) };
    }
}

/// Install a repeating CFRunLoopTimer that checks tap health every 20 seconds.
///
/// Both pointer arguments must be valid for the process lifetime:
/// - `state_ptr`  — `Arc::as_ptr(&app_state.tap_state)`
/// - `live_tap`   — `&app_state.live_tap as *const _`
///
/// Call after `set_recreate_callback` and after the initial tap creation.
pub fn install_health_check_timer(
    state_ptr: *const TapState,
    live_tap: *const Mutex<Option<Tap>>,
) {
    // Heap-allocate and leak the context; CFRunLoopTimer holds only a raw
    // `*mut c_void` — lifetime management is our responsibility.
    let hctx = Box::leak(Box::new(HealthCtx { state: state_ptr, live_tap }));

    unsafe {
        let fire_date = CFAbsoluteTimeGetCurrent() + 20.0;
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: hctx as *mut HealthCtx as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };
        let timer = CFRunLoopTimerCreate(
            ptr::null() as CFAllocatorRef, // kCFAllocatorDefault
            fire_date,                     // first fire: 20 s from now
            20.0,                          // repeat interval: 20 s
            0 as CFOptionFlags,            // flags: reserved
            0 as CFIndex,                  // order: default
            health_check_callback,
            &mut ctx,
        );
        // CFRunLoopAddTimer retains the timer; release our initial reference.
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopCommonModes);
        CFRelease(timer as CFTypeRef);
    }
}

// ---------------------------------------------------------------------------
// tap_callback — mirrors `SBFMouseCallback` (AppDelegate.m:39–63)
// ---------------------------------------------------------------------------

unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: CGEventRef,
    refcon: *mut c_void,
) -> CGEventRef {
    // ------------------------------------------------------------------
    // Robustness: macOS sends these pseudo-events when it disables the tap.
    // ------------------------------------------------------------------
    if ty == kCGEventTapDisabledByTimeout || ty == kCGEventTapDisabledByUserInput {
        if refcon.is_null() {
            return event;
        }
        let state = &*(refcon as *const TapState);

        if !state.enabled.load(Ordering::Relaxed) {
            return event; // user has turned remapping off — don't recover
        }

        let port = state.mach_port.load(Ordering::Acquire);

        // Layer 1: lightweight re-enable (handles the common timeout case).
        let revived = if !port.is_null() {
            CGEventTapEnable(port, true);
            // Verify it actually worked.  After Fast User Switch the port is
            // dead and CGEventTapEnable is a silent no-op.
            CGEventTapIsEnabled(port)
        } else {
            false
        };

        if !revived {
            // Layer 2: port is dead — schedule a full CGEventTapCreate on
            // the main queue.  Use swap as a test-and-set so only one GCD
            // block is in flight at a time.
            if !state.needs_recreate.swap(true, Ordering::AcqRel) {
                let ctx = RECREATE_CTX.load(Ordering::Acquire);
                let fn_usize = RECREATE_FN.load(Ordering::Acquire);
                if !ctx.is_null() && fn_usize != 0 {
                    let f: RecreateFn = std::mem::transmute(fn_usize);
                    let main_q = &_dispatch_main_q as *const c_void;
                    stamp_recovery_time();
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

    // AppDelegate.m:46–62: swallow both edges; fire swipe only on the
    // configured edge so each physical click produces exactly one swipe.
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
