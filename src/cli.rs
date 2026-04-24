use clap::Parser;
use std::path::PathBuf;

/// pliny — terraform plan comprehension TUI with AI risk summaries
#[derive(Parser, Debug)]
#[command(name = "pliny", version, about)]
pub struct Args {
    /// Optional path to a terraform plan JSON file to preload
    /// (output of `terraform show -json plan.binary`). If omitted,
    /// pliny starts with an empty tree and waits for commands.
    pub plan_path: Option<PathBuf>,

    /// Skip the AI summary call
    #[arg(long)]
    pub no_ai: bool,

    /// Show no-op resource changes in the tree (hidden by default)
    #[arg(long)]
    pub show_noops: bool,

    /// OpenAI model id for the summary call
    #[arg(long, default_value = "gpt-4o-mini")]
    pub model: String,
}
