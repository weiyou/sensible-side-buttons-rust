# SensibleSideButtons (Rust)

> System-wide back/forward navigation for M4/M5 mouse buttons on macOS — a complete Rust rewrite.

[![Build](https://img.shields.io/badge/build-passing-brightgreen)](#build-from-source)
[![macOS](https://img.shields.io/badge/macOS-13%2B-blue)](https://www.apple.com/macos/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](#license)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)](https://www.rust-lang.org/)

macOS handles Magic Mouse and trackpad gestures natively, but third-party mice
with M4/M5 (back/forward) buttons are left out — those buttons do nothing by
default.  **SensibleSideButtons** fixes that by intercepting M4/M5 events via
a `CGEventTap` and synthesizing the same private 3-finger swipe gesture that
Mission Control uses, making them work everywhere: Safari, Chrome, Finder,
Xcode, every app, consistently.

This is a ground-up Rust rewrite of
[Alexei Baboulevitch's original SensibleSideButtons](https://github.com/archagon/sensible-side-buttons),
incorporating robustness improvements from
[Wowfunhappy's fork](https://github.com/Wowfunhappy/sensible-side-buttons).

---

## Features

- **Universal back/forward** — works in every macOS application, not just browsers
- **Zero CPU at idle** — the event tap fires only on button presses; the menu bar icon consumes nothing
- **Three settings**, each toggle-able from the menu:
  - **Enabled** — master on/off switch
  - **Trigger on Mouse Down** — snappy response (fires on press, not release)
  - **Swap Buttons** — reverse which physical button is back vs forward
- **Persisted settings** — preferences survive reboots (`~/Library/Application Support/SensibleSideButtons/settings.toml`)
- **Robustness** — re-enables the event tap automatically if macOS suspends it for exceeding its latency budget (Wowfunhappy improvement)
- **Clean Accessibility flow** — opens the right System Settings pane on first run

---

## Installation

### Install Rust

The build needs **Rust 1.75 or newer** and the Xcode Command Line Tools (they supply the macOS linker and SDK headers).

```bash
# 1. Xcode Command Line Tools
xcode-select --install

# 2. Rust via rustup (https://rustup.rs). Accept the defaults;
#    the stable toolchain is newer than 1.75.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 3. Load cargo into this shell (new terminals pick it up automatically)
. "$HOME/.cargo/env"

# 4. Confirm
rustc --version   # 1.75.0 or newer
cargo --version
```

If Rust is already installed, bring the stable toolchain up to date:

```bash
rustup update stable
```

### Option A — Build and run directly (development)

```bash
git clone https://github.com/weiyou/sensible-side-buttons-rust
cd sensible-side-buttons-rust
cargo run --release
```

### Option B — Build a distributable `.app` bundle

```bash
./scripts/bundle.sh
# Produces SensibleSideButtons.app in the current directory.
# Copy it to /Applications:
cp -r SensibleSideButtons.app /Applications/
open /Applications/SensibleSideButtons.app
```

### Option C — `cargo-bundle` (automated `.app` generation)

```bash
cargo install cargo-bundle
cargo bundle --release
# Output: target/release/bundle/osx/SensibleSideButtons.app
```

---

## Granting Accessibility Permission

The app **requires Accessibility access** to intercept mouse-button events.

On first launch the app will:
1. Show macOS's built-in "SensibleSideButtons would like to control this computer" prompt.
2. Automatically open **System Settings → Privacy & Security → Accessibility** if access is not yet granted.

Steps:
1. Open **System Settings → Privacy & Security → Accessibility**.
2. Click the `+` button and add `SensibleSideButtons.app` (or the `sensible-side-buttons` binary if running from `cargo run`).
3. Toggle the switch on.
4. Relaunch the app.

> **Note:** Every time you replace the binary (new build), macOS revokes the
> permission. Re-add it after each update.

---

## Usage

After launch a small icon (`⇆`) appears in the menu bar.

| Menu item | Effect |
|---|---|
| **Enabled** (checkmark) | Toggle remapping on/off |
| **Trigger on Mouse Down** | Fire swipe on button press (vs. release) |
| **Swap Buttons** | Reverse back/forward assignment |
| **Quit** (`⌘Q`) | Exit the app |

The icon changes to `↮` when remapping is disabled.

---

## Build from Source

**Requirements:**
- macOS 13 Ventura or later
- Rust 1.75+ — see [Install Rust](#install-rust)
- Xcode Command Line Tools (`xcode-select --install`)

```bash
# Debug build (development):
cargo build

# Release build (optimised, stripped):
cargo build --release

# Test the gesture engine standalone:
cargo run --example swipe_simulator -- left
cargo run --example swipe_simulator -- right

# Lint:
cargo clippy

# Format:
cargo fmt
```

---

## Project Structure

```
src/
  main.rs            — NSApplication setup, run loop
  lib.rs             — AppState; module declarations
  settings.rs        — Persisted preferences (serde + TOML)
  permissions.rs     — AXIsProcessTrustedWithOptions wrapper
  tap.rs             — CGEventTap: M4/M5 intercept → swipe
  gesture/
    mod.rs           — post_swipe(Direction) public API
    synth.rs         — 3-finger swipe serialisation (port of TouchEvents.c)
    private_hid.rs   — Private IOKit/HID structs with compile-time size asserts
  ui/
    mod.rs
    status_item.rs   — NSStatusItem with enable/disable icon
    menu.rs          — NSMenu with toggle actions (objc2 define_class!)
assets/
  Info.plist.template     — Info.plist for manual .app bundling
  InfoPlistExtras.plist   — Extra keys merged by cargo-bundle
scripts/
  bundle.sh               — Build release binary + assemble .app
examples/
  swipe_simulator.rs      — Standalone gesture test harness
```

---

## Architecture Notes

### Why private gestures?

macOS exposes no public API to fire a "swipe between pages" gesture. The
original SensibleSideButtons reverse-engineered the private IOKit/HID packet
format used by the kernel's multitouch driver. This project carries a faithful
Rust port of that serialisation (`src/gesture/synth.rs`, ~440 lines) with
compile-time size assertions on every struct to guard against ABI drift.

### Hot-path discipline

The `CGEventTap` callback (`src/tap.rs`) reads only `AtomicBool` values from a
pre-built `TapState`. No allocation, no locking, no disk I/O — just atomics and
a `post_swipe()` call.

### Robustness

If the callback exceeds macOS's latency budget the system sends a
`kCGEventTapDisabledByTimeout` event instead of a real event. We detect this
and immediately call `CGEventTapEnable(true)` to revive the tap, matching
Wowfunhappy's fix. Without this the tap silently dies after heavy system load.

---

## What's Next

- **Auto-start at login** — register as a Login Item via `SMAppService` (macOS 13+)
- **Universal binary** — `cargo build --target aarch64-apple-darwin` + `lipo` for an arm64+x86_64 fat binary
- **Custom icon** — drop an `icon.icns` into `assets/` and rebuild
- **Codesigning + notarization** — required for distribution outside the Mac App Store:
  ```bash
  ./scripts/bundle.sh --sign "Developer ID Application: You (TEAMID)"
  xcrun notarytool submit SensibleSideButtons.zip --apple-id … --team-id …
  ```
- **Sparkle updates** — embed the [Sparkle](https://sparkle-project.org/) framework for auto-updates
- **Mac App Store** — would require replacing the private gesture API with a public alternative (none currently exists)

---

## Credits

- **Alexei Baboulevitch** — original [SensibleSideButtons](https://github.com/archagon/sensible-side-buttons) and the reverse-engineered `TouchEvents.c` gesture serialiser
- **Wowfunhappy** — robustness fixes in the [Wowfunhappy fork](https://github.com/Wowfunhappy/sensible-side-buttons) (tap re-enable on timeout)

---

## License

MIT © 2024 Wei You

This project contains a Rust port of `External/TouchEvents.c` from
SensibleSideButtons, originally licensed GPL-2.0 by Alexei Baboulevitch.
The Rust port is redistributed here under MIT with his knowledge of the
derivative work.
