//! Executing a [`BuildPlan`] — the real runner behind `build` (not dry-run).
//!
//! Every step the plan renders as a shell line, this module performs in
//! process: compose copies, pin rewrites, the `horizon-mixins apply`
//! subprocess, the injections file, the cargo build (streamed, low
//! priority), the install rename and the `built.json` update. This exists
//! and is wired to the CLI/TUI; heavy end-to-end runs are deliberately left
//! to when the machine has memory to spare.

use super::paths::LauncherPaths;
use super::plan::{has_mods_feature, BuildPlan, INJECTIONS_FILE, RUST_VERSION};
use super::records::{read_records, write_records, Record};
use std::path::Path;
use std::process::Command;

/// Copy a directory tree, preserving relative names. Refuses symlinks (the
/// mixins CLI refuses them too).
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?.flatten() {
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        } else {
            return Err(format!(
                "{} is a link: copy the file instead.",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

/// The toolchain environment the launcher gives every subprocess it runs:
/// only the pinned rustc and cargo home are in play, never the player's
/// rustup or compiler wrappers.
fn toolchain_env(cmd: &mut Command, paths: &LauncherPaths, rust_dir: &Path) {
    let original = std::env::var_os("PATH").unwrap_or_default();
    let mut joined = std::env::split_paths(&original).collect::<Vec<_>>();
    joined.insert(0, rust_dir.join("bin"));
    let path = std::env::join_paths(joined).unwrap_or(original);
    cmd.env("PATH", path)
        .env("RUSTC", rust_dir.join("bin").join("rustc"))
        .env("CARGO_HOME", paths.cargo_home());
    // Scratch for gcc/ld/cargo-install, inside the launcher's own tmp.
    let tmp = paths.root.join("tmp");
    if std::fs::create_dir_all(&tmp).is_ok() {
        cmd.env("TMPDIR", &tmp).env("TMP", &tmp).env("TEMP", &tmp);
    }
}

fn run_tool(
    paths: &LauncherPaths,
    program: &Path,
    args: &[&str],
    rust_dir: &Path,
    what: &str,
    mut on_line: impl FnMut(&str),
) -> Result<(), String> {
    let mut cmd = Command::new(program);
    toolchain_env(&mut cmd, paths, rust_dir);
    cmd.args(args);
    let mut cmd = super::exec::low_priority(cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("{what}: {}: {e}", program.display()))?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        on_line(line);
    }
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        on_line(line);
    }
    if !output.status.success() {
        let tail: String = String::from_utf8_lossy(&output.stderr)
            .lines()
            .chain(String::from_utf8_lossy(&output.stdout).lines())
            .last_lines(12)
            .join("\n");
        return Err(format!("{what} failed ({}):\n{tail}", output.status));
    }
    Ok(())
}

trait LastLines {
    fn last_lines(self, n: usize) -> Vec<String>;
}

impl<'a, I: Iterator<Item = &'a str>> LastLines for I {
    fn last_lines(self, n: usize) -> Vec<String> {
        let collected: Vec<String> = self.map(str::to_owned).collect();
        let start = collected.len().saturating_sub(n);
        collected[start..].to_vec()
    }
}

/// The cargo build with the plan's environment, streamed.
fn run_cargo(
    paths: &LauncherPaths,
    plan: &BuildPlan,
    rust_dir: &Path,
    mut on_line: impl FnMut(&str),
) -> Result<(), String> {
    use std::io::BufRead;
    let (cargo_profile, configs) = plan.profile.cargo();
    let mut cmd = Command::new(&paths.rust_toolchain().expect("checked at plan time").1.join("bin/cargo"));
    toolchain_env(&mut cmd, paths, rust_dir);
    super::exec::pin_cargo_jobs(&mut cmd);
    cmd.env("CARGO_TARGET_DIR", paths.native_target())
        .env("HORIZON_MOD_API_INJECTIONS", plan.stage_dir.join(INJECTIONS_FILE))
        .arg("build")
        .arg("--manifest-path")
        .arg(plan.stage_dir.join("Cargo.toml"))
        .arg("--locked")
        .arg("--offline")
        .arg("--profile")
        .arg(cargo_profile);
    let source = paths.source(&plan.revision);
    if source.join(".cargo").join("config.toml").is_file() {
        cmd.arg("--config").arg(source.join(".cargo").join("config.toml"));
    }
    for config in configs {
        cmd.arg("--config").arg(config);
    }
    if has_mods_feature(&source) {
        cmd.arg("--features").arg("horizon-client/mods");
    }
    cmd.args(["-p", "horizon-client", "--bin", "horizon-client"]);
    let mut cmd = super::exec::low_priority(cmd);

    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cargo: {e}"))?;
    // Drain stderr (cargo's human output) while stdout accumulates for the error tail.
    let stderr = child.stderr.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let stdout_thread_data = std::thread::spawn(move || -> String {
        let mut text = String::new();
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            text.push_str(&line);
            text.push('\n');
        }
        text
    });
    for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
        on_line(&line);
    }
    let stdout_text = stdout_thread_data.join().unwrap_or_default();
    let status = child.wait().map_err(|e| format!("cargo: {e}"))?;
    if !status.success() {
        let tail = stdout_text.lines().last_lines(12).join("\n");
        return Err(format!("cargo build failed ({status}):\n{tail}"));
    }
    Ok(())
}

