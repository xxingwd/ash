mod message_history;
mod modes;

use ash_protocol::Protocol;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

fn parse_protocol_arg(value: &str) -> Result<Protocol, String> {
    value
        .parse()
        .map_err(|_| format!("unknown protocol: {value}"))
}

#[derive(Parser)]
#[command(name = "ash", about = "AI coding agent", version)]
struct Cli {
    /// Model to use
    #[arg(short, long)]
    model: Option<String>,

    /// Skill to activate
    #[arg(short, long)]
    skill: Option<String>,

    /// Provider protocol (anthropic, openai, openai-responses)
    #[arg(long, value_parser = parse_protocol_arg)]
    protocol: Option<Protocol>,

    /// Base URL override
    #[arg(long)]
    base_url: Option<String>,

    /// Model context window in tokens
    #[arg(long)]
    max_context_tokens: Option<usize>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one non-interactive request and print the reply
    Run {
        /// Prompt; reads one line from stdin when omitted
        prompt: Option<String>,

        /// Print debug logs to stderr while running
        #[arg(long)]
        log: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(run_with_logs(&cli));
    modes::run(cli).await
}

const fn run_with_logs(cli: &Cli) -> bool {
    matches!(&cli.command, Some(Command::Run { log: true, .. }))
}

/// `RUST_LOG` always wins; without it, `ash run --log` raises the default level
/// to debug so request/retry steps are visible, and everything else stays
/// quiet unless something is actually wrong.
fn init_logging(enabled: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if enabled {
            EnvFilter::new("debug")
        } else {
            EnvFilter::new("error")
        }
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_subcommand_parses_with_log_flag() {
        let cli = Cli::try_parse_from(["ash", "run", "inspect this", "--log"]).unwrap();
        assert!(matches!(
            &cli.command,
            Some(Command::Run {
                prompt: Some(prompt),
                log: true
            }) if prompt == "inspect this"
        ));
        assert!(run_with_logs(&cli));
    }

    #[test]
    fn run_subcommand_reads_stdin_when_prompt_is_omitted() {
        let cli = Cli::try_parse_from(["ash", "run"]).unwrap();
        assert!(matches!(
            &cli.command,
            Some(Command::Run {
                prompt: None,
                log: false
            })
        ));
    }
}
