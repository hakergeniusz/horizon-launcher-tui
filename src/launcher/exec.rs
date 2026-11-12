//! Running planned commands, at the priority the build runner keeps.
//!
//! Two low-priority defaults are encoded here (the launcher gives its cargo
//! the same treatment via `rust_toolchain::low_priority`):
//!
//! - CPU: `nice -n 19`
//! - I/O: `ionice -c 3` (idle class)
//! - and `CARGO_BUILD_JOBS=2` is set for cargo commands when the environment
//!   does not already pin it, so a build never fans out across all cores.
//!
//! If `nice` or `ionice` is not on PATH, the command runs without it (the
//! env limit still applies).

use std::process::Command;

/// Whether the low-priority wrappers exist on PATH.
fn have(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Wrap `cmd` to run at low priority: `nice -n 19 ionice -c 3 <cmd…>`.
/// Takes ownership and returns a fresh `Command` whose program is the
/// wrapper — a `std::process::Command` cannot change its program in place.
/// Everything set so far (args, env, cwd) is carried over.
pub fn low_priority(cmd: Command) -> Command {
    let nice = have("nice");
    let ionice = have("ionice");
    if !nice {
        return cmd;
    }
    let mut wrapped = Command::new("nice");
    wrapped.arg("-n").arg("19");
    if ionice {
        wrapped.arg("ionice").arg("-c").arg("3");
    }
    if let Some(dir) = cmd.get_current_dir() {
        wrapped.current_dir(dir);
    }
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => {
                wrapped.env(k, v);
            }
            None => {
                wrapped.env_remove(k);
            }
        }
    }
    wrapped.arg(cmd.get_program());
    for arg in cmd.get_args() {
        wrapped.arg(arg);
    }
    wrapped
}

/// Set the cargo job limit this box needs (`CARGO_BUILD_JOBS=2`) unless the
/// environment already pins it.
pub fn pin_cargo_jobs(cmd: &mut Command) -> &mut Command {
    if std::env::var_os("CARGO_BUILD_JOBS").is_none() {
        cmd.env("CARGO_BUILD_JOBS", "2");
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(cmd: &Command) -> Vec<String> {
        std::iter::once(cmd.get_program().to_string_lossy().into_owned())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect()
    }

    #[test]
    fn wraps_into_nice_ionice() {
        let mut cmd = Command::new("cargo");
        cmd.arg("build").arg("--locked");
        cmd.env("CARGO_HOME", "/x");
        if have("nice") {
            let cmd = low_priority(cmd);
            let words = words(&cmd);
            assert_eq!(words[0], "nice");
            assert_eq!(words[1], "-n");
            assert_eq!(words[2], "19");
            if have("ionice") {
                assert_eq!(&words[3..6], &["ionice", "-c", "3"]);
                assert_eq!(words[6], "cargo");
                assert_eq!(&words[7..], &["build", "--locked"]);
            } else {
                assert_eq!(words[3], "cargo");
                assert_eq!(&words[4..], &["build", "--locked"]);
            }
            // Env survives the wrap.
            assert!(cmd
                .get_envs()
                .any(|(k, v)| k == std::ffi::OsStr::new("CARGO_HOME")
                    && v == Some(std::ffi::OsStr::new("/x"))));
        } else {
            // No nice on PATH: the command runs unwrapped.
            let cmd = low_priority(cmd);
            assert_eq!(words(&cmd)[0], "cargo");
        }
    }

    #[test]
    fn pins_cargo_jobs() {
        // The var may be inherited from the invoking shell; the contract is
        // "set when absent", so test that branch directly.
        let had = std::env::var_os("CARGO_BUILD_JOBS");
        std::env::remove_var("CARGO_BUILD_JOBS");
        let mut cmd = Command::new("cargo");
        pin_cargo_jobs(&mut cmd);
        assert!(cmd.get_envs().any(|(k, v)| {
            k == std::ffi::OsStr::new("CARGO_BUILD_JOBS") && v == Some(std::ffi::OsStr::new("2"))
        }));
        // And leave the environment as we found it.
        if let Some(value) = had {
            std::env::set_var("CARGO_BUILD_JOBS", value);
        }
    }
}
