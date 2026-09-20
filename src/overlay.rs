//! The visual + click-position half of KibaD, per the corrected division
//! of labor described in `input_grab.rs`: this module owns "where did the
//! click land" because it's the one piece that can legitimately know that
//! on Wayland, by being the surface the click landed on.
//!
//! Mechanism: a single fullscreen `zwlr_layer_shell_v1` surface, layered
//! at `overlay` (top of the stack), with `keyboard_interactivity: none`.
//! Critically, its `wl_surface` input region is NOT the whole screen --
//! it's the union of small rectangles: each widget's original position
//! (to intercept/drop stale clicks there) and each widget's *relocated*
//! position (to receive the real click and forward it to the app as if it
//! landed on the original widget). Every pixel outside those rectangles
//! is outside the input region, so clicks there fall through to whatever
//! app window is underneath, untouched -- this is the same click-through
//! technique Wayland panels/notification daemons already rely on.
//!
//! Because the layer surface is anchored to all four edges of its output,
//! its surface-local coordinate space has its origin at the output's
//! top-left corner -- so surface-local (x, y) from a `wl_pointer` event on
//! this surface already IS the absolute on-screen position, with no
//! separate coordinate reconstruction needed (contrast with the
//! relative-delta problem in `input_grab.rs`).
//!
//! HONEST SCOPING NOTE: rendering here uses a flat shared-memory buffer
//! with solid-color rectangles as placeholders for the mask tile and the
//! relocated button. Compositing the *actual* pixels of the app's own
//! button (true visual masking) would additionally need the
//! `wlr-screencopy-unstable-v1` protocol to grab the source pixels first;
//! that's a real next step, not implemented here, and is called out
//! explicitly rather than silently faked.

use crate::types::Rect;
use anyhow::{Context, Result};
use std::os::fd::AsFd;
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// One rectangle KibaD currently wants the overlay to own input for, and
/// what it represents -- used both to build the input region and to know
/// what to do when a click lands inside it.
#[derive(Debug, Clone, Copy)]
pub enum ActiveZone {
    /// The widget's original position: clicks here are stale (the button
    /// visually moved away) and should be swallowed, not forwarded.
    Mask(Rect),
    /// The widget's relocated position: clicks here are real and should
    /// be forwarded to the app as if they'd landed on `forward_to`.
    Relocated { visible_at: Rect, forward_to: Rect },
}

/// Emitted whenever a real (non-stale) click lands on the overlay.
#[derive(Debug, Clone, Copy)]
pub struct OverlayClick {
    /// Where to deliver the click to the underlying app -- the widget's
    /// original, real coordinates.
    pub forward_to: (i32, i32),
}

struct OverlayState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    output: Option<wl_output::WlOutput>,
    output_size: (i32, i32),
    /// The output's integer scale factor (`wl_output.scale`; 1 = standard
    /// DPI, 2 = common HiDPI). REAL BUG FIXED HERE: without this, the shm
    /// buffer was allocated at logical size and never had
    /// `wl_surface.set_buffer_scale` called on it, so the compositor would
    /// upscale a low-resolution buffer to fill a HiDPI output -- the
    /// overlay would render blurry (or, depending on compositor behavior,
    /// at the wrong apparent size) on any scale != 1 display, e.g. the
    /// KBook's panel. Note this is specifically a *rendering* bug: click
    /// *position* was never actually affected, since `wl_pointer` motion
    /// events are always delivered in surface-local logical coordinates
    /// regardless of output scale -- the compositor handles that
    /// conversion transparently for input, just not for buffer content.
    output_scale: i32,
    configured: bool,
    zones: Vec<ActiveZone>,
    /// Last pointer position reported on our surface (surface-local, which
    /// per the module doc equals absolute screen position here).
    last_pointer_pos: (f64, f64),
    /// Channel end used to hand completed clicks back to the daemon's main
    /// logic without the Wayland dispatch loop needing to know about it.
    click_tx: std::sync::mpsc::Sender<OverlayClick>,
}

/// Handle used by the rest of KibaD to update which zones the overlay is
/// currently claiming input for, and to receive resolved clicks.
pub struct OverlayHandle {
    conn: Connection,
    event_queue: EventQueue<OverlayState>,
    qh: QueueHandle<OverlayState>,
    state: OverlayState,
    pub click_rx: std::sync::mpsc::Receiver<OverlayClick>,
}

