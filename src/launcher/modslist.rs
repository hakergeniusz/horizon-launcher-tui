//! The per-mod enable state and installed-mod listings.
//!
//! Ground truth: there is **no** disabled-set file in the launcher's data
//! root. The launcher stores per-mod enable state in the game's
//! `<game home>/horizon-mods.txt` as `mod.<id>:true|false` lines
//! (`launcher/src/browser/java.rs: is_enabled` / `set_enabled`), native mods
//! "on or off in horizon-mods.txt like any mod"
//! (`native_mods.rs: INSTALLED` doc comment). At build time the launcher
//! unpacks only the enabled `.hnmod` bundles from
//! `<game home>/horizon-native-mods/` into `native/installed.incoming/`
//! (fresh each time). A mod with no line counts as **off**.
//!
//! This tool uses the same file and the same semantics — so the TUI's
//! enable/disable and the GUI launcher's Mods page stay in agreement.

use super::paths::LauncherPaths;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `true` iff `horizon-mods.txt` has `mod.<id>:true`. No line means off.
pub fn is_enabled(mods_list: &Path, id: &str) -> bool {
    let key = format!("mod.{id}");
    std::fs::read_to_string(mods_list)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(':'))
        .any(|(k, v)| k.trim() == key && v.trim() == "true")
}

/// Turn mod `id` on (or off) in `horizon-mods.txt`, keeping every other line
/// — including the game's `key.*` bindings — untouched. Creates the file when
/// missing (alongside its game home).
pub fn set_enabled(mods_list: &Path, id: &str, on: bool) -> Result<(), String> {
    if let Some(parent) = mods_list.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = std::fs::read_to_string(mods_list).unwrap_or_default();
    let key = format!("mod.{id}");
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| l.split_once(':').is_none_or(|(k, _)| k.trim() != key))
        .map(str::to_owned)
        .collect();
    if on {
        lines.push(format!("{key}:true"));
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(mods_list, out).map_err(|e| format!("{}: {e}", mods_list.display()))
}

/// One row of the mods list: what this tool knows about a native mod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModRow {
    pub id: String,
    pub version: String,
    /// The Horizon commit its manifest pins (short form), if any.
    pub pinned_horizon: Option<String>,
    /// The native hook points' version it needs, if it follows the catalog.
    pub hooks: Option<u32>,
    pub enabled: bool,
    /// Extracted into `native/installed.incoming/` (what a build composes).
    pub extracted: bool,
    /// Present as an `.hnmod` in the game's `horizon-native-mods/`.
    pub in_game_dir: bool,
}

/// List every native mod this tool can see: the extracted ones
/// (`installed.incoming/`) plus the game dir's `.hnmod` bundles that are not
/// extracted. Sorted by id.
pub fn list_mods(paths: &LauncherPaths) -> Result<Vec<ModRow>, String> {
    let mods_list = paths.mods_list();
    let mut rows: BTreeMap<String, ModRow> = BTreeMap::new();

    // Extracted bundles.
    let bundles = match super::manifests::bundles(&paths.installed_incoming()) {
        Ok(found) => found,
        Err(_) => Vec::new(), // no installed.incoming yet, or an unreadable bundle
    };
    for bundle in bundles {
        let manifest_path = bundle.dir.join(super::manifests::MANIFEST);
        let pinned = std::fs::read(&manifest_path)
            .ok()
            .and_then(|b| super::manifests::parse_mixin_manifest(&b).ok())
            .and_then(|m| m.horizon_revision);
        let enabled = is_enabled(&mods_list, &bundle.id);
        rows.insert(
            bundle.id.clone(),
            ModRow {
                id: bundle.id.clone(),
                version: bundle.version,
                pinned_horizon: pinned.map(|r| shorten(&r)),
                hooks: bundle.hooks,
                enabled,
                extracted: true,
                in_game_dir: false,
            },
        );
    }

    // Game-dir bundles not extracted (visible but outside the build set).
    for hnmod in hnmod_files(&paths.game_native_mods()) {
        let Some(id) = hnmod_id(&hnmod) else { continue };
        let Some(id) = id else { continue };
        let entry = rows.entry(id.clone()).or_insert_with(|| ModRow {
            id: id.clone(),
            version: String::new(),
            pinned_horizon: None,
            hooks: None,
            enabled: is_enabled(&mods_list, &id),
            extracted: false,
            in_game_dir: true,
        });
        entry.in_game_dir = true;
        if !entry.extracted {
            // Read the version out of the zip's manifest when it is cheap to do.
            if let Ok(manifest) = super::hnmod::read_manifest(&hnmod) {
                entry.version = manifest.version;
                entry.hooks = manifest.hooks;
            }
        }
    }

    Ok(rows.into_values().collect())
}

