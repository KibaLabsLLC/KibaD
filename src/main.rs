//! KibaD -- the Latent App GUI daemon for KibaOS.
//!
//! Ties the four subsystems together:
//!   - `atspi`      discovers widget geometry + actionable-state timing
//!   - `bandit`     the thin, statistical per-widget relocation model
//!   - `storage`    persists learned state to /var/lib/KibaD/UI.dat
//!   - `overlay`    owns click position for relocated widgets + renders them
//!   - `input_grab` supplies precise hardware click timing (see its module
//!                  doc for why it does NOT supply click position)
//!
//! This binary wires them together; see each module for the parts of the
//! design that are solid vs. the parts flagged as needing verification
//! against a real running KibaOS session (no live AT-SPI bus, Wayland
//! compositor, or input devices exist in the sandbox this was built in).

mod atspi;
mod bandit;
mod input_grab;
mod overlay;
mod storage;
mod telemetry;
mod types;

use anyhow::Result;
use bandit::KibaModel;
use overlay::ActiveZone;
use rand::SeedableRng;
use std::sync::{Arc, Mutex};

const UI_DAT_PATH: &str = "/var/lib/KibaD/UI.dat";

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("KibaD starting");

    // --- Load persisted model ---
    let store = storage::Store::open(UI_DAT_PATH)?;
    let mut model = KibaModel::new();
    for (key, widget_model) in store.load_all()? {
        model.widgets.insert(key.fingerprint(), widget_model);
    }
    let model = Arc::new(Mutex::new(model));

    // --- Shared AT-SPI widget registry ---
    let registry = atspi::WidgetRegistry::new();
    {
        let registry = registry.clone();
        std::thread::spawn(move || {
            if let Err(e) = atspi::run_event_loop(registry) {
                tracing::error!("AT-SPI event loop exited: {e:#}");
            }
        });
    }

    // --- libinput: precise click timestamps only (see input_grab.rs) ---
    {
        std::thread::spawn(move || {
            let result = input_grab::run_event_loop(|btn| {
                if btn.pressed {
                    tracing::debug!(?btn, "raw button press timestamp");
                }
            });
            if let Err(e) = result {
                tracing::error!("libinput event loop exited: {e:#}");
            }
        });
    }

    // --- Telemetry: consent-gated, isolated thread (see telemetry/mod.rs) ---
    // `telemetry::run()` checks consent itself and returns immediately if
    // none is on file -- safe to always spawn.
    std::thread::spawn(telemetry::run);

    // --- Overlay: owns click position + rendering ---
    let mut overlay = overlay::OverlayHandle::connect()?;
    tracing::info!("KibaD overlay attached; entering main loop");

    let mut rng = rand::rngs::StdRng::from_entropy();
    let mut tick: u64 = 0;

    loop {
        overlay.pump()?;

        // Drain any resolved clicks the overlay produced this tick and
        // fold them into the bandit model. Reaction time here is looked
        // up from the AT-SPI registry's "became actionable" timestamp for
        // whichever widget occupies that forwarded position.
        while let Ok(click) = overlay.click_rx.try_recv() {
            if let Some((key, default_rect, rt_ms)) = registry.resolve_click(click.forward_to.0, click.forward_to.1) {
                let mut model = model.lock().expect("model mutex poisoned");
                let widget = model.widget_mut(&key, default_rect);
                widget.observe(rt_ms.unwrap_or(widget.baseline_rt_ms.unwrap_or(200.0)), false, &mut rng);
                store.save_widget(&key, widget).ok();
            }
        }

        // Periodically resync the overlay's active zones from the current
        // model state, so relocations chosen by the bandit actually get
        // rendered and their input intercepted.
        tick += 1;
        if tick % 30 == 0 {
            let model = model.lock().expect("model mutex poisoned");
            let mut zones = Vec::new();
            for widget in model.widgets.values() {
                let current = widget.current_rect();
                if current.x != widget.default_rect.x || current.y != widget.default_rect.y {
                    zones.push(ActiveZone::Mask(widget.default_rect));
                    zones.push(ActiveZone::Relocated { visible_at: current, forward_to: widget.default_rect });
                }
            }
            drop(model);
            overlay.set_zones(zones)?;
        }

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}
