use anyhow::{Context, Result};
use ash_agent::{
    build_system_prompt, Agent, AgentConfig, AgentSession, MessageHistoryStore, Skill,
};
use ash_core::{Message, ModelId, Protocol, ProviderConfig, SessionId};
use futures::StreamExt;
use owo_colors::OwoColorize;
use secrecy::SecretString;

use crate::Cli;

enum AfterTurn {
    None,
    Undo,
    Exit,
}

pub async fn run(cli: Cli) -> Result<()> {
    let config = build_config(&cli)?;

    if cli.print {
        run_print(config, cli.prompt).await
    } else {
        run_interactive(config).await
    }
}

fn build_config(cli: &Cli) -> Result<AgentConfig> {
    let protocol_name = cli
        .protocol
        .clone()
        .or_else(|| env_value("ASH_PROTOCOL"))
        .unwrap_or_else(|| "anthropic".into());
    let protocol = match protocol_name.as_str() {
        "anthropic" => Protocol::AnthropicMessages,
        "openai" => Protocol::OpenaiCompletions,
        "openai-responses" => Protocol::OpenaiResponses,
        other => anyhow::bail!("unknown protocol: {other}"),
    };

    let api_key =
        env_value("ASH_API_KEY").context("set ASH_API_KEY in .env or the process environment")?;

    let model = cli.model.clone().or_else(|| env_value("ASH_MODEL"));
    let model = match model {
        Some(model) => model,
        None if matches!(&protocol, Protocol::AnthropicMessages) => {
            "claude-sonnet-4-20250514".into()
        }
        None => anyhow::bail!("set ASH_MODEL in .env or pass --model"),
    };
    let base_url = cli.base_url.clone().or_else(|| env_value("ASH_BASE_URL"));
    let working_dir = std::env::current_dir()?;
    let skills = Skill::load_from_dir(&working_dir.join("skills"))?;
    let active_skill = cli
        .skill
        .as_ref()
        .map(|skill_name| {
            skills
                .iter()
                .find(|skill| skill.name == *skill_name)
                .cloned()
                .with_context(|| format!("skill not found: {skill_name}"))
        })
        .transpose()?;
    let system_prompt = build_system_prompt(&working_dir, &skills, active_skill.as_ref())?;
    let tools = ash_tools::tools(
        active_skill
            .as_ref()
            .and_then(|skill| skill.tools.as_deref()),
    );

    let mut config = AgentConfig {
        provider: ProviderConfig {
            protocol,
            api_key: SecretString::from(api_key),
            base_url,
        },
        system_prompt: Some(system_prompt),
        model: ModelId::new(model),
        tools,
        max_turns: 100,
        working_dir,
        max_context_tokens: None,
        max_output_tokens: None,
        max_tool_duration: std::time::Duration::from_secs(120),
        agent_path: "/root".to_string(),
        root_session_id: None,
    };

    if let Some(skill) = active_skill {
        skill.apply_overrides(&mut config);
    }

    Ok(config)
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

async fn run_print(config: AgentConfig, prompt: Option<String>) -> Result<()> {
    let input = match prompt {
        Some(p) => p,
        None => {
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            buf
        }
    };

    let messages = vec![Message::user(&input)];
    let mut stream = Agent::run(config, messages);

    while let Some(event) = stream.next().await {
        match event {
            ash_core::Event::TextDelta(t) => print!("{t}"),
            ash_core::Event::ToolCallStart { name, .. } => {
                eprintln!("{}", format!("[tool: {name}]").cyan());
            }
            ash_core::Event::ToolCallEnd {
                is_error: true,
                output,
                ..
            } => eprintln!("{}", format!("[error: {output}]").red()),
            ash_core::Event::Error(e) => {
                eprintln!("{}", format!("[error: {e}]").red());
            }
            _ => {}
        }
    }

    println!();
    Ok(())
}

async fn run_interactive(config: AgentConfig) -> Result<()> {
    let protocol = config.provider.protocol.as_cli_name().to_string();
    let model = config.model.as_str().to_string();
    let working_dir = config.working_dir.clone();
    let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<ash_tui::UiCommand>(16);

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    let event_rx = tokio_stream::wrappers::ReceiverStream::new(event_rx);

    let history_store = MessageHistoryStore::default();
    let input_history = match history_store.load().await {
        Ok(history) => history,
        Err(error) => {
            tracing::warn!(%error, "failed to load input history");
            Vec::new()
        }
    };
    let mut session = AgentSession::new(config);
    let app = ash_tui::App::new(protocol, model, working_dir).with_input_history(input_history);
    let app_handle = tokio::spawn(async move { app.run(event_rx, command_tx).await });
    let mut pending_input = None;

    'controller: loop {
        let command = match pending_input.take() {
            Some(input) => ash_tui::UiCommand::Submit(input),
            None => match command_rx.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        match command {
            ash_tui::UiCommand::Submit(input) => {
                if let Err(error) = history_store.append(session.id(), &input).await {
                    tracing::warn!(%error, "failed to persist input history");
                }
                let cancel = ash_core::CancellationToken::new();
                let mut turn = Box::pin(session.submit(input, event_tx.clone(), cancel.clone()));
                let after_turn = loop {
                    tokio::select! {
                        _ = &mut turn => break AfterTurn::None,
                        command = command_rx.recv() => match command {
                            Some(ash_tui::UiCommand::CancelAndUndo) => {
                                cancel.cancel();
                                let _ = (&mut turn).await;
                                break AfterTurn::Undo;
                            }
                            Some(ash_tui::UiCommand::NewSession)
                            | Some(ash_tui::UiCommand::ListSessions)
                            | Some(ash_tui::UiCommand::ResumeSession(_)) => {
                                let _ = event_tx
                                    .send(ash_core::Event::Error(
                                        "That command is unavailable while working.".to_string(),
                                    ))
                                    .await;
                            }
                            Some(ash_tui::UiCommand::Exit) | None => {
                                cancel.cancel();
                                let _ = (&mut turn).await;
                                break AfterTurn::Exit;
                            }
                            Some(ash_tui::UiCommand::Submit(input)) => {
                                pending_input = Some(input);
                            }
                        }
                    }
                };
                drop(turn);
                match after_turn {
                    AfterTurn::None => {}
                    AfterTurn::Undo => {
                        pending_input = None;
                        rollback_last_turn(&mut session, &event_tx, &history_store).await;
                    }
                    AfterTurn::Exit => break 'controller,
                }
            }
            ash_tui::UiCommand::CancelAndUndo => {
                pending_input = None;
                rollback_last_turn(&mut session, &event_tx, &history_store).await;
            }
            ash_tui::UiCommand::NewSession => {
                session.reset();
                pending_input = None;
            }
            ash_tui::UiCommand::ListSessions => {
                list_sessions(&session, &event_tx).await;
            }
            ash_tui::UiCommand::ResumeSession(session_id) => {
                pending_input = None;
                resume_session(&mut session, session_id, &event_tx).await;
            }
            ash_tui::UiCommand::Exit => break,
        }
    }

    drop(event_tx);
    app_handle.await??;
    Ok(())
}

async fn rollback_last_turn(
    session: &mut AgentSession,
    event_tx: &tokio::sync::mpsc::Sender<ash_core::Event>,
    history_store: &MessageHistoryStore,
) {
    let event = match session.rollback_last_turn().await {
        Some(prompt) => {
            if let Err(error) = history_store.undo(session.id(), &prompt).await {
                tracing::warn!(%error, "failed to undo input history entry");
            }
            ash_core::Event::TurnRolledBack { prompt }
        }
        None => ash_core::Event::Error("No submitted turn is available to undo.".to_string()),
    };
    let _ = event_tx.send(event).await;
}

async fn list_sessions(
    session: &AgentSession,
    event_tx: &tokio::sync::mpsc::Sender<ash_core::Event>,
) {
    let event = match session.resumable_sessions().await {
        Ok(sessions) => ash_core::Event::SessionsListed { sessions },
        Err(error) => ash_core::Event::Error(format!("Failed to list saved chats: {error}")),
    };
    let _ = event_tx.send(event).await;
}

async fn resume_session(
    session: &mut AgentSession,
    session_id: SessionId,
    event_tx: &tokio::sync::mpsc::Sender<ash_core::Event>,
) {
    let event = match session.resume(session_id).await {
        Ok(Some(restored)) => ash_core::Event::SessionRestored {
            model: restored.model,
            protocol: restored.protocol,
            working_dir: restored.working_dir,
            messages: restored.messages,
        },
        Ok(None) => ash_core::Event::Error("That saved chat is no longer available.".to_string()),
        Err(error) => ash_core::Event::Error(format!("Failed to resume saved chat: {error}")),
    };
    let _ = event_tx.send(event).await;
}
