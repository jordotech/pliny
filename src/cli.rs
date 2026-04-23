use clap::Parser;
use std::path::PathBuf;

/// pliny — terraform plan comprehension TUI with AI risk summaries
#[derive(Parser, Debug)]
#[command(name = "pliny", version, about)]
pub struct Args {
    /// Path to a terraform plan JSON file (output of `terraform show -json plan.binary`)
    pub plan_path: PathBuf,

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
