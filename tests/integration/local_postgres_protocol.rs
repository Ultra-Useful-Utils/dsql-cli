use super::super::{
    capture_available_session_settings, connect_postgres,
    connect_postgres_without_session_settings, restore_available_session_settings,
};
use super::RecordingSink;
use crate::{
    app::{
        BoxFuture, ClusterTarget, ConnectedSession, ConnectionIntent, DatabaseRole, ExecutionEvent,
        ManagedSession, SessionConnector, SessionSetting, TransactionState,
    },
    db::tls::make_rustls_connect,
    error::ApplicationError,
};
use rcgen::generate_simple_self_signed;
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime},
};
use tokio::{task::JoinHandle, time::sleep};
use tokio_postgres::{Client, Config, config::SslMode};
use tokio_postgres_rustls::MakeRustlsConnect;

const POSTGRES_IMAGE: &str =
    "postgres:17-bookworm@sha256:4f736ae292687621d4dbe0d499ffd024a36bd2ee7d8ca6f2ccd4c800f047b394";
const POSTGRES_PASSWORD: &str = "dsql-test";

struct DockerResources {
    name: String,
    directory: PathBuf,
}

impl Drop for DockerResources {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .output();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct DockerPostgres {
    _resources: DockerResources,
    certificate: PathBuf,
    port: u16,
}

impl DockerPostgres {
    async fn start() -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let name = format!("dsql-cli-postgres-{}-{unique}", std::process::id());
        let directory = std::env::temp_dir().join(&name);
        fs::create_dir(&directory).expect("create certificate directory");
        let resources = DockerResources { name, directory };
        let certified_key = generate_simple_self_signed(vec!["localhost".into()])
            .expect("generate server certificate");
        let certificate = resources.directory.join("server.crt");
        let private_key = resources.directory.join("server.key");
        fs::write(&certificate, certified_key.cert.pem()).expect("write certificate");
        fs::write(&private_key, certified_key.signing_key.serialize_pem())
            .expect("write private key");

        let mount = format!("{}:/certs:ro", resources.directory.display());
        let output = Command::new("docker")
            .args([
                "run",
                "--detach",
                "--rm",
                "--name",
                &resources.name,
                "--publish",
                "127.0.0.1::5432",
                "--env",
                &format!("POSTGRES_PASSWORD={POSTGRES_PASSWORD}"),
                "--volume",
                &mount,
                POSTGRES_IMAGE,
                "bash",
                "-ceu",
                "cp /certs/server.crt /tmp/server.crt; cp /certs/server.key /tmp/server.key; chown postgres:postgres /tmp/server.crt /tmp/server.key; chmod 600 /tmp/server.key; exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tmp/server.crt -c ssl_key_file=/tmp/server.key",
            ])
            .output()
            .expect("start Docker");
        assert!(
            output.status.success(),
            "start PostgreSQL container: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let port = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let output = Command::new("docker")
                    .args(["port", &resources.name, "5432/tcp"])
                    .output()
                    .expect("inspect published port");
                let value = String::from_utf8_lossy(&output.stdout);
                if let Some(port) = value.trim().rsplit(':').next().and_then(|v| v.parse().ok()) {
                    break port;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Docker published the PostgreSQL port");

        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let mut config = Config::new();
                config
                    .host("localhost")
                    .hostaddr(std::net::Ipv4Addr::LOCALHOST.into())
                    .port(port)
                    .dbname("postgres")
                    .user("postgres")
                    .password(POSTGRES_PASSWORD)
                    .ssl_mode(SslMode::Require);
                let tls = make_rustls_connect(&[certificate.to_string_lossy().into_owned()])
                    .expect("trusted TLS connector");
                if let Ok((client, connection)) = config.connect(tls).await {
                    drop(client);
                    drop(connection);
                    break;
                }
                sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .expect("PostgreSQL became ready");

        Self {
            _resources: resources,
            certificate,
            port,
        }
    }

    fn config(&self) -> Config {
        let mut config = Config::new();
        config
            .host("localhost")
            .hostaddr(std::net::Ipv4Addr::LOCALHOST.into())
            .port(self.port)
            .dbname("postgres")
            .user("postgres")
            .password(POSTGRES_PASSWORD)
            .ssl_mode(SslMode::Require);
        config
    }

    fn tls(&self) -> MakeRustlsConnect {
        make_rustls_connect(&[self.certificate.to_string_lossy().into_owned()])
            .expect("trusted TLS connector")
    }

    fn name(&self) -> &str {
        &self._resources.name
    }

    fn intent(&self, application_name: &str) -> ConnectionIntent {
        ConnectionIntent::new(
            ClusterTarget::new("local", "local", Some("localhost".into())),
            DatabaseRole::Custom("postgres".into()),
            Vec::new(),
            application_name,
        )
    }

    async fn connect_client(&self) -> (Client, JoinHandle<()>) {
        let (client, connection) = self
            .config()
            .connect(self.tls())
            .await
            .expect("connect observer over verified TLS");
        (client, tokio::spawn(async move { drop(connection.await) }))
    }

    async fn connect_session(&self, application_name: &str) -> ConnectedSession {
        let mut config = self.config();
        config.application_name(application_name);
        connect_postgres_without_session_settings(
            config,
            self.tls(),
            &self.intent(application_name),
        )
        .await
        .expect("connect session over verified TLS")
    }
}

struct NoReconnect;

impl SessionConnector for NoReconnect {
    fn connect<'a>(
        &'a self,
        _: &'a ConnectionIntent,
    ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
        Box::pin(async { Err(ApplicationError::runtime("unexpected reconnect")) })
    }

