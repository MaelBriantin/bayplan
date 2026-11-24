mod cli;
mod commands;
mod compose;
mod config;
mod deps;
mod exec;
mod git;
mod install;
mod lock;
mod registration;
mod resources;
mod router;
mod state;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Up {
            bay,
            from,
            branch,
            build,
        } => commands::up(&bay, from.as_deref(), branch.as_deref(), build),
        Command::Down { bay } => commands::down(&bay),
        Command::Rm { bay, force } => commands::rm(&bay, force),
        Command::Ls => commands::ls(),
        Command::Update => install::update(),
        Command::Uninstall => install::uninstall(),
    };
    if let Err(error) = result {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
