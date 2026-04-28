//! Gesture-event synthesis. Direct port of `External/TouchEvents.c`.
//!
//! The flow is:
//!   1. Build an empty CGEvent of the (private) gesture type 29.
//!   2. Serialize it via `CGEventCreateData` to get a starter byte blob.
//!   3. Strip the trailing 24 bytes that CG appended (the gesture-fields
//!      tail) — we'll rewrite them below with our own values.
//!   4. Append an IOHID event-queue header, an IOHIDSystemQueueElement,
//!      a parent IOHIDDigitizerEventData, child touch records (none in
//!      our use), and an IOHIDVendorDefinedEventData block.
//!   5. Append the gesture-field tail (subtype, phase, swipe direction,
//!      magic floats), each with a big-endian header `[u16 count][u8 type]
//!      [u8 field-id]` followed by big-endian payload.
//!   6. Re-inflate via `CGEventCreateFromData`.
//!
//! The numbered field IDs (0x6E, 0x6F, 0x70, 0x73, 0x84, 0x85, 0x8B, 0x8C)
//! and the constants 29 (event type), 256 (flags), 0xFF00/0x1777 (vendor
//! usage) are reverse-engineered Apple internals. Treat as opaque.
//!
//! Cross-references throughout point to lines in
//! `sensible-side-buttons/External/TouchEvents.c`.

#![allow(non_snake_case, non_upper_case_globals, dead_code)]

use core_foundation_sys::base::{kCFAllocatorDefault, CFAllocatorRef, CFRelease, CFTypeRef};
use core_foundation_sys::data::{CFDataCreate, CFDataGetBytePtr, CFDataGetLength, CFDataRef};
use std::os::raw::c_void;
use std::{mem, ptr};

use super::private_hid::*;

// ---------------------------------------------------------------------------
// CGEvent FFI — declared inline rather than via the `core-graphics` crate
// because we need a tiny, stable surface and want byte-level control.
// ---------------------------------------------------------------------------

type CGEventRef       = *mut c_void;
type CGEventSourceRef = *mut c_void;
type CGEventType      = u32;
type CGEventFlags     = u64;
type CGEventTimestamp = u64;
type CGEventTapLocation = u32;

/// `kCGHIDEventTap` — post into the HID event stream so all listeners see it.
pub const kCGHIDEventTap: CGEventTapLocation = 0;

/// Undocumented CGEventType for trackpad gesture events.
/// `NSEventTypeGesture = 29` in AppKit. (TouchEvents.c:305)
const kCGEventGesture: CGEventType = 29;

/// "Magic" CGEventFlags value from the original. (TouchEvents.c:306)
const kCGEventGestureFlags: CGEventFlags = 256;

/// Number of trailing bytes `CGEventCreateData` appends for gesture events
/// that we delete and rewrite. (TouchEvents.c:316–318)
const TRAILING_GESTURE_BYTES: usize = 24;

// Vendor-defined record magic numbers. (TouchEvents.c:297–299)
const VENDOR_USAGE_PAGE: u16 = 0xFF00;
const VENDOR_USAGE:      u16 = 0x1777;
const VENDOR_VERSION:    u32 = 1;
const VENDOR_PAYLOAD_LEN: usize = 40;

// IOHID event-queue header magic byte pair. (TouchEvents.c:325)
const IOHID_QUEUE_HDR_MAGIC: [u8; 2] = [0x10, 0x6D];

// Wire-format type tags for `appendField` (TouchEvents.c:71–103).
//   0x00 — uint64 split big-endian (lo32 then hi32)
//   0x10 — uint8
//   0x40 — uint32 big-endian
//   0xC0 — float32 big-endian
const FIELD_WIRE_U64: u8 = 0x00;
const FIELD_WIRE_U8:  u8 = 0x10;
const FIELD_WIRE_U32: u8 = 0x40;
const FIELD_WIRE_F32: u8 = 0xC0;

