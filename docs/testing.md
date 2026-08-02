# Testing

## Local TLS PostgreSQL tests

The ignored local TLS PostgreSQL tests require Docker. They start a temporary
PostgreSQL 17 container with a generated TLS certificate, publish it only on
the local loopback interface, and remove the container when each test finishes.

Run the tests with:

```sh
cargo test --locked --all-targets --all-features local_tls_postgres -- --ignored
```

This stable filter selects ignored test names that begin with
`local_tls_postgres`. The tests do not require AWS credentials.

The scenarios cover:

- connecting to local PostgreSQL over verified TLS;
- query result streaming, notices, empty parameterized results, cancellation,
  and using a session after cancellation; and
- capturing and safely restoring supported session settings, including values
  requiring quoting, while rejecting unsupported restoration.

The local connector omits initial capture of the Aurora DSQL-only
`disable_sync_create_index` setting because stock PostgreSQL does not expose it.
The suite still exercises the production protocol handle and restoration path;
the opt-in live Aurora DSQL suite owns validation of the complete settings set.

## Live Aurora DSQL tests

The ignored live tests use an existing cluster. They never create, update, or
delete a cluster. Configure an AWS SDK credential source and set these values:

```sh
export AURORA_DSQL_LIVE_TEST=1
export AURORA_DSQL_LIVE_CLUSTER_ID=0123456789abcdefghijklmnop
export AURORA_DSQL_LIVE_REGION=us-east-1
```

`AWS_PROFILE` can select a shared-config profile. The common opt-in, cluster ID,
and Region are validated before the suite loads credentials or calls AWS.

Run the read-only tier with:

```sh
cargo test --locked live_dsql_read_only_discovery_admin_and_metrics -- --ignored
```

This test has a 90-second deadline and validates that the configured cluster is
discoverable and active, the current identity can connect as the admin database
role, and CloudWatch returns the complete 17-series metrics shape. Metrics may
legitimately contain `No data` samples.

Custom-role authentication is a separate release gate. Pre-create the database
role and IAM association, grant the identity `dsql:DbConnect`, then run:

```sh
export AURORA_DSQL_LIVE_CUSTOM_ROLE=app_role
cargo test --locked live_dsql_custom_role_authentication -- --ignored
```

The custom role name must be an unquoted lowercase PostgreSQL identifier. The
test does not create, alter, map, or drop the role.

The mutating tier requires a second explicit authorization:

```sh
export AURORA_DSQL_LIVE_MUTATING=1
export AURORA_DSQL_LIVE_MUTATING_CLUSTER_ID="$AURORA_DSQL_LIVE_CLUSTER_ID"
export AURORA_DSQL_LIVE_ACCOUNT_ID=123456789012
cargo test --locked live_dsql_mutating_occ_ddl_reconnect_and_limits -- --ignored
```

Run it only against a development cluster where the admin role can create and
drop tables in `public`. The separate cluster ID must exactly confirm the common
target, and STS must report the separately confirmed 12-digit account ID. It has
a 180-second scenario deadline and a 30-second cleanup deadline. It validates
DDL completion-metadata invalidation and live catalog refresh, an OC000 conflict
without automatic replay, the 3,000-row transaction limit, and proactive session
refresh with all Aurora DSQL session setting values restored.

The mutating test serializes itself within the test process and creates one
uniquely named `dsql_cli_live_*` table with a unique ownership-marker column in
the same `CREATE TABLE` statement. Cleanup runs after success, error, timeout, or
panic, verifies the marker inside the drop transaction, and refuses to drop an
unmarked object. Abrupt process or host termination can still leave the named
table behind; cleanup failures report its exact name for manual review. The test
never exercises the cluster connection-count or connection-rate quotas.

The live identity needs the applicable permissions documented in
[`iam.md`](iam.md): discovery, `dsql:DbConnectAdmin`, optional
`dsql:DbConnect`, and `cloudwatch:GetMetricData`.

## Scope boundary

Local PostgreSQL validates PostgreSQL wire/protocol semantics only. It does not
validate Aurora DSQL IAM or database authentication, one-hour connection
limits, optimistic concurrency control (OCC), Aurora DSQL-only settings or
catalog behavior, or live CloudWatch metrics.
