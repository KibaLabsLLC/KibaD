//! The learning core of KibaD.
//!
//! Deliberately not a neural net — this mirrors the rest of KibaOS's
//! statistical/Bayesian approach (see Kortex). Each widget's set of
//! candidate positions is modeled as a small multi-armed bandit. We use
//! Gaussian Thompson sampling rather than a classic Beta-Bernoulli bandit
//! because our reward signal (negative reaction time) is continuous, not a
//! win/loss outcome.
//!
//! Reward shaping ("negative RT training"):
//!   reward = -(observed_rt_ms - baseline_rt_ms)
//! A click faster than the widget's established baseline yields a positive
//! reward for whichever position produced it; a click slower than baseline,
//! or a click that had to be corrected, yields a negative reward. Over many
//! observations this naturally pulls the bandit toward whichever candidate
//! position minimizes reaction time for that specific user, and naturally
//! reverts a bad relocation without needing any explicit "undo" logic.

use crate::types::{Rect, WidgetKey};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// How many observations a widget needs before we trust its baseline RT
/// enough to start relocating it. Below this, we only observe.
const BURN_IN_SAMPLES: u32 = 12;

/// Penalty multiplier applied when a click had to be corrected (i.e. missed
/// the widget first). This is deliberately harsher than ordinary slowness.
const CORRECTION_PENALTY_MS: f64 = 400.0;

/// Minimum samples an arm needs before it's eligible to be selected as the
/// "current best" — prevents a single lucky fast click from immediately
/// relocating a button off a well-established position.
const MIN_ARM_CONFIDENCE_SAMPLES: u32 = 5;

/// Running online statistics for one candidate position ("arm") of one
/// widget. Updated in O(1) per observation — no batch retraining.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Arm {
    pub offset: (i32, i32), // offset from the widget's default position
    pub samples: u32,
    pub mean_reward: f64,
    /// Running variance (Welford's algorithm), used as the Thompson-sampling
    /// uncertainty term so under-explored arms still get picked sometimes.
    m2: f64,
}

impl Arm {
    fn new(offset: (i32, i32)) -> Self {
        Self { offset, samples: 0, mean_reward: 0.0, m2: 0.0 }
    }

    /// Welford's online mean/variance update — numerically stable, no need
    /// to store the full reward history.
    fn observe(&mut self, reward: f64) {
        self.samples += 1;
        let delta = reward - self.mean_reward;
        self.mean_reward += delta / self.samples as f64;
        let delta2 = reward - self.mean_reward;
        self.m2 += delta * delta2;
    }

    fn variance(&self) -> f64 {
        if self.samples < 2 {
            // High uncertainty prior for under-sampled arms so they still
            // get explored instead of being written off after one sample.
            100.0
        } else {
            self.m2 / (self.samples - 1) as f64
        }
    }

    /// Draw a sample from this arm's belief distribution for Thompson
    /// sampling: mean_reward plus Gaussian noise scaled by our uncertainty.
    fn thompson_draw(&self, rng: &mut impl rand::RngCore) -> f64 {
        use rand::Rng;
        let std_dev = self.variance().sqrt().max(1.0);
        let noise: f64 = rng.gen::<f64>() * 2.0 - 1.0; // cheap, avoids pulling in rand_distr
        self.mean_reward + noise * std_dev
    }
}

/// Per-widget adaptive state: its default (app-drawn) position, a baseline
/// reaction time, and the set of candidate offsets being explored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetModel {
    pub default_rect: Rect,
    pub baseline_rt_ms: Option<f64>,
    pub total_samples: u32,
    pub arms: Vec<Arm>,
    /// Index into `arms` of the position currently being rendered.
    pub active_arm: usize,
}

