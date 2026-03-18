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
        /// Print the planned execution order instead of the dependency tree.
        #[arg(long)]
        ordered: bool,
    },
    /// Execute a logical task in planned order.
    Run {
        /// Logical task name to execute.
        task: String,
    },
}
