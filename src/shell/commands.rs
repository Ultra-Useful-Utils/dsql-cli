use crate::{
    app::{ReconnectState, SessionMetadata},
    output::escape_terminal_text,
};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ShellSettings {
    pub(crate) expanded: ExpandedMode,
    pub(crate) timing: bool,
    pub(crate) pager: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ExpandedMode {
    #[default]
    Off,
    On,
    Auto,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum RefreshState {
    #[default]
    NotRequested,
    Requested,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ShellCommandState {
    pub(crate) settings: ShellSettings,
    pub(crate) refresh: RefreshState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandAction {
    Continue,
    Dashboard,
    Exit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandResult {
    pub(crate) action: CommandAction,
    pub(crate) message: String,
    pub(crate) metadata: Option<MetadataRequest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MetadataRequest {
    Relations(Option<String>),
    Tables(Option<String>),
    Schemas(Option<String>),
    Roles,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MetaCommand {
    Quit,
    Help,
    ConnectionInfo,
    Expanded(ExpandedArgument),
    Timing(ToggleArgument),
    Pager(ToggleArgument),
    Refresh,
    Metrics,
    Metadata(MetadataRequest),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToggleArgument {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpandedArgument {
    On,
    Off,
    Auto,
    Toggle,
}

#[cfg(test)]
pub(crate) fn execute(
    input: &str,
    sql_buffer_active: bool,
    metadata: &SessionMetadata,
    state: &mut ShellCommandState,
    now: SystemTime,
) -> Result<Option<CommandResult>, String> {
    execute_with_reconnect_state(
        input,
        sql_buffer_active,
        metadata,
        ReconnectState::Connected,
        state,
        now,
    )
}

pub(crate) fn execute_with_reconnect_state(
    input: &str,
    sql_buffer_active: bool,
    metadata: &SessionMetadata,
    reconnect_state: ReconnectState,
    state: &mut ShellCommandState,
    now: SystemTime,
) -> Result<Option<CommandResult>, String> {
    let Some(command) = parse(input, sql_buffer_active)? else {
        return Ok(None);
    };

    let result = match command {
        MetaCommand::Quit => CommandResult {
            action: CommandAction::Exit,
            message: String::new(),
            metadata: None,
        },
        MetaCommand::Help => CommandResult {
            action: CommandAction::Continue,
            message: help_text().into(),
            metadata: None,
        },
        MetaCommand::ConnectionInfo => CommandResult {
            action: CommandAction::Continue,
            message: connection_info(metadata, reconnect_state, now),
            metadata: None,
        },
        MetaCommand::Expanded(argument) => {
            state.settings.expanded = match argument {
                ExpandedArgument::On => ExpandedMode::On,
                ExpandedArgument::Off => ExpandedMode::Off,
                ExpandedArgument::Auto => ExpandedMode::Auto,
                ExpandedArgument::Toggle => match state.settings.expanded {
                    ExpandedMode::Off => ExpandedMode::On,
                    ExpandedMode::On | ExpandedMode::Auto => ExpandedMode::Off,
                },
            };
            expanded_result(state.settings.expanded)
        }
        MetaCommand::Timing(argument) => {
            update_toggle(&mut state.settings.timing, argument);
            toggle_result("timing", state.settings.timing)
        }
        MetaCommand::Pager(argument) => {
            update_toggle(&mut state.settings.pager, argument);
            toggle_result("pager", state.settings.pager)
        }
        MetaCommand::Refresh => {
            state.refresh = RefreshState::Requested;
            CommandResult {
                action: CommandAction::Continue,
                message: "refreshing completion metadata".into(),
                metadata: None,
            }
        }
        MetaCommand::Metrics => CommandResult {
            action: CommandAction::Dashboard,
            message: String::new(),
            metadata: None,
        },
        MetaCommand::Metadata(request) => CommandResult {
            action: CommandAction::Continue,
            message: String::new(),
            metadata: Some(request),
        },
    };
    Ok(Some(result))
}

fn parse(input: &str, sql_buffer_active: bool) -> Result<Option<MetaCommand>, String> {
    if sql_buffer_active || !input.starts_with('\\') {
        return Ok(None);
    }
    if input.contains(['\n', '\r']) {
        return Err("meta-commands must be submitted on one line".into());
    }

    let mut parts = input[1..].split_ascii_whitespace();
    let name = parts.next().ok_or("missing meta-command name")?;
    let arguments: Vec<_> = parts.collect();
    let argument_text = input[1 + name.len()..].trim();
    match name {
        "q" => no_arguments("\\q", &arguments).map(|_| MetaCommand::Quit),
        "?" => no_arguments("\\?", &arguments).map(|_| MetaCommand::Help),
        "conninfo" => no_arguments("\\conninfo", &arguments).map(|_| MetaCommand::ConnectionInfo),
        "x" => parse_expanded(&arguments).map(MetaCommand::Expanded),
        "timing" => parse_toggle("\\timing", &arguments).map(MetaCommand::Timing),
        "pager" => parse_toggle("\\pager", &arguments).map(MetaCommand::Pager),
        "refresh" => no_arguments("\\refresh", &arguments).map(|_| MetaCommand::Refresh),
        "metrics" => no_arguments("\\metrics", &arguments).map(|_| MetaCommand::Metrics),
        "d" => parse_pattern("\\d", argument_text)
            .map(|pattern| MetaCommand::Metadata(MetadataRequest::Relations(pattern))),
        "dt" => parse_pattern("\\dt", argument_text)
            .map(|pattern| MetaCommand::Metadata(MetadataRequest::Tables(pattern))),
        "dn" => parse_pattern("\\dn", argument_text)
            .map(|pattern| MetaCommand::Metadata(MetadataRequest::Schemas(pattern))),
        "du" => {
            no_arguments("\\du", &arguments).map(|_| MetaCommand::Metadata(MetadataRequest::Roles))
        }
        _ => Err(format!(
            "unsupported meta-command: \\{name}; use \\? for help"
        )),
    }
    .map(Some)
}

fn no_arguments(command: &str, arguments: &[&str]) -> Result<(), String> {
    if arguments.is_empty() {
        return Ok(());
    }
    Err(format!("{command} does not accept arguments"))
}

fn parse_toggle(command: &str, arguments: &[&str]) -> Result<ToggleArgument, String> {
    match arguments {
        [] => Ok(ToggleArgument::Toggle),
        ["on"] => Ok(ToggleArgument::On),
        ["off"] => Ok(ToggleArgument::Off),
        _ => Err(format!("usage: {command} [on|off]")),
    }
}

fn parse_expanded(arguments: &[&str]) -> Result<ExpandedArgument, String> {
    match arguments {
        [] => Ok(ExpandedArgument::Toggle),
        ["on"] => Ok(ExpandedArgument::On),
        ["off"] => Ok(ExpandedArgument::Off),
        ["auto"] => Ok(ExpandedArgument::Auto),
        _ => Err("usage: \\x [on|off|auto]".into()),
    }
}

fn parse_pattern(command: &str, argument: &str) -> Result<Option<String>, String> {
    if argument.is_empty() {
        return Ok(None);
    }
    if !argument.starts_with('"') {
        if argument.contains(char::is_whitespace) || argument.contains('"') {
            return Err(format!("usage: {command} [pattern]"));
        }
        return Ok(Some(argument.into()));
    }

    if !argument.ends_with('"') || argument.len() == 1 {
        return Err(format!("usage: {command} [pattern]"));
    }
    let characters: Vec<_> = argument[1..argument.len() - 1].chars().collect();
    let mut value = String::new();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] != '"' {
            value.push(characters[index]);
            index += 1;
        } else if characters.get(index + 1) == Some(&'"') {
            value.push('"');
            index += 2;
        } else {
            return Err(format!("usage: {command} [pattern]"));
        }
    }
    Ok(Some(value))
}

fn update_toggle(value: &mut bool, argument: ToggleArgument) {
    *value = match argument {
        ToggleArgument::On => true,
        ToggleArgument::Off => false,
        ToggleArgument::Toggle => !*value,
    };
}

fn toggle_result(name: &str, enabled: bool) -> CommandResult {
    let state = if enabled { "on" } else { "off" };
    CommandResult {
        action: CommandAction::Continue,
        message: format!("{name} is {state}"),
        metadata: None,
    }
}

fn expanded_result(mode: ExpandedMode) -> CommandResult {
    let state = match mode {
        ExpandedMode::Off => "off",
        ExpandedMode::On => "on",
        ExpandedMode::Auto => "auto",
    };
    CommandResult {
        action: CommandAction::Continue,
        message: format!("expanded display is {state}"),
        metadata: None,
    }
}

fn help_text() -> &'static str {
    "DSQL shell commands:\n\\q                         exit the interactive shell\n\\?                         show this help\n\\conninfo                  show safe connection metadata\n\\d [pattern]               list visible relations\n\\dt [pattern]              list visible tables\n\\dn [pattern]              list schemas\n\\du                        list database roles\n\\x [on|off|auto]           set expanded display\n\\timing [on|off]           set execution timing display\n\\pager [on|off]            set pager use\n\\refresh                   reload bounded completion metadata while idle\n\\metrics                   open the cluster metrics dashboard\nCtrl+C                     stop query output and cancel; press again if cancellation stalls"
}

fn connection_info(
    metadata: &SessionMetadata,
    reconnect_state: ReconnectState,
    now: SystemTime,
) -> String {
    let target = metadata.intent().target();
    let age = now
        .duration_since(metadata.connected_at())
        .map(|duration| format!("{}s", duration.as_secs()))
        .unwrap_or_else(|_| "unknown".into());
    format!(
        "Cluster: {}\nRegion: {}\nDatabase role: {}\nEndpoint: redacted\nConnection age: {age}\nReconnect state: {}",
        escape_terminal_text(target.id().as_str()),
        escape_terminal_text(target.region()),
        escape_terminal_text(metadata.intent().role().name()),
        reconnect_state.label(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CommandAction, ExpandedMode, RefreshState, ShellCommandState, execute,
        execute_with_reconnect_state,
    };
    use crate::app::{
        CancellationCapability, ClusterTarget, ConnectionIntent, DatabaseRole, ReconnectState,
        SessionMetadata, TransactionState,
    };
    use std::time::{Duration, UNIX_EPOCH};

    fn metadata() -> SessionMetadata {
        SessionMetadata::new(
            ConnectionIntent::new(
                ClusterTarget::new(
                    "cluster-1",
                    "us-east-1",
                    Some("cluster-1.dsql.us-east-1.on.aws".into()),
                ),
                DatabaseRole::Custom("app_user".into()),
                Vec::new(),
                "dsql test",
            ),
            UNIX_EPOCH,
            CancellationCapability::Available,
            TransactionState::Idle,
            Vec::new(),
        )
    }

    #[test]
    fn recognizes_only_leading_single_line_meta_commands_without_sql_buffer() {
        let metadata = metadata();
        let mut state = ShellCommandState::default();

        assert!(
            execute("SELECT '\\q';", false, &metadata, &mut state, UNIX_EPOCH)
                .expect("SQL is not a meta-command")
                .is_none()
        );
        assert!(
            execute(" \\q", false, &metadata, &mut state, UNIX_EPOCH)
                .expect("leading whitespace is SQL input")
                .is_none()
        );
        assert!(
            execute("\\q", true, &metadata, &mut state, UNIX_EPOCH)
                .expect("active SQL buffer prevents meta-command handling")
                .is_none()
        );
        assert_eq!(
            execute("\\q\nSELECT 1;", false, &metadata, &mut state, UNIX_EPOCH)
                .expect_err("multi-line meta-command is invalid"),
            "meta-commands must be submitted on one line"
        );
    }

    #[test]
    fn toggles_support_documented_modes_and_omitted_argument_toggle() {
        let metadata = metadata();
        let mut state = ShellCommandState::default();

        for input in ["\\x on", "\\timing on", "\\pager on"] {
            execute(input, false, &metadata, &mut state, UNIX_EPOCH).expect("valid toggle");
        }
        assert_eq!(
            state.settings,
            super::ShellSettings {
                expanded: ExpandedMode::On,
                timing: true,
                pager: true,
            }
        );

        execute("\\x auto", false, &metadata, &mut state, UNIX_EPOCH).expect("auto");
        assert_eq!(state.settings.expanded, ExpandedMode::Auto);
        execute("\\x", false, &metadata, &mut state, UNIX_EPOCH).expect("toggle");
        execute("\\timing off", false, &metadata, &mut state, UNIX_EPOCH).expect("off");
        execute("\\pager", false, &metadata, &mut state, UNIX_EPOCH).expect("default toggle");
        assert_eq!(state.settings.expanded, ExpandedMode::Off);
        assert!(!state.settings.timing);
        assert!(!state.settings.pager);
    }

    #[test]
    fn malformed_and_unknown_commands_fail_locally() {
        let metadata = metadata();
        let mut state = ShellCommandState::default();

        assert_eq!(
            execute(
                "\\timing sometimes",
                false,
                &metadata,
                &mut state,
                UNIX_EPOCH
            )
            .expect_err("invalid argument"),
            "usage: \\timing [on|off]"
        );
        assert!(execute("\\x toggle", false, &metadata, &mut state, UNIX_EPOCH).is_err());
        assert_eq!(
            execute("\\unknown", false, &metadata, &mut state, UNIX_EPOCH)
                .expect_err("unknown command"),
            "unsupported meta-command: \\unknown; use \\? for help"
        );
    }

    #[test]
    fn connection_info_uses_safe_metadata_and_refresh_requests_a_reload() {
        let metadata = metadata();
        let mut state = ShellCommandState::default();
        let info = execute(
            "\\conninfo",
            false,
            &metadata,
            &mut state,
            UNIX_EPOCH + Duration::from_secs(65),
        )
        .expect("connection info")
        .expect("meta-command result");

        assert!(info.message.contains("Cluster: cluster-1"));
        assert!(info.message.contains("Region: us-east-1"));
        assert!(info.message.contains("Database role: app_user"));
        assert!(info.message.contains("Connection age: 65s"));
        assert!(info.message.contains("Reconnect state: connected"));
        assert!(info.message.contains("Endpoint: redacted"));
        assert!(!info.message.contains("dsql.us-east-1.on.aws"));
        assert!(!info.message.contains("account"));

        let refresh = execute("\\refresh", false, &metadata, &mut state, UNIX_EPOCH)
            .expect("refresh")
            .expect("meta-command result");
        assert_eq!(refresh.action, CommandAction::Continue);
        assert_eq!(state.refresh, RefreshState::Requested);
        assert!(refresh.message.contains("refreshing completion metadata"));

        let due = execute_with_reconnect_state(
            "\\conninfo",
            false,
            &metadata,
            ReconnectState::Due,
            &mut state,
            UNIX_EPOCH + Duration::from_secs(65),
        )
        .expect("connection info")
        .expect("meta-command result");
        assert!(
            due.message
                .contains("Reconnect state: due before next statement")
        );
    }

    #[test]
    fn connection_info_escapes_terminal_controls() {
        let metadata = SessionMetadata::new(
            ConnectionIntent::new(
                ClusterTarget::new("cluster-1", "us-east-1", None),
                DatabaseRole::Custom("role\u{1b}]0;owned\u{7}".into()),
                Vec::new(),
                "dsql test",
            ),
            UNIX_EPOCH,
            CancellationCapability::Available,
            TransactionState::Idle,
            Vec::new(),
        );
        let mut state = ShellCommandState::default();
        let info = execute("\\conninfo", false, &metadata, &mut state, UNIX_EPOCH)
            .expect("connection info")
            .expect("meta-command result");

        assert!(info.message.contains(r"role\u{001b}]0;owned\u{0007}"));
        assert!(!info.message.contains('\u{1b}'));
    }

    #[test]
    fn help_lists_only_supported_commands_and_quit_exits() {
        let metadata = metadata();
        let mut state = ShellCommandState::default();
        let help = execute("\\?", false, &metadata, &mut state, UNIX_EPOCH)
            .expect("help")
            .expect("meta-command result");
        assert!(help.message.contains("\\refresh"));
        assert!(help.message.contains("\\dt [pattern]"));
        assert!(help.message.contains("\\metrics"));
        assert!(help.message.contains("Ctrl+C"));
        assert!(help.message.contains("stop query output and cancel"));

        let metrics = execute("\\metrics", false, &metadata, &mut state, UNIX_EPOCH)
            .expect("metrics command")
            .expect("meta-command result");
        assert_eq!(metrics.action, CommandAction::Dashboard);

        let quit = execute("\\q", false, &metadata, &mut state, UNIX_EPOCH)
            .expect("quit")
            .expect("meta-command result");
        assert_eq!(quit.action, CommandAction::Exit);
    }

    #[test]
    fn metadata_commands_keep_quoted_patterns_as_data() {
        let metadata = metadata();
        let mut state = ShellCommandState::default();
        let result = execute(
            r#"\dt "Mixed Case""Name""#,
            false,
            &metadata,
            &mut state,
            UNIX_EPOCH,
        )
        .expect("metadata command")
        .expect("command result");

        assert_eq!(
            result.metadata,
            Some(super::MetadataRequest::Tables(Some(
                "Mixed Case\"Name".into()
            )))
        );
    }

    #[test]
    fn malformed_metadata_arguments_fail_locally() {
        let metadata = metadata();
        let mut state = ShellCommandState::default();
        for input in [r#"\d "unterminated"#, r"\dn one two", r"\du role"] {
            assert!(execute(input, false, &metadata, &mut state, UNIX_EPOCH).is_err());
        }
    }
}
