//! Discovery of the Horizon launcher's data layout.
//!
//! Ground truth (verified on this machine against the launcher's own state and
//! `launcher/src/native_mods.rs`):
//!
//! - launcher root: `~/.local/share/horizon-launcher/` (overridable with
//!   `HORIZON_LAUNCHER_DIR` for tests and exotic setups)
//! - pinned game source: `native/source/<full horizon rev>/`
//! - mixins CLI: `native/tools/<rev[..12]>/bin/horizon-mixins`
//! - pinned Rust toolchain: `rust/<version>-x86_64-unknown-linux-gnu/bin/{cargo,rustc}`
//!   (the `/launcher/rust/bin` layout does **not** exist)
//! - pinned CARGO_HOME: `<root>/cargo`
//! - extracted mods: `native/installed.incoming/<mod-id>/{horizon-native.json,mixins/<mod-id>/…}`
//! - shared cargo target dir for launcher builds: `native/target/`
//! - built versions: `versions/<tag>/horizon-client` (+ `.installed` marker)
//!
//! Scoping: the game data dir follows `HORIZON_HOME` (which the game and the
//! GUI launcher honor); when an alternate launcher root is set
//! (`HORIZON_LAUNCHER_DIR`) with no `HORIZON_HOME`, the game home stays
//! beside that root (`<root>/game-home`) so a scratch install can never
//! write into the player's real game data.

use std::path::{Path, PathBuf};

pub const DEFAULT_ROOT: &str = ".local/share/horizon-launcher";
pub const DEFAULT_GAME_HOME: &str = ".local/share/horizon";

/// Every path this tool needs from the launcher's data root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherPaths {
    /// The launcher's data root (`~/.local/share/horizon-launcher`).
    pub root: PathBuf,
    /// The game's data dir (the client's cwd; `HORIZON_HOME` wins over the default).
    pub game_home: PathBuf,
}

impl LauncherPaths {
    /// Discover the launcher layout. `HORIZON_LAUNCHER_DIR` overrides the root
    /// (for pointing this tool at another install); the game home follows the
    /// launcher itself (`HORIZON_HOME`, which the game and the GUI launcher
    /// honor too).
    ///
    /// Safety rule: when the root is overridden and the game home is not, the
    /// game home defaults to `<root>/game-home`, **not** the real one — a
    /// scratch root must never write into the player's game data.
    pub fn discover() -> Self {
        let root = std::env::var_os("HORIZON_LAUNCHER_DIR").map(PathBuf::from);
        let game_home = std::env::var_os("HORIZON_HOME").map(PathBuf::from);
        Self::with_roots(root, game_home)
    }

