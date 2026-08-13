use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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

        /// Fork label. The first fork defaults to "default"; additional forks require a label.
        #[arg(long)]
        label: Option<String>,
    },

    /// List managed forks.
    List {
        /// Emit structured JSON.
        #[arg(long)]
        json: bool,
    },

    /// Rebuild a fork using its local source tree.
    Build {
        /// Fork name, workspace, or source directory. Defaults to the current directory.
        #[arg(value_name = "FORK")]
        path: Option<PathBuf>,
    },

    /// Export committed fork changes as a portable share artifact.
    Export {
        /// Fork name, workspace, or source directory. Defaults to the current directory.
        #[arg(value_name = "FORK")]
        path: Option<PathBuf>,

        /// Artifact path to create.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },

    /// Import a fork share artifact into a clean matching fork.
    Import {
        /// Artifact created by forkpkg export.
        #[arg(value_name = "FILE")]
        artifact: PathBuf,

        /// Fork name, workspace, or source directory. Defaults to the current directory.
        #[arg(value_name = "FORK")]
        path: Option<PathBuf>,
    },

    /// Print metadata for a fork workspace.
    Info {
        /// Fork name, workspace, or source directory. Defaults to the current directory.
        #[arg(value_name = "FORK")]
        path: Option<PathBuf>,
    },

    /// Discover activation targets for a fork.
    Targets {
        /// Fork name, workspace, or source directory. Defaults to the current directory.
        #[arg(value_name = "FORK")]
        path: Option<PathBuf>,

        /// Emit structured JSON.
        #[arg(long)]
        json: bool,
    },

    /// Make this machine use the forked build output.
    Enable {
        /// Fork name, workspace, or source directory. Defaults to the current directory.
        #[arg(value_name = "FORK")]
        path: Option<PathBuf>,

        /// Nix activation backend to use.
        #[arg(long, value_enum, default_value_t = ActivationBackend::Auto)]
        backend: ActivationBackend,

        /// Nix profile path for the nix-profile backend. Defaults to Nix's user profile.
        #[arg(long, value_name = "PATH")]
        profile: Option<PathBuf>,

        /// Run the backend's normal switch command after preparing module activation.
        #[arg(long)]
        switch: bool,

        /// Flake reference for --switch, for example /etc/nixos#host.
        #[arg(long, value_name = "REF")]
        flake: Option<String>,

        /// Preview activation without changing local machine state.
        #[arg(long)]
        dry_run: bool,
    },

    /// Revert a previous machine-local activation.
    Disable {
        /// Fork name, workspace, or source directory. Defaults to the current directory.
        #[arg(value_name = "FORK")]
        path: Option<PathBuf>,

        /// Preview deactivation without changing local machine state.
        #[arg(long)]
        dry_run: bool,
    },

    /// Revert every machine-local fork activation.
    DisableAll {
        /// Preview deactivation without changing local machine state.
        #[arg(long)]
        dry_run: bool,
    },

    /// Inspect forkpkg state and report stale or broken activations.
    Doctor,

    /// Show whether a fork is active on this machine.
    Status {
        /// Fork name, workspace, or source directory. Defaults to the current directory.
        #[arg(value_name = "FORK")]
        path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ActivationBackend {
    Auto,
    NixProfile,
    NixosModule,
    HomeManagerModule,
}
