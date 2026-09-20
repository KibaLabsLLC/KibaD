//! Persistence for KibaD's learned state.
//!
//! `UI.dat` is a SQLite database (WAL mode) rather than a hand-rolled binary
//! format. This matches how Kortex already persists its own state, and buys
//! us crash-safety and the ability to inspect learned state with an
//! ordinary `sqlite3` shell while debugging — worth more than the small
//! amount of I/O overhead versus a raw struct dump.
//!
//! Schema is intentionally tiny: one row per widget, storing its durable
//! key components (for debuggability/joins) plus the whole `WidgetModel`
//! serialized as JSON in a single column. We are not trying to build a
//! queryable analytics schema here — the model itself is small, and JSON
//! keeps this file free of migration pain as `WidgetModel`'s shape evolves.
//!
//! Location: per-user, under `$XDG_DATA_HOME/kibad/UI.dat`, falling back to
//! `~/.local/share/kibad/UI.dat`. See [`Store::default_path`].

use crate::bandit::WidgetModel;
use crate::types::WidgetKey;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS widgets (
    fingerprint INTEGER PRIMARY KEY,
    app_id      TEXT NOT NULL,
    role        TEXT NOT NULL,
    label       TEXT NOT NULL,
    tree_path   TEXT NOT NULL,
    model_json  TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);";

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Default location of UI.dat: `$XDG_DATA_HOME/kibad/UI.dat`, or
    /// `~/.local/share/kibad/UI.dat` when `XDG_DATA_HOME` is unset or empty.
    pub fn default_path() -> Result<PathBuf> {
        let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => {
                let home = std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .context("neither XDG_DATA_HOME nor HOME is set")?;
                PathBuf::from(home).join(".local/share")
            }
        };
        Ok(base.join("kibad").join("UI.dat"))
    }

    /// Opens UI.dat at the per-user default location.
    pub fn open_default() -> Result<Self> {
        Self::open(Self::default_path()?)
    }

    /// Opens (creating if needed) the UI.dat database at the given path,
    /// and makes sure its schema exists.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {}", path.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .with_context(|| format!("enabling WAL on {}", path.display()))?;
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("creating schema in {}", path.display()))?;
        Ok(Self { conn })
    }

    /// Opens an in-memory database — used by tests so they never touch disk.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn save_widget(&self, key: &WidgetKey, model: &WidgetModel) -> Result<()> {
        let json = serde_json::to_string(model)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.conn.execute(
            "INSERT INTO widgets (fingerprint, app_id, role, label, tree_path, model_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(fingerprint) DO UPDATE SET
                model_json = excluded.model_json,
                updated_at = excluded.updated_at",
            params![key.fingerprint() as i64, key.app_id, key.role, key.label, key.tree_path, json, now],
        )?;
        Ok(())
    }

    /// Loads every previously-seen widget model, keyed by fingerprint, along
    /// with the WidgetKey needed to reconstruct KibaModel's lookup table.
    pub fn load_all(&self) -> Result<Vec<(WidgetKey, WidgetModel)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT app_id, role, label, tree_path, model_json FROM widgets")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let app_id: String = row.get(0)?;
            let role: String = row.get(1)?;
            let label: String = row.get(2)?;
            let tree_path: String = row.get(3)?;
            let json: String = row.get(4)?;
            let model: WidgetModel = serde_json::from_str(&json)
                .context("deserializing WidgetModel from UI.dat; schema may have changed")?;
            out.push((WidgetKey::new(app_id, role, label, tree_path), model));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Rect;

    #[test]
    fn round_trips_a_widget_model() {
        let store = Store::open_in_memory().unwrap();
        let key = WidgetKey::new("org.kiba.testapp", "push button", "Save", "/0/2/1");
        let model = WidgetModel::new(Rect { x: 10, y: 20, w: 80, h: 24 });

        store.save_widget(&key, &model).unwrap();
        let loaded = store.load_all().unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, key);
        assert_eq!(loaded[0].1.default_rect.x, 10);
    }

    #[test]
    fn upsert_overwrites_rather_than_duplicates() {
        let store = Store::open_in_memory().unwrap();
        let key = WidgetKey::new("org.kiba.testapp", "push button", "Save", "/0/2/1");
        let mut model = WidgetModel::new(Rect { x: 0, y: 0, w: 10, h: 10 });

        store.save_widget(&key, &model).unwrap();
        model.total_samples = 99;
        store.save_widget(&key, &model).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1, "same widget key must upsert, not duplicate");
        assert_eq!(loaded[0].1.total_samples, 99);
    }

    #[test]
    fn default_path_lives_under_kibad_dir() {
        let path = Store::default_path().unwrap();
        assert!(path.ends_with("kibad/UI.dat"));
    }
}
