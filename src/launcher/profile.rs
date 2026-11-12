//! Build profiles, mirroring the launcher's `native_mods::Profile`.
//!
//! Ground truth (`launcher/src/native_mods.rs`): the player's choices are
//! `performance` (default), `dev` and `release`. `performance` compiles with the
//! cargo profile **`fast`** (this is what `native/built.json` keys on), `dev`
//! compiles with `fast` plus an opt-level override, `release` with `release`.
//! The name `fast` is accepted as an alias of `performance` (it was the old
//! name, and `Profile::parse` still maps it).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// The player default: cargo profile `fast`.
    #[default]
    Performance,
    /// Fast to rebuild for development: `fast` with `opt-level = 0` for the client.
    Dev,
    /// The release profile: cargo profile `release`.
    Release,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::Performance, Profile::Dev, Profile::Release];

    pub fn name(self) -> &'static str {
        match self {
            Profile::Performance => "performance",
            Profile::Dev => "dev",
            Profile::Release => "release",
        }
    }

    /// `performance` | `fast` (alias) | `dev` | `release`.
    pub fn parse(text: &str) -> Option<Profile> {
        match text.trim() {
            "performance" | "fast" => Some(Profile::Performance),
            "dev" => Some(Profile::Dev),
            "release" => Some(Profile::Release),
            _ => None,
        }
    }

    /// The cargo profile it compiles with (its folder in `native/target/`), and
    /// the extra `cargo --config` values it sets over it.
    pub fn cargo(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Profile::Performance => ("fast", &[]),
            Profile::Dev => ("fast", &["profile.fast.package.horizon-client.opt-level=0"]),
            Profile::Release => ("release", &[]),
        }
    }

    /// The folder the built binary lands in (`native/target/<profile>/…`).
    pub fn target_dir_name(self) -> &'static str {
        self.cargo().0
    }

    /// The key this build is recorded under in `native/built.json`. `Performance`
    /// and `Dev` share one (their last build in the `fast` target folder).
    pub fn record_key(self) -> String {
        self.cargo().0.to_string()
    }
}

impl std::fmt::Display for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names_and_aliases() {
        assert_eq!(Profile::parse("performance"), Some(Profile::Performance));
        assert_eq!(Profile::parse("fast"), Some(Profile::Performance));
        assert_eq!(Profile::parse("dev"), Some(Profile::Dev));
        assert_eq!(Profile::parse("release"), Some(Profile::Release));
        assert_eq!(Profile::parse("nope"), None);
    }

    #[test]
    fn performance_maps_to_fast_cargo_profile() {
        // Ground truth: the launcher's own `Profile::cargo` and the observed
        // `native/built.json` key ("fast") while `settings.json` says
        // `native_profile: "performance"`.
        assert_eq!(Profile::Performance.cargo().0, "fast");
        assert_eq!(Profile::Release.cargo().0, "release");
        assert_eq!(
            Profile::Dev.cargo().1,
            ["profile.fast.package.horizon-client.opt-level=0"]
        );
        assert_eq!(Profile::Dev.record_key(), "fast");
    }
}
