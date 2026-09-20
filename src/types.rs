//! Shared types for KibaD: widget identity, geometry, and click events.
//!
//! Widget identity is the trickiest part of this whole system. AT-SPI's own
//! object references are not stable across app restarts (a relaunched app
//! gets fresh object paths), so we never persist those. Instead every widget
//! is keyed by a hash of durable, semantic properties: which app it belongs
//! to, its accessible role, its label/name, and its path from the root of
//! the accessibility tree. That tuple is far more likely to still identify
//! "the same button" the next time the app launches.

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// A screen-space rectangle, in logical (not necessarily physical) pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// A durable, cross-restart identity for a single interactive widget.
///
/// `widget_key` is a stable hash computed from `app_id`, `role`, `label`,
/// and `tree_path` — never from the raw AT-SPI object reference, which is
/// only valid for the lifetime of one running app instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WidgetKey {
    pub app_id: String,
    pub role: String,
    pub label: String,
    pub tree_path: String,
}

impl WidgetKey {
    pub fn new(app_id: impl Into<String>, role: impl Into<String>, label: impl Into<String>, tree_path: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            role: role.into(),
            label: label.into(),
            tree_path: tree_path.into(),
        }
    }

    /// A stable u64 fingerprint, used as the SQLite primary key so we don't
    /// have to store four separate TEXT columns per row.
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

/// A single observed click, as reported by the AT-SPI event stream.
#[derive(Debug, Clone)]
pub struct ClickEvent {
    pub widget: WidgetKey,
    /// Where the click actually landed on screen.
    pub click_pos: (i32, i32),
    /// Time from the widget becoming actionable (visible + enabled) to the
    /// click landing on it. This is the raw signal the bandit learns from.
    pub reaction_time_ms: f64,
    /// True if this click initially missed the widget's extents and had to
    /// be corrected — a stronger negative signal than "just slow".
    pub was_correction: bool,
}