    fn connect_restoring<'a>(
        &'a self,
        _: &'a ConnectionIntent,
        _: &'a [SessionSetting],
    ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
        Box::pin(async { Err(ApplicationError::runtime("unexpected reconnect")) })
    }
}

async fn wait_for_query(client: &Client, marker: &str) -> i32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = client
                .query(
                    "SELECT pid FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND state = 'active' AND query LIKE $1",
                    &[&format!("%{marker}%")],
                )
                .await
                .expect("inspect active query");
            if let Some(row) = rows.first() {
                break row.get(0);
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("target query became active")
}

async fn wait_for_disconnect(client: &Client, pid: i32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let connected = client
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                    &[&pid],
                )
                .await
                .expect("inspect session shutdown")
                .get::<_, bool>(0);
            if !connected {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("session disconnected cleanly");
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn local_tls_postgres_restores_supported_session_settings_safely() {
    let postgres = DockerPostgres::start().await;
    let tls = postgres.tls();
    let config = postgres.config();
    let (source, source_connection) = config
        .connect(tls.clone())
        .await
        .expect("connect source session");
    let source_driver = tokio::spawn(source_connection);

    let mutations = [
        ("application_name", "dsql 'quoted' \\ path\nline"),
        ("client_encoding", "UTF8"),
        ("datestyle", "ISO, DMY"),
        ("extra_float_digits", "2"),
        ("intervalstyle", "iso_8601"),
        ("timezone", "Europe/London"),
        ("search_path", "\"quoted schema\", public"),
        ("enable_bitmapscan", "off"),
        ("enable_hashjoin", "off"),
        ("enable_indexonlyscan", "off"),
        ("enable_indexscan", "off"),
        ("enable_material", "off"),
        ("enable_mergejoin", "off"),
        ("enable_nestloop", "off"),
        ("enable_seqscan", "off"),
    ];
    for (name, value) in mutations {
        source
            .query_one("SELECT set_config($1, $2, false)", &[&name, &value])
            .await
            .expect("mutate source setting");
    }
    let supported_names = mutations.map(|(name, _)| name);
    let captured = capture_available_session_settings(&source, &supported_names)
        .await
        .expect("capture source settings");

    let (restored, restored_connection) = config
        .connect(tls.clone())
        .await
        .expect("connect restored session");
    let restored_driver = tokio::spawn(restored_connection);
    restore_available_session_settings(&restored, &captured)
        .await
        .expect("restore settings with parameters");
    let recaptured = capture_available_session_settings(&restored, &supported_names)
        .await
        .expect("capture restored settings");
    assert_eq!(recaptured, captured);

    drop(source);
    drop(restored);
    source_driver.abort();
    restored_driver.abort();

    let mut complete = captured;
    complete.push(SessionSetting::new("disable_sync_create_index", "on"));
    let error = match connect_postgres(
        config,
        tls,
        &postgres.intent("dsql failed restoration acceptance"),
        Some(&complete),
    )
    .await
    {
        Ok(_) => panic!("unsupported restoration must discard the new session"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "could not safely restore database session settings"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn local_tls_postgres_covers_protocol_and_transaction_state() {
    let postgres = DockerPostgres::start().await;
    let session = postgres.connect_session("dsql protocol acceptance").await;
    let connector = NoReconnect;
    let mut session = ManagedSession::new(session, &connector, SystemTime::now());
    let mut sink = RecordingSink::default();

    session
        .execute(
            "SELECT 'first'::text AS value; SELECT NULL::text AS value;",
            &mut sink,
        )
        .await
        .expect("stream multiple results");
    assert_eq!(
        sink.0,
        [
            ExecutionEvent::Columns(vec!["value".into()]),
            ExecutionEvent::Row(vec![Some("first".into())]),
            ExecutionEvent::CommandComplete { rows: 1 },
            ExecutionEvent::Columns(vec!["value".into()]),
            ExecutionEvent::Row(vec![None]),
            ExecutionEvent::CommandComplete { rows: 1 },
        ]
    );

    let before = sink.0.len();
    session
        .execute("CREATE TEMP TABLE protocol_test(id bigint);", &mut sink)
        .await
        .expect("command-only statement");
    assert_eq!(
        &sink.0[before..],
        &[ExecutionEvent::CommandComplete { rows: 0 }]
    );

    let before = sink.0.len();
    session
        .execute_params(
            "SELECT $1::text AS value WHERE false;",
            &["unused".into()],
            &mut sink,
        )
        .await
        .expect("empty parameterized result");
    assert_eq!(
        &sink.0[before..],
        &[
            ExecutionEvent::Columns(vec!["value".into()]),
            ExecutionEvent::CommandComplete { rows: 0 },
        ]
    );

    let before = sink.0.len();
    session
        .execute(
            "DO $$ BEGIN RAISE NOTICE 'session notice'; END $$;",
            &mut sink,
        )
        .await
        .expect("notice statement");
    assert!(sink.0[before..].iter().any(
        |event| matches!(event, ExecutionEvent::Notice(message) if message == "session notice")
    ));

    session.execute("BEGIN;", &mut sink).await.expect("begin");
    assert_eq!(session.state(), TransactionState::Active);
    session
        .execute("SELECT missing_protocol_column;", &mut sink)
        .await
        .expect_err("invalid statement fails transaction");
    assert_eq!(session.state(), TransactionState::Failed);
    session
        .execute("ROLLBACK;", &mut sink)
        .await
        .expect("rollback failed transaction");
    assert_eq!(session.state(), TransactionState::Idle);

    let (observer, observer_driver) = postgres.connect_client().await;
    let cancellation = session.cancellation_handle().expect("cancellation handle");
    let cancel_task = tokio::spawn(async move {
        let _ = wait_for_query(&observer, "dsql_cancel_marker").await;
        cancellation.cancel().await
    });
    let canceled = tokio::time::timeout(
        Duration::from_secs(5),
        session.execute(
            "SELECT pg_sleep(30) /* dsql_cancel_marker */;",
            &mut sink,
        ),
    )
    .await
    .expect("cancellation completed")
    .expect_err("canceled statement fails");
    cancel_task
        .await
        .expect("cancellation task")
        .expect("cancel request");
    assert!(canceled.to_string().contains("57014"));
    assert_eq!(session.state(), TransactionState::Idle);
    observer_driver.abort();

    session
        .execute("SELECT 'after cancel' AS value;", &mut sink)
        .await
        .expect("session remains usable after cancellation");
    assert!(
        sink.0
            .contains(&ExecutionEvent::Row(vec![Some("after cancel".into())]))
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn local_tls_postgres_reports_disconnect_without_replay() {
    let postgres = DockerPostgres::start().await;
    let session = postgres.connect_session("dsql disconnect acceptance").await;
    let connector = NoReconnect;
    let mut session = ManagedSession::new(session, &connector, SystemTime::now());
    let mut sink = RecordingSink::default();
    let (observer, observer_driver) = postgres.connect_client().await;
    let container_name = postgres.name().to_owned();
    let terminate_task = tokio::spawn(async move {
        let _ = wait_for_query(&observer, "dsql_disconnect_marker").await;
        Command::new("docker")
            .args(["kill", "--signal", "KILL", &container_name])
            .output()
            .expect("kill PostgreSQL container")
    });

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        session.execute(
            "SELECT pg_sleep(30) /* dsql_disconnect_marker */;",
            &mut sink,
        ),
    )
    .await
    .expect("disconnect completed")
    .expect_err("disconnected statement fails");
    let termination = terminate_task.await.expect("termination task");
    assert!(
        termination.status.success(),
        "kill PostgreSQL container: {}",
        String::from_utf8_lossy(&termination.stderr)
    );
    observer_driver.abort();

    assert!(error.to_string().contains("outcome is unknown"));
    assert!(session.reconnect_required());
    assert!(matches!(sink.0.last(), Some(ExecutionEvent::Error { .. })));
    let before = sink.0.len();
    let reconnect_error = session
        .execute("SELECT 'must not replay';", &mut sink)
        .await
        .expect_err("replacement failure must not submit SQL");
    assert_eq!(reconnect_error.to_string(), "unexpected reconnect");
    assert_eq!(sink.0.len(), before);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn local_tls_postgres_closes_the_server_session_when_dropped() {
    let postgres = DockerPostgres::start().await;
    let application_name = "dsql clean shutdown acceptance";
    let session = postgres.connect_session(application_name).await;
    let (observer, observer_driver) = postgres.connect_client().await;
    let pid = observer
        .query_one(
            "SELECT pid FROM pg_stat_activity WHERE application_name = $1",
            &[&application_name],
        )
        .await
        .expect("find connected session")
        .get::<_, i32>(0);

    drop(session);
    wait_for_disconnect(&observer, pid).await;
    observer_driver.abort();
}
