//! Native mod bundles: manifests and discovery.
//!
//! Layouts (verified against this machine and `launcher/src/native_mods.rs`):
//!
//! - extracted mod: `installed.incoming/<id>/horizon-native.json` +
//!   `installed.incoming/<id>/mixins/<id>/{horizon-mixin.toml,…}` — the extra
//!   `mixins/<id>/` level is the `.hnmod` zip's own layout; the *bundle root*
//!   this tool composes (and hashes) is `mixins/<id>`, so `horizon-mixin.toml`
//!   sits at the root of its compose dir.
//! - `horizon-native.json` (`schema` 1): `id`, `version`, optional `horizon`
//!   (the commit the mod was tested on), optional `mixins` (the horizon-mixins
//!   commit), optional `hooks` (the native hook points' version).
//! - `horizon-mixin.toml`: `[mod]` (`id`, `version`, `priority`), optional
//!   `[compatibility] horizon_revision`, `[[module]]` and `[[injection]]` tables.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "horizon-mixin.toml";
pub const NATIVE_MANIFEST: &str = "horizon-native.json";

/// One native mod, as a build takes it. `dir` is the **bundle root**: the
/// folder whose `horizon-mixin.toml` heads the composed copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub id: String,
    pub version: String,
    pub dir: PathBuf,
    /// The Horizon commit it builds on (`[compatibility] horizon_revision`,
    /// else the `.hnmod`'s own `horizon` field).
    pub horizon: Option<String>,
    /// The horizon-mixins commit it was tested with (its `.hnmod`'s).
    pub mixins: Option<String>,
    /// The native hook points' version it needs (its `.hnmod`'s `hooks`): it
    /// follows the game's hook catalog instead of pinning a commit.
    pub hooks: Option<u32>,
}

/// The parsed `[mod]`/`[compatibility]` head of a `horizon-mixin.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MixinManifest {
    pub id: String,
    pub version: String,
    pub priority: Option<i64>,
    pub horizon_revision: Option<String>,
    /// Number of `[[module]]` and `[[injection]]` tables, for summaries.
    pub modules: usize,
    pub injections: usize,
}

/// `[mod]` of a `horizon-mixin.toml`.
#[derive(Debug, Deserialize)]
struct ModTable {
    id: String,
    version: String,
    priority: Option<i64>,
}

/// `[compatibility]` of a `horizon-mixin.toml`.
#[derive(Debug, Deserialize)]
struct Compatibility {
    horizon_revision: Option<String>,
}

/// `horizon-native.json`, the `.hnmod` registry manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeManifest {
    pub id: String,
    pub version: String,
    pub horizon: Option<String>,
    pub mixins: Option<String>,
    pub hooks: Option<u32>,
}

impl NativeManifest {
    pub fn parse(bytes: &[u8]) -> Result<NativeManifest, String> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| format!("{NATIVE_MANIFEST}: {e}"))?;
        let field = |k: &str| value[k].as_str().map(str::to_owned);
        Ok(NativeManifest {
            id: field("id").ok_or(format!("{NATIVE_MANIFEST} has no id"))?,
            version: field("version").unwrap_or_else(|| "0.0.0".into()),
            horizon: field("horizon"),
            mixins: field("mixins"),
            hooks: value["hooks"].as_u64().and_then(|v| u32::try_from(v).ok()),
        })
    }
}

/// Parse a `horizon-mixin.toml`. Unknown tables/fields are ignored (the
/// launcher reads it leniently too; horizon-mixins does the strict read).
pub fn parse_mixin_manifest(bytes: &[u8]) -> Result<MixinManifest, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let value: toml::Table = text.parse().map_err(|e| format!("{MANIFEST}: {e}"))?;
    let head: ModTable = value
        .get("mod")
        .map(|m| m.clone().try_into())
        .transpose()
        .map_err(|e| format!("{MANIFEST}: [mod]: {e}"))?
        .ok_or(format!("{MANIFEST} has no [mod]"))?;
    let compat: Option<Compatibility> = value
        .get("compatibility")
        .map(|c| c.clone().try_into())
        .transpose()
        .map_err(|e| format!("{MANIFEST}: [compatibility]: {e}"))?;
    Ok(MixinManifest {
        id: head.id,
        version: head.version,
        priority: head.priority,
        horizon_revision: compat.and_then(|c| c.horizon_revision),
        modules: value.get("module").and_then(as_array_len).unwrap_or(0),
        injections: value.get("injection").and_then(as_array_len).unwrap_or(0),
    })
}

