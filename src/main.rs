//! horizon-launcher-tui: a terminal UI and CLI over the Horizon game
//! launcher's native-mods workflows.

mod cli;
mod launcher;
mod tui;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    let code = cli::run(cli);
    std::process::exit(code);
}
