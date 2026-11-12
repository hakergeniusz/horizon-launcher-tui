//! `.hnmod` bundles: reading, signature checking, installing, uninstalling.
//!
//! An `.hnmod` is a zip holding a registry manifest (`horizon-native.json`)
//! beside the bundle under `mixins/<id>/`, plus optional files under `data/`
//! — "Rust and TOML under mixins/<id>/, plus files under its data/, nothing
//! run" (the launcher checks each bundle the same way the registry does).
//! Install extracts it preserving the zip layout into
//! `native/installed.incoming/<id>/`.
//!
//! Signature checking: when a matching `.pub` key and `.minisig` sit next to
//! the `.hnmod` (the registry's download artifacts), this tool verifies the
//! bundle with the launcher's registry CLI
//! (`~/Projects/horizon/target/release/cargo-horizon-mod check …`, override
//! with `HORIZON_CARGO_MOD_CLI`). Without a key, verification is **skipped
//! with a warning** and the install proceeds. A failing check aborts it.
//!
//! Durability: the GUI launcher rebuilds `installed.incoming/` from the game's
//! `<game home>/horizon-native-mods/` on every sync, so an install here also
//! copies the `.hnmod` there (best effort). Uninstall removes both and turns
//! the mod off in `horizon-mods.txt`.

use super::manifests::NativeManifest;
use super::paths::LauncherPaths;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub id: String,
    pub version: String,
    pub files: usize,
    /// The verification note ("verified", "skipped: no .pub key next to the
    /// bundle", …).
    pub signature: String,
    pub extracted_to: PathBuf,
    /// Whether the bundle was also copied to the game's `horizon-native-mods/`.
    pub copied_to_game_dir: bool,
}

/// The registry CLI used for signature checks, when present: the launcher
/// repo's built `cargo-horizon-mod`
/// (`~/Projects/horizon/target/release/cargo-horizon-mod`, override with
/// `HORIZON_CARGO_MOD_CLI`).
pub fn registry_cli() -> Option<PathBuf> {
    let path = std::env::var_os("HORIZON_CARGO_MOD_CLI")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("Projects/horizon/target/release/cargo-horizon-mod"))
        })?;
    path.is_file().then_some(path)
}

/// Read the native manifest out of an `.hnmod` zip.
pub fn read_manifest(path: &Path) -> Result<NativeManifest, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| format!("{}: not a readable .hnmod zip: {e}", path.display()))?;
    let mut bytes = Vec::new();
    zip.by_name(super::manifests::NATIVE_MANIFEST)
        .map_err(|e| {
            format!(
                "{}: {}: {e}",
                path.display(),
                super::manifests::NATIVE_MANIFEST
            )
        })?
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    NativeManifest::parse(&bytes)
}