fn as_array_len(value: &toml::Value) -> Option<usize> {
    value.as_array().map(|a| a.len())
}

/// Rewrite `[compatibility] horizon_revision` in a manifest **copy**, the way
/// the launcher's `repin` does: parse to a TOML table, replace the one field,
/// serialize. The original bundle's file is never touched by this function —
/// it returns the new text and the caller writes it onto its own copy.
pub fn rewrite_pin(manifest_bytes: &[u8], revision: &str) -> Result<String, String> {
    let text = std::str::from_utf8(manifest_bytes).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let mut value: toml::Table = text.parse().map_err(|e| format!("{MANIFEST}: {e}"))?;
    match value.get_mut("compatibility") {
        Some(toml::Value::Table(compat)) => {
            compat.insert("horizon_revision".into(), revision.into());
        }
        _ => {
            // No compatibility table yet: create one.
            let mut compat = toml::Table::new();
            compat.insert("horizon_revision".into(), revision.into());
            value.insert("compatibility".into(), compat.into());
        }
    }
    toml::to_string(&value).map_err(|e| format!("{MANIFEST}: rewrite: {e}"))
}

/// Read the bundle rooted at `dir` (a folder with `horizon-mixin.toml` at its
/// root). `mixins` is the `.hnmod`'s `mixins` field, when a native manifest
/// sits beside the bundle.
pub fn bundle_from_dir(dir: &Path, mixins: Option<String>) -> Result<Bundle, String> {
    let manifest_path = dir.join(MANIFEST);
    let bytes = std::fs::read(&manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let manifest = parse_mixin_manifest(&bytes)?;
    Ok(Bundle {
        id: manifest.id,
        version: manifest.version,
        dir: dir.to_path_buf(),
        horizon: manifest.horizon_revision,
        mixins,
        hooks: None,
    })
}

/// Read one installed mod's folder (`installed.incoming/<id>/`): a native
/// manifest beside a `mixins/<id>/` bundle. Mirrors the launcher's
/// `bundle_in`, including the pin cross-check.
pub fn bundle_in(dir: &Path) -> Result<Option<Bundle>, String> {
    if dir.join(MANIFEST).is_file() {
        return bundle_from_dir(dir, None).map(Some);
    }
    let native_path = dir.join(NATIVE_MANIFEST);
    if !native_path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&native_path)
        .map_err(|e| format!("{}: {e}", native_path.display()))?;
    let native = NativeManifest::parse(&bytes)?;
    let found = bundle_from_dir(&dir.join("mixins").join(&native.id), native.mixins.clone())?;
    match (&found.horizon, &native.horizon) {
        (Some(pinned), Some(native_pin)) if pinned != native_pin => Err(format!(
            "{}: {NATIVE_MANIFEST} pins Horizon {}, its {MANIFEST} {pinned}.",
            native.id,
            native_pin[..8.min(native_pin.len())].to_owned()
        )),
        (_, native_pin) => Ok(Some(Bundle {
            horizon: found.horizon.or_else(|| native_pin.clone()),
            hooks: native.hooks,
            ..found
        })),
    }
}