impl OverlayHandle {
    /// Connects to the compositor and performs the initial handshake:
    /// binds the globals we need, creates the fullscreen layer surface,
    /// and waits (via `roundtrip`) for the compositor's first `configure`
    /// so we know the real output size before drawing anything.
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to Wayland display -- is WAYLAND_DISPLAY set?")?;
        let mut event_queue = conn.new_event_queue::<OverlayState>();
        let qh = event_queue.handle();
        let display = conn.display();
        let _registry = display.get_registry(&qh, ());

        let (click_tx, click_rx) = std::sync::mpsc::channel();
        let mut state = OverlayState {
            compositor: None,
            shm: None,
            layer_shell: None,
            seat: None,
            pointer: None,
            surface: None,
            layer_surface: None,
            output: None,
            output_size: (0, 0),
            output_scale: 1,
            configured: false,
            zones: Vec::new(),
            last_pointer_pos: (0.0, 0.0),
            click_tx,
        };

        // First roundtrip: collect the registry globals.
        event_queue.roundtrip(&mut state).context("initial registry roundtrip")?;

        let compositor = state.compositor.clone().context("compositor did not advertise wl_compositor")?;
        let shm = state.shm.clone().context("compositor did not advertise wl_shm")?;
        let layer_shell = state
            .layer_shell
            .clone()
            .context("compositor did not advertise zwlr_layer_shell_v1 -- is this a wlroots-based compositor?")?;
        if let Some(seat) = state.seat.clone() {
            let pointer = seat.get_pointer(&qh, ());
            state.pointer = Some(pointer);
        }

        let surface = compositor.create_surface(&qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None, // let the compositor pick the output
            zwlr_layer_shell_v1::Layer::Overlay,
            "kibad-overlay".to_string(),
            &qh,
            (),
        );
        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer_surface.set_exclusive_zone(-1); // don't reserve space, we're a pure overlay
        layer_surface.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        // Start with an EMPTY input region: fully click-through until the
        // bandit model actually has a relocation to apply. This is the
        // safe default -- KibaD should never intercept input it has no
        // reason to.
        let empty_region = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&empty_region));
        surface.commit();

        state.surface = Some(surface);
        state.layer_surface = Some(layer_surface);

        // Second roundtrip: wait for the layer surface's initial
        // `configure` event, which tells us the real output size.
        event_queue.roundtrip(&mut state).context("waiting for layer_surface configure")?;

        Ok(Self { conn, event_queue, qh, state, click_rx })
    }

    /// Replaces the set of zones the overlay owns input for, rebuilds the
    /// input region and redraws, and commits the new surface state. Called
    /// by the daemon's main loop whenever the bandit model changes which
    /// arm is active for some widget.
    pub fn set_zones(&mut self, zones: Vec<ActiveZone>) -> Result<()> {
        self.state.zones = zones;
        self.state.rebuild_input_region_and_redraw(&self.qh)?;
        self.event_queue.flush().context("flushing Wayland requests after zone update")?;
        Ok(())
    }

    /// Pumps the Wayland event queue once. Call in a loop from the main
    /// daemon task; non-blocking-ish via `dispatch_pending` plus a short
    /// read, matching the pattern the `wayland-client` docs recommend for
    /// integrating with an external event loop instead of a dedicated
    /// blocking thread.
    pub fn pump(&mut self) -> Result<()> {
        self.event_queue.dispatch_pending(&mut self.state).context("dispatching Wayland events")?;
        Ok(())
    }
}

