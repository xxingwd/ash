use crate::picker::PickerState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlashCommand {
    New,
    Clear,
    Undo,
    Fork,
    Compact,
    Resume,
    Status,
    Exit,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ParsedInput {
    Message,
    Command(SlashCommand),
    Invalid(String),
}

#[derive(Clone, Copy)]
struct CommandSpec {
    name: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    command: SlashCommand,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandCompletion {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
}

#[derive(Debug, Default)]
pub struct CommandCompletionState {
    filter: Option<String>,
    dismissed_filter: Option<String>,
    picker: PickerState<CommandCompletion>,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "new",
        aliases: &[],
        description: "start a new chat",
        command: SlashCommand::New,
    },
    CommandSpec {
        name: "clear",
        aliases: &[],
        description: "start a new chat",
        command: SlashCommand::Clear,
    },
    CommandSpec {
        name: "resume",
        aliases: &[],
        description: "choose a saved chat to resume",
        command: SlashCommand::Resume,
    },
    CommandSpec {
        name: "undo",
        aliases: &[],
        description: "undo the last submitted prompt",
        command: SlashCommand::Undo,
    },
    CommandSpec {
        name: "fork",
        aliases: &[],
        description: "choose a prompt to fork from",
        command: SlashCommand::Fork,
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        description: "compact earlier conversation history",
        command: SlashCommand::Compact,
    },
    CommandSpec {
        name: "status",
        aliases: &[],
        description: "show the current session configuration",
        command: SlashCommand::Status,
    },
    CommandSpec {
        name: "exit",
        aliases: &["quit"],
        description: "exit Ash",
        command: SlashCommand::Exit,
    },
];

impl SlashCommand {
    pub(crate) const fn can_run_while_busy(self) -> bool {
        matches!(self, Self::Exit)
    }
}

pub fn parse(input: &str) -> ParsedInput {
    let input = input.trim();
    let Some(command_line) = input.strip_prefix('/') else {
        return ParsedInput::Message;
    };
    let mut parts = command_line.split_whitespace();
    let Some(name) = parts.next() else {
        return ParsedInput::Invalid("enter a command after '/'".to_string());
    };
    if parts.next().is_some() {
        return ParsedInput::Invalid(format!("/{name} does not accept arguments"));
    }

    COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
        .map_or_else(
            || ParsedInput::Invalid(format!("unknown command '/{name}'")),
            |spec| ParsedInput::Command(spec.command),
        )
}

pub fn is_bare_exit(input: &str) -> bool {
    let input = input.trim();
    COMMANDS.iter().any(|spec| {
        spec.command == SlashCommand::Exit && (spec.name == input || spec.aliases.contains(&input))
    })
}

pub fn completion_filter(input: &str, cursor: usize) -> Option<String> {
    if !input.starts_with('/') || cursor > input.len() || !input.is_char_boundary(cursor) {
        return None;
    }
    let token_end = input[1..]
        .find(char::is_whitespace)
        .map_or(input.len(), |index| index + 1);
    if cursor > token_end {
        return None;
    }
    Some(input[1..cursor.max(1)].to_ascii_lowercase())
}

fn completions(filter: &str) -> Vec<CommandCompletion> {
    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    for spec in COMMANDS {
        let names = std::iter::once(spec.name).chain(spec.aliases.iter().copied());
        let is_exact = names.clone().any(|name| name == filter);
        let is_prefix = names.into_iter().any(|name| name.starts_with(filter));
        let completion = CommandCompletion {
            name: spec.name,
            description: spec.description,
        };
        if is_exact {
            exact.push(completion);
        } else if is_prefix {
            prefix.push(completion);
        }
    }
    exact.extend(prefix);
    exact
}

impl CommandCompletionState {
    pub(crate) fn sync(&mut self, input: &str, cursor: usize) {
        let filter = completion_filter(input, cursor);
        if filter != self.filter {
            self.filter.clone_from(&filter);
            self.dismissed_filter = None;
            self.picker.set_selected(0);
        }
        if filter.is_some() && filter == self.dismissed_filter {
            self.picker.clear();
            return;
        }
        let items = filter.as_deref().map(completions).unwrap_or_default();
        self.picker.replace_items(items);
    }

    pub(crate) fn items(&self) -> &[CommandCompletion] {
        self.picker.items()
    }

    pub(crate) const fn selected_index(&self) -> usize {
        self.picker.selected_index()
    }

    pub(crate) fn selected(&self) -> Option<CommandCompletion> {
        let index = self.picker.selected_index();
        self.picker.items().get(index).copied()
    }

    pub(crate) const fn move_up(&mut self) {
        self.picker.move_up();
    }

    pub(crate) const fn move_down(&mut self) {
        self.picker.move_down();
    }

    pub(crate) fn dismiss(&mut self) {
        self.dismissed_filter.clone_from(&self.filter);
        self.picker.clear();
    }

    pub(crate) const fn is_visible(&self) -> bool {
        self.picker.is_visible()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_aliases() {
        assert_eq!(parse("/new"), ParsedInput::Command(SlashCommand::New));
        assert_eq!(parse("/undo"), ParsedInput::Command(SlashCommand::Undo));
        assert_eq!(parse("/fork"), ParsedInput::Command(SlashCommand::Fork));
        assert_eq!(
            parse("/compact"),
            ParsedInput::Command(SlashCommand::Compact)
        );
        assert_eq!(parse(" /quit "), ParsedInput::Command(SlashCommand::Exit));
        assert!(is_bare_exit(" exit "));
        assert!(is_bare_exit("quit"));
        assert!(!is_bare_exit("close"));
    }

    #[test]
    fn keeps_regular_messages_untouched() {
        assert_eq!(parse("hello"), ParsedInput::Message);
    }

    #[test]
    fn rejects_unknown_commands_and_arguments() {
        assert!(matches!(parse("/missing"), ParsedInput::Invalid(_)));
        assert!(matches!(parse("/new now"), ParsedInput::Invalid(_)));
    }

    #[test]
    fn only_exit_can_run_during_a_turn() {
        for command in [
            SlashCommand::New,
            SlashCommand::Clear,
            SlashCommand::Undo,
            SlashCommand::Fork,
            SlashCommand::Compact,
            SlashCommand::Resume,
            SlashCommand::Status,
        ] {
            assert!(!command.can_run_while_busy(), "{command:?}");
        }
        assert!(SlashCommand::Exit.can_run_while_busy());
    }

    #[test]
    fn completes_commands_by_name_and_alias_prefix() {
        assert_eq!(
            completions("cl"),
            vec![CommandCompletion {
                name: "clear",
                description: "start a new chat",
            }]
        );
        assert_eq!(completions("q")[0].name, "exit");
        assert_eq!(
            completions("fork"),
            vec![CommandCompletion {
                name: "fork",
                description: "choose a prompt to fork from",
            }]
        );
        assert_eq!(
            completions("undo"),
            vec![CommandCompletion {
                name: "undo",
                description: "undo the last submitted prompt",
            }]
        );
    }

    #[test]
    fn completion_state_navigates_and_can_be_dismissed() {
        let mut state = CommandCompletionState::default();
        state.sync("/", 1);
        assert_eq!(state.selected().unwrap().name, "new");
        state.move_down();
        assert_eq!(state.selected().unwrap().name, "clear");
        state.move_up();
        assert_eq!(state.selected().unwrap().name, "new");
        state.dismiss();
        state.sync("/", 1);
        assert!(!state.is_visible());
        state.sync("/st", 3);
        assert_eq!(state.selected().unwrap().name, "status");
    }

    #[test]
    fn completion_always_shows_every_command() {
        let items = completions("");
        assert_eq!(items.len(), COMMANDS.len());
    }
}
