use clap::{Parser, Subcommand};

/// Runs an isolated copy of a Docker Compose stack for each git worktree.
#[derive(Parser)]
#[command(name = "bp", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Creates what is missing for a bay, then starts it
    Up {
        bay: String,
        /// Git ref the worktree starts from, when it does not exist yet
        #[arg(long)]
        from: Option<String>,
        /// Rebuilds the images of the bay
        #[arg(long)]
        build: bool,
    },
    /// Stops a bay, keeping its worktree and resources
    Down { bay: String },
    /// Removes a bay: containers, resources, registrations and worktree
    Rm {
        bay: String,
        /// Removes the worktree even if it has uncommitted changes
        #[arg(long)]
        force: bool,
    },
    /// Lists the bays and their state
    Ls,
    /// Updates bp to the latest release
    Update,
    /// Removes bp from this computer, leaving the bays as they are
    Uninstall,
}