impl OverlayState {
    fn rebuild_input_region_and_redraw(&mut self, qh: &QueueHandle<OverlayState>) -> Result<()> {
        let (Some(compositor), Some(surface)) = (&self.compositor, &self.surface) else {
            return Ok(());
        };

        let region = compositor.create_region(qh, ());
        for zone in &self.zones {
            let rect = match zone {
                ActiveZone::Mask(r) => *r,
                ActiveZone::Relocated { visible_at, .. } => *visible_at,
            };
            region.add(rect.x, rect.y, rect.w, rect.h);
        }
        surface.set_input_region(Some(&region));

        // Redraw: allocate a fresh shm buffer sized to the output, fill it
        // transparent, then paint each zone as a flat placeholder rect.
        // See the module-level "HONEST SCOPING NOTE" -- this is where real
        // pixel content would replace the placeholder fill.
        if let Some(buffer_attach) = self.draw_placeholder_buffer(qh)? {
            surface.attach(Some(&buffer_attach), 0, 0);
        }
        // damage_buffer takes buffer-pixel coordinates, not logical ones --
        // this must scale with output_scale for the same reason the shm
        // buffer allocation does (see draw_placeholder_buffer's doc).
        let scale = self.output_scale.max(1);
        surface.damage_buffer(0, 0, self.output_size.0 * scale, self.output_size.1 * scale);
        surface.commit();
        Ok(())
    }

    /// Allocates and paints the overlay's shm buffer.
    ///
    /// SCALE FIX: `output_size` (from `zwlr_layer_surface_v1::Event::
    /// Configure`) is in surface-local *logical* pixels -- the same units
    /// `wl_pointer` coordinates and AT-SPI's reported widget rects use, so
    /// zone rects need no conversion for click handling or the input
    /// region. But the shm buffer backing what's actually drawn is raw
    /// pixels, and on a HiDPI output (`output_scale > 1`) a buffer
    /// allocated at the *logical* size is lower resolution than the
    /// output really is; the compositor then upscales it to fill the
    /// logical area, which is why this needed fixing -- previously the
    /// buffer was sized to `output_size` directly with no
    /// `set_buffer_scale` call, so on e.g. a scale-2 panel the overlay
    /// would render at half the real pixel density (blurry), even though
    /// click handling itself was never actually affected by this bug.
    ///
    /// The fix: allocate the buffer at `output_size * scale` physical
    /// pixels, paint zone rects scaled up to match, and call
    /// `wl_surface.set_buffer_scale(scale)` so the compositor knows one
    /// buffer pixel does *not* map 1:1 to one logical pixel.
    fn draw_placeholder_buffer(&self, qh: &QueueHandle<OverlayState>) -> Result<Option<wl_buffer::WlBuffer>> {
        let (logical_w, logical_h) = self.output_size;
        if logical_w == 0 || logical_h == 0 {
            return Ok(None); // not configured yet
        }
        let Some(shm) = &self.shm else { return Ok(None) };
        let scale = self.output_scale.max(1);

        let (buf_w, buf_h) = (logical_w * scale, logical_h * scale);
        let stride = buf_w * 4;
        let size = (stride * buf_h) as usize;

        let mut mem = memmap_anon_shm(size).context("allocating shm buffer for overlay")?;
        // ARGB8888, fully transparent by default.
        for px in mem.as_mut_slice().chunks_exact_mut(4) {
            px.copy_from_slice(&[0, 0, 0, 0]);
        }
        for zone in &self.zones {
            let (rect, argb): (Rect, [u8; 4]) = match zone {
                // Semi-opaque neutral tile approximating "erased" -- see
                // the scoping note; a real implementation would sample the
                // app's own background color here instead of a flat gray.
                ActiveZone::Mask(r) => (*r, [180, 180, 180, 235]),
                // A visibly distinct placeholder "button" at the relocated
                // position.
                ActiveZone::Relocated { visible_at, .. } => (*visible_at, [60, 130, 220, 255]),
            };
            // Zone rects are in logical coordinates; scale them up to
            // match the physical-pixel buffer being painted into.
            let scaled = Rect { x: rect.x * scale, y: rect.y * scale, w: rect.w * scale, h: rect.h * scale };
            paint_rect(mem.as_mut_slice(), buf_w, buf_h, scaled, argb);
        }

        let pool = shm.create_pool(mem.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, buf_w, buf_h, stride, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();

        if let Some(surface) = &self.surface {
            surface.set_buffer_scale(scale);
        }
        Ok(Some(buffer))
    }
}

fn paint_rect(buf: &mut [u8], stride_w: i32, stride_h: i32, rect: Rect, argb: [u8; 4]) {
    let x0 = rect.x.max(0);
    let y0 = rect.y.max(0);
    let x1 = (rect.x + rect.w).min(stride_w);
    let y1 = (rect.y + rect.h).min(stride_h);
    for y in y0..y1 {
        for x in x0..x1 {
            let idx = ((y * stride_w + x) * 4) as usize;
            if idx + 4 <= buf.len() {
                // wl_shm Argb8888 is byte order B,G,R,A on little-endian.
                buf[idx] = argb[2];
                buf[idx + 1] = argb[1];
                buf[idx + 2] = argb[0];
                buf[idx + 3] = argb[3];
            }
        }
    }
}

/// Thin wrapper around an anonymous, memfd-backed shared memory mapping --
/// what `wl_shm_pool` needs as its backing storage.
struct AnonShm {
    fd: std::os::fd::OwnedFd,
    map: memmap2::MmapMut,
}
impl AnonShm {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.map[..]
    }
}
fn memmap_anon_shm(size: usize) -> Result<AnonShm> {
    let fd = rustix::fs::memfd_create("kibad-overlay", rustix::fs::MemfdFlags::CLOEXEC)
        .context("memfd_create failed")?;
    rustix::fs::ftruncate(&fd, size as u64).context("ftruncate on shm fd failed")?;
    let map = unsafe { memmap2::MmapOptions::new().len(size).map_mut(&fd)? };
    Ok(AnonShm { fd, map })
}

