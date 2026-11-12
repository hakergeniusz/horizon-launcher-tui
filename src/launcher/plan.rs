//! Build planning: the exact command sequence a native build runs, and the
//! state it will produce — without touching anything when only planning.
//!
//! The recipe (verified against the launcher's own `native/build.log`,
//! `native/built.json` and `launcher/src/native_mods.rs`):
//!
//! 1. Compose `--mods`: one bundle per subdirectory, `horizon-mixin.toml` at
//!    its root. The extracted layout `installed.incoming/<id>/mixins/<id>/…`
//!    is re-rooted: the bundle's content lands in `<compose>/<id>/`.
//! 2. Rewrite pins: bundles whose `horizon_revision` differs from the build's
//!    Horizon revision (or that follow the hook catalog) get their
//!    `[compatibility] horizon_revision` rewritten **on the compose copy**,
//!    never on the `installed.incoming` original. Mods pinning another commit
//!    produce a warning, as in the launcher's own `build.log`.
//! 3. Apply: `horizon-mixins apply --workspace <source> --mods <compose> --out <stage>`.
//! 4. Write `<stage>/.horizon-mod-api-injections.json` with content `[]`
//!    (the game's `build.rs` fails without it).
//! 5. Build: `cargo build --manifest-path <stage>/Cargo.toml --locked --offline
//!    --profile <fast|release> -p horizon-client --bin horizon-client` with the
//!    pinned toolchain, the pinned `CARGO_HOME` and the shared
//!    `native/target/` as `CARGO_TARGET_DIR`.
//! 6. Install: copy `target/<profile>/horizon-client` to
//!    `versions/<tag>/.horizon-client.new`, then rename it onto
//!    `versions/<tag>/horizon-client` (a same-directory rename; a running game
//!    keeps the old binary), and write the `.installed` marker.
//! 7. Update `native/built.json` in the launcher's format (backing it up).

use super::manifests::{Bundle, MANIFEST};
use super::paths::LauncherPaths;
use super::profile::Profile;
use super::records::{mod_hash, read_records, Record};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The mixins rev bundles that name none use (`native_mods.rs::MIXINS_REV`).
pub const MIXINS_REV: &str = "5a64af938ec86b00d53d16a408aabc0038fb3e57";
/// The pinned Rust toolchain version the launcher installs (`rust_toolchain::VERSION`).
pub const RUST_VERSION: &str = "1.99.1";
/// Version tag prefix (`native-<7 of the rev>-<12 hex>`).
pub const VERSION_PREFIX: &str = "native-";
/// The features key hashed for client builds (`native_mods.rs::FEATURES`).
const FEATURES_KEY: &str = "horizon-client/mods";
/// The injections file the game's build.rs requires in the stage.
pub const INJECTIONS_FILE: &str = ".horizon-mod-api-injections.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMod {
    pub id: String,
    pub version: String,
    /// The extracted bundle root (`installed.incoming/<id>/mixins/<id>`).
    pub source_dir: PathBuf,
    /// Where the compose copy will live (`<compose>/<id>`).
    pub compose_dir: PathBuf,
    /// Pin rewrite on the copy: (old revision, new revision), when needed.
    pub repin: Option<(Option<String>, String)>,
    /// The manifest text the copy will carry (rewritten when repinning).
    pub manifest_text: String,
    /// The bundle files as they will be on the copy, sorted by name —
    /// exactly what `mod_hash` and the version hash take.
    pub files: Vec<(String, Vec<u8>)>,
    pub files_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanKind {
    /// The version is already installed (`versions/<tag>/.installed`): nothing compiles.
    UpToDate,
    /// An earlier build of this profile exists in `native/target/`: only what changed recompiles.
    Incremental(Vec<String>),
    /// Nothing built yet in this profile: the whole game compiles.
    First,
}

/// One step of the plan: what it does and the exact shell line for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub title: String,
    pub command: String,
    /// Whether running this step is implemented by this tool's executor
    /// (all of them are; the flag exists for reporting).
    pub executed_by_tool: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    pub profile: Profile,
    pub revision: String,
    pub mixins_rev: String,
    pub version_tag: String,
    pub plan_kind: PlanKind,
    pub warnings: Vec<String>,
    pub mods: Vec<PlannedMod>,
    /// Mods installed but turned off (excluded from the build).
    pub disabled: Vec<String>,
    pub steps: Vec<PlanStep>,
    /// The record this build would write into `built.json` under its profile key.
    pub record: Record,
    pub record_key: String,
    pub target_binary: PathBuf,
    pub version_dir: PathBuf,
    pub stage_dir: PathBuf,
    pub compose_dir: PathBuf,
}

