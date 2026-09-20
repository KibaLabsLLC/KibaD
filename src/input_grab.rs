//! Raw libinput integration.
//!
//! IMPORTANT DESIGN CORRECTION, found while implementing this rather than
//! assumed up front: Wayland deliberately does *not* let an arbitrary
//! client query the compositor's global pointer position (unlike X11's
//! `XQueryPointer`). And at the libinput/evdev level below Wayland
//! entirely, a normal mouse only ever reports *relative* motion deltas
//! (`dx`/`dy`) -- there is no absolute screen coordinate at this layer for
//! anything but touchscreens and tablets (`absolute_x`/`absolute_y`).
//!
//! That means this module is NOT the right place to reconstruct "where on
//! screen did this click land" -- doing that here would mean KibaD
//! independently re-accumulating pointer deltas and re-implementing
//! acceleration/clamping/multi-monitor layout to guess a position, running
//! a second, easily-desynced copy of state the compositor already owns
//! correctly. That's a real trap, not a minor detail.
//!
//! The corrected division of labor:
//!   - Screen position of a click comes from the overlay's own
//!     `wl_surface` input regions (see `overlay.rs`), which naturally
//!     receive surface-local coordinates already aligned to the output
//!     because the overlay spans the full screen. This is standards-
//!     compliant and requires no elevated privileges.
//!   - What THIS module is still genuinely useful for: libinput
//!     timestamps every physical button event in hardware time
//!     (`Event::time_usec`), which is a cleaner, jitter-free anchor for
//!     reaction-time measurement than timing derived from Wayland's own
//!     event delivery, which can be delayed by compositor scheduling.
//!
//! So `input_grab` supplies *precise click timing*; `overlay` supplies
//! *click position*. Neither module tries to do both.

use anyhow::{Context, Result};
use input::event::pointer::{ButtonState, PointerEvent, PointerEventTrait};
use input::event::Event;
use input::{Libinput, LibinputInterface};
use std::fs::OpenOptions;
use std::os::unix::{fs::OpenOptionsExt, io::OwnedFd};
use std::path::Path;

/// A single hardware-timestamped button transition, handed off to whatever
/// is correlating it with an on-screen click position.
#[derive(Debug, Clone, Copy)]
pub struct RawButtonEvent {
    pub button_code: u32,
    pub pressed: bool,
    /// Hardware timestamp in microseconds, from libinput itself -- not
    /// `Instant::now()` at the time we happened to process the event.
    pub time_usec: u64,
}

/// Minimal `LibinputInterface`: KibaD needs read access to the seat's input
/// devices. In production this daemon should run as a system service in
/// the `input` group (or under a udev-granted capability) rather than as
/// root -- opening with the flags libinput itself requests is all that's
/// needed here.
struct KibadInputInterface;

impl LibinputInterface for KibadInputInterface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> std::result::Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read(true)
            .write(flags & (libc::O_RDWR | libc::O_WRONLY) != 0)
            .open(path)
            .map(|f| f.into())
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(fd); // OwnedFd's Drop closes it; explicit for clarity/documentation
    }
}

/// Runs the libinput dispatch loop, invoking `on_button` for every physical
/// button press/release. Blocks the calling thread -- like `atspi`'s event
/// loop, this is meant to be spawned on its own `std::thread`.
pub fn run_event_loop(mut on_button: impl FnMut(RawButtonEvent)) -> Result<()> {
    let mut ctx = Libinput::new_with_udev(KibadInputInterface);
    ctx.udev_assign_seat("seat0")
        .map_err(|_| anyhow::anyhow!("udev_assign_seat failed -- is this process in the 'input' group?"))?;

    tracing::info!("KibaD libinput listener attached to seat0");

    loop {
        ctx.dispatch().context("libinput dispatch failed")?;
        for event in &mut ctx {
            if let Event::Pointer(PointerEvent::Button(btn)) = event {
                let pressed = matches!(btn.button_state(), ButtonState::Pressed);
                on_button(RawButtonEvent {
                    button_code: btn.button(),
                    pressed,
                    time_usec: btn.time_usec(),
                });
            }
        }
        // Avoid a hot spin when idle; dispatch() itself only drains what's
        // already queued, it doesn't block waiting for new events.
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
}
