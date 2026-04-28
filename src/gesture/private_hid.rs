//! Private/undocumented IOKit HID structures used by the gesture
//! serializer. Direct translation of `External/IOHIDEventData.h` and
//! `External/IOHIDEventTypes.h` (Apple Public Source / SensibleSideButtons).
//!
//! These layouts are NOT public API. They must match the C header bit
//! for bit. Sizes verified against the C definitions:
//!   sizeof(IOHIDEventData)             = 24
//!   sizeof(IOHIDDigitizerEventData)    = 96
//!   sizeof(IOHIDVendorDefinedEventData)= 32  (excluding flexible array)
//!   sizeof(IOHIDSystemQueueElement)    = 24
//!
//! Field names mirror the C source verbatim (camelCase) so that
//! line-by-line porting of `tl_CGEventCreateFromGesture` stays legible.

#![allow(non_snake_case, non_upper_case_globals, dead_code)]

// ---------------------------------------------------------------------------
// Primitive typedefs (IOHIDEventTypes.h:80, <IOKit/IOTypes.h>)
// ---------------------------------------------------------------------------

/// `typedef SInt32 IOFixed;` — 16.16 signed fixed point.
pub type IOFixed = i32;

/// `typedef uint32_t IOHIDEventType;` (IOHIDEventTypes.h:80)
pub type IOHIDEventType = u32;

/// `typedef uint32_t IOHIDSwipeMask;` (IOHIDEventTypes.h:115)
pub type IOHIDSwipeMask = u32;

// ---------------------------------------------------------------------------
// IOHIDEventType enum values (IOHIDEventTypes.h:58–79)
// ---------------------------------------------------------------------------

pub const kIOHIDEventTypeNULL:          IOHIDEventType = 0;
pub const kIOHIDEventTypeVendorDefined: IOHIDEventType = 1;
pub const kIOHIDEventTypeButton:        IOHIDEventType = 2;
pub const kIOHIDEventTypeKeyboard:      IOHIDEventType = 3;
pub const kIOHIDEventTypeTranslation:   IOHIDEventType = 4;
pub const kIOHIDEventTypeRotation:      IOHIDEventType = 5;
pub const kIOHIDEventTypeScroll:        IOHIDEventType = 6;
pub const kIOHIDEventTypeScale:         IOHIDEventType = 7;
pub const kIOHIDEventTypeZoom:          IOHIDEventType = 8;
pub const kIOHIDEventTypeVelocity:      IOHIDEventType = 9;
pub const kIOHIDEventTypeOrientation:   IOHIDEventType = 10;
pub const kIOHIDEventTypeDigitizer:     IOHIDEventType = 11;

// ---------------------------------------------------------------------------
// Swipe directions (IOHIDEventTypes.h:109–114)
//
// Note: the kTLInfoSwipe* constants in TouchEvents.h:35–40 are bit-identical
// to these — the C code uses them interchangeably.
// ---------------------------------------------------------------------------

pub const kIOHIDSwipeUp:    IOHIDSwipeMask = 0x00000001;
pub const kIOHIDSwipeDown:  IOHIDSwipeMask = 0x00000002;
pub const kIOHIDSwipeLeft:  IOHIDSwipeMask = 0x00000004;
pub const kIOHIDSwipeRight: IOHIDSwipeMask = 0x00000008;

// ---------------------------------------------------------------------------
// Digitizer transducer types (IOHIDEventTypes.h:153–159)
// ---------------------------------------------------------------------------

pub const kIOHIDDigitizerTransducerTypeStylus: u32 = 0x20;
pub const kIOHIDDigitizerTransducerTypePuck:   u32 = 0x21;
pub const kIOHIDDigitizerTransducerTypeFinger: u32 = 0x22;
pub const kIOHIDDigitizerTransducerTypeHand:   u32 = 0x23;

// ---------------------------------------------------------------------------
// Event option bits (IOHIDEventTypes.h:202–207, IOHIDEventData.h:57–61)
// ---------------------------------------------------------------------------

pub const kIOHIDEventOptionIsAbsolute:   u32 = 0x00000001;
pub const kIOHIDEventOptionIsCollection: u32 = 0x00000002;
pub const kIOHIDEventOptionPixelUnits:   u32 = 0x00000004;

pub const kIOHIDTransducerRange:  u32 = 0x00010000;
pub const kIOHIDTransducerTouch:  u32 = 0x00020000;
pub const kIOHIDTransducerInvert: u32 = 0x00040000;

// ---------------------------------------------------------------------------
// Digitizer orientation type tag (IOHIDEventData.h:63–68)
// ---------------------------------------------------------------------------

pub const kIOHIDDigitizerOrientationTypeTilt:    u8 = 0;
pub const kIOHIDDigitizerOrientationTypePolar:   u8 = 1;
pub const kIOHIDDigitizerOrientationTypeQuality: u8 = 2;

// ---------------------------------------------------------------------------
// Layout helpers shared by every IOHID event struct
//   (IOHIDEventData.h:28–32, the IOHIDEVENT_BASE macro)
// ---------------------------------------------------------------------------
//
// Concrete layout (64-bit darwin):
//   offset 0 :  u32 size
//   offset 4 :  u32 type
//   offset 8 :  u64 timestamp     (8-aligned, no padding before)
//   offset 16:  u32 options
// `IOHIDEventData` itself is the 24-byte struct above (the trailing 4
// bytes are tail-padding to 8-byte struct alignment).

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct IOHIDEventData {
    pub size:      u32,
    pub r#type:    IOHIDEventType,
    pub timestamp: u64,
    pub options:   u32,
}

