# horizon-launcher-tui

A terminal UI and scriptable CLI for the [Horizon](https://github.com/horizon-rust/horizon)
game launcher's native-mods workflows — a port of what the GUI launcher does
with **native mods** into ratatui + crossterm and plain subcommands:

- discover the launcher's pinned game source, horizon-mixins tool and Rust toolchain
- list installed native mods and turn them on/off (the same `horizon-mods.txt`
  the game and the GUI launcher use)
- plan (and run) the full native client build: compose the `--mods` dir, rewrite
  revision pins on copies, `horizon-mixins apply`, the injections file, the
  pinned-toolchain cargo build, the safe install rename and the `built.json` record
- launch a built client in the game's data dir
- install `.hnmod` bundles (signature-checked when a key is available) and uninstall them

## Layout it operates on

Everything lives under the launcher's data root (default
`~/.local/share/horizon-launcher`, override with `HORIZON_LAUNCHER_DIR`):

```
native/source/<horizon-rev>/        pinned game source
native/tools/<rev[..12]>/bin/horizon-mixins
rust/1.99.1-x86_64-unknown-linux-gnu/bin/{cargo,rustc}
cargo/                              pinned CARGO_HOME
native/installed.incoming/<id>/     extracted mods (horizon-native.json + mixins/<id>/)
native/target/                      shared cargo target dir (this tool never makes a second one)
native/built.json                   last-build record per profile (written with a .bak backup)
versions/<tag>/horizon-client       built binaries (tag: native-<rev7>-<12 hex>)
```

The game's data dir (the client's cwd) defaults to `~/.local/share/horizon` and
follows `HORIZON_HOME` like the launcher itself. When an alternate launcher
root is set with no `HORIZON_HOME`, the game home stays beside that root
(`<root>/game-home`) so a scratch install can never write into the player's
real game data.

## Commands

With no subcommand the TUI starts (tabs: Dashboard, Mods, Build, Launch,
Settings; `Tab`/`1..5` switch, `q` quits, per-tab keys shown in the status line).

```
horizon-launcher-tui list [--verbose]      native mods: id, version, on/off, where, pin
horizon-launcher-tui doctor                read-only health report (revs, toolchain, client sha256)
horizon-launcher-tui build [--profile performance|dev|release] [--revision <40 hex>] [--dry-run]
horizon-launcher-tui launch [--version <tag>] [--control <path>] [--dry-run]
horizon-launcher-tui install <path/to/mod.hnmod>
horizon-launcher-tui uninstall <id>
horizon-launcher-tui enable <id>           turn a mod on in horizon-mods.txt
horizon-launcher-tui disable <id>          turn a mod off in horizon-mods.txt
```

`--profile` also accepts `fast`, the old name of `performance`.

## Semantics (mirrored from the launcher's own behavior)

- **Enable state** lives in the game's `horizon-mods.txt` as `mod.<id>:true|false`
  — the same file the game's Options > Mods and the GUI launcher write. There is
  no separate disabled-set file; a mod with no line counts as **off**. A build
  composes only the enabled mods.
- **Profiles**: `performance` compiles with the cargo profile **`fast`** (this is
  also the key in `native/built.json`), `dev` compiles with `fast` plus
  `profile.fast.package.horizon-client.opt-level=0`, `release` with `release`.
  `Performance` and `Dev` share one record key.
- **Revision pins**: bundles whose `[compatibility] horizon_revision` differs
  from the build's revision — and bundles that follow the native hook catalog
  (`hooks` in `horizon-native.json`) — get their pin rewritten **on a copy** in
  the compose dir; the `installed.incoming` originals are never mutated. Mods
  pinning another known commit produce the launcher's "made for other Horizon
  versions … Building anyway" warning.
- **The revision to build on** is the last build's (`native/built.json`),
  overridable with `--revision` (it must have a pinned source under
  `native/source/`). The mixins revision follows the mods' declarations, newest
  by the dates the launcher records in `native/commit-dates.json`.
- **Version tags** are computed exactly like the launcher's `version_for`
  (SHA-256 over mixins rev, cargo profile, features key, Rust version, revision
  and every bundle file) → `native-<rev7>-<12 hex>`; a tag already installed
  (`.installed` marker) is reported as up to date.
- **Install** extracts the `.hnmod` zip preserving its layout into
  `installed.incoming/<id>/`, and also keeps the bundle in the game's
  `horizon-native-mods/`, where the GUI launcher re-syncs from. A failing
  signature check aborts; **without** a matching `.pub`/`.minisig` next to the
  bundle the check is skipped with a warning (the registry CLI
  `cargo-horizon-mod` is used when present; `HORIZON_CARGO_MOD_CLI` overrides).
- **Install of a new client** goes through `versions/<tag>/.horizon-client.new`
  and a same-directory rename onto `horizon-client` — a running game keeps the
  old binary (and no ETXTBSY).
- **built.json** is backed up to `built.json.bak` before every write.

## Low-priority defaults

Builds are never allowed to take the machine over: this tool wraps build
commands in `nice -n 19 ionice -c 3` (when available) and sets
`CARGO_BUILD_JOBS=2` unless the environment pins it. This mirrors the
launcher's own `rust_toolchain::low_priority`.

## Limitations

- **No auth/sign-in**: account features are out of scope entirely.
- **No registry browsing or download**: the mod browser is future work; this
  tool installs a local `.hnmod` you already have.
- **No native-mods.json writes**: per-version build records the GUI launcher
  keeps (`versions/<tag>/native-mods.json`) are read where needed but not
  written by builds made here.
- **No dependency resolution**: mods declaring `[[dependencies]]` are not
  reordered or validated (none of the current bundles use it); compose order is
  sorted by id, which is the order the launcher discovers in.
- **No mixins-failure isolation**: if `horizon-mixins` refuses one mod, the
  launcher retries without it ("left out"); here the build reports the failure
  and stops. Disable the offending mod and rebuild.
- **The heavy end-to-end build and a real game launch are deliberately not
  exercised on this machine yet** (a 15 GiB box, other large processes running).
  `build --dry-run` and `launch --dry-run` print the exact sequences, and the
  executor code paths are implemented and unit-tested; run them when memory
  frees up:

  ```
  ./target/release/horizon-launcher-tui build --profile performance
  ./target/release/horizon-launcher-tui launch
  ```

## License

MIT — see [LICENSE](LICENSE).