// kTLInfo* gesture subtype constants (TouchEvents.h:24–33).
pub const kTLInfoSubtypeRotate:        u32 = 0x05;
pub const kTLInfoSubtypeSub6:          u32 = 0x06;
pub const kTLInfoSubtypeMagnify:       u32 = 0x08;
pub const kTLInfoSubtypeGesture:       u32 = 0x0B;
pub const kTLInfoSubtypeSwipe:         u32 = 0x10;
pub const kTLInfoSubtypeBeginGesture:  u32 = 0x3D;
pub const kTLInfoSubtypeEndGesture:    u32 = 0x3E;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventCreateData(allocator: CFAllocatorRef, event: CGEventRef) -> CFDataRef;
    fn CGEventCreateFromData(allocator: CFAllocatorRef, data: CFDataRef) -> CGEventRef;
    fn CGEventSetType(event: CGEventRef, ty: CGEventType);
    fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);
    fn CGEventSetTimestamp(event: CGEventRef, timestamp: CGEventTimestamp);
    fn CGEventPost(tap: CGEventTapLocation, event: CGEventRef);
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Logical swipe direction. Values map to `kTLInfoSwipe*` /
/// `kIOHIDSwipe*`. (TouchEvents.h:35–40)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    fn as_swipe_mask(self) -> u32 {
        match self {
            Direction::Up    => kIOHIDSwipeUp,
            Direction::Down  => kIOHIDSwipeDown,
            Direction::Left  => kIOHIDSwipeLeft,
            Direction::Right => kIOHIDSwipeRight,
        }
    }
}

/// Mirrors the input dictionary that `tl_CGEventCreateFromGesture` reads
/// (TouchEvents.c:14–22, kTLInfoKey* keys).
#[derive(Debug, Default, Clone)]
pub struct GestureInfo {
    /// `kTLInfoKeyGestureSubtype` — one of `kTLInfoSubtype*`.
    pub subtype: u32,
    /// `kTLInfoKeyGesturePhase` — IOHIDEventPhaseBits value
    /// (1 = begin, 4 = ended).
    pub phase: u32,
    /// `kTLInfoKeySwipeDirection` — only consulted when subtype == swipe.
    pub swipe_direction: Option<u32>,
    /// `kTLInfoKeyMagnification` — only when subtype == magnify.
    pub magnification: Option<f32>,
    /// `kTLInfoKeyRotation` — degrees, only when subtype == rotate.
    pub rotation: Option<f32>,
    /// `kTLInfoKeyNextSubtype` — only for begin/end gesture wrappers.
    pub next_subtype: Option<u32>,
    /// `kTLInfoKeyTimestamp` — falls back to `clock_gettime_nsec_np`.
    pub timestamp: Option<u64>,
    /// `kTLInfoKeyDeviceID` — defaults to 0.
    pub device_id: u64,
}

/// High-level entry point: post a complete back/forward 3-finger swipe.
/// Equivalent to `SBFFakeSwipe` in `AppDelegate.m:28–37`.
pub fn post_swipe(direction: Direction) {
    // The trick is two CGEvents in sequence:
    //   1) phase = 1 (begin)        — no direction yet
    //   2) phase = 4 (ended), with the swipe direction
    // macOS treats this pair as a complete navigation swipe.
    let begin = GestureInfo {
        subtype: kTLInfoSubtypeSwipe,
        phase: 1,
        ..GestureInfo::default()
    };
    let end = GestureInfo {
        subtype: kTLInfoSubtypeSwipe,
        phase: 4,
        swipe_direction: Some(direction.as_swipe_mask()),
        ..GestureInfo::default()
    };

    unsafe {
        let e1 = create_event_from_gesture(&begin);
        let e2 = create_event_from_gesture(&end);

        if !e1.is_null() {
            CGEventPost(kCGHIDEventTap, e1);
            CFRelease(e1 as CFTypeRef);
        }
        if !e2.is_null() {
            CGEventPost(kCGHIDEventTap, e2);
            CFRelease(e2 as CFTypeRef);
        }
    }
}

// ---------------------------------------------------------------------------
// Wire-format helpers — port of TouchEvents.c:61–117
// ---------------------------------------------------------------------------

