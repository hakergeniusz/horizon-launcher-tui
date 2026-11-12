//! Launcher settings (`settings.json`) and this tool's own overrides.
//!
//! The launcher keeps its settings in `<root>/settings.json`; the field this
//! tool cares about is `native_profile` (`performance` | `dev` | `release`).
//! This tool **reads** that file and never writes it. Overrides made through
//! this tool go into its own small file, `<root>/horizon-launcher-tui.json`,
//! which wins over `settings.json` when present. With neither, the profile
//! falls back to the last one recorded in `native/built.json`, then to
//! `performance` (the launcher's default).

use super::paths::LauncherPaths;
use super::profile::Profile;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// The version the launcher has selected (`settings.json: selected`).
    pub selected: Option<String>,
    /// The profile the launcher itself would build with.
    pub launcher_profile: Option<Profile>,
    /// The profile overridden through this tool (its own overrides file).
    pub tool_profile: Option<Profile>,
    /// The last profile anything built with (`native/built.json` keys).
    pub built_profile: Option<Profile>,
}

impl Settings {
    /// Read the launcher's `settings.json` (missing file or fields are fine)
    /// plus `native/built.json`'s profile keys.
    pub fn load(paths: &LauncherPaths) -> Settings {
        let selected = read_string(&paths.settings_json(), "selected");
        let launcher_profile = read_string(&paths.settings_json(), "native_profile")
            .and_then(|s| Profile::parse(&s));
        let tool_profile = read_string(&paths.overrides_json(), "profile")
            .and_then(|s| Profile::parse(&s));
        let built_profile = super::records::read_records(&paths.built_json())
            .keys()
            .find_map(|k| Profile::parse(k));
        Settings {
            selected,
            launcher_profile,
            tool_profile,
            built_profile,
        }
    }

    /// The profile a build runs with: the tool override, else the launcher's
    /// `native_profile`, else the last built profile, else `performance`.
    pub fn effective_profile(&self) -> Profile {
        self.tool_profile
            .or(self.launcher_profile)
            .or(self.built_profile)
            .unwrap_or(Profile::Performance)
    }

    /// Store the profile override in this tool's own file (not the launcher's
    /// `settings.json`), keeping any other keys the file already has.
    pub fn store_profile(paths: &LauncherPaths, profile: Profile) -> Result<(), String> {
        let path = paths.overrides_json();
        let mut value: serde_json::Map<String, serde_json::Value> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        value.insert(
            "profile".into(),
            serde_json::Value::String(profile.name().into()),
        );
        let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&paths.root).map_err(|e| e.to_string())?;
        std::fs::write(&path, text + "\n").map_err(|e| e.to_string())
    }
}

fn read_string(path: &Path, key: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value[key].as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::testutil::temp_root;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn effective_profile_precedence() {
        let tmp = temp_root("settings-precedence");
        let paths = LauncherPaths { root: tmp.clone(), game_home: tmp.join("game") };
        // No files at all: the launcher's default.
        assert_eq!(Settings::load(&paths).effective_profile(), Profile::Performance);

        // built.json only.
        write(
            &paths.built_json(),
            r#"{"release": {"horizon": "a", "mixins": "b", "mods": {}}}"#,
        );
        assert_eq!(Settings::load(&paths).effective_profile(), Profile::Release);

        // The launcher's settings.json wins over built.json.
        write(
            &paths.settings_json(),
            r#"{"selected": "nightly-2026-11-08-fdc87ae", "native_profile": "performance"}"#,
        );
        assert_eq!(Settings::load(&paths).effective_profile(), Profile::Performance);

        // This tool's own override wins over both.
        Settings::store_profile(&paths, Profile::Dev).unwrap();
        assert_eq!(Settings::load(&paths).effective_profile(), Profile::Dev);
        let raw = std::fs::read_to_string(paths.overrides_json()).unwrap();
        assert!(raw.contains("\"profile\": \"dev\""), "{raw}");
    }

    #[test]
    fn real_settings_read_only() {
        let paths = LauncherPaths::discover();
        let settings = Settings::load(&paths);
        if !paths.settings_json().is_file() {
            return;
        }
        // The real file's profile parses (it says "performance" on this machine).
        assert!(settings.launcher_profile.is_some());
    }
}
