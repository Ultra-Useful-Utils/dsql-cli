use crate::{
    app::{
        ColumnName, ConnectedSession, DatabaseRole, ExecutionEvent, ExecutionSink, ManagedSession,
        MetadataSnapshot, RelationName, SessionCancellation,
    },
    error::ApplicationError,
};
use std::time::{Duration, SystemTime};

const LOAD_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_millis(500);
const SCHEMA_LIMIT: usize = 256;
const RELATION_LIMIT: usize = 2_048;
const COLUMN_LIMIT: usize = 8_192;
const ROLE_LIMIT: usize = 256;
const METADATA_BYTES_LIMIT: usize = 1024 * 1024;
const METADATA_FIELD_BYTES_LIMIT: usize = 1024;

pub(crate) fn is_schema_changing(statement: &str) -> bool {
    let keywords = crate::sql::scanner::leading_keywords(statement, 3);
    let is = |index: usize, keyword: &str| {
        keywords
            .get(index)
            .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
    };

    matches!(
        keywords.first().map(|keyword| keyword.to_ascii_uppercase()),
        Some(keyword)
            if matches!(
                keyword.as_str(),
                "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "COMMENT" | "GRANT" | "REVOKE"
            )
    ) || (is(0, "SECURITY") && is(1, "LABEL"))
        || (is(0, "AWS") && is(1, "IAM") && (is(2, "GRANT") || is(2, "REVOKE")))
}

/// Loads completion data with one bounded, fixed catalog query per name kind.
/// Catalog access is optional: an unavailable or slow catalog produces a stale
/// partial snapshot rather than ending an established interactive shell.
#[cfg(test)]
pub(crate) async fn load_snapshot(session: &mut ConnectedSession) -> MetadataSnapshot {
    load_snapshot_from(session).await
}

pub(crate) async fn load_managed_snapshot(session: &mut ManagedSession<'_>) -> MetadataSnapshot {
    load_snapshot_from(session).await
}

trait MetadataSession {
    fn cancellation_handle(&self) -> Option<std::sync::Arc<dyn SessionCancellation>>;

    fn execute_params<'a>(
        &'a mut self,
        statement: &'a str,
        params: &'a [String],
        sink: &'a mut dyn ExecutionSink,
    ) -> crate::app::BoxFuture<'a, Result<(), ApplicationError>>;

    fn require_reconnect(&mut self) {}
}

impl MetadataSession for ConnectedSession {
    fn cancellation_handle(&self) -> Option<std::sync::Arc<dyn SessionCancellation>> {
        ConnectedSession::cancellation_handle(self)
    }

    fn execute_params<'a>(
        &'a mut self,
        statement: &'a str,
        params: &'a [String],
        sink: &'a mut dyn ExecutionSink,
    ) -> crate::app::BoxFuture<'a, Result<(), ApplicationError>> {
        ConnectedSession::execute_params(self, statement, params, sink)
    }
}

impl MetadataSession for ManagedSession<'_> {
    fn cancellation_handle(&self) -> Option<std::sync::Arc<dyn SessionCancellation>> {
        ManagedSession::cancellation_handle(self)
    }

    fn execute_params<'a>(
        &'a mut self,
        statement: &'a str,
        params: &'a [String],
        sink: &'a mut dyn ExecutionSink,
    ) -> crate::app::BoxFuture<'a, Result<(), ApplicationError>> {
        Box::pin(ManagedSession::execute_params(
            self, statement, params, sink,
        ))
    }

    fn require_reconnect(&mut self) {
        ManagedSession::require_reconnect(self);
    }
}

async fn load_snapshot_from(session: &mut impl MetadataSession) -> MetadataSnapshot {
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    let (schemas, schemas_stale) =
        load_rows(session, SNAPSHOT_SCHEMAS_SQL, SCHEMA_LIMIT, deadline).await;
    let (relations, relations_stale) =
        load_rows(session, SNAPSHOT_RELATIONS_SQL, RELATION_LIMIT, deadline).await;
    let (columns, columns_stale) =
        load_rows(session, SNAPSHOT_COLUMNS_SQL, COLUMN_LIMIT, deadline).await;
    let (roles, roles_stale) = load_rows(session, SNAPSHOT_ROLES_SQL, ROLE_LIMIT, deadline).await;

    MetadataSnapshot::new(
        schemas
            .into_iter()
            .filter_map(|row| row.first().cloned())
            .collect(),
        relations
            .into_iter()
            .filter_map(|row| Some(RelationName::new(row.first()?.clone(), row.get(1)?.clone())))
            .collect(),
        columns
            .into_iter()
            .filter_map(|row| {
                Some(ColumnName::new(
                    row.first()?.clone(),
                    row.get(1)?.clone(),
                    row.get(2)?.clone(),
                ))
            })
            .collect(),
        roles
            .into_iter()
            .filter_map(|row| {
                row.first().map(|name| {
                    if name == "admin" {
                        DatabaseRole::Admin
                    } else {
                        DatabaseRole::Custom(name.clone())
                    }
                })
            })
            .collect(),
        Some(SystemTime::now()),
        schemas_stale || relations_stale || columns_stale || roles_stale,
    )
}