/// Write `[u16 count BE][u8 type][u8 field]` header.
/// (TouchEvents.c:61–67, `appendHeader`)
fn append_header(buf: &mut Vec<u8>, field: u8, ty: u8, count: u16) {
    buf.extend_from_slice(&count.to_be_bytes());
    buf.push(ty);
    buf.push(field);
}

/// (TouchEvents.c:106–111, `appendIntegerField`)
fn append_integer_field(buf: &mut Vec<u8>, field: u8, value: u32) {
    append_header(buf, field, FIELD_WIRE_U32, 1);
    buf.extend_from_slice(&value.to_be_bytes());
}

/// (TouchEvents.c:113–117, `appendFloatField`)
fn append_float_field(buf: &mut Vec<u8>, field: u8, value: f32) {
    append_header(buf, field, FIELD_WIRE_F32, 1);
    buf.extend_from_slice(&value.to_be_bytes());
}

/// (TouchEvents.c:69–104, `appendField`)
///
/// Generic field appender. Unused by the swipe code path — kept for
/// faithfulness to the C source and for future gesture types.
#[allow(dead_code)]
fn append_field(buf: &mut Vec<u8>, field: u8, ty: u8, values: &[FieldValue]) {
    append_header(buf, field, ty, values.len() as u16);
    for v in values {
        match (ty, v) {
            (FIELD_WIRE_U64, FieldValue::U64(x)) => {
                // TouchEvents.c:72–82 — note the unusual lo32/hi32 order:
                // the low 32 bits come first (still big-endian internally).
                let lo = (*x as u32).to_be_bytes();
                let hi = ((*x >> 32) as u32).to_be_bytes();
                buf.extend_from_slice(&lo);
                buf.extend_from_slice(&hi);
            }
            (FIELD_WIRE_U8, FieldValue::U8(x)) => buf.push(*x),
            (FIELD_WIRE_U32, FieldValue::U32(x)) => buf.extend_from_slice(&x.to_be_bytes()),
            (FIELD_WIRE_F32, FieldValue::F32(x)) => buf.extend_from_slice(&x.to_be_bytes()),
            _ => unreachable!("appendField: type/value mismatch"),
        }
    }
}

#[allow(dead_code)]
pub enum FieldValue {
    U64(u64),
    U32(u32),
    U8(u8),
    F32(f32),
}

// ---------------------------------------------------------------------------
// Uptime helper — replaces `tl_uptime()` (TouchEvents.c:51–54).
//
// The C version goes through `AbsoluteToNanoseconds(UpTime())`. On modern
// macOS, `clock_gettime_nsec_np(CLOCK_UPTIME_RAW)` returns nanoseconds
// since boot directly — semantically identical (same monotonic uptime
// clock, no wall-clock leaps).
// ---------------------------------------------------------------------------

fn uptime_nanoseconds() -> u64 {
    // `CLOCK_UPTIME_RAW` is the macOS `_np` clock id whose
    // `clock_gettime_nsec_np` reading equals `AbsoluteToNanoseconds(UpTime())`.
    // Not all libc crate versions re-export it, so declare directly.
    const CLOCK_UPTIME_RAW: u32 = 8;
    extern "C" {
        fn clock_gettime_nsec_np(clk_id: u32) -> u64;
    }
    unsafe { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }
}

// ---------------------------------------------------------------------------
// The big one — port of `tl_CGEventCreateFromGesture` (TouchEvents.c:199–402)
// ---------------------------------------------------------------------------

