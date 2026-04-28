//! Equivalent of `sensible-side-buttons/SwipeSimulator/main.m`.
//!
//! Posts a single 3-finger swipe event and exits. Useful for verifying
//! the gesture serializer in isolation:
//!
//! ```text
//! cargo run --example swipe_simulator              # left (default)
//! cargo run --example swipe_simulator -- left
//! cargo run --example swipe_simulator -- right
//! cargo run --example swipe_simulator -- up
//! cargo run --example swipe_simulator -- down
//! ```
//!
//! Run with Safari open and a back-history loaded; a left swipe should
//! navigate back. NOTE: like the original, this needs the Accessibility
//! permission for the parent process / shell to actually take effect.

use ssb::gesture::{post_swipe, Direction};
use std::{env, process, thread, time::Duration};

fn main() {
    let arg = env::args().nth(1);
    let dir = match arg.as_deref() {
        None | Some("left") => Direction::Left,
        Some("right")       => Direction::Right,
        Some("up")          => Direction::Up,
        Some("down")        => Direction::Down,
        Some(other) => {
            eprintln!("unknown direction: {other:?} (expected left|right|up|down)");
            process::exit(2);
        }
    };

    eprintln!("posting swipe: {dir:?}");
    post_swipe(dir);

    // The original does `usleep(1000000/128)` ≈ 7.8 ms before exit so the
    // posted events have a chance to drain through the event loop. Keep
    // the same hold so behaviour matches the reference.
    // (SwipeSimulator/main.m:60)
    thread::sleep(Duration::from_micros(1_000_000 / 128));
}
