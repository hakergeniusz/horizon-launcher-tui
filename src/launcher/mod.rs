//! The `launcher` library module: pure logic over the Horizon launcher's
//! data layout, shared by the CLI and the TUI.
//!
//! Everything here reads the launcher's state (and the launcher's own Rust
//! source was consulted for the exact semantics); only the explicitly
//! writing entry points (`store_profile`, `set_enabled`, `install`,
//! `uninstall`, `run_build`) change anything on disk.

pub mod build_run;
pub mod doctor;
pub mod exec;
pub mod hnmod;
pub mod launch;
pub mod manifests;
pub mod modslist;
pub mod paths;
pub mod plan;
pub mod profile;
pub mod records;
pub mod settings;

#[cfg(test)]
pub(crate) mod testutil;

pub use doctor::{doctor, DoctorReport, Severity};
pub use hnmod::{install, uninstall};
pub use launch::LaunchPlan;
pub use modslist::{list_mods, ModRow};
pub use paths::LauncherPaths;
pub use settings::Settings;
