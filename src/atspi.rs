//! AT-SPI2 integration: KibaD's primary source of widget geometry and
//! identity, used instead of computer vision wherever an app cooperates.
//!
//! AT-SPI2 does not run on the ordinary session bus. A client must first
//! ask the session bus's `org.a11y.Bus` service for the address of the
//! actual accessibility bus (`GetAddress`), then connect to *that* bus and
//! listen for events on the `org.a11y.atspi.Event.Object` interface. This
//! module handles that handshake and maintains a live registry mapping
//! `WidgetKey -> (Rect, last_actionable_instant)`, which is what lets the
//! rest of KibaD compute reaction time: "how long between this button
//! becoming actionable and a click landing on it."
//!
//! NOTE ON VALIDATION: this module is written against the real `dbus`
//! crate 0.9 API and the documented AT-SPI2 D-Bus wire protocol, and
//! compiles against the real system libdbus. It has not been exercised
//! against a live `at-spi2-registryd` in this environment (no desktop
//! session is available here to test against), so treat the exact
//! signal-matching rules as a solid starting point to verify against a
//! real KibaOS session rather than as already field-tested.

use crate::types::{Rect, WidgetKey};
use anyhow::{Context, Result};
use dbus::arg::Variant;
use dbus::blocking::{BlockingSender, Connection};
use dbus::message::{MatchRule, Message};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Live, shared view of every widget KibaD currently knows about. Cheap to
/// clone (it's an `Arc`) so both the AT-SPI listener thread and the
/// input-handling side (see `input_grab.rs`) can hold a handle to it.
#[derive(Clone, Default)]
pub struct WidgetRegistry {
    inner: Arc<Mutex<HashMap<WidgetKey, RegistryEntry>>>,
}

#[derive(Clone, Debug)]
struct RegistryEntry {
    rect: Rect,
    /// When this widget most recently became actionable (shown + enabled).
    /// `None` until we've observed a state transition, so we don't report
    /// a bogus reaction time for widgets that were already on-screen when
    /// KibaD started.
    became_actionable_at: Option<Instant>,
}

impl WidgetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn upsert_rect(&self, key: WidgetKey, rect: Rect) {
        let mut map = self.inner.lock().expect("registry mutex poisoned");
        map.entry(key).or_insert(RegistryEntry { rect, became_actionable_at: None }).rect = rect;
    }

    fn mark_actionable(&self, key: &WidgetKey) {
        let mut map = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = map.get_mut(key) {
            entry.became_actionable_at = Some(Instant::now());
        }
    }

    /// Given raw screen coordinates of a click (from the input-grab layer),
    /// find which known widget it landed inside, and the reaction time
    /// since that widget last became actionable (if known).
    pub fn resolve_click(&self, x: i32, y: i32) -> Option<(WidgetKey, Rect, Option<f64>)> {
        let map = self.inner.lock().expect("registry mutex poisoned");
        for (key, entry) in map.iter() {
            if entry.rect.contains(x, y) {
                let rt_ms = entry
                    .became_actionable_at
                    .map(|t| t.elapsed().as_secs_f64() * 1000.0);
                return Some((key.clone(), entry.rect, rt_ms));
            }
        }
        None
    }

    pub fn snapshot_len(&self) -> usize {
        self.inner.lock().expect("registry mutex poisoned").len()
    }
}

/// Connects to the session bus, asks it for the accessibility bus address,
/// and returns a `Connection` to that a11y bus.
///
/// This is the standard AT-SPI2 bootstrap handshake, documented as part of
/// the at-spi2-core protocol: `org.a11y.Bus` on the session bus exposes a
/// single method, `GetAddress`, returning the D-Bus address string of the
/// separate accessibility bus every AT-SPI-aware app connects to.
pub fn connect_a11y_bus() -> Result<Connection> {
    let session = Connection::new_session().context("connecting to session bus")?;

    let msg = Message::new_method_call(
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus",
        "GetAddress",
    )
    .map_err(|e| anyhow::anyhow!("building GetAddress call: {e}"))?;

    let reply = session
        .send_with_reply_and_block(msg, std::time::Duration::from_secs(5))
        .context("calling org.a11y.Bus.GetAddress -- is at-spi2-core running?")?;

    let address: String = reply
        .get1()
        .context("GetAddress reply did not contain the expected string")?;

    Connection::new_address(&address)
        .with_context(|| format!("connecting to a11y bus at {address}"))
}

