# Security and privacy

## Credentials and network boundaries

`dsql` resolves credentials through the AWS SDK default credential chain and
can select a shared profile with `--profile`. It consumes cached credentials
from SDK-supported IAM Identity Center (SSO) profiles; it does not perform an
SSO device login or run the AWS CLI. Token creation is local and is not proof
that a database role is authorized.

The client can call Aurora DSQL discovery APIs, optional STS
`GetCallerIdentity`, the selected cluster's PostgreSQL endpoint, and
CloudWatch `GetMetricData` when `\metrics` is requested. It has no telemetry,
crash upload, dashboard creation, or alarm creation behavior.

## Transport and authentication

Connections always use database `postgres` on port 5432 with certificate-chain
and hostname verification. There is no plaintext, `sslmode=disable`,
`--insecure`, or hostname-bypass mode. `--ssl-root-cert` adds PEM roots to the
normal trust roots; it does not replace them. Keep custom trust-anchor files
readable only by appropriate local users.

The connection token is passed only during PostgreSQL connection setup. It is
not printed, serialized, persisted, or included in diagnostic chains. The
`admin` database role requires the distinct elevated permission documented in
[IAM permissions](iam.md); custom database roles require an IAM-to-database-role
association and `dsql:DbConnect`.

## Local data and terminal safety

Interactive history uses the OS-standard data directory and is owner-only on
Unix. `--no-history` disables all history writes, `--history-file` chooses an
alternate path, and statements beginning with a space are not stored. History
never receives generated connection strings or authentication tokens.

Verbose output is redacted: it does not log credentials, authentication tokens,
signed URLs, SQL text, or result data. Untrusted database and AWS text is
sanitized before terminal rendering so control sequences cannot control the
terminal. Redirected CSV/TSV remains machine data under its documented encoding.
Pagers are spawned without shell evaluation.

## Reporting vulnerabilities

Do not include credentials, tokens, cluster endpoints, SQL containing sensitive
data, or result data in a report. Contact the project maintainer through the
private security channel associated with the source repository and provide a
minimal redacted reproduction. Do not open a public issue for a suspected
credential disclosure, authentication bypass, TLS-verification bypass, or data
corruption issue before maintainers acknowledge it.
