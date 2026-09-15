use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "ramiz",
    version,
    about = "Ridiculously cheap Git worktrees using filesystem copy-on-write"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create an ordinary Git worktree using filesystem copy-on-write.
    Add(AddArgs),
    /// Probe copy-on-write support on an actual destination filesystem.
    Doctor(DoctorArgs),
    /// Check for or apply an installer-aware Ramiz update.
    Update(UpdateArgs),
}

impl Command {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Add(_) => "add",
            Self::Doctor(_) => "doctor",
            Self::Update(_) => "update",
        }
    }
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("branch-mode").args(["branch", "detach"])))]
pub struct AddArgs {
    /// Create a new branch.
    #[arg(short = 'b', value_name = "new-branch")]
    pub branch: Option<String>,
    /// Create a detached worktree.
    #[arg(long)]
    pub detach: bool,
    /// Clone the donor's complete lived-in state.
    #[arg(long)]
    pub inherit: bool,
    /// Select a registered worktree as the physical donor.
    #[arg(long, value_name = "path")]
    pub from: Option<PathBuf>,
    /// Permit inherited links that resolve inside the donor.
    #[arg(long, requires = "inherit")]
    pub allow_donor_links: bool,
    /// Refuse ordinary-checkout fallback.
    #[arg(long, conflicts_with = "allow_copy")]
    pub require_cow: bool,
    /// Permit physical-copy fallback for inheritance.
    #[arg(long, requires = "inherit")]
    pub allow_copy: bool,
    /// Emit one ramiz.cli/v1 envelope on stdout.
    #[arg(long)]
    pub json: bool,
    /// Worktree destination.
    #[arg(value_name = "path")]
    pub path: PathBuf,
    /// Branch or commit-ish to check out.
    #[arg(value_name = "start-point")]
    pub start_point: Option<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Filesystem location to probe.
    #[arg(value_name = "path")]
    pub path: Option<PathBuf>,
    /// Emit one ramiz.cli/v1 envelope on stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Check without changing the installation.
    #[arg(long)]
    pub check: bool,
    /// Adopt a verified standalone archive installation for managed updates.
    #[arg(long, conflicts_with = "check")]
    pub adopt: bool,
    /// Emit one ramiz.cli/v1 envelope on stdout.
    #[arg(long)]
    pub json: bool,
}
