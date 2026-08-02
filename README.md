# Aurora DSQL CLI

`dsql` is a DSQL-native command-line client for Amazon Aurora DSQL. It helps
you discover **clusters**, connect with IAM database authentication, run SQL,
and use an interactive shell with a temporary metrics dashboard. It is inspired
by PostgreSQL shell workflows, not a `psql` compatibility layer.

## Status

Version 1.0.0 is available on
[crates.io](https://crates.io/crates/dsql-cli) and
[GitHub Releases](https://github.com/Ultra-Useful-Utils/dsql-cli/releases).
Live Aurora DSQL validation remains an explicitly authorized release gate. The
client never creates, updates, or deletes a cluster.

## Install

### crates.io

```sh
cargo install dsql-cli --locked
dsql --version
```

### Release archive

Download the archive for your platform and verify it against the published
`SHA256SUMS` before adding `dsql` to `PATH`:

```sh
sha256sum --check SHA256SUMS
tar -xzf dsql-*.tar.gz
./dsql --version
```

On macOS, use `shasum -a 256 -c SHA256SUMS` when GNU `sha256sum` is not
available.

### Build a checkout

Rust 1.94 or later is required. Building and viewing help do not contact AWS.

```sh
cargo build --locked --release
./target/release/dsql --help
```

## Five-minute quickstart

`dsql` uses the AWS SDK default credential chain. Configure a Region through
that chain or pass `--region`; use `--profile` for a shared SDK profile. Cached
SDK-supported IAM Identity Center (SSO) credentials work, but `dsql` does not
perform an SSO login flow.

```sh
# List clusters discoverable by the active AWS identity. Discoverability does
# not imply that the identity can connect.
dsql clusters --region us-east-1

# Open an interactive shell. A cluster selector is a cluster ID, cluster ARN,
# or canonical Aurora DSQL endpoint.
dsql 0123456789abcdefghijklmnop --region us-east-1

# Run SQL without prompts. Noninteractive use requires both a cluster selector
# and a database role.
dsql 0123456789abcdefghijklmnop --region us-east-1 -U app_role \
  -c 'SELECT current_user;'
```

Aurora DSQL has one built-in database, `postgres`, and `dsql` connects to it on
port 5432. A **database role** is the PostgreSQL role supplied as the username;
the predefined `admin` role is elevated. A **custom role** requires its
IAM-to-database-role association as well as connection permission.

## Common commands

```text
dsql [OPTIONS] [CLUSTER]
dsql clusters [OPTIONS]
```

| Command | Use |
| --- | --- |
| `dsql clusters --region REGION` | List discoverable clusters without connecting. |
| `dsql CLUSTER -U ROLE -c 'SQL'` | Run SQL and exit. `-c` may be repeated. |
| `dsql CLUSTER -U ROLE -f query.sql` | Run a UTF-8 SQL file and exit. |
| `dsql CLUSTER -U ROLE < query.sql` | Run semicolon-terminated SQL from standard input. |
| `dsql CLUSTER --format jsonl -U ROLE -c 'SQL'` | Emit machine-readable JSON Lines. |
| `dsql CLUSTER --ssl-root-cert root.pem` | Add a PEM trust anchor without disabling normal TLS validation. |

`table` is the default output format; `csv`, `tsv`, and `jsonl` are also
available. See [the CLI reference](docs/cli.md) for input, exit-code, and
format rules.

## Interactive shell

Without `-c`, `-f`, or redirected standard input, `dsql` starts an
**interactive shell** after connecting. It accepts SQL plus a small, explicit
set of backslash commands:

```text
0123456789abcdefghijklmnop/app_role=> SELECT 1 AS ready;
 ready
-------
 1
(1 row)

0123456789abcdefghijklmnop/app_role=> \conninfo
Cluster: 0123456789abcdefghijklmnop
Region: us-east-1
Database role: app_role

0123456789abcdefghijklmnop/app_role=> \metrics
```

Use `\?` for shell help, `\d`/`\dt`/`\dn`/`\du` for catalog inspection,
`\refresh` to safely refresh completion metadata while idle, and `\q` to
exit. `\metrics` opens the temporary **metrics dashboard** for the connected
cluster; `q` or Esc returns to the shell. See [Interactive shell](docs/interactive-shell.md).

## IAM and security

Start with least privilege:

- `dsql:ListClusters` and `dsql:GetCluster` are needed only for discovery.
- `dsql:DbConnectAdmin` is required for the elevated `admin` database role.
- `dsql:DbConnect` and an IAM-to-database-role association are required for a
  custom database role.
- `cloudwatch:GetMetricData` is needed only for `\metrics`.

The client always verifies TLS certificate chains and hostnames. It has no
plaintext or insecure mode, does not print credentials or authentication
tokens, and does not send telemetry. Read [IAM permissions](docs/iam.md) and
[security and privacy](docs/security.md) before connecting.

## Troubleshooting and documentation

- [CLI reference](docs/cli.md)
- [IAM least-privilege policies](docs/iam.md)
- [Interactive shell](docs/interactive-shell.md)
- [Output formats and JSON Lines schema](docs/output-formats.md)
- [Security and privacy](docs/security.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Testing and live-test policy](docs/testing.md)
- [Manual QA plan](docs/manual-qa.md)
- [Release checklist](docs/release-checklist.md)

## Contributing and testing

Contributions should preserve the terminology in `CONTEXT.md`, update public
documentation with behavior changes, and run the local gates:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

Ignored Docker and live Aurora DSQL tests are opt-in. Never run a live test
without explicit authorization and an appropriate development cluster; see
[testing](docs/testing.md).

## License

Licensed under [Apache-2.0](LICENSE).