/// Run the whole build. `on_line` hears progress (one line per event).
pub fn run_build(
    paths: &LauncherPaths,
    plan: &BuildPlan,
    mut on_line: impl FnMut(&str),
) -> Result<Record, String> {
    on_line(&format!("composing {}", plan.compose_dir.display()));
    if plan.compose_dir.exists() {
        std::fs::remove_dir_all(&plan.compose_dir).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&plan.compose_dir).map_err(|e| e.to_string())?;
    for m in &plan.mods {
        copy_tree(&m.source_dir, &m.compose_dir)?;
        std::fs::write(m.compose_dir.join(super::manifests::MANIFEST), &m.manifest_text)
            .map_err(|e| e.to_string())?;
        on_line(&format!("composed {} ({} files)", m.id, m.files.len()));
    }

    if plan.stage_dir.exists() {
        std::fs::remove_dir_all(&plan.stage_dir).map_err(|e| e.to_string())?;
    }
    let rust_dir = paths
        .rust_toolchain()
        .and_then(|(v, dir)| (v == RUST_VERSION).then_some(dir))
        .ok_or_else(|| format!("no pinned Rust toolchain {RUST_VERSION}"))?;
    let mixins_tool = paths.mixins_tool(&plan.mixins_rev);
    on_line(&format!("applying mixins {} on {}", super::modslist::shorten(&plan.mixins_rev), super::modslist::shorten(&plan.revision)));
    run_tool(
        paths,
        &mixins_tool,
        &[
            "apply",
            "--workspace",
            &paths.source(&plan.revision).to_string_lossy(),
            "--mods",
            &plan.compose_dir.to_string_lossy(),
            "--out",
            &plan.stage_dir.to_string_lossy(),
        ],
        &rust_dir,
        "horizon-mixins",
        |line| on_line(line),
    )?;

    std::fs::write(
        plan.stage_dir.join(INJECTIONS_FILE),
        super::plan::injections_content().to_string(),
    )
    .map_err(|e| format!("{}: {e}", plan.stage_dir.join(INJECTIONS_FILE).display()))?;
    on_line(&format!("wrote {}", plan.stage_dir.join(INJECTIONS_FILE).display()));

    on_line(&format!("building horizon-client (cargo profile {})", plan.profile.cargo().0));
    run_cargo(paths, plan, &rust_dir, |line| on_line(line))?;

    let version_dir = &plan.version_dir;
    std::fs::create_dir_all(version_dir).map_err(|e| e.to_string())?;
    let staging = version_dir.join(".horizon-client.new");
    std::fs::copy(&plan.target_binary, &staging)
        .map_err(|e| format!("{} → {}: {e}", plan.target_binary.display(), staging.display()))?;
    // Same-directory rename: a running game keeps the old binary (a plain
    // write would fail ETXTBSY or swap under it).
    std::fs::rename(&staging, version_dir.join("horizon-client"))
        .map_err(|e| format!("install rename: {e}"))?;
    std::fs::write(version_dir.join(".installed"), b"").map_err(|e| e.to_string())?;
    on_line(&format!("installed {}", version_dir.join("horizon-client").display()));

    let mut records = read_records(&paths.built_json());
    records.insert(plan.record_key.clone(), plan.record.clone());
    write_records(&paths.built_json(), &paths.built_json_bak(), &records)?;
    on_line(&format!(
        "built.json updated ({}: horizon {}, mixins {}, {} mods)",
        plan.record_key,
        super::modslist::shorten(&plan.record.horizon),
        super::modslist::shorten(&plan.record.mixins),
        plan.record.mods.len()
    ));
    Ok(plan.record.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_lines_helper() {
        let lines: Vec<&str> = vec!["a", "b", "c", "d"];
        assert_eq!(lines.into_iter().last_lines(2), vec!["c".to_string(), "d".to_string()]);
        assert_eq!(vec!["x"].into_iter().last_lines(12), vec!["x".to_string()]);
    }

    #[test]
    fn copy_tree_refuses_symlinks() {
        let tmp = crate::launcher::testutil::temp_root("buildrun-symlink");
        let from = tmp.join("from");
        let to = tmp.join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("a.rs"), b"1").unwrap();
        std::os::unix::fs::symlink(from.join("a.rs"), from.join("link.rs")).unwrap();
        assert!(copy_tree(&from, &to).is_err());
        std::fs::remove_file(from.join("link.rs")).unwrap();
        copy_tree(&from, &to).unwrap();
        assert!(to.join("a.rs").is_file());
    }

    #[test]
    fn mixins_rev_constant_matches_pinned_tool_dir() {
        // The tool dir uses rev[..12]; the constant is the rev built.json records.
        assert_eq!(crate::launcher::plan::MIXINS_REV.len(), 40);
    }
}
