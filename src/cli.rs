use crate::{
    app::{
        ClusterStatus, ClusterTarget, ConnectionIntent, DatabaseRole, DiscoverableCluster,
        EnrichmentErrorCategory, EnrichmentState, ExecutionSink, ManagedSession, SessionConnector,
    },
    aws::{
        clusters::discover_aws_clusters,
        config::{AwsConfigRequest, RegionPrompt, load_aws_configuration},
        identity::resolve_aws_caller_identity,
        metrics::cloudwatch_metrics_provider,
    },
    db::session::DsqlSessionConnector,
    error::ApplicationError,
    output::{
        delimited::DelimitedSink, escape_terminal_text, jsonl::JsonlExecutionSink,
        table::TableExecutionSink,
    },
    shell,
    sql::scanner::{MAX_STATEMENT_BYTES, StatementStream},
    target::{ClusterSelector, parse_cluster_selector},
};
use clap::{ArgAction, ArgMatches, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::{
    fs,
    io::{self, BufRead, BufReader, IsTerminal, Write},
};

/// Aurora DSQL command-line client.
#[derive(Debug, Parser)]
#[command(
    name = "dsql",
    version,
    about = "Command-line client for Amazon Aurora DSQL",
    long_about = None,
    color = clap::ColorChoice::Never,
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// Cluster ID, ARN, or canonical Aurora DSQL endpoint.
    pub(crate) cluster: Option<String>,

    /// AWS profile used by the SDK credential chain.
    #[arg(long, global = true)]
    pub(crate) profile: Option<String>,

    /// AWS Region for discovery or the selected cluster.
    #[arg(long, global = true)]
    pub(crate) region: Option<String>,

    /// PostgreSQL database role. The admin role is elevated.
    #[arg(short = 'U', long = "username", global = true)]
    pub(crate) username: Option<String>,

    /// Inventory output format.
    #[arg(long, value_enum, default_value_t = InventoryFormat::Table, global = true)]
    pub(crate) format: InventoryFormat,

    /// Execute SQL and exit. May be specified more than once.
    #[arg(short = 'c', long = "command", global = true, action = ArgAction::Append)]
    sql_command: Vec<String>,

    /// Execute a UTF-8 SQL file and exit. May be specified more than once.
    #[arg(short = 'f', long = "file", global = true, action = ArgAction::Append)]
    sql_file: Vec<String>,

    /// Add a PEM trust anchor while retaining the normal trust roots.
    #[arg(long = "ssl-root-cert", global = true, action = ArgAction::Append)]
    ssl_root_cert: Vec<String>,

    /// Disable interactive shell history for this session.
    #[arg(long, global = true)]
    no_history: bool,

    /// Override the interactive shell history path.
    #[arg(long, global = true)]
    history_file: Option<std::path::PathBuf>,

    /// Show safe AWS configuration diagnostics.
    #[arg(long, global = true)]
    verbose: bool,

    #[arg(skip)]
    script_inputs: Vec<ScriptInput>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List discoverable Aurora DSQL clusters without connecting.
    Clusters,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ScriptInput {
    Command(String),
    File(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum InventoryFormat {
    Table,
    Csv,
    Tsv,
    Jsonl,
}

/// Interactive input/output boundary.  It deliberately writes prompts and
/// warnings to stderr, leaving stdout suitable for inventory data.
trait Prompt: RegionPrompt {
    fn select_cluster(
        &mut self,
        clusters: &[DiscoverableCluster],
    ) -> Result<Option<String>, ApplicationError>;
    fn confirm_unknown_cluster(
        &mut self,
        cluster: &DiscoverableCluster,
    ) -> Result<bool, ApplicationError>;
    fn manual_selector(&mut self) -> Result<Option<String>, ApplicationError>;
    fn select_role(&mut self) -> Result<Option<RoleChoice>, ApplicationError>;
    fn custom_role_name(&mut self) -> Result<Option<String>, ApplicationError>;
    fn warning(&mut self, message: &str) -> Result<(), ApplicationError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RoleChoice {
    Admin,
    Custom,
}

struct TerminalPrompt {
    input: io::Stdin,
    stderr: io::Stderr,
}

impl TerminalPrompt {
    fn new() -> Self {
        Self {
            input: io::stdin(),
            stderr: io::stderr(),
        }
    }

    fn ask(&mut self, question: &str) -> Result<Option<String>, ApplicationError> {
        let mut stderr = self.stderr.lock();
        write!(stderr, "{question}").map_err(output_error)?;
        stderr.flush().map_err(output_error)?;
        drop(stderr);

        let mut answer = String::new();
        let bytes = self
            .input
            .lock()
            .read_line(&mut answer)
            .map_err(|_| ApplicationError::runtime("could not read interactive input"))?;
        if bytes == 0 {
            return Ok(None);
        }
        let answer = answer.trim().to_owned();
        Ok((!answer.is_empty()).then_some(answer))
    }
}

impl RegionPrompt for TerminalPrompt {
    fn prompt_region(&mut self) -> Result<Option<String>, ApplicationError> {
        self.ask("Region: ")
    }
}

impl Prompt for TerminalPrompt {
    fn select_cluster(
        &mut self,
        clusters: &[DiscoverableCluster],
    ) -> Result<Option<String>, ApplicationError> {
        self.ask(&format!(
            "Select a cluster by ID ({} discoverable): ",
            clusters.len()
        ))
    }

    fn confirm_unknown_cluster(
        &mut self,
        cluster: &DiscoverableCluster,
    ) -> Result<bool, ApplicationError> {
        Ok(matches!(
            self.ask(&format!(
                "Cluster {} has unknown status; continue? [y/N]: ",
                escape_terminal_text(cluster.id().as_str())
            ))?
            .as_deref(),
            Some("y" | "Y" | "yes" | "YES")
        ))
    }

    fn manual_selector(&mut self) -> Result<Option<String>, ApplicationError> {
        self.ask("Enter a cluster ID, ARN, or canonical endpoint: ")
    }

    fn select_role(&mut self) -> Result<Option<RoleChoice>, ApplicationError> {
        match self
            .ask("Database role ([a]dmin elevated, [c]ustom): ")?
            .as_deref()
        {
            Some("a" | "A" | "admin") => Ok(Some(RoleChoice::Admin)),
            Some("c" | "C" | "custom") => Ok(Some(RoleChoice::Custom)),
            Some(_) => Err(ApplicationError::usage(
                "choose admin or custom for the database role",
            )),
            None => Ok(None),
        }
    }

    fn custom_role_name(&mut self) -> Result<Option<String>, ApplicationError> {
        self.ask("Custom database role: ")
    }

    fn warning(&mut self, message: &str) -> Result<(), ApplicationError> {
        writeln!(self.stderr.lock(), "warning: {message}").map_err(output_error)
    }
}

struct NoPrompt;

impl RegionPrompt for NoPrompt {
    fn prompt_region(&mut self) -> Result<Option<String>, ApplicationError> {
        Err(ApplicationError::usage("interactive input is required"))
    }
}

/// Run after Clap has parsed the command.  Consequently Clap's built-in help
/// and version exits occur before AWS configuration or any service call.
impl Cli {
    /// Restores the original order of `-c` and `-f` occurrences, which Clap
    /// otherwise stores in separate vectors.
    pub(crate) fn with_script_input_order(mut self, matches: &ArgMatches) -> Self {
        let commands = matches
            .indices_of("sql_command")
            .into_iter()
            .flatten()
            .zip(self.sql_command.iter())
            .map(|(index, value)| (index, ScriptInput::Command(value.clone())));
        let files = matches
            .indices_of("sql_file")
            .into_iter()
            .flatten()
            .zip(self.sql_file.iter())
            .map(|(index, value)| (index, ScriptInput::File(value.clone())));
        let mut ordered = commands.chain(files).collect::<Vec<_>>();
        ordered.sort_by_key(|(index, _)| *index);
        self.script_inputs = ordered.into_iter().map(|(_, input)| input).collect();
        self
    }

    pub(crate) async fn run(self) -> Result<(), ApplicationError> {
        match self.command {
            Some(Command::Clusters) => self.run_clusters().await,
            None => self.run_preview().await,
        }
    }

    async fn run_clusters(self) -> Result<(), ApplicationError> {
        if self.cluster.is_some() {
            return Err(ApplicationError::usage(
                "the clusters subcommand does not accept a cluster selector",
            ));
        }
        if !self.sql_command.is_empty() || !self.sql_file.is_empty() {
            return Err(ApplicationError::usage(
                "the clusters subcommand does not accept -c/--command or -f/--file",
            ));
        }
        let mut prompt = NoPrompt;
        let configuration = load_aws_configuration(
            AwsConfigRequest::new(self.profile, self.region, None, false),
            &mut prompt,
        )
        .await?;
        if self.verbose {
            emit_region_diagnostics(&mut io::stderr(), configuration.region_diagnostics())?;
        }
        let identity = resolve_aws_caller_identity(&configuration).await;
        emit_identity_context(&mut io::stderr(), &identity)?;
        let clusters = discover_aws_clusters(&configuration).await?;
        let mut stdout = io::stdout();
        let terminal = stdout.is_terminal();
        render_inventory(&clusters, self.format, &mut stdout, terminal)
    }

    async fn run_preview(self) -> Result<(), ApplicationError> {
        let direct = self.cluster.as_deref().map(parse_selector).transpose()?;
        let has_stdin_script =
            uses_stdin_script(self.script_inputs.is_empty(), io::stdin().is_terminal());
        let noninteractive = !self.script_inputs.is_empty() || has_stdin_script;
        if noninteractive {
            return self.run_script(direct, has_stdin_script).await;
        }

        let interactive = io::stdin().is_terminal();
        if !interactive {
            validate_noninteractive_preview(direct.as_ref(), self.username.as_deref())?;
        }

        let mut prompt = TerminalPrompt::new();
        let configuration = load_aws_configuration(
            AwsConfigRequest::new(
                self.profile,
                self.region,
                direct
                    .as_ref()
                    .and_then(|selector| selector.region().map(str::to_owned)),
                interactive,
            ),
            &mut prompt,
        )
        .await?;
        if self.verbose {
            emit_region_diagnostics(&mut io::stderr(), configuration.region_diagnostics())?;
        }
        let identity = resolve_aws_caller_identity(&configuration).await;
        emit_identity_context(&mut io::stderr(), &identity)?;

        let target = match direct {
            Some(selector) => target_from_selector(&selector, configuration.context().region())?,
            None => {
                let inventory = discover_aws_clusters(&configuration).await;
                if let Ok(clusters) = &inventory
                    && !clusters.is_empty()
                {
                    let mut stdout = io::stdout();
                    render_inventory(clusters, InventoryFormat::Table, &mut stdout, true)?;
                }
                select_inventory_target(&mut prompt, inventory, configuration.context().region())?
            }
        };
        let role = select_database_role(&mut prompt, self.username)?;
        let intent = ConnectionIntent::new(
            target,
            role,
            self.ssl_root_cert,
            format!("dsql {}", env!("CARGO_PKG_VERSION")),
        );
        let connector = DsqlSessionConnector::new(configuration.sdk_config().clone());
        let metrics = cloudwatch_metrics_provider(&configuration);
        let session = connector.connect(&intent).await?;
        let mut session = ManagedSession::new(session, &connector, std::time::SystemTime::now());
        shell::run(
            &mut session,
            configuration.context(),
            &metrics,
            self.no_history,
            self.history_file,
        )
        .await
    }

    async fn run_script(
        self,
        direct: Option<ClusterSelector>,
        use_stdin: bool,
    ) -> Result<(), ApplicationError> {
        validate_noninteractive_preview(direct.as_ref(), self.username.as_deref())?;
        let selector = direct.expect("validated direct selector");
        let role = database_role_from_username(self.username.expect("validated username"))?;
        let mut prompt = NoPrompt;
        let configuration = load_aws_configuration(
            AwsConfigRequest::new(
                self.profile,
                self.region,
                selector.region().map(str::to_owned),
                false,
            ),
            &mut prompt,
        )
        .await?;
        if self.verbose {
            emit_region_diagnostics(&mut io::stderr(), configuration.region_diagnostics())?;
        }
        let target = target_from_selector(&selector, configuration.context().region())?;
        let intent = ConnectionIntent::new(
            target,
            role,
            self.ssl_root_cert,
            format!("dsql {}", env!("CARGO_PKG_VERSION")),
        );
        let connector = DsqlSessionConnector::new(configuration.sdk_config().clone());
        let mut sink = execution_sink(self.format, io::stdout().is_terminal());
        execute_script_inputs(
            &connector,
            &intent,
            &self.script_inputs,
            use_stdin,
            sink.as_mut(),
        )
        .await
    }
}

fn validate_noninteractive_preview(
    selector: Option<&ClusterSelector>,
    username: Option<&str>,
) -> Result<(), ApplicationError> {
    if selector.is_none() || username.is_none() {
        return Err(ApplicationError::usage(
            "a cluster selector and -U/--username are required when input is not interactive",
        ));
    }
    Ok(())
}

fn database_role_from_username(username: String) -> Result<DatabaseRole, ApplicationError> {
    match username.as_str() {
        "admin" => Ok(DatabaseRole::Admin),
        value if !value.trim().is_empty() => Ok(DatabaseRole::Custom(username)),
        _ => Err(ApplicationError::usage("--username must not be empty")),
    }
}

fn uses_stdin_script(has_no_explicit_input: bool, stdin_is_terminal: bool) -> bool {
    has_no_explicit_input && !stdin_is_terminal
}

async fn execute_script_inputs(
    connector: &dyn SessionConnector,
    intent: &ConnectionIntent,
    inputs: &[ScriptInput],
    use_stdin: bool,
    sink: &mut dyn ExecutionSink,
) -> Result<(), ApplicationError> {
    let session = connector.connect(intent).await?;
    let mut session = ManagedSession::new(session, connector, std::time::SystemTime::now());
    if use_stdin {
        return execute_script_reader(io::stdin().lock(), "standard input", &mut session, sink)
            .await;
    }

    for input in inputs {
        match input {
            ScriptInput::Command(command) => {
                for statement in split_command(command)? {
                    session.execute(&statement, sink).await?;
                }
            }
            ScriptInput::File(path) => {
                let file = fs::File::open(path)
                    .map_err(|_| ApplicationError::runtime("could not open SQL file"))?;
                execute_script_reader(BufReader::new(file), "SQL file", &mut session, sink).await?;
            }
        }
    }
    Ok(())
}

fn split_command(input: &str) -> Result<Vec<String>, ApplicationError> {
    split_command_with_limit(input, MAX_STATEMENT_BYTES)
}

fn split_command_with_limit(
    input: &str,
    max_statement_bytes: usize,
) -> Result<Vec<String>, ApplicationError> {
    if input.len() > max_statement_bytes {
        return Err(ApplicationError::usage(format!(
            "command input is larger than {} MiB",
            MAX_STATEMENT_BYTES / (1024 * 1024)
        )));
    }
    let mut stream = StatementStream::new();
    let mut statements = stream
        .push_bounded(input, max_statement_bytes)
        .map_err(|()| statement_too_large("command input"))?;
    if !stream.pending_is_trivia() {
        let statement = stream.take_complete_statement().ok_or_else(|| {
            ApplicationError::usage("command input ends with an incomplete SQL statement")
        })?;
        statements.push(statement);
    }
    Ok(statements
        .into_iter()
        .map(|statement| statement.into_text())
        .collect())
}

#[cfg(test)]
fn split_script(input: &str, source: &str) -> Result<Vec<String>, ApplicationError> {
    split_script_with_limit(input, source, MAX_STATEMENT_BYTES)
}

#[cfg(test)]
fn split_script_with_limit(
    input: &str,
    source: &str,
    max_statement_bytes: usize,
) -> Result<Vec<String>, ApplicationError> {
    let mut stream = StatementStream::new();
    let statements = stream
        .push_bounded(input, max_statement_bytes)
        .map_err(|()| statement_too_large(source))?
        .into_iter()
        .map(|statement| statement.into_text())
        .collect();
    if stream.pending_is_trivia() {
        Ok(statements)
    } else {
        Err(ApplicationError::usage(format!(
            "{source} ends with an incomplete SQL statement"
        )))
    }
}

async fn execute_script_reader(
    mut reader: impl BufRead,
    source: &str,
    session: &mut ManagedSession<'_>,
    sink: &mut dyn ExecutionSink,
) -> Result<(), ApplicationError> {
    const READ_CHUNK_BYTES: usize = 64 * 1024;

    let mut stream = StatementStream::new();
    let mut utf8 = Vec::with_capacity(READ_CHUNK_BYTES + 3);
    loop {
        let chunk = {
            let available = reader.fill_buf().map_err(|_| {
                ApplicationError::runtime(format!("could not read {source} as UTF-8"))
            })?;
            if available.is_empty() {
                Vec::new()
            } else {
                available[..available.len().min(READ_CHUNK_BYTES)].to_vec()
            }
        };
        if chunk.is_empty() {
            break;
        }
        reader.consume(chunk.len());
        utf8.extend_from_slice(&chunk);

        let valid_bytes = match std::str::from_utf8(&utf8) {
            Ok(_) => utf8.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => {
                return Err(ApplicationError::runtime(format!(
                    "could not read {source} as UTF-8"
                )));
            }
        };
        if valid_bytes == 0 {
            continue;
        }
        let text = std::str::from_utf8(&utf8[..valid_bytes])
            .expect("validated UTF-8 prefix")
            .to_owned();
        utf8.drain(..valid_bytes);
        for statement in stream
            .push_bounded(&text, MAX_STATEMENT_BYTES)
            .map_err(|()| statement_too_large(source))?
        {
            let statement = statement.into_text();
            session.execute(&statement, sink).await?;
        }
    }
    if !utf8.is_empty() {
        return Err(ApplicationError::runtime(format!(
            "could not read {source} as UTF-8"
        )));
    }
    if !stream.pending_is_trivia() {
        return Err(ApplicationError::usage(format!(
            "{source} ends with an incomplete SQL statement"
        )));
    }
    Ok(())
}

fn statement_too_large(source: &str) -> ApplicationError {
    ApplicationError::usage(format!(
        "{source} contains a SQL statement larger than {} MiB",
        MAX_STATEMENT_BYTES / (1024 * 1024)
    ))
}

fn execution_sink(format: InventoryFormat, stdout_is_terminal: bool) -> Box<dyn ExecutionSink> {
    match format {
        InventoryFormat::Table => Box::new(TableExecutionSink::new(io::stdout(), io::stderr(), 80)),
        InventoryFormat::Csv => Box::new(
            DelimitedSink::csv(io::stdout(), io::stderr())
                .with_terminal_escaping(stdout_is_terminal),
        ),
        InventoryFormat::Tsv => Box::new(
            DelimitedSink::tsv(io::stdout(), io::stderr())
                .with_terminal_escaping(stdout_is_terminal),
        ),
        InventoryFormat::Jsonl => Box::new(JsonlExecutionSink::new(io::stdout())),
    }
}

fn select_inventory_target(
    prompt: &mut dyn Prompt,
    inventory: Result<Vec<DiscoverableCluster>, ApplicationError>,
    region: &str,
) -> Result<ClusterTarget, ApplicationError> {
    match inventory {
        Ok(clusters) if !clusters.is_empty() => select_discovered_target(prompt, &clusters),
        Ok(_) => {
            prompt.warning(
                "no discoverable clusters were returned; discovery permission does not imply connection permission",
            )?;
            manual_target(prompt, region)
        }
        Err(_) => {
            prompt.warning(
                "cluster discovery was unavailable; discovery permission is separate from permission to connect",
            )?;
            manual_target(prompt, region)
        }
    }
}

fn manual_target(prompt: &mut dyn Prompt, region: &str) -> Result<ClusterTarget, ApplicationError> {
    let selector = prompt
        .manual_selector()?
        .ok_or_else(|| ApplicationError::usage("a cluster selector is required"))?;
    target_from_selector(&parse_selector(&selector)?, region)
}

fn parse_selector(value: &str) -> Result<ClusterSelector, ApplicationError> {
    parse_cluster_selector(value).map_err(|error| ApplicationError::usage(error.to_string()))
}

fn target_from_selector(
    selector: &ClusterSelector,
    region: &str,
) -> Result<ClusterTarget, ApplicationError> {
    selector
        .check_region(region)
        .map_err(|error| ApplicationError::usage(error.to_string()))?;
    let endpoint = selector
        .endpoint()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}.dsql.{region}.on.aws", selector.identifier()));
    Ok(ClusterTarget::resolved(
        selector.identifier(),
        region,
        Some(endpoint),
        selector.arn().map(str::to_owned),
    ))
}

fn select_discovered_target(
    prompt: &mut dyn Prompt,
    clusters: &[DiscoverableCluster],
) -> Result<ClusterTarget, ApplicationError> {
    for cluster in clusters {
        let status = status_value(cluster.status()).unwrap_or("unknown");
        prompt.warning(&format!(
            "{} ({status})",
            escape_terminal_text(cluster.id().as_str())
        ))?;
    }
    loop {
        let selected = prompt
            .select_cluster(clusters)?
            .ok_or_else(|| ApplicationError::usage("a cluster selection is required"))?;
        let cluster = clusters
            .iter()
            .find(|cluster| cluster.id().as_str() == selected)
            .ok_or_else(|| {
                ApplicationError::usage("selected cluster is not in the discoverable inventory")
            })?;
        match cluster.status() {
            Some(ClusterStatus::Active) => return Ok(ClusterTarget::from_discovered(cluster)),
            None | Some(ClusterStatus::Unknown) => {
                prompt.warning("the selected cluster has unknown status")?;
                if prompt.confirm_unknown_cluster(cluster)? {
                    return Ok(ClusterTarget::from_discovered(cluster));
                }
            }
            Some(_) => {
                prompt.warning("the selected cluster is not active and cannot be selected")?
            }
        }
    }
}

fn select_database_role(
    prompt: &mut dyn Prompt,
    username: Option<String>,
) -> Result<DatabaseRole, ApplicationError> {
    match username {
        Some(username) if username == "admin" => Ok(DatabaseRole::Admin),
        Some(username) if !username.trim().is_empty() => Ok(DatabaseRole::Custom(username)),
        Some(_) => Err(ApplicationError::usage("--username must not be empty")),
        None => match prompt
            .select_role()?
            .ok_or_else(|| ApplicationError::usage("a database role is required"))?
        {
            RoleChoice::Admin => Ok(DatabaseRole::Admin),
            RoleChoice::Custom => prompt
                .custom_role_name()?
                .filter(|name| !name.trim().is_empty())
                .map(DatabaseRole::Custom)
                .ok_or_else(|| ApplicationError::usage("a custom database role is required")),
        },
    }
}

fn emit_identity_context(
    stderr: &mut dyn Write,
    lookup: &crate::aws::identity::CallerIdentityLookup,
) -> Result<(), ApplicationError> {
    if let Some(warning) = lookup.warning() {
        writeln!(stderr, "warning: {}", warning.diagnostic()).map_err(output_error)?;
    } else if let Some(identity) = lookup.identity() {
        writeln!(
            stderr,
            "AWS identity: account {}, principal {}",
            escape_terminal_text(identity.account_id().unwrap_or("unavailable")),
            escape_terminal_text(identity.principal().unwrap_or("unavailable"))
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn emit_region_diagnostics(
    stderr: &mut dyn Write,
    diagnostics: &crate::aws::config::RegionDiagnostics,
) -> Result<(), ApplicationError> {
    writeln!(
        stderr,
        "AWS Region source: {:?}, profile: {}",
        diagnostics.source(),
        escape_terminal_text(diagnostics.profile_label().unwrap_or("default"))
    )
    .map_err(output_error)
}

fn output_error(error: io::Error) -> ApplicationError {
    if error.kind() == io::ErrorKind::BrokenPipe {
        ApplicationError::broken_pipe("could not write command output")
    } else {
        ApplicationError::runtime("could not write command output")
    }
}

#[derive(Serialize)]
struct JsonCluster<'a> {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    region: &'a str,
    status: Option<&'static str>,
    endpoint: Option<&'a str>,
    name: Option<&'a str>,
    enrichment: &'static str,
}

fn render_inventory(
    clusters: &[DiscoverableCluster],
    format: InventoryFormat,
    stdout: &mut dyn Write,
    stdout_is_terminal: bool,
) -> Result<(), ApplicationError> {
    let mut clusters = clusters.iter().collect::<Vec<_>>();
    clusters.sort_by(|left, right| {
        left.id()
            .as_str()
            .cmp(right.id().as_str())
            .then_with(|| left.region().cmp(right.region()))
    });
    match format {
        InventoryFormat::Table => render_table(&clusters, stdout),
        InventoryFormat::Csv => render_delimited(&clusters, b',', stdout, stdout_is_terminal),
        InventoryFormat::Tsv => render_delimited(&clusters, b'\t', stdout, stdout_is_terminal),
        InventoryFormat::Jsonl => render_jsonl(&clusters, stdout),
    }
}

fn inventory_cells(cluster: &DiscoverableCluster) -> [&str; 6] {
    [
        cluster.id().as_str(),
        cluster.region(),
        status_value(cluster.status()).unwrap_or(""),
        cluster.endpoint().unwrap_or(""),
        cluster.display_name().unwrap_or(""),
        enrichment_value(cluster.enrichment()),
    ]
}

fn render_delimited(
    clusters: &[&DiscoverableCluster],
    delimiter: u8,
    stdout: &mut dyn Write,
    stdout_is_terminal: bool,
) -> Result<(), ApplicationError> {
    let mut writer = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(stdout);
    writer
        .write_record(["ID", "REGION", "STATUS", "ENDPOINT", "NAME", "ENRICHMENT"])
        .map_err(inventory_csv_error)?;
    for cluster in clusters {
        if stdout_is_terminal {
            writer
                .write_record(inventory_cells(cluster).map(escape_terminal_text))
                .map_err(inventory_csv_error)?;
        } else {
            writer
                .write_record(inventory_cells(cluster))
                .map_err(inventory_csv_error)?;
        }
    }
    writer.flush().map_err(|error| {
        if error.kind() == io::ErrorKind::BrokenPipe {
            ApplicationError::broken_pipe("could not render inventory")
        } else {
            ApplicationError::runtime("could not render inventory")
        }
    })
}

fn inventory_csv_error(error: csv::Error) -> ApplicationError {
    if matches!(
        error.kind(),
        csv::ErrorKind::Io(error) if error.kind() == io::ErrorKind::BrokenPipe
    ) {
        ApplicationError::broken_pipe("could not render inventory")
    } else {
        ApplicationError::runtime("could not render inventory")
    }
}

fn render_jsonl(
    clusters: &[&DiscoverableCluster],
    stdout: &mut dyn Write,
) -> Result<(), ApplicationError> {
    for cluster in clusters {
        serde_json::to_writer(
            &mut *stdout,
            &JsonCluster {
                version: 1,
                kind: "cluster",
                id: cluster.id().as_str(),
                region: cluster.region(),
                status: status_value(cluster.status()),
                endpoint: cluster.endpoint(),
                name: cluster.display_name(),
                enrichment: enrichment_value(cluster.enrichment()),
            },
        )
        .map_err(|error| {
            if error.io_error_kind() == Some(io::ErrorKind::BrokenPipe) {
                ApplicationError::broken_pipe("could not render inventory")
            } else {
                ApplicationError::runtime("could not render inventory")
            }
        })?;
        writeln!(stdout).map_err(output_error)?;
    }
    Ok(())
}

fn render_table(
    clusters: &[&DiscoverableCluster],
    stdout: &mut dyn Write,
) -> Result<(), ApplicationError> {
    let headers = ["ID", "REGION", "STATUS", "ENDPOINT", "NAME", "ENRICHMENT"].map(str::to_owned);
    let mut widths = headers.each_ref().map(|value| value.len());
    for cluster in clusters {
        let row = inventory_cells(cluster).map(escape_terminal_text);
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(value.len());
        }
    }
    write_table_row(stdout, &headers, &widths)?;
    let separators = widths.map(|width| "-".repeat(width));
    write_table_row(stdout, &separators, &widths)?;
    for cluster in clusters {
        let row = inventory_cells(cluster).map(escape_terminal_text);
        write_table_row(stdout, &row, &widths)?;
    }
    Ok(())
}

fn write_table_row(
    stdout: &mut dyn Write,
    row: &[String; 6],
    widths: &[usize; 6],
) -> Result<(), ApplicationError> {
    for (index, value) in row.iter().enumerate() {
        if index > 0 {
            write!(stdout, "  ").map_err(output_error)?;
        }
        write!(stdout, "{value:width$}", width = widths[index]).map_err(output_error)?;
    }
    writeln!(stdout).map_err(output_error)
}

fn status_value(status: Option<ClusterStatus>) -> Option<&'static str> {
    match status {
        Some(ClusterStatus::Creating) => Some("creating"),
        Some(ClusterStatus::Active) => Some("active"),
        Some(ClusterStatus::Idle) => Some("idle"),
        Some(ClusterStatus::Inactive) => Some("inactive"),
        Some(ClusterStatus::Updating) => Some("updating"),
        Some(ClusterStatus::Deleting) => Some("deleting"),
        Some(ClusterStatus::Deleted) => Some("deleted"),
        Some(ClusterStatus::Failed) => Some("failed"),
        Some(ClusterStatus::PendingSetup) => Some("pending_setup"),
        Some(ClusterStatus::PendingDelete) => Some("pending_delete"),
        Some(ClusterStatus::Unknown) => Some("unknown"),
        None => None,
    }
}

fn enrichment_value(enrichment: EnrichmentState) -> &'static str {
    match enrichment {
        EnrichmentState::Complete => "complete",
        EnrichmentState::Unavailable(EnrichmentErrorCategory::AccessDenied) => {
            "unavailable_access_denied"
        }
        EnrichmentState::Unavailable(EnrichmentErrorCategory::Throttled) => "unavailable_throttled",
        EnrichmentState::Unavailable(EnrichmentErrorCategory::NotFound) => "unavailable_not_found",
        EnrichmentState::Unavailable(EnrichmentErrorCategory::Other) => "unavailable_other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        CancellationCapability, ClusterId, ConnectedSession, ExecutionSink, SessionHandle,
        SessionMetadata, TransactionState,
    };
    use clap::{CommandFactory, FromArgMatches, Parser, error::ErrorKind};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::SystemTime;

    const ID_A: &str = "0123456789abcdefghijklmnop";
    const ID_B: &str = "1123456789abcdefghijklmnop";

    #[test]
    fn help_and_version_render_before_runtime_work() {
        let help = Cli::try_parse_from(["dsql", "--help"]).expect_err("help exits");
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("Usage: dsql"));

        let version = Cli::try_parse_from(["dsql", "--version"]).expect_err("version exits");
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);
        assert_eq!(version.to_string().trim(), "dsql 1.0.0");
    }

    fn cluster(id: &str, status: Option<ClusterStatus>) -> DiscoverableCluster {
        DiscoverableCluster::new(
            ClusterId::new(id),
            "us-east-1",
            Some(format!("{id}.dsql.us-east-1.on.aws")),
            status,
            Some("orders".into()),
        )
    }

    #[test]
    fn all_inventory_formats_are_deterministic_and_preserve_unavailable_enrichment() {
        let unavailable = DiscoverableCluster::inventory(
            ClusterId::new(ID_B),
            format!("arn:aws:dsql:us-east-1:123456789012:cluster/{ID_B}"),
            "us-east-1",
            None,
            None,
            None,
            EnrichmentState::Unavailable(EnrichmentErrorCategory::AccessDenied),
        );
        let clusters = vec![unavailable, cluster(ID_A, Some(ClusterStatus::Active))];
        for format in [
            InventoryFormat::Table,
            InventoryFormat::Csv,
            InventoryFormat::Tsv,
            InventoryFormat::Jsonl,
        ] {
            let mut output = Vec::new();
            render_inventory(&clusters, format, &mut output, false).expect("render inventory");
            let output = String::from_utf8(output).expect("UTF-8 output");
            assert!(output.contains(ID_A));
            assert!(output.contains("unavailable_access_denied"));
            assert!(output.find(ID_A) < output.find(ID_B));
            if format == InventoryFormat::Jsonl {
                assert!(output.contains("\"version\":1"));
                assert!(output.contains("\"status\":null"));
                assert!(output.contains("\"endpoint\":null"));
            }
        }
    }

    #[test]
    fn terminal_inventory_escapes_csi_osc_and_c1_metadata() {
        let cluster = DiscoverableCluster::inventory(
            ClusterId::new(ID_A),
            format!("arn:aws:dsql:us-east-1:123456789012:cluster/{ID_A}"),
            "us-east-1",
            Some("endpoint\u{1b}[2J".into()),
            Some(ClusterStatus::Active),
            Some("name\u{1b}]8;;url\u{7}link\u{9b}\u{9d}".into()),
            EnrichmentState::Complete,
        );

        for format in [
            InventoryFormat::Table,
            InventoryFormat::Csv,
            InventoryFormat::Tsv,
        ] {
            let mut output = Vec::new();
            render_inventory(std::slice::from_ref(&cluster), format, &mut output, true)
                .expect("terminal inventory");
            let output = String::from_utf8(output).expect("UTF-8");
            assert!(!output.contains('\u{1b}'));
            assert!(!output.contains('\u{9b}'));
            assert!(!output.contains('\u{9d}'));
        }

        let mut redirected = Vec::new();
        render_inventory(&[cluster], InventoryFormat::Csv, &mut redirected, false)
            .expect("redirected inventory");
        assert!(
            String::from_utf8(redirected)
                .expect("UTF-8")
                .contains('\u{1b}')
        );
    }

    #[test]
    fn aws_context_escapes_external_identity_and_profile_controls() {
        let identity = crate::aws::identity::CallerIdentityLookup::test_resolved(
            crate::app::CallerIdentity::new(
                Some("123\u{1b}[2J".into()),
                Some("arn\u{9b}\u{9d}".into()),
            ),
        );
        let mut output = Vec::new();
        emit_identity_context(&mut output, &identity).expect("identity context");
        let diagnostics = crate::aws::config::RegionDiagnostics::test_new(
            crate::aws::config::RegionResolutionSource::ExplicitFlag,
            Some("profile\u{1b}]8;;url\u{7}".into()),
        );
        emit_region_diagnostics(&mut output, &diagnostics).expect("region context");

        let output = String::from_utf8(output).expect("UTF-8");
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains('\u{9b}'));
        assert!(!output.contains('\u{9d}'));
    }

    #[test]
    fn clap_contract_accepts_global_options_and_clusters_subcommand() {
        let cli = Cli::try_parse_from([
            "dsql",
            "clusters",
            "--profile",
            "preview",
            "--region",
            "us-east-1",
            "-U",
            "app_user",
            "--format",
            "jsonl",
        ])
        .expect("parse command");

        assert!(matches!(cli.command, Some(Command::Clusters)));
        assert_eq!(cli.profile.as_deref(), Some("preview"));
        assert_eq!(cli.region.as_deref(), Some("us-east-1"));
        assert_eq!(cli.username.as_deref(), Some("app_user"));
        assert_eq!(cli.format, InventoryFormat::Jsonl);
    }

    #[test]
    fn interactive_history_options_are_accepted_globally() {
        let cli = Cli::try_parse_from([
            "dsql",
            "--no-history",
            "--history-file",
            "/tmp/dsql-history",
        ])
        .expect("parse history options");

        assert!(cli.no_history);
        assert_eq!(
            cli.history_file.as_deref(),
            Some(std::path::Path::new("/tmp/dsql-history"))
        );
    }

    #[test]
    fn command_and_file_inputs_keep_their_argument_order() {
        let matches = Cli::command()
            .try_get_matches_from([
                "dsql",
                "-c",
                "first",
                "-f",
                "one.sql",
                "--command",
                "second",
                "--file",
                "two.sql",
            ])
            .expect("parse command");
        let cli = Cli::from_arg_matches(&matches)
            .expect("build CLI")
            .with_script_input_order(&matches);

        assert_eq!(
            cli.script_inputs,
            vec![
                ScriptInput::Command("first".into()),
                ScriptInput::File("one.sql".into()),
                ScriptInput::Command("second".into()),
                ScriptInput::File("two.sql".into()),
            ]
        );
    }

    #[tokio::test]
    async fn clusters_rejects_script_inputs_before_aws_work() {
        let matches = Cli::command()
            .try_get_matches_from([
                "dsql",
                "clusters",
                "--region",
                "us-east-1",
                "-c",
                "SELECT 1",
            ])
            .expect("parse command");
        let cli = Cli::from_arg_matches(&matches)
            .expect("build CLI")
            .with_script_input_order(&matches);

        let error = cli.run_clusters().await.expect_err("script input rejected");
        assert_eq!(error.category(), crate::error::ErrorCategory::Usage);
        assert!(error.to_string().contains("does not accept"));
    }

    #[test]
    fn scripts_require_a_terminated_final_statement_but_allow_trailing_comments() {
        assert_eq!(
            split_script("SELECT 1; -- done\n/* done */", "standard input")
                .expect("trailing trivia"),
            ["SELECT 1;"]
        );
        let error = split_script("SELECT 1", "standard input").expect_err("incomplete SQL");
        assert_eq!(error.category(), crate::error::ErrorCategory::Usage);

        let error = split_script_with_limit("SELECT 12;", "SQL file", 9)
            .expect_err("oversized statement rejected");
        assert_eq!(error.category(), crate::error::ErrorCategory::Usage);
        assert!(error.to_string().contains("larger than"));
    }

    #[test]
    fn command_input_frames_multiple_statements_and_allows_a_complete_final_suffix() {
        assert_eq!(
            split_command_with_limit("SELECT 1; SELECT 2", 64).expect("command frames"),
            vec!["SELECT 1;", " SELECT 2"]
        );
        assert!(split_command_with_limit("SELECT 'unterminated", 64).is_err());
        assert!(split_command_with_limit("SELECT 12345", 4).is_err());
        assert!(split_command_with_limit("A;B;C;D;", 8).is_ok());
        assert!(split_command_with_limit("A;B;C;D;E;", 8).is_err());
    }

    #[test]
    fn stdin_is_used_only_without_explicit_script_input() {
        assert!(uses_stdin_script(true, false));
        assert!(!uses_stdin_script(false, false));
        assert!(!uses_stdin_script(true, true));
    }

    struct RecordingSink;

    impl ExecutionSink for RecordingSink {
        fn emit(&mut self, _: crate::app::ExecutionEvent) -> Result<(), ApplicationError> {
            Ok(())
        }
    }

    struct StopHandle(Arc<Mutex<Vec<String>>>);

    impl SessionHandle for StopHandle {
        fn execute<'a>(
            &'a mut self,
            statement: &'a str,
            _: &'a mut dyn ExecutionSink,
        ) -> crate::app::BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(async move {
                self.0.lock().expect("statements").push(statement.into());
                if statement == "bad" {
                    Err(ApplicationError::runtime("statement failed"))
                } else {
                    Ok(())
                }
            })
        }

        fn cancellation_handle(
            &self,
        ) -> Option<std::sync::Arc<dyn crate::app::SessionCancellation>> {
            None
        }
    }

    struct StopConnector {
        connects: AtomicUsize,
        statements: Arc<Mutex<Vec<String>>>,
    }

    impl SessionConnector for StopConnector {
        fn connect<'a>(
            &'a self,
            intent: &'a ConnectionIntent,
        ) -> crate::app::BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
            Box::pin(async move {
                self.connects.fetch_add(1, Ordering::SeqCst);
                Ok(ConnectedSession::new(
                    SessionMetadata::new(
                        intent.clone(),
                        SystemTime::now(),
                        CancellationCapability::Unavailable,
                        TransactionState::Idle,
                        Vec::new(),
                    ),
                    Box::new(StopHandle(self.statements.clone())),
                ))
            })
        }
    }

    #[tokio::test]
    async fn statement_execution_connects_once_and_stops_at_the_first_failure() {
        let connector = StopConnector {
            connects: AtomicUsize::new(0),
            statements: Arc::new(Mutex::new(Vec::new())),
        };
        let intent = ConnectionIntent::new(
            ClusterTarget::new(
                "cluster",
                "us-east-1",
                Some("cluster.dsql.us-east-1.on.aws".into()),
            ),
            DatabaseRole::Admin,
            Vec::new(),
            "dsql test",
        );
        let inputs = vec![
            ScriptInput::Command("first".into()),
            ScriptInput::Command("bad".into()),
            ScriptInput::Command("last".into()),
        ];
        let error = execute_script_inputs(&connector, &intent, &inputs, false, &mut RecordingSink)
            .await
            .expect_err("failure stops execution");

        assert_eq!(error.category(), crate::error::ErrorCategory::Runtime);
        assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
        assert_eq!(
            connector.statements.lock().expect("statements").as_slice(),
            ["first", "bad"]
        );
    }

    #[tokio::test]
    async fn later_file_read_failure_does_not_prevent_earlier_execution() {
        let connector = StopConnector {
            connects: AtomicUsize::new(0),
            statements: Arc::new(Mutex::new(Vec::new())),
        };
        let intent = ConnectionIntent::new(
            ClusterTarget::new(
                "cluster",
                "us-east-1",
                Some("cluster.dsql.us-east-1.on.aws".into()),
            ),
            DatabaseRole::Admin,
            Vec::new(),
            "dsql test",
        );
        let missing = std::env::temp_dir().join(format!(
            "dsql-cli-missing-{}-{}.sql",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let inputs = vec![
            ScriptInput::Command("first".into()),
            ScriptInput::File(missing.to_string_lossy().into_owned()),
            ScriptInput::Command("last".into()),
        ];

        let error = execute_script_inputs(&connector, &intent, &inputs, false, &mut RecordingSink)
            .await
            .expect_err("missing file stops at its position");

        assert_eq!(error.to_string(), "could not open SQL file");
        assert_eq!(
            connector.statements.lock().expect("statements").as_slice(),
            ["first"]
        );
    }

    #[test]
    fn inventory_output_handles_more_than_one_hundred_rows() {
        let clusters = (0..101)
            .map(|index| cluster(&format!("{index:026}"), Some(ClusterStatus::Active)))
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        render_inventory(&clusters, InventoryFormat::Jsonl, &mut output, false)
            .expect("render rows");
        assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 101);
    }

    #[test]
    fn selector_region_conflicts_before_a_target_is_created() {
        let selector = parse_selector(&format!(
            "arn:aws:dsql:us-east-1:123456789012:cluster/{ID_A}"
        ))
        .expect("parse ARN");
        let error = target_from_selector(&selector, "eu-west-1").expect_err("conflicting Region");
        assert_eq!(error.category(), crate::error::ErrorCategory::Usage);
    }

    #[test]
    fn ids_and_arns_derive_the_canonical_endpoint() {
        for selector in [
            ID_A.to_owned(),
            format!("arn:aws:dsql:us-east-1:123456789012:cluster/{ID_A}"),
        ] {
            let selector = parse_selector(&selector).expect("parse selector");
            let target = target_from_selector(&selector, "us-east-1").expect("target");
            assert_eq!(
                target.endpoint(),
                Some(format!("{ID_A}.dsql.us-east-1.on.aws").as_str())
            );
        }
    }

    #[test]
    fn noninteractive_preview_accepts_direct_inputs_and_rejects_missing_inputs() {
        let selector = parse_selector(ID_A).expect("valid cluster selector");

        assert!(validate_noninteractive_preview(Some(&selector), Some("app_user")).is_ok());
        for (selector, username) in [
            (None, Some("app_user")),
            (Some(&selector), None),
            (None, None),
        ] {
            let error = validate_noninteractive_preview(selector, username)
                .expect_err("missing direct input must be rejected");
            assert_eq!(error.category(), crate::error::ErrorCategory::Usage);
        }
    }

    #[test]
    fn known_inactive_cluster_is_not_selected_and_unknown_requires_confirmation() {
        struct FakePrompt {
            choices: Vec<Option<String>>,
            confirmations: Vec<bool>,
            warnings: Vec<String>,
        }
        impl RegionPrompt for FakePrompt {
            fn prompt_region(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
        }
        impl Prompt for FakePrompt {
            fn select_cluster(
                &mut self,
                _: &[DiscoverableCluster],
            ) -> Result<Option<String>, ApplicationError> {
                Ok(self.choices.remove(0))
            }
            fn confirm_unknown_cluster(
                &mut self,
                _: &DiscoverableCluster,
            ) -> Result<bool, ApplicationError> {
                Ok(self.confirmations.remove(0))
            }
            fn manual_selector(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
            fn select_role(&mut self) -> Result<Option<RoleChoice>, ApplicationError> {
                Ok(None)
            }
            fn custom_role_name(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
            fn warning(&mut self, message: &str) -> Result<(), ApplicationError> {
                self.warnings.push(message.into());
                Ok(())
            }
        }
        let inactive = cluster(ID_A, Some(ClusterStatus::Inactive));
        let unknown = cluster(ID_B, None);
        let mut prompt = FakePrompt {
            choices: vec![Some(ID_A.into()), Some(ID_B.into())],
            confirmations: vec![true],
            warnings: Vec::new(),
        };
        let target =
            select_discovered_target(&mut prompt, &[inactive, unknown]).expect("unknown confirmed");
        assert_eq!(target.id().as_str(), ID_B);
        assert!(
            prompt
                .warnings
                .iter()
                .any(|warning| warning.contains("not active"))
        );
    }

    #[test]
    fn username_avoids_role_prompt_while_absent_username_requires_an_explicit_choice() {
        struct FakePrompt {
            calls: usize,
        }
        impl RegionPrompt for FakePrompt {
            fn prompt_region(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
        }
        impl Prompt for FakePrompt {
            fn select_cluster(
                &mut self,
                _: &[DiscoverableCluster],
            ) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
            fn confirm_unknown_cluster(
                &mut self,
                _: &DiscoverableCluster,
            ) -> Result<bool, ApplicationError> {
                Ok(false)
            }
            fn manual_selector(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
            fn select_role(&mut self) -> Result<Option<RoleChoice>, ApplicationError> {
                self.calls += 1;
                Ok(None)
            }
            fn custom_role_name(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
            fn warning(&mut self, _: &str) -> Result<(), ApplicationError> {
                Ok(())
            }
        }
        let mut prompt = FakePrompt { calls: 0 };
        assert_eq!(
            select_database_role(&mut prompt, Some("app_user".into()))
                .expect("direct role")
                .name(),
            "app_user"
        );
        assert_eq!(prompt.calls, 0);
        assert!(select_database_role(&mut prompt, None).is_err());
        assert_eq!(prompt.calls, 1);
    }

    #[test]
    fn denied_or_empty_inventory_offers_manual_cluster_entry() {
        struct FakePrompt {
            manual: Option<String>,
            warnings: Vec<String>,
        }
        impl RegionPrompt for FakePrompt {
            fn prompt_region(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
        }
        impl Prompt for FakePrompt {
            fn select_cluster(
                &mut self,
                _: &[DiscoverableCluster],
            ) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
            fn confirm_unknown_cluster(
                &mut self,
                _: &DiscoverableCluster,
            ) -> Result<bool, ApplicationError> {
                Ok(false)
            }
            fn manual_selector(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(self.manual.take())
            }
            fn select_role(&mut self) -> Result<Option<RoleChoice>, ApplicationError> {
                Ok(None)
            }
            fn custom_role_name(&mut self) -> Result<Option<String>, ApplicationError> {
                Ok(None)
            }
            fn warning(&mut self, message: &str) -> Result<(), ApplicationError> {
                self.warnings.push(message.into());
                Ok(())
            }
        }

        for inventory in [
            Ok(Vec::new()),
            Err(ApplicationError::runtime("discovery denied")),
        ] {
            let mut prompt = FakePrompt {
                manual: Some(ID_A.into()),
                warnings: Vec::new(),
            };

            let target = select_inventory_target(&mut prompt, inventory, "us-east-1")
                .expect("manual target");

            assert_eq!(target.id().as_str(), ID_A);
            assert_eq!(prompt.warnings.len(), 1);
        }
    }

    #[test]
    fn identity_context_is_separate_from_machine_stdout() {
        let mut stderr = Vec::new();
        let sts = crate::aws::identity::CallerIdentityLookup::test_unavailable(
            crate::aws::identity::CallerIdentityFailure::AccessDenied,
        );
        emit_identity_context(&mut stderr, &sts).expect("warning");
        let mut stdout = Vec::new();
        render_inventory(
            &[cluster(ID_A, Some(ClusterStatus::Active))],
            InventoryFormat::Csv,
            &mut stdout,
            false,
        )
        .expect("CSV");
        assert!(
            String::from_utf8(stderr)
                .expect("stderr")
                .starts_with("warning:")
        );
        assert!(
            !String::from_utf8(stdout)
                .expect("stdout")
                .contains("warning:")
        );
    }
}
