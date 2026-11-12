//! `doctor`: read-only health report of the launcher's native build setup.

use super::paths::LauncherPaths;
use super::plan::{MIXINS_REV, RUST_VERSION};
use super::profile::Profile;
use super::records::{last_record, read_records};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub severity: Severity,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
    pub horizon_rev: Option<String>,
    pub mixins_rev: Option<String>,
    pub profile: Option<Profile>,
    pub installed_client: Option<(String, String)>, // (tag, sha256)
    pub mods_extracted: usize,
    pub mods_enabled: usize,
}

impl DoctorReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(rev) = &self.horizon_rev {
            out.push_str(&format!("Horizon revision: {} (pinned source)\n", &rev[..7.min(rev.len())]));
        }
        if let Some(rev) = &self.mixins_rev {
            out.push_str(&format!("Mixins revision:  {} \n", &rev[..12.min(rev.len())]));
        }
        if let Some(profile) = self.profile {
            out.push_str(&format!("Build profile:    {} (cargo profile {})\n", profile.name(), profile.cargo().0));
        }
        if let Some((tag, sha)) = &self.installed_client {
            out.push_str(&format!("Installed client: {}\n  sha256: {sha}\n", tag));
        }
        out.push_str(&format!(
            "Mods:             {} enabled of {} extracted\n\n",
            self.mods_enabled, self.mods_extracted
        ));
        for check in &self.checks {
            let mark = match check.severity {
                Severity::Ok => "ok  ",
                Severity::Warn => "warn",
                Severity::Fail => "FAIL",
            };
            out.push_str(&format!("[{mark}] {}: {}\n", check.label, check.detail));
        }
        out
    }

    pub fn failures(&self) -> usize {
        self.checks.iter().filter(|c| c.severity == Severity::Fail).count()
    }

    pub fn warnings(&self) -> usize {
        self.checks.iter().filter(|c| c.severity == Severity::Warn).count()
    }
}

