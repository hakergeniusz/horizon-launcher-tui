//! Launching the built game.
//!
//! Ground truth (`launcher/src/backend.rs::client_command`): the client
//! `versions/<tag>/horizon-client` runs with the game's data dir as its cwd
//! and `HORIZON_HOME` set to it, with `--resources
//! versions/<tag>/.local/horizon-client/<minecraft version>` (the version
//! comes from the version's `research-lock.json`), `PIPEWIRE_PROPS` naming the
//! game "Horizon" when the environment does not set it, and the launcher's
//! renderer workarounds stripped. `--control` is passed only when asked.

use super::paths::LauncherPaths;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub version_tag: String,
    pub client: PathBuf,
    pub cwd: PathBuf,
    pub resources: PathBuf,
    pub control: Option<String>,
    pub env: Vec<(String, String)>,
    pub removed_env: Vec<String>,
    pub exists: bool,
}

impl LaunchPlan {
    /// The exact command line, as a shell would run it.
    pub fn command_line(&self) -> String {
        let mut line = String::new();
        for (k, v) in &self.env {
            line.push_str(&format!("{k}={}", super::plan::sh(Path::new(v))));
            line.push(' ');
        }
        line.push_str(&super::plan::sh(&self.client));
        line.push_str(" --resources ");
        line.push_str(&super::plan::sh(&self.resources));
        if let Some(control) = &self.control {
            line.push_str(" --control ");
            line.push_str(&super::plan::sh(Path::new(control)));
        }
        line.push_str(&format!("   # cwd {}", self.cwd.display()));
        line
    }
}

/// One installed version this tool can consider launching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    pub tag: String,
    pub native: bool,
    pub installed: bool,
    pub client_sha256: Option<String>,
}

/// The `<minecraft version>` a version's resources live under
/// (`research-lock.json` → `minecraft.version`).
fn game_version(version_dir: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Lock {
        #[serde(default)]
        minecraft: serde_json::Value,
    }
    let bytes = std::fs::read(version_dir.join("research-lock.json")).ok()?;
    let lock: Lock = serde_json::from_slice(&bytes).ok()?;
    lock.minecraft.get("version").and_then(|v| v.as_str()).map(str::to_owned)
}