/// Builds a synthesized gesture CGEvent. Ownership: returns a `CGEventRef`
/// with retain count +1, identical to the C convention. Caller must
/// `CFRelease` it.
///
/// This skeleton ignores the `touches` argument the C version takes
/// (`CFArrayRef touches`) — SensibleSideButtons always passes an empty
/// array (`AppDelegate.m:29–30`). Adding touch support would mean
/// allowing per-touch IOHIDDigitizerEventData records to be appended
/// after the parent.
///
/// # Safety
/// Calls CoreGraphics C APIs. The returned `CGEventRef` must be released
/// by the caller (or passed to `CGEventPost`, which consumes it).
pub unsafe fn create_event_from_gesture(info: &GestureInfo) -> CGEventRef {
    let timestamp = info.timestamp.unwrap_or_else(uptime_nanoseconds);

    // -------------------------------------------------------------------
    // (TouchEvents.c:222–227) Parent IOHIDDigitizerEventData.
    // -------------------------------------------------------------------
    let mut parent: IOHIDDigitizerEventData = mem::zeroed();
    parent.size            = mem::size_of::<IOHIDDigitizerEventData>() as u32;
    parent.r#type          = kIOHIDEventTypeDigitizer;
    parent.timestamp       = timestamp;
    parent.options         = kIOHIDEventOptionIsCollection;
    parent.transducerType  = kIOHIDDigitizerTransducerTypeHand;
    // Touch averaging (TouchEvents.c:230–282) is a no-op when `touches`
    // is empty — parent.position stays zero.

    let num_touches: u32 = 0;

    // -------------------------------------------------------------------
    // (TouchEvents.c:284–301) Vendor-defined record.
    //
    // Layout: 32-byte fixed header followed by VENDOR_PAYLOAD_LEN bytes
    // of payload. The first 8 payload bytes hold the device ID in
    // little-endian; the remaining 32 bytes are zero.
    // -------------------------------------------------------------------
    let vendor_data_size = mem::size_of::<IOHIDVendorDefinedEventData>() + VENDOR_PAYLOAD_LEN;
    let mut vendor_buf = vec![0u8; vendor_data_size];
    {
        let hdr = vendor_buf.as_mut_ptr() as *mut IOHIDVendorDefinedEventData;
        (*hdr).size      = vendor_data_size as u32;
        (*hdr).r#type    = kIOHIDEventTypeVendorDefined;
        (*hdr).timestamp = 0;
        (*hdr).options   = 0;
        (*hdr).usagePage = VENDOR_USAGE_PAGE;
        (*hdr).usage     = VENDOR_USAGE;
        (*hdr).version   = VENDOR_VERSION;
        (*hdr).length    = VENDOR_PAYLOAD_LEN as u32;
    }
    let payload_off = mem::size_of::<IOHIDVendorDefinedEventData>();
    vendor_buf[payload_off..payload_off + 8]
        .copy_from_slice(&info.device_id.to_le_bytes());

    // -------------------------------------------------------------------
    // (TouchEvents.c:303–313) Create proto event, snapshot to bytes.
    // -------------------------------------------------------------------
    let proto = CGEventCreate(ptr::null_mut());
    if proto.is_null() {
        return ptr::null_mut();
    }
    CGEventSetType(proto, kCGEventGesture);
    CGEventSetFlags(proto, kCGEventGestureFlags);
    CGEventSetTimestamp(proto, timestamp);

    let base_data = CGEventCreateData(kCFAllocatorDefault, proto);
    if base_data.is_null() {
        CFRelease(proto as CFTypeRef);
        return ptr::null_mut();
    }
    let base_len = CFDataGetLength(base_data) as usize;
    let base_ptr = CFDataGetBytePtr(base_data);
    let mut gesture_data: Vec<u8> = std::slice::from_raw_parts(base_ptr, base_len).to_vec();
    CFRelease(base_data as CFTypeRef);
    CFRelease(proto as CFTypeRef);

    // -------------------------------------------------------------------
    // (TouchEvents.c:315–318) Strip the gesture-tail bytes that
    // CGEventCreateData added so we can rebuild them ourselves.
    // -------------------------------------------------------------------
    if gesture_data.len() >= TRAILING_GESTURE_BYTES {
        let new_len = gesture_data.len() - TRAILING_GESTURE_BYTES;
        gesture_data.truncate(new_len);
    }

    // -------------------------------------------------------------------
    // (TouchEvents.c:320–325) IOHID event-queue header for the
    // upcoming queue element.
    //   [u16 BE total_size][0x10][0x6D]
    // -------------------------------------------------------------------
    let total_size: u16 = (mem::size_of::<IOHIDSystemQueueElement>()
        + vendor_data_size
        + (num_touches as usize + 1) * mem::size_of::<IOHIDDigitizerEventData>())
        as u16;
    gesture_data.extend_from_slice(&total_size.to_be_bytes());
    gesture_data.extend_from_slice(&IOHID_QUEUE_HDR_MAGIC);

    // -------------------------------------------------------------------
    // (TouchEvents.c:327–332) Queue-element collection header.
    // Appended raw, host-endian, like the C version.
    // -------------------------------------------------------------------
    let queue_element = IOHIDSystemQueueElement {
        timeStamp:  timestamp,
        deviceID:   0,
        options:    parent.options,
        eventCount: num_touches + 2,
    };
    let qe_bytes = std::slice::from_raw_parts(
        &queue_element as *const _ as *const u8,
        mem::size_of::<IOHIDSystemQueueElement>(),
    );
    gesture_data.extend_from_slice(qe_bytes);

    // -------------------------------------------------------------------
    // (TouchEvents.c:334–337) Parent digitizer event + per-touch records.
    // We have zero touches, so we only write the parent.
    // -------------------------------------------------------------------
    let parent_bytes = std::slice::from_raw_parts(
        &parent as *const _ as *const u8,
        parent.size as usize,
    );
    gesture_data.extend_from_slice(parent_bytes);
    // (no touches to append)

    // -------------------------------------------------------------------
    // (TouchEvents.c:339–341) Vendor record block.
    // -------------------------------------------------------------------
    gesture_data.extend_from_slice(&vendor_buf);

    // -------------------------------------------------------------------
    // (TouchEvents.c:343–351) Three integer fields: gesture subtype, plus
    // two zero "magic" fields whose meaning is undocumented.
    // -------------------------------------------------------------------
    let gesture_subtype = if info.subtype == 0 {
        kTLInfoSubtypeGesture
    } else {
        info.subtype
    };
    append_integer_field(&mut gesture_data, 0x6E, gesture_subtype);
    append_integer_field(&mut gesture_data, 0x6F, 0); // magic
    append_integer_field(&mut gesture_data, 0x70, 0); // magic

    // -------------------------------------------------------------------
    // (TouchEvents.c:353–359) Phase + magic field 0x85.
    // -------------------------------------------------------------------
    append_integer_field(&mut gesture_data, 0x84, info.phase);
    append_integer_field(&mut gesture_data, 0x85, 0); // magic?

    // -------------------------------------------------------------------
    // (TouchEvents.c:361–395) Subtype-specific field.
    // -------------------------------------------------------------------
    match gesture_subtype {
        s if s == kTLInfoSubtypeMagnify => {
            append_float_field(&mut gesture_data, 0x71, info.magnification.unwrap_or(0.0));
        }
        s if s == kTLInfoSubtypeRotate => {
            append_float_field(&mut gesture_data, 0x72, info.rotation.unwrap_or(0.0));
        }
        s if s == kTLInfoSubtypeSwipe => {
            append_integer_field(&mut gesture_data, 0x73, info.swipe_direction.unwrap_or(0));
        }
        s if s == kTLInfoSubtypeBeginGesture || s == kTLInfoSubtypeEndGesture => {
            append_integer_field(&mut gesture_data, 0x75, info.next_subtype.unwrap_or(0));
        }
        _ => {} // other subtypes carry no extra payload
    }

    // -------------------------------------------------------------------
    // (TouchEvents.c:397–398) Two trailing magic floats.
    // -------------------------------------------------------------------
    append_float_field(&mut gesture_data, 0x8B, 0.0);
    append_float_field(&mut gesture_data, 0x8C, 0.0);

    // -------------------------------------------------------------------
    // (TouchEvents.c:400–402) Re-inflate to a CGEvent.
    // -------------------------------------------------------------------
    let cfdata = CFDataCreate(
        kCFAllocatorDefault,
        gesture_data.as_ptr(),
        gesture_data.len() as core_foundation_sys::base::CFIndex,
    );
    if cfdata.is_null() {
        return ptr::null_mut();
    }
    let synth_event = CGEventCreateFromData(kCFAllocatorDefault, cfdata);
    CFRelease(cfdata as CFTypeRef);

    synth_event
}