/// Run every check against the real (or overridden) layout. Writes nothing.
pub fn doctor(paths: &LauncherPaths) -> DoctorReport {
    let mut report = DoctorReport::default();
    let mut checks = |severity: Severity, label: &str, detail: String| {
        report.checks.push(Check { severity, label: label.to_string(), detail });
    };

    // Launcher root and native layout.
    if paths.root.is_dir() {
        checks(Severity::Ok, "launcher root", paths.root.display().to_string());
    } else {
        checks(Severity::Fail, "launcher root", format!("{} does not exist", paths.root.display()));
        return report;
    }
    checks(
        if paths.native().is_dir() { Severity::Ok } else { Severity::Fail },
        "native dir",
        paths.native().display().to_string(),
    );

    // built.json: the revs this machine last built with.
    let records = read_records(&paths.built_json());
    let record = last_record(&records).map(|(k, r)| (k, r.clone()));
    if let Some((key, rec)) = &record {
        report.horizon_rev = Some(rec.horizon.clone());
        report.mixins_rev = Some(rec.mixins.clone());
        let source = paths.source(&rec.horizon);
        checks(
            if source.is_dir() { Severity::Ok } else { Severity::Fail },
            "pinned game source",
            if source.is_dir() {
                format!("{} (profile {key})", source.display())
            } else {
                let available = paths.source_revs();
                format!(
                    "{} missing for rev {} (pinned sources: {})",
                    source.display(),
                    &rec.horizon[..7.min(rec.horizon.len())],
                    if available.is_empty() {
                        "none".to_string()
                    } else {
                        available.iter().map(|r| r[..7.min(r.len())].to_string()).collect::<Vec<_>>().join(", ")
                    }
                )
            },
        );
    } else if paths.built_json().is_file() {
        checks(Severity::Warn, "built.json", "present but unreadable".to_string());
    } else {
        checks(Severity::Warn, "built.json", "missing (no native build yet)".to_string());
    }

    // Mixins tool.
    let mixins_rev = report
        .mixins_rev
        .clone()
        .unwrap_or_else(|| MIXINS_REV.to_string());
    let tool = paths.mixins_tool(&mixins_rev);
    checks(
        if tool.is_file() { Severity::Ok } else { Severity::Warn },
        "horizon-mixins tool",
        if tool.is_file() {
            tool.display().to_string()
        } else {
            format!("{} missing (the launcher builds it on demand)", tool.display())
        },
    );

    // Pinned toolchain.
    match paths.rust_toolchain() {
        Some((version, dir)) if version == RUST_VERSION && dir.join("bin/rustc").is_file() => {
            checks(Severity::Ok, "pinned Rust toolchain", format!("{} ({version})", dir.display()));
        }
        Some((version, dir)) => {
            checks(
                Severity::Warn,
                "pinned Rust toolchain",
                format!("found {version} at {}, expected {RUST_VERSION}", dir.display()),
            );
        }
        None => checks(Severity::Fail, "pinned Rust toolchain", format!("none under {}", paths.root.join("rust").display())),
    }
    checks(
        if paths.cargo_home().is_dir() { Severity::Ok } else { Severity::Warn },
        "pinned CARGO_HOME",
        paths.cargo_home().display().to_string(),
    );

    // The launcher's own build log: ground truth of what it last ran.
    let log = paths.build_log();
    if log.is_file() {
        match std::fs::read_to_string(&log) {
            Ok(text) => {
                let warnings = text.lines().filter(|l| l.starts_with("warning:")).count();
                let last = text.lines().last().unwrap_or("").to_string();
                checks(
                    Severity::Ok,
                    "launcher build.log",
                    format!("{log} ({warnings} warning line(s), last: {last})", log = log.display()),
                );
            }
            Err(e) => checks(Severity::Warn, "launcher build.log", format!("{}: {e}", log.display())),
        }
    } else {
        checks(Severity::Warn, "launcher build.log", format!("{} missing (no launcher build yet)", log.display()));
    }

    // Settings and profile.
    let settings = super::settings::Settings::load(paths);
    report.profile = Some(settings.effective_profile());
    if paths.settings_json().is_file() {
        checks(
            Severity::Ok,
            "launcher settings",
            format!(
                "{} (selected {}, profile {})",
                paths.settings_json().display(),
                settings.selected.as_deref().unwrap_or("-"),
                settings.launcher_profile.map(|p| p.name()).unwrap_or("-"),
            ),
        );
    } else {
        checks(Severity::Warn, "launcher settings", format!("{} missing", paths.settings_json().display()));
    }

    // Game data dir and the enable-state file.
    checks(
        if paths.game_home.is_dir() { Severity::Ok } else { Severity::Warn },
        "game data dir",
        paths.game_home.display().to_string(),
    );
    if !paths.mods_list().is_file() {
        checks(Severity::Warn, "horizon-mods.txt", format!("{} missing (every mod counts as off)", paths.mods_list().display()));
    }

    // Mods.
    match super::modslist::list_mods(paths) {
        Ok(rows) => {
            report.mods_extracted = rows.iter().filter(|r| r.extracted).count();
            report.mods_enabled = rows.iter().filter(|r| r.enabled && r.extracted).count();
            let game_only = rows.iter().filter(|r| !r.extracted).count();
            checks(
                Severity::Ok,
                "installed mods",
                format!(
                    "{} extracted ({} enabled), {game_only} only in the game dir",
                    report.mods_extracted, report.mods_enabled
                ),
            );
        }
        Err(e) => checks(Severity::Fail, "installed mods", e),
    }

    // Installed client(s): newest native version with a .installed marker.
    let versions = super::launch::launchable_versions(paths);
    if let Some(newest) = versions.iter().find(|v| v.native) {
        checks(
            Severity::Ok,
            "installed native client",
            format!("{} (sha256 {})", newest.tag, newest.client_sha256.clone().unwrap_or_default()),
        );
        report.installed_client = newest
            .client_sha256
            .clone()
            .map(|sha| (newest.tag.clone(), sha));
        let stale: Vec<&String> = versions.iter().skip_while(|v| v.tag != newest.tag).skip(1).map(|v| &v.tag).collect();
        if !stale.is_empty() {
            checks(Severity::Warn, "older native versions", format!("{} more installed ({})", stale.len(), stale.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
        }
    } else {
        checks(
            Severity::Warn,
            "installed native client",
            "none (no native-* version with .installed)".to_string(),
        );
    }

    // The registry CLI for signature checks (optional).
    match super::hnmod::registry_cli() {
        Some(cli) => checks(Severity::Ok, "registry CLI", cli.display().to_string()),
        None => checks(
            Severity::Warn,
            "registry CLI",
            "cargo-horizon-mod not built; .hnmod signature checks will be skipped".to_string(),
        ),
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::testutil::temp_root;

    #[test]
    fn doctor_on_missing_root_fails_fast() {
        let tmp = temp_root("doctor-missing");
        let paths = LauncherPaths { root: tmp.join("nope"), game_home: tmp.join("game") };
        let report = doctor(&paths);
        assert_eq!(report.failures(), 1);
        assert_eq!(report.checks[0].label, "launcher root");
    }

    #[test]
    fn doctor_on_real_layout() {
        let paths = LauncherPaths::discover();
        if !paths.native().is_dir() {
            return;
        }
        let report = doctor(&paths);
        assert!(report.horizon_rev.is_some(), "real built.json is readable");
        assert!(report.mods_extracted > 0);
        // The installed client's sha256 must match an independent computation.
        if let Some((tag, sha)) = &report.installed_client {
            let client = paths.version_dir(tag).join("horizon-client");
            assert_eq!(sha, &crate::launcher::records::file_sha256(&client).unwrap());
        }
    }
}
