use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "cargo-flux", bin_name = "cargo-flux")]
#[command(about = "Resolve workspace topology and task order across Rust, Node, and uv projects.")]
pub struct Cli {
    #[arg(long, short, default_value = ".", global = true)]
    pub root: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print discovered packages and their internal dependencies.
    Graph,
    /// Print packages in topological order.
    Topo,
    /// Print the planned task execution tree for a logical task.
    Plan {
        /// Logical task name to plan.
        task: String,
        /// Limit execution to packages affected since this base ref.
        #[arg(long, visible_alias = "affected-from", value_name = "BASE")]
        affected: Option<String>,
        /// Print the planned execution order instead of the dependency tree.
        #[arg(long, conflicts_with = "stamp_args")]
        ordered: bool,
        /// Print repeatable stamp package arguments for every package in the plan.
        #[arg(long, conflicts_with = "ordered")]
        stamp_args: bool,
    },
    /// Execute a logical task in planned order.
    Run {
        /// Logical task name to execute.
        task: String,
        /// Limit execution to packages affected since this base ref.
        #[arg(long, visible_alias = "affected-from", value_name = "BASE")]
        affected: Option<String>,
    },
    /// Print packages affected since the merge base with a base ref.
    Affected {
        /// Branch, tag, or commit the current branch will merge into.
        #[arg(long, value_name = "BASE")]
        base: String,
    },
    /// Print the next calculated semantic version.
    Version {
        /// Override the release channel instead of auto-detecting from branch.
        #[arg(long)]
        channel: Option<String>,
    },
    /// Stamp a version into selected workspace manifests.
    Stamp {
        /// Version to stamp. If omitted, calculates the next version automatically.
        version: Option<String>,
        /// Stamp only packages with this name. May be repeated.
        #[arg(long = "package", short = 'p')]
        packages: Vec<String>,
        /// Do not stamp packages with this name. May be repeated.
        #[arg(long)]
        exclude: Vec<String>,
        /// Do not stamp packages whose current version equals this value. May be repeated.
        #[arg(long = "exclude-version")]
        exclude_versions: Vec<String>,
    },
}