/// Runs the AT-SPI event loop, updating `registry` as widgets appear, move,
/// or change actionable state. Blocks the calling thread; intended to be
/// spawned on a dedicated `std::thread` since the `dbus` crate's blocking
/// API doesn't cooperate with a tokio executor directly.
pub fn run_event_loop(registry: WidgetRegistry) -> Result<()> {
    let conn = connect_a11y_bus()?;

    // Subscribe to the two event families we actually act on:
    //   - Object:BoundsChanged  -> geometry updates
    //   - Object:StateChanged   -> a widget becoming "showing"+"enabled"
    // Every AT-SPI event arrives as a signal on org.a11y.atspi.Event.Object,
    // distinguished by its `member` field.
    for member in ["BoundsChanged", "StateChanged"] {
        let rule = MatchRule::new_signal("org.a11y.atspi.Event.Object", member);
        conn.add_match(rule, {
            let registry = registry.clone();
            move |_args: (), _conn: &Connection, msg: &Message| {
                handle_object_event(&registry, msg);
                true
            }
        })
        .context("registering AT-SPI match rule")?;
    }

    tracing::info!("KibaD AT-SPI listener attached; watching for widget events");

    loop {
        // process_all blocks briefly waiting for the next message batch;
        // looping forever here is the intended long-running daemon shape.
        conn.process(std::time::Duration::from_millis(500))
            .context("processing AT-SPI bus messages")?;
    }
}

/// Best-effort decode of one AT-SPI Object event into a registry update.
///
/// Every AT-SPI event body shares the same wire shape, confirmed against
/// the AT-SPI2 protocol's own type validation (`atspi-common`'s
/// `EventBodyOwned`, signature `(siiva{sv})`):
///   kind: String        -- e.g. "showing", "enabled", "focused" for
///                           StateChanged; unused for BoundsChanged
///   detail1: i32         -- for StateChanged, 0/1 = state now off/on
///   detail2: i32         -- generally unused for the two events we track
///   any_data: Variant    -- for BoundsChanged, wraps an AtspiRect
///                           (x, y, width, height), all i32
///   properties: a{sv}    -- unused here; `get4` below reads only the
///                           first four fields and ignores this trailing
///                           dict, which `dbus`'s typed getters allow.
///
/// Widget identity (role/label) still isn't resolved here -- that needs a
/// follow-up call to the sender's `org.a11y.atspi.Accessible` interface,
/// which is real remaining work, not something this pass adds.
fn handle_object_event(registry: &WidgetRegistry, msg: &Message) {
    let Some(member) = msg.member() else { return };
    let sender_app = msg.sender().map(|s| s.to_string()).unwrap_or_else(|| "unknown".into());
    let path = msg.path().map(|p| p.to_string()).unwrap_or_else(|| "/unknown".into());

    match &*member.to_string() {
        "BoundsChanged" => {
            if let Some((x, y, w, h)) = extract_bounds(msg) {
                let key = WidgetKey::new(sender_app, "unknown", "unknown", path);
                registry.upsert_rect(key, Rect { x, y, w, h });
            }
        }
        "StateChanged" => {
            if is_now_actionable(msg) {
                let key = WidgetKey::new(sender_app, "unknown", "unknown", path);
                registry.mark_actionable(&key);
            }
        }
        _ => {}
    }
}

/// Reads `any_data` as `Variant<(i32, i32, i32, i32)>` -- the AtspiRect
/// shape -- and returns it as `(x, y, w, h)` if the event matches.
fn extract_bounds(msg: &Message) -> Option<(i32, i32, i32, i32)> {
    let (_kind, _detail1, _detail2, any_data) =
        msg.get4::<String, i32, i32, Variant<(i32, i32, i32, i32)>>();
    let (x, y, w, h) = any_data?.0;
    Some((x, y, w, h))
}

/// True if this StateChanged event reports a "showing" or "enabled"
/// transition turning ON (detail1 == 1) -- the two states that together
/// mean a widget just became actionable and clickable. `any_data`'s exact
/// type varies by event and isn't needed for this check, so only the
/// first three fields are read.
fn is_now_actionable(msg: &Message) -> bool {
    let (kind, detail1, _detail2) = msg.get3::<String, i32, i32>();
    match (kind, detail1) {
        (Some(kind), Some(1)) => matches!(kind.as_str(), "showing" | "enabled"),
        _ => false,
    }
}
