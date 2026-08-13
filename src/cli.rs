use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "forkpkg")]
#[command(about = "Create editable local source forks of Nix packages")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a local editable fork for a Nix installable.
    Fork {
        /// Nix installable, for example nixpkgs#ripgrep.
        installable: String,
    },

    /// List managed forks.
    List {
        /// Emit structured JSON.
        #[arg(long)]
        json: bool,
    },

    /// Rebuild a fork using its local source tree.
    Build {
        /// Fork workspace or source directory. Defaults to the current directory.
        path: Option<PathBuf>,
    },

    /// Print metadata for a fork workspace.
    Info {
        /// Fork workspace or source directory. Defaults to the current directory.
        path: Option<PathBuf>,
    },

    /// Make this machine use the forked build output.
    Enable {
        /// Fork workspace or source directory. Defaults to the current directory.
        path: Option<PathBuf>,
    },

    /// Revert a previous machine-local activation.
    Disable {
        /// Fork workspace or source directory. Defaults to the current directory.
        path: Option<PathBuf>,
    },

    /// Show whether a fork is active on this machine.
    Status {
        /// Fork workspace or source directory. Defaults to the current directory.
        path: Option<PathBuf>,
    },
}