async fn load_rows(
    session: &mut impl MetadataSession,
    statement: &'static str,
    limit: usize,
    deadline: tokio::time::Instant,
) -> (Vec<Vec<String>>, bool) {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return (Vec::new(), true);
    }

    let cancellation = session.cancellation_handle();
    let mut sink = CatalogSink::new(limit);
    let params = Vec::new();
    let (stale, uncertain) = {
        let execution = session.execute_params(statement, &params, &mut sink);
        tokio::pin!(execution);
        let result = tokio::time::timeout(remaining, &mut execution).await;
        match result {
            Ok(Ok(())) => (false, false),
            Ok(Err(_)) => (true, false),
            Err(_) => {
                if let Some(cancellation) = cancellation {
                    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, cancellation.cancel()).await;
                }
                // The request was already submitted. Wait for that exact request to
                // finish after cancellation, but never let optional completion data
                // block the shell indefinitely. The request is deliberately not replayed.
                let completed = tokio::time::timeout(CLEANUP_TIMEOUT, execution.as_mut())
                    .await
                    .is_ok();
                (true, !completed)
            }
        }
    };
    if uncertain {
        // Catalog queries are fixed read-only work issued only at an idle
        // boundary. Discard an unsettled connection rather than poisoning the
        // interactive shell; the next database command reconnects first.
        session.require_reconnect();
    }
    (sink.rows, stale || sink.truncated)
}

struct CatalogSink {
    rows: Vec<Vec<String>>,
    limit: usize,
    retained_bytes: usize,
    truncated: bool,
}

impl CatalogSink {
    fn new(limit: usize) -> Self {
        Self {
            rows: Vec::with_capacity(limit),
            limit,
            retained_bytes: 0,
            truncated: false,
        }
    }
}

