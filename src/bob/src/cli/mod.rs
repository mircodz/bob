pub mod admin;
pub mod launch;
mod sessions;
#[cfg(test)]
mod tests;

use bob_types::ReasoningEffort;
use clap::Parser;
use std::path::PathBuf;

fn parse_reasoning(value: &str) -> Result<ReasoningEffort, String> {
    ReasoningEffort::parse(value).ok_or_else(|| "expected off, low, medium, high or max".to_owned())
}

#[derive(Debug, Parser)]
#[command(
    name = "bob",
    about = "A provider-agnostic, multi-agent coding assistant on the durable kernel.",
    version,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<admin::Command>,
    #[arg(
        long,
        help = "Durable database override (default: ~/.bob/sessions.sqlite); required when a previous durable store exists"
    )]
    pub database: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    pub workspace: PathBuf,
    #[arg(
        short = 'p',
        long,
        global = true,
        help = "Provider id: copilot | anthropic | openai | deepseek"
    )]
    pub provider: Option<String>,
    #[arg(
        short = 'm',
        long,
        global = true,
        help = "Model id within the provider, or a provider-qualified model id"
    )]
    pub model: Option<String>,
    #[arg(
        short = 'r',
        long,
        value_name = "EFFORT",
        value_parser = parse_reasoning,
        help = "Reasoning effort: off | low | medium | high | max. Overrides the configured default and a resumed session's level"
    )]
    pub reasoning: Option<ReasoningEffort>,
    #[arg(long, help = "Model id for delegated work")]
    pub worker_model: Option<String>,
    #[arg(
        long,
        help = "Disable terminal titles and automatic session title generation"
    )]
    pub no_terminal_title: bool,
    #[arg(long, visible_alias = "restore", num_args = 0..=1, default_missing_value = "", help = "Resume by id; omit the id for the Local/All session picker (Tab switches views)")]
    pub resume: Option<String>,
    #[arg(
        short = 'c',
        long,
        help = "Continue the most recent session in this directory (no picker)"
    )]
    pub r#continue: bool,
    #[arg(
        long,
        help = "Print a bounded JSON view without initializing providers or executing work"
    )]
    pub inspect: bool,
    #[arg(
        long,
        help = "Read an artifact without initializing providers or executing work"
    )]
    pub artifact: Option<String>,
    #[arg(long, requires = "inspect")]
    pub thread: Option<String>,
    #[arg(long, requires = "inspect")]
    pub before: Option<i64>,
    #[arg(long, default_value_t = 80, value_parser = crate::page_limit)]
    pub limit: usize,
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    #[arg(long, default_value_t = 16384, value_parser = crate::artifact_limit)]
    pub artifact_limit: usize,
}

pub async fn selection(
    args: &Args,
    database: &std::path::Path,
    workspace: &std::path::Path,
) -> anyhow::Result<bob::application::SessionSelection> {
    use bob::application::SessionSelection;
    if args.r#continue {
        return Ok(SessionSelection::Latest);
    }
    match args.resume.as_deref() {
        Some("") => sessions::pick_session(database, workspace, !args.no_terminal_title).await,
        Some(id) => Ok(SessionSelection::Id(bob_kernel::SessionId(id.to_owned()))),
        None => Ok(SessionSelection::New),
    }
}