// ---- Dispatch implementations ----
// Each of these is deliberately minimal: KibaD only reacts to the specific
// events it needs (registry globals, layer_surface configure, pointer
// motion/button), and ignores everything else via the `_ => {}` arms.

impl Dispatch<wl_registry::WlRegistry, ()> for OverlayState {
    fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(4), qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), qh, ())),
                "wl_output" => {
                    // Only the first advertised output is tracked -- real
                    // multi-monitor support needs one layer surface *per*
                    // output (each can have a different scale), which is
                    // real remaining work flagged here rather than faked.
                    if state.output.is_none() {
                        let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                        state.output = Some(output);
                    }
                }
                "zwlr_layer_shell_v1" => state.layer_shell = Some(registry.bind(name, version.min(4), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for OverlayState {
    fn event(state: &mut Self, surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                surface.ack_configure(serial);
                state.output_size = (width as i32, height as i32);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                tracing::warn!("KibaD overlay layer surface was closed by the compositor");
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for OverlayState {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.last_pointer_pos = (surface_x, surface_y);
            }
            wl_pointer::Event::Button { state: btn_state, .. } => {
                if btn_state == wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed) {
                    let (x, y) = state.last_pointer_pos;
                    if let Some(fwd) = resolve_zone_click(&state.zones, x as i32, y as i32) {
                        let _ = state.click_tx.send(OverlayClick { forward_to: fwd });
                    }
                    // A click inside a Mask zone intentionally produces no
                    // send at all -- it's swallowed, per the module doc.
                }
            }
            _ => {}
        }
    }
}

fn resolve_zone_click(zones: &[ActiveZone], x: i32, y: i32) -> Option<(i32, i32)> {
    for zone in zones {
        if let ActiveZone::Relocated { visible_at, forward_to } = zone {
            if visible_at.contains(x, y) {
                return Some(forward_to.center());
            }
        }
    }
    None
}

// Trivial no-op Dispatch impls for objects whose events we don't need.
impl Dispatch<wl_compositor::WlCompositor, ()> for OverlayState {
    fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_shm::WlShm, ()> for OverlayState {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_shm_pool::WlShmPool, ()> for OverlayState {
    fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_buffer::WlBuffer, ()> for OverlayState {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_surface::WlSurface, ()> for OverlayState {
    fn event(_: &mut Self, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_region::WlRegion, ()> for OverlayState {
    fn event(_: &mut Self, _: &wl_region::WlRegion, _: wl_region::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_seat::WlSeat, ()> for OverlayState {
    fn event(state: &mut Self, seat: &wl_seat::WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            if let wayland_client::WEnum::Value(caps) = capabilities {
                if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for OverlayState {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Scale { factor } = event {
            state.output_scale = factor.max(1);
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for OverlayState {
    fn event(_: &mut Self, _: &zwlr_layer_shell_v1::ZwlrLayerShellV1, _: zwlr_layer_shell_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