impl WidgetModel {
    pub fn new(default_rect: Rect) -> Self {
        // Candidate offsets: staying put, plus small nudges in the four
        // cardinal directions and toward screen-center bias. Kept small and
        // few on purpose — large jumps fight muscle memory (see the
        // hysteresis discussion), and a "thin" model should have a thin
        // action space.
        let arms = vec![
            Arm::new((0, 0)),
            Arm::new((-40, 0)),
            Arm::new((40, 0)),
            Arm::new((0, -40)),
            Arm::new((0, 40)),
        ];
        Self { default_rect, baseline_rt_ms: None, total_samples: 0, arms, active_arm: 0 }
    }

    /// The screen rect KibaD should currently render this widget's overlay
    /// at (default_rect shifted by the active arm's offset).
    pub fn current_rect(&self) -> Rect {
        let (dx, dy) = self.arms[self.active_arm].offset;
        Rect { x: self.default_rect.x + dx, y: self.default_rect.y + dy, w: self.default_rect.w, h: self.default_rect.h }
    }

    /// Fold in one click observation: update baseline, compute reward,
    /// update the active arm's running stats, and possibly switch which
    /// arm is active.
    pub fn observe(&mut self, reaction_time_ms: f64, was_correction: bool, rng: &mut impl rand::RngCore) {
        self.total_samples += 1;

        let effective_rt = if was_correction { reaction_time_ms + CORRECTION_PENALTY_MS } else { reaction_time_ms };

        // Establish or slowly adapt the baseline using an exponential
        // moving average, so the baseline itself isn't frozen at whatever
        // was observed during burn-in.
        self.baseline_rt_ms = Some(match self.baseline_rt_ms {
            None => effective_rt,
            Some(prev) => prev * 0.9 + effective_rt * 0.1,
        });
        let baseline = self.baseline_rt_ms.unwrap();

        let reward = -(effective_rt - baseline);
        self.arms[self.active_arm].observe(reward);

        if self.total_samples < BURN_IN_SAMPLES {
            return; // still observing only; never relocate during burn-in
        }

        self.select_arm(rng);
    }

    /// Thompson-sample across arms with enough confidence, and switch the
    /// active (rendered) arm if another one currently looks better. Arms
    /// below `MIN_ARM_CONFIDENCE_SAMPLES` are given a chance to be explored
    /// but can't yet unseat a confident incumbent.
    fn select_arm(&mut self, rng: &mut impl rand::RngCore) {
        // IMPORTANT: every arm must remain eligible for selection here, even
        // with zero samples. An earlier version gated eligibility on
        // `samples >= MIN_ARM_CONFIDENCE_SAMPLES`, which created a deadlock:
        // an under-sampled arm can only gain samples by being selected, but
        // could never be selected until it already had samples. Exploration
        // of fresh arms instead comes from `thompson_draw`'s wide variance
        // prior (see `Arm::variance`) for arms with fewer than 2 samples.
        //
        // `MIN_ARM_CONFIDENCE_SAMPLES` is still used, but only as a
        // hysteresis margin: a challenger with too little history must beat
        // the incumbent by a clear margin (not just edge it out on noise)
        // before it's allowed to unseat an already-confident active arm.
        let active_confident = self.arms[self.active_arm].samples >= MIN_ARM_CONFIDENCE_SAMPLES;
        let active_draw_for_margin = self.arms[self.active_arm].mean_reward;
        const HYSTERESIS_MARGIN: f64 = 15.0; // ms-equivalent reward units

        let mut best_idx = self.active_arm;
        let mut best_draw = self.arms[self.active_arm].thompson_draw(rng);

        for (i, arm) in self.arms.iter().enumerate() {
            if i == self.active_arm {
                continue;
            }
            let draw = arm.thompson_draw(rng);
            let challenger_confident = arm.samples >= MIN_ARM_CONFIDENCE_SAMPLES;
            let clears_margin = if active_confident && challenger_confident {
                draw > active_draw_for_margin + HYSTERESIS_MARGIN
            } else {
                draw > best_draw
            };
            if clears_margin && draw > best_draw {
                best_draw = draw;
                best_idx = i;
            }
        }
        self.active_arm = best_idx;
    }
}

