# CLI reference

`dsql` is an Aurora DSQL client, not a general `psql` replacement. A **cluster
selector** is a cluster ID, cluster ARN, or canonical Aurora DSQL endpoint. A
**database role** is the PostgreSQL role used as the connection username.

```text
dsql [OPTIONS] [CLUSTER]
dsql clusters [OPTIONS]
```

## Options

| Option | Meaning |
| --- | --- |
| `--profile PROFILE` | Select an AWS SDK shared profile. |
| `--region REGION` | Select the Region; it has highest precedence. |
| `-U`, `--username ROLE` | Select a database role. `admin` uses the elevated admin authentication token. |
| `-c`, `--command SQL` | Execute SQL and exit; may be repeated. |
| `-f`, `--file PATH` | Execute a UTF-8 SQL file and exit; may be repeated. |
| `--format table|csv|tsv|jsonl` | Choose output; `table` is the default. |
| `--ssl-root-cert PATH` | Add a PEM trust anchor without replacing system roots. |
| `--no-history` | Disable history writes for this interactive shell. |
| `--history-file PATH` | Select an alternate interactive-history file. |
| `--verbose` | Show redacted configuration diagnostics. |
| `--version`, `-h`, `--help` | Print locally without loading AWS configuration. |

`clusters` lists discoverable clusters without connecting. It accepts the
global profile, Region, output-format, and verbose options, but no selector or
SQL input.

## Input, Region, and exits

Bare `dsql` is interactive and can prompt for a Region, discoverable cluster,
and database role. A direct selector avoids discovery, but interactive role
selection remains available. `-c`, `-f`, and redirected standard input are
noninteractive: they never prompt and require all connection inputs to resolve.
Repeated `-c` and `-f` values execute in their original command-line order.
Files and standard input must end in a lexically complete SQL statement.

For an explicit selector, Region resolution is `--region`, then an ARN or
canonical-endpoint Region, then the AWS SDK Region provider chain, then an
interactive prompt. Conflicting explicit and selector Regions are rejected.
A bare cluster ID does not imply a Region.

| Exit code | Meaning |
| --- | --- |
| `0` | Requested work completed. |
| `1` | AWS, discovery, credential, network, TLS, SQL, output, or runtime failure. |
| `2` | Invalid arguments or unresolved required configuration. |
| `130` | User interruption. |

## Interactive shell

The shell accepts SQL and only the following backslash commands:

| Command | Meaning |
| --- | --- |
| `\q` | Exit safely. |
| `\?` | Show supported shell help. |
| `\conninfo` | Show redacted connection context. |
| `\d [PATTERN]`, `\dt [PATTERN]`, `\dn`, `\du` | Inspect visible relations, tables, schemas, and database roles. |
| `\x [on\|off\|auto]` | Set expanded display. |
| `\timing [on\|off]` | Set execution timing output. |
| `\pager [on\|off]` | Set optional paging. |
| `\refresh` | Reconnect safely and refresh completion metadata when no transaction is active. |
| `\metrics` | Open the temporary metrics dashboard. |

Ctrl+C clears idle input or requests cancellation of a query; canceled SQL is
never replayed. Ctrl+D exits only with an empty buffer. The shell refreshes an
idle connection before the Aurora DSQL connection lifetime, but never across an
explicit active, failed, or uncertain transaction. See
[Interactive Shell](interactive-shell.md) for history, completion, and
dashboard behavior.

## Compatibility boundaries

The lexical scanner frames PostgreSQL SQL but does not decide whether Aurora
DSQL supports it. Aurora DSQL remains authoritative, so DSQL syntax such as
`AWS IAM GRANT` passes unchanged. `dsql` does not implement shell escapes,
variables, conditional scripts, `\copy`, startup files, client-side COPY
streaming, plaintext TLS, automatic SQL retries, or broad `psql` compatibility.

CSV and TSV allow one row-producing result per invocation. JSON Lines uses the
frozen version-1 frames in [Output formats](output-formats.md).
