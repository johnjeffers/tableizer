//! On-disk persistence in the OS config dir: the recent-files list, per-file saved views, and the
//! theme/appearance settings. Each is a small self-contained submodule.

/// Recent-files list, persisted in the OS *config* dir (separate from the index cache in the state dir).
pub mod recent {
    use std::path::{Path, PathBuf};

    fn file() -> Option<PathBuf> {
        let base = directories::BaseDirs::new()?;
        Some(base.config_dir().join("tableizer").join("recent.txt"))
    }

    pub fn load() -> Vec<PathBuf> {
        let Some(f) = file() else {
            return Vec::new();
        };
        std::fs::read_to_string(f)
            .map(|s| s.lines().map(PathBuf::from).collect())
            .unwrap_or_default()
    }

    pub fn add(recent: &mut Vec<PathBuf>, path: &Path) {
        recent.retain(|p| p != path);
        recent.insert(0, path.to_path_buf());
        recent.truncate(10);
        let Some(f) = file() else {
            return;
        };
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = recent
            .iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(f, body);
    }

    /// Empty the recent-files list and delete its on-disk store.
    pub fn clear(recent: &mut Vec<PathBuf>) {
        recent.clear();
        if let Some(f) = file() {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Saved-view persistence in the OS config dir, keyed by source path.
pub mod views {
    use crate::model::SavedView;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};

    fn file(source: &Path) -> Option<PathBuf> {
        let base = directories::BaseDirs::new()?;
        let mut hasher = DefaultHasher::new();
        source.hash(&mut hasher);
        let name = format!("{:016x}.json", hasher.finish());
        Some(base.config_dir().join("tableizer").join("views").join(name))
    }

    pub fn load(source: &Path) -> Option<SavedView> {
        let data = std::fs::read(file(source)?).ok()?;
        serde_json::from_slice(&data).ok()
    }

    pub fn save(source: &Path, view: &SavedView) {
        let Some(path) = file(source) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec_pretty(view) {
            let _ = std::fs::write(path, data);
        }
    }
}

/// Theme-settings persistence in the OS config dir.
pub mod prefs {
    use crate::theme::Settings;
    use std::path::PathBuf;

    fn file() -> Option<PathBuf> {
        let base = directories::BaseDirs::new()?;
        Some(base.config_dir().join("tableizer").join("theme.json"))
    }

    pub fn load() -> Settings {
        file()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(settings: &Settings) {
        let Some(path) = file() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec_pretty(settings) {
            let _ = std::fs::write(path, data);
        }
    }
}