fn hnmod_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .is_some_and(|e| e == "hnmod")
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// The id of an `.hnmod` file: read from its native manifest when possible.
/// Returns `None` for a file whose manifest is unreadable; `Some(None)` when
/// the zip does not open (still worth listing).
fn hnmod_id(path: &Path) -> Option<Option<String>> {
    match super::hnmod::read_manifest(path) {
        Ok(m) => Some(Some(m.id)),
        Err(_) => Some(None),
    }
}

/// Short form of a commit for display (`2ebf00a0…` → `2ebf00a`).
pub fn shorten(rev: &str) -> String {
    rev.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::testutil::temp_root;

    fn fixture(paths: &LauncherPaths) {
        std::fs::create_dir_all(paths.installed_incoming()).unwrap();
        std::fs::create_dir_all(&paths.game_home).unwrap();
        let zoom = paths.installed_mod("zoom").join("mixins").join("zoom");
        std::fs::create_dir_all(&zoom).unwrap();
        std::fs::write(
            zoom.join(super::super::manifests::MANIFEST),
            br#"[mod]
id = "zoom"
version = "2.0.0"

[compatibility]
horizon_revision = "6f8c7a0e5d531d3834c4d0be4659a1db8e7e5c2a"
"#,
        )
        .unwrap();
        std::fs::write(
            paths.installed_mod("zoom").join(super::super::manifests::NATIVE_MANIFEST),
            br#"{"game":"26.3","hooks":1,"horizon":"6f8c7a0e5d531d3834c4d0be4659a1db8e7e5c2a","id":"zoom","mixins":"0d6b103c70633d8fd9443d8d0acd3aadaacedb06","schema":1,"version":"2.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            paths.mods_list(),
            "mod.zoom:true\nmod.nofall:false\nkey.zoom.zoom:key.keyboard.c\n",
        )
        .unwrap();
    }

    #[test]
    fn enable_state_reads_and_writes() {
        let tmp = temp_root("modslist-state");
        let paths = LauncherPaths { root: tmp.clone(), game_home: tmp.join("game") };
        fixture(&paths);
        assert!(is_enabled(&paths.mods_list(), "zoom"));
        assert!(!is_enabled(&paths.mods_list(), "nofall"));
        assert!(!is_enabled(&paths.mods_list(), "absent"), "no line means off");

        set_enabled(&paths.mods_list(), "zoom", false).unwrap();
        assert!(!is_enabled(&paths.mods_list(), "zoom"));
        let text = std::fs::read_to_string(paths.mods_list()).unwrap();
        assert!(text.contains("mod.nofall:false"), "other lines kept: {text}");
        assert!(text.contains("key.zoom.zoom:key.keyboard.c"), "key bindings kept: {text}");

        set_enabled(&paths.mods_list(), "shulker_peek", true).unwrap();
        assert!(is_enabled(&paths.mods_list(), "shulker_peek"));
    }

    #[test]
    fn list_rows_merge_extracted_and_game_dir() {
        let tmp = temp_root("modslist-rows");
        let paths = LauncherPaths { root: tmp.clone(), game_home: tmp.join("game") };
        fixture(&paths);
        let rows = list_mods(&paths).unwrap();
        assert_eq!(rows.len(), 1);
        let zoom = &rows[0];
        assert_eq!(zoom.id, "zoom");
        assert_eq!(zoom.version, "2.0.0");
        assert!(zoom.enabled);
        assert!(zoom.extracted);
        assert_eq!(zoom.pinned_horizon.as_deref(), Some("6f8c7a0"));
    }

    #[test]
    fn real_launcher_mods_list_counts() {
        // Read-only probe of the real machine; tolerated to be absent.
        let paths = LauncherPaths::discover();
        if !paths.installed_incoming().is_dir() {
            return;
        }
        let rows = list_mods(&paths).unwrap();
        let extracted: Vec<&ModRow> = rows.iter().filter(|r| r.extracted).collect();
        // Every extracted bundle appears exactly once.
        let mut ids: Vec<&str> = extracted.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), extracted.len());
    }
}
