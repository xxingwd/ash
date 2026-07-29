mod modes;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "ash", about = "AI coding agent", version)]
struct Cli {
    /// Model to use
    #[arg(short, long)]
    model: Option<String>,

    /// Run in non-interactive print mode
    #[arg(long)]
    print: bool,

    /// Skill to activate
    #[arg(short, long)]
    skill: Option<String>,

    /// Prompt (for print mode)
    prompt: Option<String>,

    /// Provider protocol (anthropic, openai, openai-responses)
    #[arg(long)]
    protocol: Option<String>,

    /// Base URL override
    #[arg(long)]
    base_url: Option<String>,

    /// Model context window in tokens
    #[arg(long)]
    max_context_tokens: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    modes::run(cli).await
}