impl BuildPlan {
    /// Human-readable summary lines (used by `build --dry-run` and the TUI).
    pub fn render(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!("Profile:          {} (cargo profile {})", self.profile.name(), self.profile.cargo().0));
        out.push(format!("Horizon revision: {}", self.revision));
        out.push(format!("Mixins revision:  {}", self.mixins_rev));
        out.push(format!("Version tag:      {}", self.version_tag));
        let kind = match &self.plan_kind {
            PlanKind::UpToDate => "up to date (no build needed)".to_string(),
            PlanKind::Incremental(changes) => format!("incremental rebuild: {}", changes.join("; ")),
            PlanKind::First => "first build: the whole game (several minutes)".to_string(),
        };
        out.push(format!("Plan:             {kind}"));
        out.push(format!(
            "Mods in build:    {} ({} disabled)",
            self.mods.len(),
            self.disabled.len()
        ));
        for warning in &self.warnings {
            out.push(format!("warning: {warning}"));
        }
        for m in &self.mods {
            let repin = match &m.repin {
                Some((from, to)) => match from {
                    Some(from) => format!(" (pin rewritten {} → {})", super::modslist::shorten(from), super::modslist::shorten(to)),
                    None => format!(" (pin set to {})", super::modslist::shorten(to)),
                },
                None => String::new(),
            };
            out.push(format!(
                "  {:<20} v{:<8} {} file(s), hash {}{}",
                m.id,
                m.version,
                m.files.len(),
                m.files_hash,
                repin
            ));
        }
        if !self.disabled.is_empty() {
            out.push(format!("Disabled mods:    {}", self.disabled.join(", ")));
        }
        out.push(String::new());
        out.push("Steps:".to_string());
        for (i, step) in self.steps.iter().enumerate() {
            out.push(format!("{:>2}. {}", i + 1, step.title));
            out.push(format!("    $ {}", step.command));
        }
        out
    }
}

/// Which revision to build on, and how it was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionChoice {
    /// Explicitly requested (`build --revision`).
    Explicit(String),
    /// The last build's revision from `native/built.json`.
    FromBuiltJson(String),
}

/// The revision this tool builds on: the explicit one, else the last build's
/// (`built.json`, any profile, `fast` preferred). The GUI launcher resolves
/// its base version through GitHub; this tool stays offline and follows what
/// this machine last built, which is also what `native/source/` pins.
pub fn choose_revision(paths: &LauncherPaths, explicit: Option<&str>) -> Result<RevisionChoice, String> {
    if let Some(rev) = explicit {
        let rev = rev.trim().to_string();
        if rev.len() != 40 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{rev} is not a Horizon commit (40 hex digits)."));
        }
        let source = paths.source(&rev);
        if !source.is_dir() {
            return Err(format!(
                "No pinned source for {rev}: {} is missing.",
                source.display()
            ));
        }
        return Ok(RevisionChoice::Explicit(rev));
    }
    let records = read_records(&paths.built_json());
    let (_, record) = super::records::last_record(&records).ok_or_else(|| {
        format!(
            "No revision to build on: {} has no records and no --revision was given.",
            paths.built_json().display()
        )
    })?;
    let rev = record.horizon.clone();
    let source = paths.source(&rev);
    if !source.is_dir() {
        return Err(format!(
            "The last build's Horizon revision {rev} has no pinned source ({} is missing); pass --revision.",
            source.display()
        ));
    }
    Ok(RevisionChoice::FromBuiltJson(rev))
}

/// The mixins revision the build uses: when the mods name fewer than two
/// distinct revisions, that one (or the launcher's default); when they
/// disagree, the newest by commit date, with the launcher's own default
/// mixed in. Dates come from `native/commit-dates.json` only — the launcher
/// fills it from GitHub; offline this tool refuses to guess.
pub fn choose_mixins_rev(paths: &LauncherPaths, mods: &[&Bundle]) -> Result<String, String> {
    let mut pins: Vec<&str> = mods.iter().filter_map(|b| b.mixins.as_deref()).collect();
    pins.sort_unstable();
    pins.dedup();
    if pins.len() < 2 {
        return Ok(pins.first().map(|s| s.to_string()).unwrap_or_else(|| MIXINS_REV.to_string()));
    }
    if let Some(bad) = pins.iter().find(|p| !(p.len() == 40 && p.chars().all(|c| c.is_ascii_hexdigit()))) {
        return Err(format!("{bad} is not a horizon-mixins commit (40 hex digits)."));
    }
    let mut candidates: Vec<&str> = pins.clone();
    if !candidates.contains(&MIXINS_REV) {
        candidates.push(MIXINS_REV);
    }
    let dates = commit_dates(&paths.native().join("commit-dates.json"));
    let mut dated: Vec<(String, &str)> = Vec::new();
    for pin in candidates {
        let when = dates.get(pin).ok_or_else(|| {
            format!(
                "Can't tell which of the mods' horizon-mixins revisions is newest: {} has no date for {pin}. Open the GUI launcher once (or build once with it) so it records the date, then retry.",
                paths.native().join("commit-dates.json").display()
            )
        })?;
        dated.push((when.clone(), pin));
    }
    dated.sort();
    Ok(dated.last().map(|(_, p)| p.to_string()).unwrap_or_default())
}