/// Every bundle under `folder`: either `folder` is one bundle, or each
/// subdirectory is one (the `installed.incoming` layout), in sorted path
/// order — the order the launcher discovers in, and the order that decides
/// trait dispatch and the version hash.
pub fn bundles(folder: &Path) -> Result<Vec<Bundle>, String> {
    if let Some(found) = bundle_in(folder)? {
        return Ok(vec![found]);
    }
    let mut dirs = super::paths::list_dirs(folder);
    if dirs.is_empty() {
        return Err(format!(
            "{} holds no native mod ({MANIFEST} or {NATIVE_MANIFEST}).",
            folder.display()
        ));
    }
    let mut found = Vec::new();
    for dir in dirs.drain(..) {
        match bundle_in(&dir)? {
            Some(bundle) => found.push(bundle),
            None => {
                return Err(format!(
                    "{} holds no native mod ({MANIFEST} or {NATIVE_MANIFEST}).",
                    dir.display()
                ))
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::testutil::temp_root;

    pub const SAMPLE_MIXIN: &str = r#"[mod]
id = "freecam"
version = "0.1.0"
priority = 100

# The commit it was tested on.
[compatibility]
horizon_revision = "af55a734865da4f1b11836164a979ff06a1f0449"

[[module]]
id = "runtime"
file = "crates/horizon-client/src/game.rs"
name = "freecam"
source = "runtime.rs"

[[injection]]
id = "key"
kind = "HEAD"
target = "horizon_client::game::native_hooks::Game<'_>::native_key"
helper = "key.rs"
"#;

    #[test]
    fn parses_mixin_manifest() {
        let m = parse_mixin_manifest(SAMPLE_MIXIN.as_bytes()).unwrap();
        assert_eq!(m.id, "freecam");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.priority, Some(100));
        assert_eq!(
            m.horizon_revision.as_deref(),
            Some("af55a734865da4f1b11836164a979ff06a1f0449")
        );
        assert_eq!(m.modules, 1);
        assert_eq!(m.injections, 1);
    }

    #[test]
    fn rewrites_pin_only() {
        let rewritten = rewrite_pin(SAMPLE_MIXIN.as_bytes(), "2ebf00a0281c592f79191e76fce0e7d522c6f425").unwrap();
        let m = parse_mixin_manifest(rewritten.as_bytes()).unwrap();
        assert_eq!(m.id, "freecam", "other fields survive");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.injections, 1);
        assert_eq!(
            m.horizon_revision.as_deref(),
            Some("2ebf00a0281c592f79191e76fce0e7d522c6f425")
        );
    }

    /// The sample manifest, with its `[mod] id` replaced (the id in the
    /// manifest must match the folder the mod sits in).
    fn mixin_for(id: &str) -> String {
        SAMPLE_MIXIN.replace("\"freecam\"", &format!("\"{id}\""))
    }

    #[test]
    fn reads_installed_layout_and_cross_check() {
        let tmp = temp_root("manifests-installed");
        let dir = tmp.join("installed.incoming").join("zoom");
        let bundle_root = dir.join("mixins").join("zoom");
        std::fs::create_dir_all(&bundle_root).unwrap();
        std::fs::write(bundle_root.join(MANIFEST), mixin_for("zoom")).unwrap();
        std::fs::write(
            dir.join(NATIVE_MANIFEST),
            br#"{"game":"26.3","hooks":1,"horizon":"af55a734865da4f1b11836164a979ff06a1f0449","id":"zoom","mixins":"5a64af938ec86b00d53d16a408aabc0038fb3e57","schema":1,"version":"2.0.0"}"#,
        )
        .unwrap();
        let found = bundle_in(&dir).unwrap().unwrap();
        assert_eq!(found.id, "zoom");
        assert_eq!(found.version, "0.1.0", "the mixin manifest's version");
        assert_eq!(found.hooks, Some(1));
        assert_eq!(
            found.mixins.as_deref(),
            Some("5a64af938ec86b00d53d16a408aabc0038fb3e57")
        );
        // A native manifest pin disagreeing with the mixin manifest is an error.
        std::fs::write(
            dir.join(NATIVE_MANIFEST),
            br#"{"game":"26.3","horizon":"ba6344a59cb9e755b730dcf02285fc0d33b97d3c","id":"zoom","schema":1,"version":"2.0.0"}"#,
        )
        .unwrap();
        assert!(bundle_in(&dir).is_err());
    }

    #[test]
    fn bundles_sort_by_dir() {
        let tmp = temp_root("manifests-sorted");
        let inc = tmp.join("installed.incoming");
        for id in ["zoom", "chatheads", "fps_overlay"] {
            let dir = inc.join(&id);
            let root = dir.join("mixins").join(&id);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(MANIFEST), mixin_for(&id)).unwrap();
            std::fs::write(
                dir.join(NATIVE_MANIFEST),
                format!(r#"{{"game":"26.3","id":"{id}","schema":1,"version":"1.0.0"}}"#),
            )
            .unwrap();
        }
        let found = bundles(&inc).unwrap();
        let ids: Vec<&str> = found.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["chatheads", "fps_overlay", "zoom"]);
    }
}