/// The versions this tool can launch, newest first (mtime order): only
/// `native-*` dirs carrying a `.installed` marker and a `horizon-client`.
pub fn launchable_versions(paths: &LauncherPaths) -> Vec<VersionRow> {
    let mut rows: Vec<(std::time::SystemTime, VersionRow)> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(paths.versions()) {
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(tag) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let native = tag.starts_with("native-");
            let installed = path.join(".installed").is_file();
            let client = path.join("horizon-client");
            if !installed || !client.is_file() {
                continue;
            }
            if !seen.insert(tag.clone()) {
                continue;
            }
            let sha = super::records::file_sha256(&client).ok();
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            rows.push((mtime, VersionRow { tag, native, installed, client_sha256: sha }));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// The version `launch` runs: `--version <tag>` when given, else the newest
/// launchable native version.
pub fn pick_version(paths: &LauncherPaths, explicit: Option<&str>) -> Result<String, String> {
    if let Some(tag) = explicit {
        let dir = paths.version_dir(tag);
        if !dir.join("horizon-client").is_file() {
            return Err(format!("{} has no horizon-client.", dir.display()));
        }
        return Ok(tag.to_owned());
    }
    launchable_versions(paths)
        .into_iter()
        .find(|row| row.native)
        .map(|row| row.tag)
        .ok_or_else(|| {
            format!(
                "No installed native version under {}: build one first (`build`).",
                paths.versions().display()
            )
        })
}

/// Plan the launch of `tag` — everything the launcher's `client_command`
/// does, without spawning anything.
pub fn plan_launch(
    paths: &LauncherPaths,
    tag: &str,
    control: Option<&str>,
) -> Result<LaunchPlan, String> {
    let version_dir = paths.version_dir(tag);
    let client = version_dir.join("horizon-client");
    if !client.is_file() {
        return Err(format!("{} does not exist.", client.display()));
    }
    let mc_version = game_version(&version_dir).unwrap_or_else(|| "26.3".into());
    let resources = version_dir.join(".local/horizon-client").join(&mc_version);
    let exists = resources.is_dir();
    if !exists {
        return Err(format!(
            "The version's resources are missing: {} (prepare this version with the GUI launcher first).",
            resources.display()
        ));
    }
    let game_home = paths.game_home.clone();
    let mut env = vec![("HORIZON_HOME".to_string(), game_home.to_string_lossy().into_owned())];
    if std::env::var_os("PIPEWIRE_PROPS").is_none() {
        env.push((
            "PIPEWIRE_PROPS".to_string(),
            r#"{ application.name = "Horizon" media.role = "Game" }"#.to_string(),
        ));
    }
    Ok(LaunchPlan {
        version_tag: tag.to_owned(),
        client,
        cwd: game_home.clone(),
        resources,
        control: control.map(str::to_owned),
        env,
        removed_env: vec![
            "HORIZON_ACCOUNT_SESSIONS".into(),
            "LIBGL_ALWAYS_SOFTWARE".into(),
            "GALLIUM_DRIVER".into(),
            "WEBKIT_DISABLE_DMABUF_RENDERER".into(),
            "WEBKIT_DISABLE_COMPOSITING_MODE".into(),
        ],
        exists,
    })
}

/// Actually spawn the game (detached): the caller owns when this runs.
pub fn spawn(plan: &LaunchPlan) -> Result<u32, String> {
    if !plan.cwd.is_dir() {
        std::fs::create_dir_all(&plan.cwd).map_err(|e| format!("{}: {e}", plan.cwd.display()))?;
    }
    let mut cmd = std::process::Command::new(&plan.client);
    cmd.current_dir(&plan.cwd);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    for k in &plan.removed_env {
        cmd.env_remove(k);
    }
    cmd.arg("--resources").arg(&plan.resources);
    if let Some(control) = &plan.control {
        cmd.arg("--control").arg(control);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", plan.client.display()))?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::testutil::temp_root;

    fn fixture(tmp: &Path) -> LauncherPaths {
        let paths = LauncherPaths { root: tmp.to_path_buf(), game_home: tmp.join("game") };
        let dir = paths.version_dir("native-2ebf00a-42ebf5dd24f9");
        std::fs::create_dir_all(dir.join(".local/horizon-client/26.3")).unwrap();
        std::fs::write(dir.join("horizon-client"), b"MZ-fake").unwrap();
        std::fs::write(dir.join(".installed"), b"").unwrap();
        std::fs::write(
            dir.join("research-lock.json"),
            br#"{"minecraft": {"version": "26.3"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(&paths.game_home).unwrap();
        paths
    }

    #[test]
    fn plans_launch_with_resources_and_env() {
        let tmp = temp_root("launch-plan");
        let paths = fixture(&tmp);
        let tag = pick_version(&paths, None).unwrap();
        assert_eq!(tag, "native-2ebf00a-42ebf5dd24f9");
        let plan = plan_launch(&paths, &tag, None).unwrap();
        assert_eq!(plan.cwd, paths.game_home);
        assert!(plan.client.ends_with("horizon-client"));
        assert!(plan
            .resources
            .ends_with(".local/horizon-client/26.3"), "{}", plan.resources.display());
        assert!(plan.env.iter().any(|(k, v)| k == "HORIZON_HOME" && v.as_str() == paths.game_home.to_string_lossy()));
        assert!(plan.env.iter().any(|(k, _)| k == "PIPEWIRE_PROPS"));
        assert!(plan.removed_env.contains(&"HORIZON_ACCOUNT_SESSIONS".to_string()));
        let line = plan.command_line();
        assert!(line.contains("--resources"), "{line}");
        assert!(!line.contains("--control"), "{line}");

        // --control only behind the flag.
        let with_control = plan_launch(&paths, &tag, Some("/tmp/ctl.sock")).unwrap();
        assert!(with_control.command_line().contains("--control /tmp/ctl.sock"));
    }

    #[test]
    fn missing_resources_are_an_error() {
        let tmp = temp_root("launch-nores");
        let paths = fixture(&tmp);
        let dir = paths.version_dir("native-2ebf00a-42ebf5dd24f9");
        std::fs::remove_dir_all(dir.join(".local")).unwrap();
        assert!(plan_launch(&paths, "native-2ebf00a-42ebf5dd24f9", None)
            .unwrap_err()
            .contains("resources are missing"));
    }

    #[test]
    fn newest_native_version_wins() {
        let tmp = temp_root("launch-newest");
        let paths = LauncherPaths { root: tmp.to_path_buf(), game_home: tmp.join("game") };
        // The older version first, so its mtime is older (creation order).
        let make = |tag: &str| {
            let dir = paths.version_dir(tag);
            std::fs::create_dir_all(dir.join(".local/horizon-client/26.3")).unwrap();
            std::fs::write(dir.join("horizon-client"), b"MZ-fake").unwrap();
            std::fs::write(dir.join(".installed"), b"").unwrap();
        };
        make("native-af55a73-2c5378cf9a95");
        make("native-2ebf00a-42ebf5dd24f9");
        let rows = launchable_versions(&paths);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].tag, "native-2ebf00a-42ebf5dd24f9");
        assert!(rows[0].client_sha256.is_some());
        assert_eq!(rows[0].client_sha256, rows[1].client_sha256, "same bytes");
    }
}