// ---------------------------------------------------------------------------
// Axis position helper (IOHIDEventData.h:34–39, IOHIDAXISEVENT_BASE)
//   struct { IOFixed x; IOFixed y; IOFixed z; } position;
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct AxisPosition {
    pub x: IOFixed,
    pub y: IOFixed,
    pub z: IOFixed,
}

// ---------------------------------------------------------------------------
// Digitizer "quality" orientation variant of the union in
// IOHIDDigitizerEventData (IOHIDEventData.h:116–122).
//
// The C union has three variants (tilt / polar / quality), but TouchEvents.c
// always tags as `kIOHIDDigitizerOrientationTypeQuality` and writes only this
// variant. We model the union by its largest member, so the struct ends up
// the same total size as the C version (5 × IOFixed = 20 bytes).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct OrientationQuality {
    pub quality:      IOFixed,
    pub density:      IOFixed,
    pub irregularity: IOFixed,
    pub majorRadius:  IOFixed,
    pub minorRadius:  IOFixed,
}

// ---------------------------------------------------------------------------
// IOHIDDigitizerEventData (IOHIDEventData.h:90–124)
//
// Layout walk (64-bit darwin):
//   IOHIDEVENT_BASE                                            // 0 .. 20
//   IOHIDAXISEVENT_BASE   (3 IOFixed)                          // 20 .. 32
//   transducerIndex u32                                        // 32
//   transducerType  u32                                        // 36
//   identity        u32                                        // 40
//   eventMask       u32                                        // 44
//   childEventMask  u32                                        // 48
//   buttonMask      u32                                        // 52
//   tipPressure     IOFixed                                    // 56
//   barrelPressure  IOFixed                                    // 60
//   twist           IOFixed                                    // 64
//   orientationType u32                                        // 68
//   orientation     union  (5 IOFixed, quality variant)        // 72 .. 92
//   tail-padding to 8-byte struct alignment                    // 92 .. 96
// total size: 96 bytes
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct IOHIDDigitizerEventData {
    // IOHIDEVENT_BASE
    pub size:            u32,
    pub r#type:          IOHIDEventType,
    pub timestamp:       u64,
    pub options:         u32,

    // IOHIDAXISEVENT_BASE
    pub position:        AxisPosition,

    pub transducerIndex: u32,
    pub transducerType:  u32,
    pub identity:        u32,
    pub eventMask:       u32,
    pub childEventMask:  u32,

    pub buttonMask:      u32,
    pub tipPressure:     IOFixed,
    pub barrelPressure:  IOFixed,

    pub twist:           IOFixed,
    pub orientationType: u32,
    pub orientation:     OrientationQuality,
}

// ---------------------------------------------------------------------------
// IOHIDVendorDefinedEventData (IOHIDEventData.h:47–54)
//
//   IOHIDEVENT_BASE
//   uint16_t usagePage
//   uint16_t usage
//   uint32_t version
//   uint32_t length
//   uint8_t  data[0]      // flexible array — NOT modelled here
//
// Layout:
//   0 .. 20 : IOHIDEVENT_BASE
//   20      : usagePage  u16
//   22      : usage      u16
//   24      : version    u32
//   28      : length     u32
//   32      : end-of-fixed-portion (8-aligned)
//
// We size_of::<IOHIDVendorDefinedEventData>() == 32, then append the
// payload bytes manually (see synth.rs).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct IOHIDVendorDefinedEventData {
    pub size:      u32,
    pub r#type:    IOHIDEventType,
    pub timestamp: u64,
    pub options:   u32,

    pub usagePage: u16,
    pub usage:     u16,
    pub version:   u32,
    pub length:    u32,
    // `data[0]` flexible array intentionally omitted; callers allocate
    // `size_of::<Self>() + payload_len` and copy the payload after the
    // header.
}

// ---------------------------------------------------------------------------
// IOHIDSystemQueueElement (IOHIDEventData.h:152–158)
//
//   uint64_t timeStamp
//   uint64_t deviceID
//   uint32_t options
//   uint32_t eventCount
//   IOHIDEventData events[]   // flexible array
//
// 24 bytes (8-aligned). Flexible array omitted; events are appended
// after the struct as raw bytes by synth.rs.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct IOHIDSystemQueueElement {
    pub timeStamp:  u64,
    pub deviceID:   u64,
    pub options:    u32,
    pub eventCount: u32,
}

// ---------------------------------------------------------------------------
// Compile-time sanity asserts on the layouts above. If any of these fail,
// the gesture event will be malformed. (Cheap to keep — they vanish in
// release builds.)
// ---------------------------------------------------------------------------

const _: [(); 24] = [(); std::mem::size_of::<IOHIDEventData>()];
const _: [(); 12] = [(); std::mem::size_of::<AxisPosition>()];
const _: [(); 20] = [(); std::mem::size_of::<OrientationQuality>()];
const _: [(); 96] = [(); std::mem::size_of::<IOHIDDigitizerEventData>()];
const _: [(); 32] = [(); std::mem::size_of::<IOHIDVendorDefinedEventData>()];
const _: [(); 24] = [(); std::mem::size_of::<IOHIDSystemQueueElement>()];