/// Verify the bundle against a `.pub`/`.minisig` pair next to it, using the
/// registry CLI. Returns the note, or the failure.
fn verify_signature(hnmod: &Path) -> Result<String, String> {
    let stem = hnmod
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{}: no file name", hnmod.display()))?;
    let dir = hnmod.parent().unwrap_or(Path::new("."));
    let key = dir.join(format!("{stem}.pub"));
    let sig = dir.join(format!("{stem}.minisig"));
    if !key.is_file() || !sig.is_file() {
        return Ok(format!(
            "skipped: no {} next to the bundle (unverified)",
            key.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
    }
    let Some(cli) = registry_cli() else {
        return Ok("skipped: the registry CLI (cargo-horizon-mod) is not built".into());
    };
    let key_text = std::fs::read_to_string(&key).map_err(|e| format!("{}: {e}", key.display()))?;
    let key_text = key_text.trim().to_string();
    let mut cmd = Command::new(&cli);
    cmd.arg("check")
        .arg(hnmod)
        .arg("--key")
        .arg(&key_text)
        .arg("--signature")
        .arg(&sig);
    let out = cmd
        .output()
        .map_err(|e| format!("{}: {e}", cli.display()))?;
    if out.status.success() {
        Ok("verified with cargo-horizon-mod".into())
    } else {
        Err(format!(
            "signature check failed ({}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The zip entries an `.hnmod` may hold: the manifest, the bundle under
/// `mixins/<id>/`, optional data under `data/`. Anything else is refused.
fn checked_entries(
    archive: &mut zip::ZipArchive<std::fs::File>,
    id: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue; // directory entry
        }
        let ok = name == super::manifests::NATIVE_MANIFEST
            || (name.starts_with(&format!("mixins/{id}/"))
                && !name[format!("mixins/{id}/").len()..].contains(".."))
            || name.starts_with("data/");
        if !ok {
            return Err(format!("{name}: not part of an .hnmod bundle"));
        }
        if name.contains("..") || name.starts_with('/') || name.contains('\\') {
            return Err(format!("{name}: unsafe path"));
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{name}: {e}"))?;
        out.push((name, bytes));
    }
    if out.is_empty() {
        return Err("the archive is empty".into());
    }
    Ok(out)
}

/// Install an `.hnmod`: verify when possible, then extract into
/// `installed.incoming/<id>/` preserving the zip layout.
pub fn install(paths: &LauncherPaths, hnmod: &Path) -> Result<InstallReport, String> {
    let signature = verify_signature(hnmod)?;
    let manifest = read_manifest(hnmod)?;
    let id = manifest.id.clone();

    let file = std::fs::File::open(hnmod).map_err(|e| format!("{}: {e}", hnmod.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("{}: {e}", hnmod.display()))?;
    let entries = checked_entries(&mut archive, &id)?;

    // Replacing a mod: the old extraction goes first.
    let out_dir = paths.installed_mod(&id);
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    }
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    for (name, bytes) in &entries {
        let to = out_dir.join(name);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&to, bytes).map_err(|e| format!("{}: {e}", to.display()))?;
    }

    // Keep the bundle in the game's own dir, where the launcher re-syncs from.
    let mut copied_to_game_dir = false;
    let game_dir = paths.game_native_mods();
    if game_dir.is_dir() || std::fs::create_dir_all(&game_dir).is_ok() {
        let file_name = hnmod
            .file_name()
            .map(|n| n.to_owned())
            .unwrap_or_else(|| format!("{id}.hnmod").into());
        let to = game_dir.join(file_name);
        if std::fs::copy(hnmod, &to).is_ok() {
            copied_to_game_dir = true;
        }
    }

    Ok(InstallReport {
        id,
        version: manifest.version,
        files: entries.len(),
        signature,
        extracted_to: out_dir,
        copied_to_game_dir,
    })
}

/// Remove an installed mod: the extraction, the game dir's bundle, and its
/// enable line (turned off, as the launcher's own uninstall leaves `off`).
pub fn uninstall(paths: &LauncherPaths, id: &str) -> Result<Vec<String>, String> {
    if id.contains('/') || id.contains('\\') || id.starts_with('.') {
        return Err(format!("{id:?} is not a mod id"));
    }
    let mut did = Vec::new();
    let extracted = paths.installed_mod(id);
    if extracted.is_dir() {
        std::fs::remove_dir_all(&extracted).map_err(|e| format!("{}: {e}", extracted.display()))?;
        did.push(format!("removed {}", extracted.display()));
    }
    let bundle = paths.game_native_mods().join(format!("{id}.hnmod"));
    if bundle.is_file() {
        std::fs::remove_file(&bundle).map_err(|e| format!("{}: {e}", bundle.display()))?;
        did.push(format!("removed {}", bundle.display()));
    }
    super::modslist::set_enabled(&paths.mods_list(), id, false)?;
    did.push(format!("turned off in {}", paths.mods_list().display()));
    Ok(did)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::testutil::temp_root;
    use std::io::Write;

    /// A tiny .hnmod zip, written the way the registry writes them.
    fn write_hnmod(path: &Path, id: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(crate::launcher::manifests::NATIVE_MANIFEST, options)
            .unwrap();
        zip.write_all(
            format!(
                r#"{{"game":"26.3","hooks":1,"horizon":"2ebf00a0281c592f79191e76fce0e7d522c6f425","id":"{id}","mixins":"5a64af938ec86b00d53d16a408aabc0038fb3e57","schema":1,"version":"1.0.0"}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        zip.start_file(format!("mixins/{id}/horizon-mixin.toml"), options)
            .unwrap();
        zip.write_all(
            format!(
                "[mod]\nid = \"{id}\"\nversion = \"1.0.0\"\n\n[compatibility]\nhorizon_revision = \"2ebf00a0281c592f79191e76fce0e7d522c6f425\"\n"
            )
            .as_bytes(),
        )
        .unwrap();
        zip.start_file(format!("mixins/{id}/mod.rs"), options)
            .unwrap();
        zip.write_all(b"pub const MOD: u32 = 1;\n").unwrap();
        zip.start_file("data/settings.toml", options).unwrap();
        zip.write_all(b"scale = 1\n").unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn reads_manifest_and_installs_layout() {
        let tmp = temp_root("hnmod-install");
        let paths = LauncherPaths {
            root: tmp.clone(),
            game_home: tmp.join("game"),
        };
        std::fs::create_dir_all(paths.game_native_mods()).unwrap();
        let hnmod = tmp.join("zoomy.hnmod");
        write_hnmod(&hnmod, "zoomy");

        let manifest = read_manifest(&hnmod).unwrap();
        assert_eq!(manifest.id, "zoomy");
        assert_eq!(manifest.hooks, Some(1));

        let report = install(&paths, &hnmod).unwrap();
        assert_eq!(report.id, "zoomy");
        assert_eq!(report.files, 4);
        assert!(report.signature.contains("skipped"), "{}", report.signature);
        // Zip layout preserved: horizon-native.json at the root, mixins/<id>/ below.
        assert!(report
            .extracted_to
            .join(crate::launcher::manifests::NATIVE_MANIFEST)
            .is_file());
        let mixin = report
            .extracted_to
            .join("mixins")
            .join("zoomy")
            .join("horizon-mixin.toml");
        assert!(mixin.is_file());
        assert!(report.extracted_to.join("mixins/zoomy/mod.rs").is_file());
        assert!(report.extracted_to.join("data/settings.toml").is_file());
        // Also kept where the launcher re-syncs from.
        assert!(report.copied_to_game_dir);
        assert!(paths.game_native_mods().join("zoomy.hnmod").is_file());

        // Reinstall replaces the extraction.
        let again = install(&paths, &hnmod).unwrap();
        assert_eq!(again.id, "zoomy");
    }

    #[test]
    fn foreign_zip_entries_are_refused() {
        let tmp = temp_root("hnmod-refuse");
        let paths = LauncherPaths {
            root: tmp.clone(),
            game_home: tmp.join("game"),
        };
        let hnmod = tmp.join("evil.hnmod");
        let file = std::fs::File::create(&hnmod).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(crate::launcher::manifests::NATIVE_MANIFEST, options)
            .unwrap();
        zip.write_all(br#"{"game":"26.3","id":"evil","schema":1,"version":"1.0.0"}"#)
            .unwrap();
        zip.start_file("mixins/evil/horizon-mixin.toml", options)
            .unwrap();
        zip.write_all(b"[mod]\nid = \"evil\"\nversion = \"1.0.0\"\n")
            .unwrap();
        zip.start_file("crates/horizon-client/src/evil.rs", options)
            .unwrap();
        zip.write_all(b"mod evil;").unwrap();
        zip.finish().unwrap();
        assert!(install(&paths, &hnmod).is_err());
    }

    #[test]
    fn uninstall_removes_all_three() {
        let tmp = temp_root("hnmod-uninstall");
        let paths = LauncherPaths {
            root: tmp.clone(),
            game_home: tmp.join("game"),
        };
        std::fs::create_dir_all(paths.game_native_mods()).unwrap();
        let hnmod = tmp.join("zoomy.hnmod");
        write_hnmod(&hnmod, "zoomy");
        install(&paths, &hnmod).unwrap();
        super::super::modslist::set_enabled(&paths.mods_list(), "zoomy", true).unwrap();

        let did = uninstall(&paths, "zoomy").unwrap();
        assert_eq!(did.len(), 3, "{did:?}");
        assert!(!paths.installed_mod("zoomy").exists());
        assert!(!paths.game_native_mods().join("zoomy.hnmod").exists());
        assert!(!super::super::modslist::is_enabled(
            &paths.mods_list(),
            "zoomy"
        ));
    }
}
