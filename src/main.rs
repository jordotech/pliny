mod cli;
mod digest;
mod groups;
mod llm;
mod plan;
mod runner;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pliny: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let args = cli::Args::parse();

    let (plan, groups, summary) = if let Some(path) = &args.plan_path {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read plan file: {}", path.display()))?;
        let plan = plan::Plan::parse(&raw)
            .with_context(|| format!("failed to parse plan JSON from {}", path.display()))?;
        let groups = groups::group_plan(&plan, args.show_noops);
        let summary = if args.no_ai || std::env::var_os("OPENAI_API_KEY").is_none() {
            llm::Summary::Disabled(llm::disabled_reason(args.no_ai))
        } else {
            let digest = digest::digest_with_budget(&plan, &groups);
            llm::fetch_summary(&digest, &args.model)
        };
        (Some(plan), groups, summary)
    } else {
        (None, Vec::new(), llm::Summary::Disabled("no plan loaded"))
    };

    ui::run(ui::InitialState {
        plan,
        groups,
        summary,
        show_noops: args.show_noops,
        no_ai: args.no_ai,
        model: args.model,
    })
}
