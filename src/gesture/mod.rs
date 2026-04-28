//! Gesture synthesis — ports `External/TouchEvents.c` and the
//! `SBFFakeSwipe` helper from `SideButtonFixer/AppDelegate.m`.

mod private_hid;
mod synth;

pub use synth::{create_event_from_gesture, post_swipe, Direction, GestureInfo};