    /// The pure core of [`Self::discover`]: an explicit root with an implicit
    /// game home keeps the game data beside the root (`<root>/game-home`),
    /// never the player's real game dir.
    pub fn with_roots(root: Option<PathBuf>, game_home: Option<PathBuf>) -> Self {
        let explicit_root = root.filter(|p| !p.as_os_str().is_empty());
        let root = explicit_root
            .clone()
            .unwrap_or_else(|| home_dir().join(DEFAULT_ROOT));
        let game_home = game_home
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| match &explicit_root {
                Some(_) => root.join("game-home"),
                None => home_dir().join(DEFAULT_GAME_HOME),
            });
        Self { root, game_home }
    }

    pub fn native(&self) -> PathBuf {
        self.root.join("native")
    }

    /// The pinned game source for `rev` (`native/source/<rev>`).
    pub fn source(&self, rev: &str) -> PathBuf {
        self.native().join("source").join(rev)
    }

    /// Every pinned game source rev present, sorted (the names are full commits).
    pub fn source_revs(&self) -> Vec<String> {
        let mut revs: Vec<String> = list_dirs(&self.native().join("source"))
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        revs.sort();
        revs
    }

    /// The horizon-mixins CLI for `rev` (`native/tools/<rev[..12]>/bin/horizon-mixins`).
    pub fn mixins_tool(&self, rev: &str) -> PathBuf {
        self.native()
            .join("tools")
            .join(&rev[..rev.len().min(12)])
            .join("bin")
            .join("horizon-mixins")
    }

    /// The pinned Rust toolchain dir, e.g. `rust/1.99.1-x86_64-unknown-linux-gnu`.
    /// The launcher pins `VERSION = "1.99.1"`; this tool accepts any single
    /// installed one, preferring that version.
    pub fn rust_toolchain(&self) -> Option<(String, PathBuf)> {
        let dir = self.root.join("rust");
        let mut found: Vec<(String, PathBuf)> = list_dirs(&dir)
            .iter()
            .filter_map(|p| {
                let name = p.file_name()?.to_string_lossy().into_owned();
                // `<version>-<target-triple>`; the version is everything before
                // the first `-` (versions carry no dashes themselves).
                let ver = name.split('-').next()?.to_string();
                (p.join("bin").join("cargo").is_file()).then_some((ver, p.clone()))
            })
            .collect();
        found.sort();
        found.iter().find(|(v, _)| v == "1.99.1").cloned().or_else(|| found.pop())
    }

    /// The pinned `CARGO_HOME` (`<root>/cargo`).
    pub fn cargo_home(&self) -> PathBuf {
        self.root.join("cargo")
    }

    /// The shared cargo target dir for launcher builds (`native/target/`).
    /// Never replaced by a second persistent one.
    pub fn native_target(&self) -> PathBuf {
        self.native().join("target")
    }

    /// Extracted (installed) mods: `native/installed.incoming/`.
    pub fn installed_incoming(&self) -> PathBuf {
        self.native().join("installed.incoming")
    }

    /// A mod's extraction dir (`native/installed.incoming/<id>/`).
    pub fn installed_mod(&self, id: &str) -> PathBuf {
        self.installed_incoming().join(id)
    }

    /// The compose dir for the `--mods` input of `horizon-mixins apply`
    /// (the launcher calls it `native/mods.incoming`).
    pub fn compose_dir(&self) -> PathBuf {
        self.native().join("mods.incoming")
    }

    /// The stage dir the mixin apply writes into (the launcher's `native/stage.new`).
    pub fn stage_dir(&self) -> PathBuf {
        self.native().join("stage.new")
    }

    /// `native/built.json`, the launcher's last-build record per profile.
    pub fn built_json(&self) -> PathBuf {
        self.native().join("built.json")
    }

    /// `native/built.json.bak`, this tool's one-step backup before a write.
    pub fn built_json_bak(&self) -> PathBuf {
        self.native().join("built.json.bak")
    }

    /// The launcher's own build log (`native/build.log`).
    pub fn build_log(&self) -> PathBuf {
        self.native().join("build.log")
    }

    /// Installed game versions (`versions/`).
    pub fn versions(&self) -> PathBuf {
        self.root.join("versions")
    }

    /// One installed version (`versions/<tag>/`).
    pub fn version_dir(&self, tag: &str) -> PathBuf {
        self.versions().join(tag)
    }

    /// The native mods the game itself keeps as bundles (`<game home>/horizon-native-mods/`).
    pub fn game_native_mods(&self) -> PathBuf {
        self.game_home.join("horizon-native-mods")
    }

    /// `horizon-mods.txt`: the per-mod enable state (`mod.<id>:true|false`),
    /// shared with the game and the GUI launcher.
    pub fn mods_list(&self) -> PathBuf {
        self.game_home.join("horizon-mods.txt")
    }

    /// The launcher's own `settings.json`.
    pub fn settings_json(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    /// This tool's own overrides file, next to the launcher's state. Written
    /// only when the user changes a setting through this tool; never touches
    /// the launcher's `settings.json`.
    pub fn overrides_json(&self) -> PathBuf {
        self.root.join("horizon-launcher-tui.json")
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Directories directly under `dir`, sorted. Empty when `dir` is missing.
pub fn list_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolchain_picks_pinned_version() {
        let tmp = crate::launcher::testutil::temp_root("paths-toolchain");
        let paths = LauncherPaths {
            root: tmp.clone(),
            game_home: tmp.join("game"),
        };
        assert!(paths.rust_toolchain().is_none());
        let mk = |v: &str| {
            let d = tmp.join("rust").join(format!("{v}-x86_64-unknown-linux-gnu"));
            std::fs::create_dir_all(d.join("bin")).unwrap();
            std::fs::write(d.join("bin").join("cargo"), "").unwrap();
        };
        mk("1.80.0");
        mk("1.99.1");
        let (ver, dir) = paths.rust_toolchain().unwrap();
        assert_eq!(ver, "1.99.1");
        assert!(dir.ends_with("1.99.1-x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn mixins_tool_uses_12_char_rev() {
        let tmp = crate::launcher::testutil::temp_root("paths-mixins");
        let paths = LauncherPaths {
            root: tmp.clone(),
            game_home: tmp.join("game"),
        };
        let tool = paths.mixins_tool("5a64af938ec86b00d53d16a408aabc0038fb3e57");
        assert!(tool.ends_with("tools/5a64af938ec8/bin/horizon-mixins"));
    }

    #[test]
    fn real_launcher_layout_present() {
        // Read-only probe of the real machine's layout; tolerated to be absent.
        let paths = LauncherPaths::discover();
        if !paths.root.is_dir() {
            return;
        }
        assert!(paths.versions().is_dir(), "versions/ missing under {}", paths.root.display());
    }

    #[test]
    fn explicit_root_never_defaults_to_real_game_home() {
        // A scratch root with no explicit game home keeps the game data
        // beside it — never the player's real game dir.
        let tmp = crate::launcher::testutil::temp_root("paths-scoping");
        let paths = LauncherPaths::with_roots(Some(tmp.clone()), None);
        assert_eq!(paths.root, tmp);
        assert_eq!(paths.game_home, tmp.join("game-home"));
        assert_ne!(
            paths.game_home,
            home_dir().join(DEFAULT_GAME_HOME),
            "an alternate root must not write into the real game data"
        );
        // An explicit game home always wins.
        let paths = LauncherPaths::with_roots(Some(tmp.clone()), Some(tmp.join("game")));
        assert_eq!(paths.game_home, tmp.join("game"));
        // The default root keeps the real default game home.
        let paths = LauncherPaths::with_roots(None, None);
        assert_eq!(paths.root, home_dir().join(DEFAULT_ROOT));
        assert_eq!(paths.game_home, home_dir().join(DEFAULT_GAME_HOME));
    }
}