fn commit_dates(path: &Path) -> BTreeMap<String, String> {
    std::fs::read(path)
        .ok()
        .and_then(|b| {
            let value: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&b).ok()?;
            Some(
                value
                    .into_iter()
                    .filter_map(|(k, v)| {
                        // Keys are "horizon-rust/horizon-mixins@<rev>" or "…/horizon@<rev>".
                        let rev = k.split('@').nth(1)?.to_string();
                        v.as_str().map(|d| (rev, d.to_string()))
                    })
                    .collect(),
            )
        })
        .unwrap_or_default()
}

/// The version tag a build of these compose copies produces
/// (`native-<7 of the rev>-<12 hex>`), the launcher's `version_for` exactly:
/// SHA-256 over the parts each followed by a 0 byte — mixins rev, cargo
/// profile, the profile's `--config` values, the features key, the Rust
/// version, the revision — then, per bundle in compose order, the bundle id
/// (0 byte) and its files as `(name bytes, length LE, bytes)`. Key: hex of
/// the first 6 bytes.
pub fn version_for(
    revision: &str,
    mixins: &str,
    profile: Profile,
    mods: &[PlannedMod],
) -> String {
    let (cargo_profile, configs) = profile.cargo();
    let mut hash = Sha256::new();
    for part in [mixins, cargo_profile]
        .into_iter()
        .chain(configs.iter().copied())
        .chain([FEATURES_KEY, RUST_VERSION, revision])
    {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    for m in mods {
        hash.update(m.id.as_bytes());
        hash.update([0]);
        for (name, bytes) in &m.files {
            hash.update(name.as_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    let key: String = hash.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{VERSION_PREFIX}{}-{key}", &revision[..7])
}

/// Every file of the planned copy: the bundle's files with the manifest
/// replaced by the (possibly rewritten) manifest text, sorted by name with
/// `/` separators.
fn planned_files(bundle: &Bundle, manifest_text: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(&bundle.dir, "", &mut out)?;
    out.retain(|(name, _)| name != MANIFEST);
    out.push((MANIFEST.to_string(), manifest_text.as_bytes().to_vec()));
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn collect_files(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = if prefix.is_empty() {
            entry.file_name().to_string_lossy().into_owned()
        } else {
            format!("{prefix}/{}", entry.file_name().to_string_lossy())
        };
        let kind = entry.file_type().map_err(|e| format!("{}: {e}", path.display()))?;
        if kind.is_dir() {
            collect_files(&path, &name, out)?;
        } else if kind.is_file() {
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push((name, bytes));
        }
        // Symlinks are refused by horizon-mixins too; skip them here.
    }
    Ok(())
}

/// Whether the source at `source` has the `mods` feature (the launcher adds
/// `--features horizon-client/mods` when it does; later revisions dropped it).
pub fn has_mods_feature(source: &Path) -> bool {
    let path = source.join("crates/horizon-client/Cargo.toml");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let value: toml::Table = text.parse().unwrap_or_default();
    value
        .get("features")
        .and_then(|f| f.get("mods"))
        .is_some()
}

/// Whether the workspace carries `.cargo/config.toml` — the stage is a copy
/// of the source with the mods applied, so the stage's config is the source's
/// (the launcher passes it as a `--config` when it exists).
fn stage_has_cargo_config(source: &Path) -> bool {
    source.join(".cargo").join("config.toml").is_file()
}

/// Compare the planned build with the last record of the same profile, to say
/// what the build will do (the launcher's `classify`).
pub fn classify(exists: bool, before: Option<&Record>, now: &Record) -> PlanKind {
    if exists {
        return PlanKind::UpToDate;
    }
    let Some(before) = before else {
        return PlanKind::First;
    };
    let mut changes = Vec::new();
    if before.horizon != now.horizon {
        let short = |r: &str| r.chars().take(7).collect::<String>();
        changes.push(format!(
            "Horizon commit {} to {}",
            short(&before.horizon),
            short(&now.horizon)
        ));
    }
    let changed: Vec<String> = now
        .mods
        .iter()
        .filter(|(id, hash)| before.mods.get(*id) != Some(*hash))
        .map(|(id, _)| id.clone())
        .chain(
            before
                .mods
                .keys()
                .filter(|id| !now.mods.contains_key(*id))
                .cloned(),
        )
        .collect();
    if !changed.is_empty() {
        changes.push(format!("mods {}", changed.join(", ")));
    }
    if before.mixins != now.mixins {
        changes.push("horizon-mixins".into());
    }
    if changes.is_empty() {
        changes.push("the build settings".into());
    }
    PlanKind::Incremental(changes)
}

/// Plan the whole build. Reads the launcher's state; writes nothing.
pub fn plan_build(
    paths: &LauncherPaths,
    profile: Profile,
    revision_choice: &RevisionChoice,
) -> Result<BuildPlan, String> {
    let revision = match revision_choice {
        RevisionChoice::Explicit(rev) | RevisionChoice::FromBuiltJson(rev) => rev.clone(),
    };
    let source = paths.source(&revision);
    let compose_dir = paths.compose_dir();
    let stage_dir = paths.stage_dir();

    // The extracted mods, then the enabled ones only.
    let all = super::manifests::bundles(&paths.installed_incoming())?;
    let mods_list = paths.mods_list();
    let mut mods: Vec<Bundle> = Vec::new();
    let mut disabled = Vec::new();
    for bundle in all {
        if super::modslist::is_enabled(&mods_list, &bundle.id) {
            mods.push(bundle);
        } else {
            disabled.push(bundle.id);
        }
    }
    if mods.is_empty() {
        return Err(format!(
            "No native mod is turned on in {}: enable one first (Mods tab, `enable <id>`, or the launcher's Mods page).",
            mods_list.display()
        ));
    }

    // The mixins rev the build uses (newest among the mods' declarations).
    let bundle_refs: Vec<&Bundle> = mods.iter().collect();
    let mixins_rev = choose_mixins_rev(paths, &bundle_refs)?;

    // Planned compose copies, with pins rewritten as the launcher does.
    let mut planned: Vec<PlannedMod> = Vec::new();
    let mut mismatched: Vec<String> = Vec::new();
    for bundle in &mods {
        let manifest_path = bundle.dir.join(MANIFEST);
        let original = std::fs::read(&manifest_path)
            .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
        let needs_repin =
            bundle.hooks.is_some() || bundle.horizon.as_ref().is_some_and(|r| *r != revision);
        let repin = if needs_repin {
            Some((bundle.horizon.clone(), revision.clone()))
        } else {
            None
        };
        let manifest_text = match &repin {
            Some(_) => super::manifests::rewrite_pin(&original, &revision)?,
            None => String::from_utf8(original.clone())
                .map_err(|e| format!("{}: {e}", manifest_path.display()))?,
        };
        if bundle.hooks.is_none() && bundle.horizon.as_ref().is_some_and(|r| *r != revision) {
            mismatched.push(format!(
                "{} → {}",
                bundle.id,
                super::modslist::shorten(bundle.horizon.as_deref().unwrap_or(""))
            ));
        }
        let files = planned_files(bundle, &manifest_text)?;
        let files_hash = mod_hash(&files);
        planned.push(PlannedMod {
            id: bundle.id.clone(),
            version: bundle.version.clone(),
            source_dir: bundle.dir.clone(),
            compose_dir: compose_dir.join(&bundle.id),
            repin,
            manifest_text,
            files,
            files_hash,
        });
    }

    let mut warnings = Vec::new();
    if !mismatched.is_empty() {
        warnings.push(format!(
            "Some mods were made for other Horizon versions ({}; the build uses {}). Building anyway on {}.",
            mismatched.join(", "),
            super::modslist::shorten(&revision),
            super::modslist::shorten(&revision)
        ));
    }

    // The version tag, and whether it is already installed.
    let version_tag = version_for(&revision, &mixins_rev, profile, &planned);
    let version_dir = paths.version_dir(&version_tag);
    let target_binary = paths
        .native_target()
        .join(profile.target_dir_name())
        .join("horizon-client");
    let exists = version_dir.join(".installed").is_file() && version_dir.join("horizon-client").is_file();

    // The record this build will produce.
    let record = Record {
        horizon: revision.clone(),
        mixins: mixins_rev.clone(),
        mods: planned
            .iter()
            .map(|m| (m.id.clone(), m.files_hash.clone()))
            .collect(),
    };
    let records = read_records(&paths.built_json());
    let before = records.get(&profile.record_key());
    let plan_kind = classify(exists, before, &record);

    // The toolchain and the exact commands.
    let rust_dir = paths
        .rust_toolchain()
        .and_then(|(v, dir)| (v == RUST_VERSION).then_some(dir))
        .ok_or_else(|| format!("No pinned Rust toolchain {} under {}.", RUST_VERSION, paths.root.join("rust").display()))?;
    let cargo_bin = rust_dir.join("bin").join("cargo");
    let mixins_tool = paths.mixins_tool(&mixins_rev);

    let mut steps: Vec<PlanStep> = Vec::new();
    // 1. Compose: clean, then one copy per bundle (re-rooted).
    steps.push(PlanStep {
        title: format!("Compose the --mods dir ({})", compose_dir.display()),
        command: format!("rm -rf {} && mkdir -p {}", sh(&compose_dir), sh(&compose_dir)),
        executed_by_tool: true,
    });
    for m in &planned {
        steps.push(PlanStep {
            title: format!("Copy mod {} (re-rooted, {} files)", m.id, m.files.len()),
            command: format!(
                "rm -rf {} && cp -a {}/. {}/",
                sh(&m.compose_dir),
                sh(&m.source_dir),
                sh(&m.compose_dir)
            ),
            executed_by_tool: true,
        });
        if let Some((from, to)) = &m.repin {
            let from_note = from
                .as_deref()
                .map(super::modslist::shorten)
                .unwrap_or_else(|| "(none)".into());
            steps.push(PlanStep {
                title: format!("Rewrite pin of {} ({} → {})", m.id, from_note, super::modslist::shorten(to)),
                command: format!(
                    "sed -i 's/horizon_revision = \"[0-9a-f]*\"/horizon_revision = \"{to}\"/' {}/horizon-mixin.toml",
                    sh(&m.compose_dir)
                ),
                executed_by_tool: true,
            });
        }
    }
    // 2. Apply the mixins.
    steps.push(PlanStep {
        title: format!("Apply mixins into {}", stage_dir.display()),
        command: format!(
            "rm -rf {out} && {tool} apply --workspace {ws} --mods {mods} --out {out}",
            out = sh(&stage_dir),
            tool = sh(&mixins_tool),
            ws = sh(&source),
            mods = sh(&compose_dir),
        ),
        executed_by_tool: true,
    });
    // 3. The injections file the game's build.rs requires.
    steps.push(PlanStep {
        title: format!("Write {INJECTIONS_FILE} with content []"),
        command: format!(
            "printf '[]' > {}",
            sh(&stage_dir.join(INJECTIONS_FILE))
        ),
        executed_by_tool: true,
    });
    // 4. Build.
    let build_cmd = build_command(paths, profile, &cargo_bin, &stage_dir, &source, &rust_dir);
    steps.push(PlanStep {
        title: format!("Build horizon-client (cargo profile {})", profile.cargo().0),
        command: build_cmd,
        executed_by_tool: true,
    });
    // 5. Install: cp to .new, same-directory rename, marker.
    steps.push(PlanStep {
        title: format!("Install into versions/{version_tag}/"),
        command: format!(
            "mkdir -p {dir} && cp {bin} {new} && mv {new} {client} && : > {marker}",
            dir = sh(&version_dir),
            bin = sh(&target_binary),
            new = sh(&version_dir.join(".horizon-client.new")),
            client = sh(&version_dir.join("horizon-client")),
            marker = sh(&version_dir.join(".installed")),
        ),
        executed_by_tool: true,
    });
    // 6. built.json: backup, then update this profile's record.
    steps.push(PlanStep {
        title: format!(
            "Update built.json (backup to {})",
            paths.built_json_bak().display()
        ),
        command: format!(
            "cp {} {}.bak && write built.json[{}] = horizon {h}, mixins {m}, {} mod hash(es)",
            sh(&paths.built_json()),
            sh(&paths.built_json()),
            profile.record_key(),
            record.mods.len(),
            h = super::modslist::shorten(&record.horizon),
            m = super::modslist::shorten(&record.mixins),
        ),
        executed_by_tool: true,
    });

    Ok(BuildPlan {
        profile,
        revision,
        mixins_rev,
        version_tag,
        plan_kind,
        warnings,
        mods: planned,
        disabled,
        steps,
        record,
        record_key: profile.record_key(),
        target_binary,
        version_dir,
        stage_dir,
        compose_dir,
    })
}

/// The exact cargo command for the build, as the launcher runs it (plus this
/// tool's low-priority and job-count defaults).
fn build_command(
    paths: &LauncherPaths,
    profile: Profile,
    cargo_bin: &Path,
    stage_dir: &Path,
    source: &Path,
    rust_dir: &Path,
) -> String {
    let (cargo_profile, configs) = profile.cargo();
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("CARGO_HOME={}", sh(&paths.cargo_home())));
    parts.push(format!("CARGO_TARGET_DIR={}", sh(&paths.native_target())));
    parts.push(format!(
        "HORIZON_MOD_API_INJECTIONS={}",
        sh(&stage_dir.join(INJECTIONS_FILE))
    ));
    parts.push(format!("PATH={}:$PATH", sh(&rust_dir.join("bin"))));
    parts.push(format!("RUSTC={}", sh(&rust_dir.join("bin").join("rustc"))));
    parts.push(format!("TMPDIR={}", sh(&paths.root.join("tmp"))));
    parts.push("CARGO_BUILD_JOBS=2".to_string());
    parts.push("nice -n 19 ionice -c 3".to_string());
    parts.push(sh(cargo_bin));
    parts.push("build".to_string());
    parts.push(format!("--manifest-path {}", sh(&stage_dir.join("Cargo.toml"))));
    parts.push("--locked".to_string());
    parts.push("--offline".to_string());
    if stage_has_cargo_config(source) {
        parts.push(format!(
            "--config {}",
            sh(&stage_dir.join(".cargo").join("config.toml"))
        ));
    }
    parts.push(format!("--profile {cargo_profile}"));
    for config in configs {
        parts.push(format!("--config {config}"));
    }
    if has_mods_feature(source) {
        parts.push(format!("--features {FEATURES_KEY}"));
    }
    parts.push("-p horizon-client".to_string());
    parts.push("--bin horizon-client".to_string());
    parts.join(" ")
}

/// Minimal shell quoting: single-quote anything with spaces or specials.
pub fn sh(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.is_empty() {
        return "''".into();
    }
    if text.chars().all(|c| {
        c.is_ascii_alphanumeric() || "/._-+=:@%,".contains(c)
    }) {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The injections file's exact content for the stage ([] — an empty JSON
/// array; the game's build.rs refuses to run without the file).
pub fn injections_content() -> serde_json::Value {
    json!([])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::testutil::temp_root;

    const REV: &str = "2ebf00a0281c592f79191e76fce0e7d522c6f425";
    const FREECAM_REV: &str = "af55a734865da4f1b11836164a979ff06a1f0449";

    /// A minimal launcher layout: pinned source, toolchain, one mod, state.
    fn fixture(tmp: &Path, enabled: bool) -> LauncherPaths {
        let paths = LauncherPaths {
            root: tmp.to_path_buf(),
            game_home: tmp.join("game"),
        };
        // Pinned source with no mods feature.
        let client_toml = paths.source(REV).join("crates/horizon-client/Cargo.toml");
        std::fs::create_dir_all(client_toml.parent().unwrap()).unwrap();
        std::fs::write(client_toml, "[features]\ndefault = [\"audio\"]\n").unwrap();
        // Pinned toolchain.
        let bin = paths
            .root
            .join(format!("rust/{RUST_VERSION}-x86_64-unknown-linux-gnu"))
            .join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("cargo"), "").unwrap();
        std::fs::write(bin.join("rustc"), "").unwrap();
        // Mixins tool.
        let tool = paths.mixins_tool(MIXINS_REV);
        std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
        std::fs::write(&tool, "").unwrap();
        // One mod (a freecam pinning an older Horizon).
        let bundle = paths
            .installed_mod("freecam")
            .join("mixins")
            .join("freecam");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(
            bundle.join(MANIFEST),
            format!(
                "[mod]\nid = \"freecam\"\nversion = \"0.1.0\"\npriority = 100\n\n[compatibility]\nhorizon_revision = \"{FREECAM_REV}\"\n"
            ),
        )
        .unwrap();
        std::fs::write(bundle.join("runtime.rs"), b"pub const FREECAM: u32 = 1;\n").unwrap();
        std::fs::write(
            paths.installed_mod("freecam").join(super::super::manifests::NATIVE_MANIFEST),
            br#"{"game":"26.3","horizon":"af55a734865da4f1b11836164a979ff06a1f0449","id":"freecam","mixins":"4342a0bfbe71d4d14185528d64e496d6022f16a1","schema":1,"version":"0.1.0"}"#,
        )
        .unwrap();
        // Enable state.
        std::fs::create_dir_all(&paths.game_home).unwrap();
        std::fs::write(
            paths.mods_list(),
            if enabled { "mod.freecam:true\n" } else { "mod.freecam:false\n" },
        )
        .unwrap();
        // built.json with the revision we build on.
        std::fs::create_dir_all(paths.native()).unwrap();
        std::fs::write(
            paths.built_json(),
            format!(
                r#"{{"fast": {{"horizon": "{REV}", "mixins": "{MIXINS_REV}", "mods": {{}}}}}}"#
            ),
        )
        .unwrap();
        // Commit dates for the mixins-rev resolution (the launcher records
        // these from GitHub).
        std::fs::write(
            paths.native().join("commit-dates.json"),
            format!(
                r#"{{
  "horizon-rust/horizon-mixins@0d6b103c70633d8fd9443d8d0acd3aadaacedb06": "2026-10-28T19:48:20Z",
  "horizon-rust/horizon-mixins@4342a0bfbe71d4d14185528d64e496d6022f16a1": "2026-10-31T04:11:36Z",
  "horizon-rust/horizon-mixins@5a64af938ec86b00d53d16a408aabc0038fb3e57": "2026-11-05T19:46:05Z"
}}"#
            ),
        )
        .unwrap();
        paths
    }

    #[test]
    fn plans_full_sequence_with_repin_and_warning() {
        let tmp = temp_root("plan-full");
        let paths = fixture(&tmp, true);
        let choice = choose_revision(&paths, None).unwrap();
        assert!(matches!(choice, RevisionChoice::FromBuiltJson(_)));
        let plan = plan_build(&paths, Profile::Performance, &choice).unwrap();

        // The mod is repinned (its manifest pins an older Horizon) and warned about.
        assert_eq!(plan.mods.len(), 1);
        assert_eq!(
            plan.mods[0].repin,
            Some((Some(FREECAM_REV.to_string()), REV.to_string()))
        );
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("freecam → af55a73"), "{}", plan.warnings[0]);
        assert_eq!(plan.disabled, Vec::<String>::new());

        // The planned manifest text carries the new pin.
        assert!(plan.mods[0].manifest_text.contains(REV));

        // The steps match the recipe, in order.
        let titles: Vec<&str> = plan.steps.iter().map(|s| s.title.as_str()).collect();
        assert!(titles[0].starts_with("Compose the --mods dir"), "{titles:?}");
        assert!(titles.iter().any(|t| t.starts_with("Copy mod freecam")));
        assert!(titles.iter().any(|t| t.starts_with("Rewrite pin of freecam")));
        assert!(titles.iter().any(|t| t.starts_with("Apply mixins into")));
        assert!(titles.iter().any(|t| t.starts_with("Write .horizon-mod-api-injections.json")));
        assert!(titles.iter().any(|t| t.starts_with("Build horizon-client")));
        assert!(titles.iter().any(|t| t.starts_with("Install into")));
        assert!(titles.iter().any(|t| t.starts_with("Update built.json")));

        // The apply command names the launcher's real flags.
        let apply = plan
            .steps
            .iter()
            .find(|s| s.title.starts_with("Apply mixins"))
            .unwrap();
        assert!(apply.command.contains("apply --workspace"), "{}", apply.command);
        assert!(apply.command.contains("--mods"), "{}", apply.command);
        assert!(apply.command.contains("--out"), "{}", apply.command);

        // The build command carries the env and flags of the recipe.
        let build = plan
            .steps
            .iter()
            .find(|s| s.title.starts_with("Build horizon-client"))
            .unwrap();
        assert!(build.command.contains("CARGO_HOME="), "{}", build.command);
        assert!(build.command.contains("CARGO_TARGET_DIR="), "{}", build.command);
        assert!(build.command.contains("HORIZON_MOD_API_INJECTIONS="), "{}", build.command);
        assert!(build.command.contains("--manifest-path"), "{}", build.command);
        assert!(build.command.contains("--locked"), "{}", build.command);
        assert!(build.command.contains("--offline"), "{}", build.command);
        assert!(build.command.contains("--profile fast"), "{}", build.command);
        assert!(build.command.contains("-p horizon-client"), "{}", build.command);
        assert!(build.command.contains("--bin horizon-client"), "{}", build.command);
        assert!(build.command.contains("nice -n 19 ionice -c 3"), "{}", build.command);
        assert!(build.command.contains("CARGO_BUILD_JOBS=2"), "{}", build.command);
        // The source has no `mods` feature, so no --features flag.
        assert!(!build.command.contains("--features"), "{}", build.command);

        // Version tag shape and record.
        assert!(plan.version_tag.starts_with("native-2ebf00a-"), "{}", plan.version_tag);
        assert_eq!(plan.version_tag.len(), "native-2ebf00a-42ebf5dd24f9".len());
        assert_eq!(plan.record.horizon, REV);
        assert_eq!(plan.record_key, "fast");
        // built.json holds an earlier build of this profile with no freecam
        // hash: the plan says what would recompile.
        match &plan.plan_kind {
            PlanKind::Incremental(changes) => {
                assert!(
                    changes.iter().any(|c| c.contains("mods freecam")),
                    "{changes:?}"
                );
            }
            other => panic!("expected an incremental plan, got {other:?}"),
        }
        let rendered = plan.render().join("\n");
        assert!(rendered.contains("profile fast"), "{rendered}");
    }

    #[test]
    fn disabled_mods_are_left_out() {
        let tmp = temp_root("plan-disabled");
        let paths = fixture(&tmp, false);
        let choice = choose_revision(&paths, None).unwrap();
        // Nothing enabled: the plan refuses, naming the enable-state file.
        let err = plan_build(&paths, Profile::Performance, &choice).unwrap_err();
        assert!(err.contains("No native mod is turned on"), "{err}");
        assert!(err.contains("horizon-mods.txt"), "{err}");
    }

    #[test]
    fn hooks_mods_repin_without_warning() {
        let tmp = temp_root("plan-hooks");
        let paths = fixture(&tmp, true);
        // zoom follows the hook catalog (hooks: 1), so its pin is rewritten
        // silently — that is why the launcher's own build.log warns only about
        // the pin-pinned mods.
        let bundle = paths.installed_mod("zoom").join("mixins").join("zoom");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(
            bundle.join(MANIFEST),
            format!(
                "[mod]\nid = \"zoom\"\nversion = \"2.0.0\"\n\n[compatibility]\nhorizon_revision = \"6f8c7a0e5d531d3834c4d0be4659a1db8e7e5c2a\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            paths.installed_mod("zoom").join(super::super::manifests::NATIVE_MANIFEST),
            br#"{"game":"26.3","hooks":1,"horizon":"6f8c7a0e5d531d3834c4d0be4659a1db8e7e5c2a","id":"zoom","mixins":"0d6b103c70633d8fd9443d8d0acd3aadaacedb06","schema":1,"version":"2.0.0"}"#,
        )
        .unwrap();
        std::fs::write(paths.mods_list(), "mod.freecam:true\nmod.zoom:true\n").unwrap();
        let choice = choose_revision(&paths, None).unwrap();
        let plan = plan_build(&paths, Profile::Performance, &choice).unwrap();
        let zoom = plan.mods.iter().find(|m| m.id == "zoom").unwrap();
        assert_eq!(zoom.repin, Some((Some("6f8c7a0e5d531d3834c4d0be4659a1db8e7e5c2a".into()), REV.into())));
        // The warning still names freecam, not zoom.
        assert!(plan.warnings[0].contains("freecam"));
        assert!(!plan.warnings[0].contains("zoom"));
    }

    #[test]
    fn version_tag_is_stable_across_runs() {
        let tmp = temp_root("plan-version-stable");
        let paths = fixture(&tmp, true);
        let choice = choose_revision(&paths, None).unwrap();
        let a = plan_build(&paths, Profile::Performance, &choice).unwrap();
        let b = plan_build(&paths, Profile::Performance, &choice).unwrap();
        assert_eq!(a.version_tag, b.version_tag);
        // A different profile is a different tag (cargo profile feeds the hash).
        let c = plan_build(&paths, Profile::Release, &choice).unwrap();
        assert_ne!(a.version_tag, c.version_tag);
    }

    #[test]
    fn classify_reports_changes() {
        let now = Record {
            horizon: REV.into(),
            mixins: MIXINS_REV.into(),
            mods: BTreeMap::from([("freecam".into(), "92cdd20ffba6a139".into())]),
        };
        // Same state: incremental with nothing but settings.
        let kind = classify(false, Some(&now), &now);
        assert_eq!(kind, PlanKind::Incremental(vec!["the build settings".into()]));
        // A changed mod hash shows up.
        let before = Record {
            horizon: REV.into(),
            mixins: MIXINS_REV.into(),
            mods: BTreeMap::from([("freecam".into(), "0000000000000000".into())]),
        };
        let kind = classify(false, Some(&before), &now);
        assert_eq!(kind, PlanKind::Incremental(vec!["mods freecam".into()]));
        // Installed already: up to date.
        assert_eq!(classify(true, Some(&before), &now), PlanKind::UpToDate);
        assert_eq!(classify(false, None, &now), PlanKind::First);
    }

    #[test]
    fn mixins_rev_newest_by_commit_date() {
        let tmp = temp_root("plan-mixins-rev");
        let paths = fixture(&tmp, true);
        // No dates: two distinct pins among the mods would refuse; one pin is taken as-is.
        let one = Bundle {
            id: "zoom".into(),
            version: "2.0.0".into(),
            dir: tmp.clone(),
            horizon: Some(REV.into()),
            mixins: Some("0d6b103c70633d8fd9443d8d0acd3aadaacedb06".into()),
            hooks: None,
        };
        assert_eq!(
            choose_mixins_rev(&paths, &[&one]).unwrap(),
            "0d6b103c70633d8fd9443d8d0acd3aadaacedb06"
        );
        // Two distinct pins: newest by the recorded date, MIXINS_REV joins in.
        let dates = paths.native().join("commit-dates.json");
        std::fs::write(
            &dates,
            r#"{
  "horizon-rust/horizon-mixins@0d6b103c70633d8fd9443d8d0acd3aadaacedb06": "2026-10-28T19:48:20Z",
  "horizon-rust/horizon-mixins@4342a0bfbe71d4d14185528d64e496d6022f16a1": "2026-10-31T04:11:36Z",
  "horizon-rust/horizon-mixins@5a64af938ec86b00d53d16a408aabc0038fb3e57": "2026-11-05T19:46:05Z"
}"#,
        )
        .unwrap();
        let two = Bundle {
            id: "freecam".into(),
            version: "0.1.0".into(),
            dir: tmp.clone(),
            horizon: Some(REV.into()),
            mixins: Some("4342a0bfbe71d4d14185528d64e496d6022f16a1".into()),
            hooks: None,
        };
        assert_eq!(
            choose_mixins_rev(&paths, &[&one, &two]).unwrap(),
            MIXINS_REV,
            "2026-11-05 is the newest of the three"
        );
    }

    #[test]
    fn explicit_revision_must_be_40_hex() {
        let tmp = temp_root("plan-explicit-rev");
        let paths = fixture(&tmp, true);
        assert!(choose_revision(&paths, Some("nope")).is_err());
        assert!(choose_revision(&paths, Some(REV)).is_ok());
        assert!(choose_revision(&paths, Some(&"0".repeat(40))).is_err(), "no source for it");
    }

    #[test]
    fn injections_content_is_empty_array() {
        assert_eq!(injections_content().to_string(), "[]");
    }
}