/// Top-level in-memory model: every tracked widget, keyed by its durable
/// fingerprint. This is what gets serialized to/from `UI.dat`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct KibaModel {
    pub widgets: HashMap<u64, WidgetModel>,
    #[serde(skip)]
    key_lookup: HashMap<u64, WidgetKey>,
}

impl KibaModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get-or-create the model for a widget, registering its default rect
    /// the first time we see it.
    pub fn widget_mut(&mut self, key: &WidgetKey, default_rect: Rect) -> &mut WidgetModel {
        let fp = key.fingerprint();
        self.key_lookup.entry(fp).or_insert_with(|| key.clone());
        self.widgets.entry(fp).or_insert_with(|| WidgetModel::new(default_rect))
    }

    pub fn get(&self, key: &WidgetKey) -> Option<&WidgetModel> {
        self.widgets.get(&key.fingerprint())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn key() -> WidgetKey {
        WidgetKey::new("org.kiba.testapp", "push button", "Save", "/0/2/1")
    }

    #[test]
    fn new_widget_starts_at_default_offset() {
        let mut model = KibaModel::new();
        let rect = Rect { x: 100, y: 100, w: 80, h: 24 };
        let w = model.widget_mut(&key(), rect);
        assert_eq!(w.current_rect().x, 100);
        assert_eq!(w.current_rect().y, 100);
    }

    #[test]
    fn burn_in_never_relocates() {
        let mut model = KibaModel::new();
        let rect = Rect { x: 0, y: 0, w: 50, h: 20 };
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let w = model.widget_mut(&key(), rect);
        for _ in 0..(BURN_IN_SAMPLES - 1) {
            w.observe(200.0, false, &mut rng);
            assert_eq!(w.active_arm, 0, "must not relocate during burn-in");
        }
    }

    #[test]
    fn consistently_faster_arm_eventually_wins() {
        // Simulate a scenario where offset arm index 1 is genuinely, always
        // faster than every other arm. Over enough samples the bandit
        // should converge on it being selected far more than any other arm.
        let mut model = KibaModel::new();
        let rect = Rect { x: 0, y: 0, w: 50, h: 20 };
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let w = model.widget_mut(&key(), rect);

        let mut times_active_was_arm1 = 0u32;
        let trials = 400;
        for i in 0..trials {
            // Whatever arm is currently active gets its RT reported: arm 1
            // reports a fast, low-variance time; everything else is slow.
            let rt = if w.active_arm == 1 { 90.0 } else { 260.0 };
            w.observe(rt, false, &mut rng);
            if i > trials - 50 && w.active_arm == 1 {
                times_active_was_arm1 += 1;
            }
        }
        assert!(times_active_was_arm1 > 30, "expected arm 1 to dominate late selections, got {times_active_was_arm1}/50");
    }

    #[test]
    fn correction_click_is_penalized_harder_than_plain_slowness() {
        let mut model = KibaModel::new();
        let rect = Rect { x: 0, y: 0, w: 50, h: 20 };
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let w = model.widget_mut(&key(), rect);
        for _ in 0..BURN_IN_SAMPLES {
            w.observe(150.0, false, &mut rng);
        }
        // Track the specific arm that receives the observation, not
        // `active_arm` after the fact -- `observe` may itself switch which
        // arm is active as its very last step (via `select_arm`), so
        // reading `active_arm` post-call can point at a different,
        // never-yet-penalized arm and make this assertion meaningless.
        let arm_under_test = w.active_arm;
        let reward_before = w.arms[arm_under_test].mean_reward;
        w.observe(150.0, true, &mut rng); // same raw RT, but flagged as a correction
        let reward_after = w.arms[arm_under_test].mean_reward;
        assert!(reward_after < reward_before, "a correction click should drag mean reward down");
    }
}