impl ExecutionSink for CatalogSink {
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
        if let ExecutionEvent::Row(values) = event {
            if self.rows.len() >= self.limit {
                self.truncated = true;
                return Ok(());
            }
            let row: Vec<_> = values
                .into_iter()
                .map(|value| value.unwrap_or_default())
                .collect();
            let row_bytes = row.iter().try_fold(0usize, |total, value| {
                (value.len() <= METADATA_FIELD_BYTES_LIMIT)
                    .then(|| total.checked_add(value.len()))
                    .flatten()
            });
            if let Some(row_bytes) = row_bytes
                && self
                    .retained_bytes
                    .checked_add(row_bytes)
                    .is_some_and(|total| total <= METADATA_BYTES_LIMIT)
            {
                self.retained_bytes += row_bytes;
                self.rows.push(row);
            } else {
                self.truncated = true;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        METADATA_FIELD_BYTES_LIMIT, MetadataQuery, is_schema_changing, load_managed_snapshot,
        load_snapshot, relation_pattern,
    };
    use crate::{
        app::{
            BoxFuture, CancellationCapability, ClusterTarget, ConnectedSession, ConnectionIntent,
            DatabaseRole, ExecutionEvent, ExecutionSink, ManagedSession, ReconnectState,
            SessionCancellation, SessionConnector, SessionHandle, SessionMetadata, SessionSetting,
            TransactionState,
        },
        error::ApplicationError,
    };
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::Notify;

    #[test]
    fn patterns_are_bound_and_never_interpolated_into_fixed_sql() {
        let malicious = "orders'; DROP TABLE pg_class; --";
        let query = MetadataQuery::Relations;
        assert_eq!(
            query.params(Some(malicious)),
            vec![relation_pattern(malicious)]
        );
        assert!(!query.sql().contains(malicious));
        assert!(query.sql().contains(r"LIKE $1 ESCAPE '\'"));
    }

    #[test]
    fn shell_patterns_preserve_case_and_escape_like_metacharacters() {
        assert_eq!(relation_pattern("Mixed Case*%_\\"), "Mixed Case%\\%\\_\\\\");
        assert_eq!(MetadataQuery::Schemas.params(None), vec!["%"]);
        assert!(MetadataQuery::Roles.params(None).is_empty());
    }

    #[test]
    fn every_metadata_query_is_a_fixed_text_projection() {
        for query in [
            MetadataQuery::Relations,
            MetadataQuery::Tables,
            MetadataQuery::Schemas,
            MetadataQuery::Roles,
        ] {
            assert!(query.sql().contains("::text"), "{query:?}");
            assert!(!query.sql().contains("{}"), "{query:?}");
        }
    }

    #[test]
    fn schema_change_detection_is_comment_aware_and_conservative() {
        for statement in [
            "CREATE TABLE orders (id bigint);",
            "/* migration */ AlTeR TABLE orders ADD COLUMN total bigint;",
            "-- remove it\nDROP TABLE orders;",
            "TRUNCATE orders;",
            "COMMENT ON TABLE orders IS 'current';",
            "GRANT SELECT ON orders TO reporter;",
            "REVOKE SELECT ON orders FROM reporter;",
            "SECURITY LABEL ON TABLE orders IS 'classified';",
            "AWS IAM GRANT ACCESS TO app_user;",
            "AWS IAM REVOKE ACCESS FROM app_user;",
        ] {
            assert!(is_schema_changing(statement), "{statement}");
        }

        for statement in [
            "SELECT 'CREATE TABLE hidden';",
            "-- DROP TABLE hidden",
            "INSERT INTO audit_log(message) VALUES ('ALTER TABLE');",
            "UPDATE orders SET total = 1;",
            "DELETE FROM orders;",
        ] {
            assert!(!is_schema_changing(statement), "{statement}");
        }
    }

    #[test]
    fn completion_snapshot_queries_are_fixed_and_bounded() {
        for (query, limit) in [
            (super::SNAPSHOT_SCHEMAS_SQL, "LIMIT 256"),
            (super::SNAPSHOT_RELATIONS_SQL, "LIMIT 2048"),
            (super::SNAPSHOT_COLUMNS_SQL, "LIMIT 8192"),
            (super::SNAPSHOT_ROLES_SQL, "LIMIT 256"),
        ] {
            assert!(query.contains(limit));
            assert!(query.contains("::text"));
            assert!(!query.contains('$'));
        }
    }

    #[tokio::test]
    async fn permission_failures_produce_a_partial_stale_snapshot() {
        let session = test_session(vec![
            Action::Error,
            Action::Rows(vec![vec!["public".into(), "orders".into()]]),
            Action::Rows(vec![vec!["public".into(), "orders".into(), "id".into()]]),
            Action::Rows(vec![vec!["reporter".into()]]),
        ]);
        let mut session = session;

        let snapshot = load_snapshot(&mut session).await;
        assert!(snapshot.stale());
        assert!(snapshot.schemas().is_empty());
        assert_eq!(snapshot.relations()[0].relation(), "orders");
        assert_eq!(snapshot.columns()[0].column(), "id");
        assert_eq!(snapshot.roles()[0].name(), "reporter");
    }

    #[tokio::test]
    async fn timeout_cancels_and_awaits_the_original_catalog_execution() {
        let cancellation = Arc::new(TestCancellation::default());
        let session = test_session_with_cancellation(vec![Action::Wait], cancellation.clone());
        let mut session = session;

        let snapshot = load_snapshot(&mut session).await;
        assert!(snapshot.stale());
        assert_eq!(cancellation.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn collecting_sink_caps_rows_even_if_an_adapter_overproduces() {
        let rows = (0..300)
            .map(|index| vec![format!("schema_{index}")])
            .collect();
        let mut session = test_session(vec![
            Action::Rows(rows),
            Action::Rows(Vec::new()),
            Action::Rows(Vec::new()),
            Action::Rows(Vec::new()),
        ]);

        let snapshot = load_snapshot(&mut session).await;
        assert_eq!(snapshot.schemas().len(), 256);
        assert!(snapshot.stale());
    }

    #[tokio::test]
    async fn collecting_sink_rejects_oversized_metadata_fields() {
        let mut session = test_session(vec![
            Action::Rows(vec![vec!["x".repeat(METADATA_FIELD_BYTES_LIMIT + 1)]]),
            Action::Rows(Vec::new()),
            Action::Rows(Vec::new()),
            Action::Rows(Vec::new()),
        ]);

        let snapshot = load_snapshot(&mut session).await;
        assert!(snapshot.schemas().is_empty());
        assert!(snapshot.stale());
    }

    #[tokio::test]
    async fn managed_snapshot_reconnects_once_before_catalog_queries() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let connector = SnapshotConnector {
            attempts: attempts.clone(),
        };
        let mut session = ManagedSession::new(test_session(vec![]), &connector, UNIX_EPOCH);

        let snapshot = load_managed_snapshot(&mut session).await;

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(snapshot.schemas(), &["public"]);
        assert_eq!(snapshot.relations()[0].relation(), "orders");
        assert_eq!(snapshot.columns()[0].column(), "id");
        assert_eq!(snapshot.roles()[0].name(), "reporter");
        assert!(session.take_reconnected());
        assert!(!session.take_reconnected());
    }

    #[tokio::test]
    async fn unconfirmed_managed_snapshot_timeout_requires_an_idle_reconnect() {
        let connector = NoReconnect;
        let initial = ConnectedSession::new(test_metadata(), Box::new(StuckHandle));
        let mut session = ManagedSession::new(initial, &connector, SystemTime::now());

        let snapshot = load_managed_snapshot(&mut session).await;

        assert!(snapshot.stale());
        assert_eq!(session.state(), TransactionState::Idle);
        assert!(session.reconnect_required());
        assert_eq!(
            session.reconnect_state(SystemTime::now()),
            ReconnectState::Required
        );
    }

    enum Action {
        Rows(Vec<Vec<String>>),
        Error,
        Wait,
    }

    struct TestHandle {
        actions: Arc<Mutex<VecDeque<Action>>>,
        cancellation: Arc<TestCancellation>,
    }

    impl SessionHandle for TestHandle {
        fn execute<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(async { Ok(()) })
        }

        fn execute_params<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a [String],
            sink: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            let action = self.actions.lock().expect("actions lock").pop_front();
            let cancellation = self.cancellation.clone();
            Box::pin(async move {
                match action.expect("catalog action") {
                    Action::Rows(rows) => {
                        for row in rows {
                            sink.emit(ExecutionEvent::Row(row.into_iter().map(Some).collect()))?;
                        }
                        Ok(())
                    }
                    Action::Error => Err(ApplicationError::runtime("catalog denied")),
                    Action::Wait => {
                        cancellation.finished.notified().await;
                        Err(ApplicationError::runtime("catalog cancelled"))
                    }
                }
            })
        }

        fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            Some(self.cancellation.clone())
        }
    }

    #[derive(Default)]
    struct TestCancellation {
        calls: AtomicUsize,
        finished: Notify,
    }

    struct SnapshotConnector {
        attempts: Arc<AtomicUsize>,
    }

    impl SessionConnector for SnapshotConnector {
        fn connect<'a>(
            &'a self,
            _: &'a ConnectionIntent,
        ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
            Box::pin(async { Err(ApplicationError::runtime("unexpected direct connect")) })
        }

        fn connect_restoring<'a>(
            &'a self,
            _: &'a ConnectionIntent,
            _: &'a [SessionSetting],
        ) -> BoxFuture<'a, Result<ConnectedSession, ApplicationError>> {
            Box::pin(async move {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                Ok(test_session(vec![
                    Action::Rows(vec![vec!["public".into()]]),
                    Action::Rows(vec![vec!["public".into(), "orders".into()]]),
                    Action::Rows(vec![vec!["public".into(), "orders".into(), "id".into()]]),
                    Action::Rows(vec![vec!["reporter".into()]]),
                ]))
            })
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
    }

    struct StuckHandle;

    impl SessionHandle for StuckHandle {
        fn execute<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(std::future::pending())
        }

        fn execute_params<'a>(
            &'a mut self,
            _: &'a str,
            _: &'a [String],
            _: &'a mut dyn ExecutionSink,
        ) -> BoxFuture<'a, Result<(), ApplicationError>> {
            Box::pin(std::future::pending())
        }

        fn cancellation_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            None
        }
    }

    impl SessionCancellation for TestCancellation {
        fn cancel(&self) -> BoxFuture<'_, Result<(), ApplicationError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.finished.notify_waiters();
            Box::pin(async { Ok(()) })
        }
    }

    fn test_session(actions: Vec<Action>) -> ConnectedSession {
        test_session_with_cancellation(actions, Arc::new(TestCancellation::default()))
    }

    fn test_session_with_cancellation(
        actions: Vec<Action>,
        cancellation: Arc<TestCancellation>,
    ) -> ConnectedSession {
        ConnectedSession::new(
            test_metadata(),
            Box::new(TestHandle {
                actions: Arc::new(Mutex::new(actions.into())),
                cancellation,
            }),
        )
    }

    fn test_metadata() -> SessionMetadata {
        SessionMetadata::new(
            ConnectionIntent::new(
                ClusterTarget::new("cluster-1", "us-east-1", None),
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetadataQuery {
    Relations,
    Tables,
    Schemas,
    Roles,
}

impl MetadataQuery {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::Relations => RELATIONS_SQL,
            Self::Tables => TABLES_SQL,
            Self::Schemas => SCHEMAS_SQL,
            Self::Roles => ROLES_SQL,
        }
    }

    pub(crate) fn params(self, pattern: Option<&str>) -> Vec<String> {
        match self {
            Self::Relations | Self::Tables | Self::Schemas => {
                vec![pattern.map_or_else(|| "%".into(), relation_pattern)]
            }
            Self::Roles => Vec::new(),
        }
    }
}

pub(crate) fn relation_pattern(pattern: &str) -> String {
    let mut value = String::with_capacity(pattern.len());
    for character in pattern.chars() {
        match character {
            '*' => value.push('%'),
            '%' | '_' | '\\' => {
                value.push('\\');
                value.push(character);
            }
            _ => value.push(character),
        }
    }
    value
}

const RELATIONS_SQL: &str = r#"
SELECT n.nspname::text AS "Schema",
       c.relname::text AS "Name",
       CASE c.relkind
           WHEN 'r' THEN 'table'
           WHEN 'p' THEN 'partitioned table'
           WHEN 'v' THEN 'view'
           WHEN 'm' THEN 'materialized view'
           WHEN 'f' THEN 'foreign table'
           WHEN 'S' THEN 'sequence'
           WHEN 'c' THEN 'composite type'
           ELSE c.relkind::text
       END::text AS "Type",
       pg_catalog.pg_get_userbyid(c.relowner)::text AS "Owner"
FROM pg_catalog.pg_class AS c
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S', 'c')
  AND pg_catalog.pg_table_is_visible(c.oid)
  AND c.relname LIKE $1 ESCAPE '\'
ORDER BY 1, 2
"#;

const TABLES_SQL: &str = r#"
SELECT n.nspname::text AS "Schema",
       c.relname::text AS "Name",
       pg_catalog.pg_get_userbyid(c.relowner)::text AS "Owner"
FROM pg_catalog.pg_class AS c
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p')
  AND pg_catalog.pg_table_is_visible(c.oid)
  AND c.relname LIKE $1 ESCAPE '\'
ORDER BY 1, 2
"#;

const SCHEMAS_SQL: &str = r#"
SELECT schema_name::text AS "Name",
       schema_owner::text AS "Owner"
FROM information_schema.schemata
WHERE schema_name <> 'information_schema'
  AND schema_name NOT LIKE 'pg_%'
  AND schema_name LIKE $1 ESCAPE '\'
ORDER BY 1
"#;

const ROLES_SQL: &str = r#"
SELECT rolname::text AS "Role name",
       rolcanlogin::text AS "Can login",
       rolsuper::text AS "Superuser"
FROM pg_catalog.pg_roles
ORDER BY 1
"#;

const SNAPSHOT_SCHEMAS_SQL: &str = r#"
SELECT nspname::text
FROM pg_catalog.pg_namespace
WHERE nspname <> 'information_schema' AND nspname NOT LIKE 'pg_%'
ORDER BY 1
LIMIT 256
"#;

const SNAPSHOT_RELATIONS_SQL: &str = r#"
SELECT n.nspname::text, c.relname::text
FROM pg_catalog.pg_class AS c
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S', 'c')
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1, 2
LIMIT 2048
"#;

const SNAPSHOT_COLUMNS_SQL: &str = r#"
SELECT n.nspname::text, c.relname::text, a.attname::text
FROM pg_catalog.pg_attribute AS a
JOIN pg_catalog.pg_class AS c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE a.attnum > 0 AND NOT a.attisdropped
  AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1, 2, a.attnum
LIMIT 8192
"#;

const SNAPSHOT_ROLES_SQL: &str = r#"
SELECT rolname::text
FROM pg_catalog.pg_roles
ORDER BY 1
LIMIT 256
"#;
